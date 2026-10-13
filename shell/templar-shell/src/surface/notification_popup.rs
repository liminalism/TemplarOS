//! The bottom-right notification host's pure model: presentation rows,
//! duration classes, the bounded stack, deadlines, safe insets and output
//! stickiness.
//!
//! This module touches no D-Bus, owns no window and runs no timers. The
//! shell's [`DataUpdate`](super::app::Wake) feed hands it structured rows
//! parsed from the service's versioned read model; it decides what shows,
//! for how long, and where; the host window in `app.rs` presents that.
//!
//! The service's `Snapshot`/`TransitionsSince` journal stays the authority for active
//! state, expiry and history. Hiding a toast here never deletes history and
//! never dismisses the server row: only an explicit user dismiss/invoke
//! calls back into the service.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use templar_window::{Layer, Output, Rect as WindowRect, ShellToplevel, WindowId};

use crate::surface::bar_screen::BAR_HEIGHT;

/// Compact card width in logical pixels. Must equal
/// `pixelkit_ui::toast::TOAST_WIDTH`; the host window is exactly this wide.
pub const TOAST_CARD_WIDTH: i32 = 380;
/// Logical gap between stacked cards, and around the stack.
pub const TOAST_GAP: i32 = 12;
/// Logical margin from the output's right and bottom safe edges.
pub const TOAST_EDGE_MARGIN: i32 = 12;
/// Logical height of the `+N more in history` strip.
pub const OVERFLOW_STRIP_HEIGHT: i32 = 28;

/// Cards on screen at once. Further arrivals wait rather than covering
/// whatever is being worked on.
pub const MAX_VISIBLE_TOASTS: usize = 3;
/// Arrivals held for a free slot. Beyond this they stay in history with a
/// compact count, not in an unbounded popup queue.
pub const MAX_WAITING_TOASTS: usize = 20;
/// Reviewable queue freshness bound, independent of a card's presentation dwell.
/// Twenty waiters can drain behind three visible five-second cards within this window.
pub const MAX_QUEUE_AGE: Duration = Duration::from_secs(40);

/// Full presentation data from one versioned service snapshot or arrival transition.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ToastPresentation {
    pub id: u32,
    pub event_id: String,
    pub sequence: u64,
    pub app_name: String,
    pub app_id: Option<String>,
    /// Managed source launch, when the service linked the sender's kernel identity.
    #[serde(default)]
    pub source_instance_id: Option<String>,
    pub summary: String,
    pub body: String,
    pub icon: String,
    /// Freedesktop urgency: 0 low, 1 normal, 2 critical.
    pub urgency: u8,
    pub category: String,
    /// Declared `(key, label)` actions in sender order.
    pub actions: Vec<(String, String)>,
    pub created_ms: i64,
    /// Server-computed expiry, wall milliseconds. `None` is no server
    /// expiry (freedesktop `expire_timeout = 0`).
    pub expires_ms: Option<i64>,
}

impl ToastPresentation {
    /// Parse one `ListActive` JSON row. `None` only when the row has no id:
    /// everything else defaults, so a newer or older service row still
    /// presents rather than vanishing.
    pub fn parse_active_json(raw: &str) -> Option<ToastPresentation> {
        let row: serde_json::Value = serde_json::from_str(raw).ok()?;
        let id = u32::try_from(row.get("id")?.as_u64()?).ok()?;
        let actions = row
            .get("actions")
            .and_then(serde_json::Value::as_array)
            .map(|pairs| {
                pairs
                    .iter()
                    .filter_map(|pair| {
                        let key = pair.get(0)?.as_str()?.to_owned();
                        let label = pair.get(1)?.as_str()?.to_owned();
                        Some((key, label))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(ToastPresentation {
            id,
            event_id: row
                .get("event_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            sequence: row
                .get("sequence")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            app_name: row
                .get("app_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Application")
                .to_owned(),
            app_id: row
                .get("app_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            source_instance_id: row
                .get("source_instance_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            summary: row
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Notification")
                .to_owned(),
            body: row
                .get("body")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            icon: row
                .get("icon")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            urgency: row
                .get("urgency")
                .and_then(serde_json::Value::as_u64)
                .and_then(|urgency| u8::try_from(urgency).ok())
                .unwrap_or(1),
            category: row
                .get("category")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            actions,
            created_ms: row
                .get("created_ms")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0),
            expires_ms: row.get("expires_ms").and_then(serde_json::Value::as_i64),
        })
    }

    /// Whether the server already expired this row. Such a row is never
    /// presented: an expired message gets no dwell merely for arriving late.
    pub fn server_expired(&self, now_ms: i64) -> bool {
        self.expires_ms.is_some_and(|expires| expires <= now_ms)
    }
}

/// Notification body activation uses declared default actions first, then an existing
/// source window. It never turns display text into an executable to launch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BodyAction {
    Invoke(String),
    Activate(WindowId),
    Unavailable,
}

pub fn body_action(
    actions: &[(String, String)],
    app_id: Option<&str>,
    source_instance_id: Option<&str>,
    windows: &[ShellToplevel],
    catalogue: &[crate::app_search::AppEntry],
) -> BodyAction {
    if actions.iter().any(|(key, _)| key == "default") {
        return BodyAction::Invoke("default".into());
    }
    let source = source_instance_id.and_then(templar_types::InstanceId::parse);
    // Legacy windows may expose only their desktop ID claim. Permit an exact,
    // unambiguous catalogue stem solely for this explicit user's focus request.
    let claim_app = app_id
        .and_then(|id| catalogue.iter().find(|app| app.id.as_str() == id))
        .filter(|app| {
            !app.desktop_file_id.is_empty()
                && catalogue
                    .iter()
                    .filter(|other| other.desktop_file_id == app.desktop_file_id)
                    .count()
                    == 1
        });
    // A supplied source that disappeared or is malformed must not open a different
    // terminal instance merely because both instances have the same application ID.
    let window = windows
        .iter()
        .filter(|window| matches!(window.layer, Layer::Normal | Layer::AppOnTop))
        .filter(|window| {
            if source_instance_id.is_some() {
                source.is_some()
                    && window.verified_identity
                    && window.instance_id == source
                    && app_id.is_none_or(|app| {
                        window
                            .canonical_app_id
                            .as_ref()
                            .map(templar_types::AppId::as_str)
                            == Some(app)
                    })
            } else {
                app_id.is_some()
                    && (window
                        .canonical_app_id
                        .as_ref()
                        .map(templar_types::AppId::as_str)
                        == app_id
                        || (window.canonical_app_id.is_none()
                            && !window.verified_identity
                            && claim_app.is_some_and(|app| {
                                window.claimed_app_id.as_deref().is_some_and(|claim| {
                                    claim == app.desktop_file_id.trim_end_matches(".desktop")
                                        || claim == app.id.as_str()
                                })
                            })))
            }
        })
        .min_by_key(|window| (window.mru_rank, window.window_id));
    window.map_or(BodyAction::Unavailable, |window| {
        BodyAction::Activate(window.window_id)
    })
}

/// Claim the card body only after PixelKit has handled its controls. The same measured
/// layout excludes dismiss/actions and gaps, so one press cannot dispatch two actions.
pub fn take_card_body_click(
    input: &mut pixelkit_shell::Input,
    card: pixelkit_raster::Rect,
    layout: &pixelkit_ui::toast::ToastLayout,
) -> bool {
    let Some((x, y)) = input.press_position() else {
        return false;
    };
    if layout.dismiss.contains(x, y) || layout.actions.iter().any(|area| area.contains(x, y)) {
        return false;
    }
    input.take_click(card)
}

/// Presentation dwell class. Dwell begins on first presentation, not receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurationClass {
    /// A successful terminal job: 3 s.
    Success,
    /// General information, and anything unrecognised: 4 s.
    Info,
    /// A failure, or anything actionable: 5 s.
    Failure,
}

impl DurationClass {
    /// Classify a row. Unknown producers land on [`DurationClass::Info`]:
    /// caller-supplied names are not trusted identity, so recognition is by
    /// urgency and category only.
    pub fn classify(urgency: u8, category: &str, has_actions: bool) -> DurationClass {
        if urgency >= 2 {
            return DurationClass::Failure;
        }
        let category = category.to_lowercase();
        if category.contains("error") || category.contains("fail") {
            return DurationClass::Failure;
        }
        if has_actions {
            // Actionable: the person needs time to read the choices.
            return DurationClass::Failure;
        }
        if category.contains("success")
            || category.contains("complete")
            || category.contains("finished")
            || category.contains("done")
        {
            return DurationClass::Success;
        }
        DurationClass::Info
    }

    /// Popup dwell for the class.
    pub fn dwell(self) -> Duration {
        match self {
            DurationClass::Success => Duration::from_secs(3),
            DurationClass::Info => Duration::from_secs(4),
            DurationClass::Failure => Duration::from_secs(5),
        }
    }
}

/// Dwell for one presentation row.
pub fn dwell_for(presentation: &ToastPresentation) -> Duration {
    DurationClass::classify(
        presentation.urgency,
        &presentation.category,
        !presentation.actions.is_empty(),
    )
    .dwell()
}

/// Freedesktop critical. Critical toasts may show over fullscreen; ordinary
/// ones defer.
pub fn is_critical(urgency: u8) -> bool {
    urgency >= 2
}

/// Wall milliseconds since the Unix epoch, the unit every `*_ms` service
/// column uses. Mirrors `templar_state::now_ms` without taking the
/// dependency for one clock.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// One card on screen: its row, when it first presented, its pause account
/// and the output it is stuck to until it leaves.
#[derive(Clone, Debug)]
pub struct VisibleToast {
    pub presentation: ToastPresentation,
    pub first_presented: Instant,
    paused_total: Duration,
    pause_started: Option<Instant>,
    /// Output name chosen at promotion. Hotplug/scale re-clamps the rect;
    /// the toast does not migrate outputs mid-dwell.
    pub output: String,
}

impl VisibleToast {
    /// Dwell elapsed, excluding paused time. Hover/keyboard interaction
    /// stops this clock; resuming continues the remainder.
    pub fn elapsed(&self, now: Instant) -> Duration {
        let end = self.pause_started.unwrap_or(now);
        end.saturating_duration_since(self.first_presented)
            .saturating_sub(self.paused_total)
    }

    /// Whether the dwell clock is currently stopped.
    pub fn paused(&self) -> bool {
        self.pause_started.is_some()
    }
}

/// One arrival held for a free visible slot.
#[derive(Clone, Debug)]
struct WaitingToast {
    presentation: ToastPresentation,
    enqueued: Instant,
}

/// The bounded presentation queue: at most [`MAX_VISIBLE_TOASTS`] cards on
/// screen, at most [`MAX_WAITING_TOASTS`] held, a compact count beyond that.
/// Everything excess stays in the service's history; nothing replays stale.
#[derive(Clone, Debug, Default)]
pub struct ToastStack {
    visible: VecDeque<VisibleToast>,
    waiting: VecDeque<WaitingToast>,
    overflow: usize,
    /// Only the current active generations, not a lossy fixed-size ID watermark.
    observed: HashMap<u32, String>,
}

impl ToastStack {
    /// Cards currently on screen, oldest first.
    pub fn visible(&self) -> &VecDeque<VisibleToast> {
        &self.visible
    }

    /// How many cards are on screen.
    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    /// How many arrivals are held for a slot.
    pub fn waiting_len(&self) -> usize {
        self.waiting.len()
    }

    /// Arrivals beyond visible + waiting: still in history, counted here.
    pub fn overflow(&self) -> usize {
        self.overflow
    }

    /// Whether neither a card nor a waiter remains. The host drops its
    /// window and its deadline exactly then.
    pub fn is_empty(&self) -> bool {
        self.visible.is_empty() && self.waiting.is_empty()
    }

    fn remove(&mut self, id: u32) {
        self.visible.retain(|toast| toast.presentation.id != id);
        self.waiting.retain(|toast| toast.presentation.id != id);
    }

    /// Adopt a snapshot as already-seen without presenting anything. The
    /// startup snapshot — and, after a service-owner change, the first
    /// re-read — is history's business, not a burst of fresh arrivals.
    pub fn adopt_snapshot(&mut self, active: &[ToastPresentation]) {
        *self = Self::default();
        self.observed
            .extend(active.iter().map(|row| (row.id, row.event_id.clone())));
    }

    /// Reconcile with the service's live set: closed rows vanish at once,
    /// same-id replacements update in place, unseen ids enqueue oldest
    /// first. Never keyed on a maximum id: a replacement keeps its id, and
    /// a burst carries many.
    pub fn note_active(&mut self, active: &[ToastPresentation], now: Instant, now_ms: i64) {
        let live: Vec<u32> = active.iter().map(|row| row.id).collect();
        // Server close/expiry cancels presentation immediately.
        self.visible
            .retain(|toast| live.contains(&toast.presentation.id));
        self.waiting
            .retain(|toast| live.contains(&toast.presentation.id));
        self.observed.retain(|id, _| live.contains(id));
        // The feed publishes newest first; the queue holds arrival order.
        for row in active.iter().rev() {
            let new_generation = self.observed.get(&row.id) != Some(&row.event_id);
            if new_generation {
                self.observed.insert(row.id, row.event_id.clone());
            }
            if row.server_expired(now_ms) {
                self.remove(row.id);
                continue;
            }
            if let Some(shown) = self
                .visible
                .iter_mut()
                .find(|toast| toast.presentation.id == row.id)
            {
                if new_generation {
                    // Same card, new content: the replacement's dwell starts
                    // on its first presentation, like any arrival's.
                    shown.presentation = row.clone();
                    shown.first_presented = now;
                    shown.paused_total = Duration::ZERO;
                    shown.pause_started = None;
                }
                continue;
            }
            if let Some(held) = self
                .waiting
                .iter_mut()
                .find(|toast| toast.presentation.id == row.id)
            {
                if new_generation {
                    held.presentation = row.clone();
                    held.enqueued = now;
                }
                continue;
            }
            if !new_generation {
                continue;
            }
            // A catch-up batch may arrive before the host gets a chance to fill its free
            // visible slots. Stage those slots too; promote bounds the remaining waiters.
            let pending_capacity =
                MAX_WAITING_TOASTS + MAX_VISIBLE_TOASTS.saturating_sub(self.visible.len());
            if self.waiting.len() < pending_capacity {
                self.waiting.push_back(WaitingToast {
                    presentation: row.clone(),
                    enqueued: now,
                });
            } else {
                self.overflow = self.overflow.saturating_add(1);
            }
        }
        if self.is_empty() {
            self.overflow = 0;
        }
    }

    /// Fill free visible slots from the queue, oldest first. Stale or
    /// server-expired waiters drop without ever presenting. With
    /// `only_critical` (ordinary fullscreen), ordinary waiters hold their
    /// place; critical ones still present.
    pub fn promote(&mut self, now: Instant, now_ms: i64, output: &str, only_critical: bool) {
        let mut index = 0;
        while self.visible.len() < MAX_VISIBLE_TOASTS && index < self.waiting.len() {
            let drop = {
                let waiter = &self.waiting[index];
                waiter.presentation.server_expired(now_ms) || waiter.enqueued + MAX_QUEUE_AGE <= now
            };
            if drop {
                self.waiting.remove(index);
                continue;
            }
            if only_critical && !is_critical(self.waiting[index].presentation.urgency) {
                index += 1;
                continue;
            }
            if let Some(waiter) = self.waiting.remove(index) {
                self.visible.push_back(VisibleToast {
                    presentation: waiter.presentation,
                    first_presented: now,
                    paused_total: Duration::ZERO,
                    pause_started: None,
                    output: output.to_owned(),
                });
            }
        }
        // Fullscreen can prevent the staged visible slots from being used. Excess stays
        // in history, just as it does when the ordinary waiting queue fills.
        while self.waiting.len() > MAX_WAITING_TOASTS {
            self.waiting.pop_back();
            self.overflow = self.overflow.saturating_add(1);
        }
    }

    /// Retire dwell-expired and server-expired cards, and drop stale
    /// waiters. Returns whether anything left. Retirement here is
    /// presentation only: the service row and its history entry are
    /// untouched.
    pub fn retire_expired(&mut self, now: Instant, now_ms: i64) -> bool {
        let mut changed = false;
        let before = self.visible.len();
        self.visible.retain(|toast| {
            let done = toast.elapsed(now) >= dwell_for(&toast.presentation)
                || toast.presentation.server_expired(now_ms);
            !done
        });
        changed |= before != self.visible.len();
        let before = self.waiting.len();
        self.waiting.retain(|waiter| {
            let stale = waiter.presentation.server_expired(now_ms)
                || waiter.enqueued + MAX_QUEUE_AGE <= now;
            !stale
        });
        changed |= before != self.waiting.len();
        if self.is_empty() {
            self.overflow = 0;
        }
        changed
    }

    /// Earliest visible retirement or queued freshness/server-expiry boundary.
    /// Nothing remains scheduled once both visible cards and waiters are gone.
    /// Hover pauses visible dwell only; server expiry and queue freshness keep running.
    pub fn next_deadline(&self, now: Instant, now_ms: i64) -> Option<Instant> {
        self.visible
            .iter()
            .filter_map(|toast| {
                if toast.presentation.server_expired(now_ms) {
                    return Some(now);
                }
                let mut at = None;
                if toast.pause_started.is_none() {
                    at = Some(
                        toast.first_presented + dwell_for(&toast.presentation) + toast.paused_total,
                    );
                }
                if let Some(expires) = toast.presentation.expires_ms {
                    let remaining = expires.saturating_sub(now_ms).max(0) as u64;
                    let server_at = now + Duration::from_millis(remaining);
                    at = Some(at.map_or(server_at, |dwell_at| dwell_at.min(server_at)));
                }
                at
            })
            .chain(self.waiting.iter().map(|waiter| {
                let stale = waiter.enqueued + MAX_QUEUE_AGE;
                waiter.presentation.expires_ms.map_or(stale, |expires| {
                    stale.min(
                        now + Duration::from_millis(expires.saturating_sub(now_ms).max(0) as u64),
                    )
                })
            }))
            .min()
    }

    /// Hide everything without dismissing anything: the lock path. Message
    /// content leaves the screen, painting stops, history is untouched, and
    /// the remembered ids keep unlock from replaying what was hidden.
    pub fn hide_all(&mut self) -> bool {
        let changed = !self.is_empty();
        self.visible.clear();
        self.waiting.clear();
        self.overflow = 0;
        changed
    }

    /// Ordinary fullscreen: ordinary cards step back into the queue (keeping
    /// their age, so a long fullscreen stales them instead of replaying),
    /// critical cards stay. Idempotent: a second call finds only critical
    /// cards visible and moves nothing.
    pub fn hide_ordinary_for_fullscreen(&mut self) {
        let mut kept = VecDeque::new();
        for toast in self.visible.drain(..) {
            if is_critical(toast.presentation.urgency) {
                kept.push_back(toast);
            } else {
                self.waiting.push_front(WaitingToast {
                    presentation: toast.presentation,
                    enqueued: toast.first_presented,
                });
            }
        }
        self.visible = kept;
        while self.waiting.len() > MAX_WAITING_TOASTS {
            if self.waiting.pop_back().is_some() {
                self.overflow = self.overflow.saturating_add(1);
            }
        }
    }

    /// Drop one card by explicit user dismiss. The host then dismisses the
    /// server row; this only clears presentation.
    pub fn dismiss(&mut self, id: u32) -> bool {
        let before = self.visible.len() + self.waiting.len();
        self.remove(id);
        let found = before != self.visible.len() + self.waiting.len();
        if self.is_empty() {
            self.overflow = 0;
        }
        found
    }

    /// Drop one card whose declared action was invoked. The host then
    /// invokes on the service; this only clears presentation.
    pub fn invoke(&mut self, id: u32) -> bool {
        self.dismiss(id)
    }

    /// Stop one card's dwell clock (hover/keyboard). Returns whether the
    /// card is visible.
    pub fn pause(&mut self, id: u32, now: Instant) -> bool {
        if let Some(toast) = self
            .visible
            .iter_mut()
            .find(|toast| toast.presentation.id == id)
        {
            if toast.pause_started.is_none() {
                toast.pause_started = Some(now);
            }
            true
        } else {
            false
        }
    }

    /// Restart one card's dwell clock with its remainder intact.
    pub fn resume(&mut self, id: u32, now: Instant) -> bool {
        if let Some(toast) = self
            .visible
            .iter_mut()
            .find(|toast| toast.presentation.id == id)
        {
            if let Some(started) = toast.pause_started.take() {
                toast.paused_total += now.saturating_duration_since(started);
            }
            true
        } else {
            false
        }
    }
}

/// Whether an ordinary fullscreen window is showing, so ordinary toasts
/// defer. Maximized is ordinary work, not fullscreen; a revealed Home lifts
/// over the windows and the toasts show. Shelved windows cover nothing.
pub fn fullscreen_defers(toplevels: &[ShellToplevel], home_revealed: bool) -> bool {
    !home_revealed
        && toplevels
            .iter()
            .any(|window| window.fullscreen && !window.shelved)
}

/// Bottom inset in logical pixels clearing the nonexclusive bottom bar, the
/// machine cluster in its strip, corner controls and the bottom gesture
/// zones. `Output::usable` cannot see any of those — the bar reserves
/// nothing — so the shell computes this locally instead.
pub fn notification_bottom_inset(bar_present: bool) -> i32 {
    if bar_present {
        BAR_HEIGHT + TOAST_EDGE_MARGIN
    } else {
        TOAST_EDGE_MARGIN
    }
}

/// Bottom-right host rect for a `stack_height`-tall stack inside `usable`,
/// clamped onto tiny outputs rather than hung off-screen.
pub fn notification_host_rect(
    usable: WindowRect,
    stack_height: i32,
    right_inset: i32,
    bottom_inset: i32,
) -> WindowRect {
    let width = TOAST_CARD_WIDTH.clamp(1, usable.width.max(1));
    let height = stack_height.clamp(1, usable.height.max(1));
    let x = usable.x + (usable.width - width - right_inset.max(0)).max(0);
    let y = usable.y + (usable.height - height - bottom_inset.max(0)).max(0);
    WindowRect::new(x, y, width, height)
}

/// Stack height in the host rect's units for measured card heights plus the
/// overflow strip.
pub fn stack_height(card_heights: &[i32], overflow: usize) -> i32 {
    if card_heights.is_empty() && overflow == 0 {
        return 0;
    }
    let mut total: i32 = card_heights.iter().sum();
    if !card_heights.is_empty() {
        total += TOAST_GAP * (card_heights.len() as i32 - 1);
    }
    if overflow > 0 {
        if !card_heights.is_empty() {
            total += TOAST_GAP;
        }
        total += OVERFLOW_STRIP_HEIGHT;
    }
    total.max(1)
}

fn contains(rect: WindowRect, x: i32, y: i32) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

/// Choose the toast output: the focused window's output, else the pointer's.
/// `None` when neither is known — the host then falls back to its own
/// monitor — and always `None` without outputs.
pub fn choose_output(
    outputs: &[Output],
    focused: Option<WindowRect>,
    pointer: Option<(i32, i32)>,
) -> Option<&Output> {
    if outputs.is_empty() {
        return None;
    }
    if let Some(geometry) = focused {
        let center = geometry.center();
        if let Some(output) = outputs
            .iter()
            .find(|output| contains(output.logical, center.x, center.y))
        {
            return Some(output);
        }
        let mut best: Option<&Output> = None;
        let mut best_area = 0;
        for output in outputs {
            let area = output.logical.intersection_area(geometry);
            if area > best_area {
                best_area = area;
                best = Some(output);
            }
        }
        if let Some(output) = best {
            return Some(output);
        }
    }
    if let Some((x, y)) = pointer
        && let Some(output) = outputs.iter().find(|output| contains(output.logical, x, y))
    {
        return Some(output);
    }
    None
}

/// Keep the toast on its output until it leaves: resolve the sticky name
/// against the current topology. `None` is an unplugged output, and the
/// host re-chooses.
pub fn sticky_output<'o>(outputs: &'o [Output], current: &str) -> Option<&'o Output> {
    outputs.iter().find(|output| output.name == current)
}

/// Move a rect wholly into the usable area after a hotplug/scale change.
pub fn reclamp(rect: WindowRect, usable: WindowRect) -> WindowRect {
    let width = rect.width.clamp(1, usable.width.max(1));
    let height = rect.height.clamp(1, usable.height.max(1));
    let x = rect
        .x
        .clamp(usable.x, (usable.x + usable.width - width).max(usable.x));
    let y = rect
        .y
        .clamp(usable.y, (usable.y + usable.height - height).max(usable.y));
    WindowRect::new(x, y, width, height)
}

/// The layer contract the host relies on: Notification draws above ordinary
/// applications and below the presentation overlay, system modals, recovery
/// and the secure lock. Compositor-side, `Layer::Notification` maps to a
/// non-application role, which is what keeps the host out of Alt-Tab, work
/// cards and MRU; shell-side the host never requests keyboard focus.
pub fn notification_layer_is_ordered() -> bool {
    use std::cmp::Ordering;
    Layer::Notification.cmp(&Layer::Normal) == Ordering::Less
        && Layer::Notification.cmp(&Layer::AppOnTop) == Ordering::Less
        && Layer::SecureLock.cmp(&Layer::Notification) == Ordering::Less
        && Layer::Recovery.cmp(&Layer::Notification) == Ordering::Less
        && Layer::SystemModal.cmp(&Layer::Notification) == Ordering::Less
        && Layer::Presentation.cmp(&Layer::Notification) == Ordering::Less
        && Layer::Notification.cmp(&Layer::ShellOverlay) == Ordering::Less
        && templar_window::may_request_layer(
            templar_window::ClientKind::TrustedShell,
            Layer::Notification,
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presentation(id: u32) -> ToastPresentation {
        ToastPresentation {
            id,
            event_id: format!("event-{id}"),
            sequence: u64::from(id),
            app_name: "Terminal".to_owned(),
            app_id: Some("org.templaros.Terminal".to_owned()),
            source_instance_id: None,
            summary: format!("Job {id} finished"),
            body: "elapsed 12s".to_owned(),
            icon: String::new(),
            urgency: 1,
            category: String::new(),
            actions: Vec::new(),
            created_ms: 1_000 + i64::from(id),
            expires_ms: None,
        }
    }

    fn stack_with(count: u32, now: Instant) -> ToastStack {
        let mut stack = ToastStack::default();
        stack.adopt_snapshot(&[]);
        let active: Vec<ToastPresentation> = (1..=count).map(presentation).collect();
        stack.note_active(&active, now, now_ms());
        stack.promote(now, now_ms(), "HDMI-1", false);
        stack
    }

    fn toplevel(fullscreen: bool, maximized: bool, shelved: bool) -> ShellToplevel {
        ShellToplevel {
            window_id: templar_window::WindowId(7),
            canonical_app_id: None,
            claimed_app_id: None,
            instance_id: None,
            title: "video".to_owned(),
            verified_identity: false,
            activated: true,
            mru_rank: 0,
            fullscreen,
            shelved,
            maximized,
            geometry: WindowRect::new(0, 0, 1920, 1080),
            layer: Layer::Normal,
            group: None,
            tab_count: None,
        }
    }

    #[test]
    fn parse_reads_the_full_service_row_and_ignores_unknown_fields() {
        let raw = serde_json::json!({
            "id": 41,
            "app_name": "Build",
            "app_id": "org.example.Build",
            "source_instance_id": "instance-from-service",
            "summary": "Done",
            "body": "42 targets",
            "icon": "build",
            "urgency": 0,
            "category": "transfer.complete",
            "actions": [["open", "Open"], ["dismiss", "Dismiss"]],
            "created_ms": 7_000,
            "expires_ms": 12_000,
            "future_field": {"nested": true},
        });
        let row =
            ToastPresentation::parse_active_json(&raw.to_string()).expect("a full row parses");
        assert_eq!(row.id, 41);
        assert_eq!(row.app_name, "Build");
        assert_eq!(row.app_id.as_deref(), Some("org.example.Build"));
        assert_eq!(
            row.source_instance_id.as_deref(),
            Some("instance-from-service")
        );
        assert_eq!(row.summary, "Done");
        assert_eq!(row.body, "42 targets");
        assert_eq!(row.icon, "build");
        assert_eq!(row.urgency, 0);
        assert_eq!(row.category, "transfer.complete");
        assert_eq!(
            row.actions,
            vec![
                ("open".to_owned(), "Open".to_owned()),
                ("dismiss".to_owned(), "Dismiss".to_owned())
            ]
        );
        assert_eq!(row.created_ms, 7_000);
        assert_eq!(row.expires_ms, Some(12_000));
    }

    #[test]
    fn body_activation_returns_to_the_exact_source_instance_including_parked_windows() {
        let first = templar_types::InstanceId::new();
        let second = templar_types::InstanceId::new();
        let mut original = toplevel(false, false, true);
        original.window_id = WindowId(41);
        original.canonical_app_id = Some("org.example.Terminal".parse().unwrap());
        original.instance_id = Some(first);
        original.verified_identity = true;
        original.mru_rank = 7;
        let mut other = original.clone();
        other.window_id = WindowId(42);
        other.instance_id = Some(second);
        other.shelved = false;
        other.mru_rank = 0;
        let windows = [other.clone(), original];
        assert_eq!(
            body_action(
                &[],
                Some("org.example.Terminal"),
                Some(&first.to_string()),
                &windows,
                &[]
            ),
            BodyAction::Activate(WindowId(41))
        );
        assert_eq!(
            body_action(
                &[],
                Some("org.example.Terminal"),
                Some(&first.to_string()),
                &[other],
                &[]
            ),
            BodyAction::Unavailable,
            "a gone source must not retarget another terminal"
        );
        assert_eq!(
            body_action(
                &[],
                Some("org.example.Terminal"),
                Some("malformed"),
                &windows,
                &[]
            ),
            BodyAction::Unavailable
        );
    }

    #[test]
    fn body_activation_prefers_declared_default_then_canonical_application_recency() {
        let mut first = toplevel(false, false, false);
        first.window_id = WindowId(11);
        first.canonical_app_id = Some("org.example.Mail".parse().unwrap());
        first.mru_rank = 3;
        let mut recent = first.clone();
        recent.window_id = WindowId(12);
        recent.mru_rank = 1;
        let mut misleading = recent.clone();
        misleading.window_id = WindowId(13);
        misleading.mru_rank = 0;
        misleading.canonical_app_id = None;
        misleading.claimed_app_id = Some("org.example.Mail".into());
        let windows = [first, recent, misleading];
        assert_eq!(
            body_action(
                &[("default".into(), "Open".into())],
                Some("org.example.Mail"),
                None,
                &windows,
                &[]
            ),
            BodyAction::Invoke("default".into())
        );
        assert_eq!(
            body_action(
                &[("reply".into(), "Reply".into())],
                Some("org.example.Mail"),
                None,
                &windows,
                &[]
            ),
            BodyAction::Activate(WindowId(12)),
            "nondefault chips remain separate and an unverified raw claim does not retarget"
        );
        assert_eq!(
            body_action(&[], None, None, &windows, &[]),
            BodyAction::Unavailable
        );
    }

    #[test]
    fn unmanaged_terminal_focus_uses_an_unambiguous_installed_desktop_claim() {
        let app = crate::app_search::AppEntry {
            id: "desktop-import.kitty.desktop".parse().unwrap(),
            name: "kitty".into(),
            desktop_file_id: "kitty.desktop".into(),
            icon: String::new(),
            categories: vec!["TerminalEmulator".into()],
            keywords: Vec::new(),
            no_display: false,
            logical: String::new(),
            variants: 1,
        };
        let mut window = toplevel(false, false, false);
        window.window_id = WindowId(27);
        window.canonical_app_id = None;
        window.claimed_app_id = Some("kitty".into());
        let windows = [window];
        assert_eq!(
            body_action(
                &[],
                Some(app.id.as_str()),
                None,
                &windows,
                std::slice::from_ref(&app)
            ),
            BodyAction::Activate(WindowId(27))
        );
        let mut collision = app.clone();
        collision.id = "org.example.OtherKitty".parse().unwrap();
        assert_eq!(
            body_action(
                &[],
                Some(app.id.as_str()),
                None,
                &windows,
                &[app.clone(), collision]
            ),
            BodyAction::Unavailable
        );
        assert_eq!(
            body_action(
                &[],
                Some(app.id.as_str()),
                Some(&templar_types::InstanceId::new().to_string()),
                &windows,
                std::slice::from_ref(&app)
            ),
            BodyAction::Unavailable,
            "a legacy claim never replaces a verified source instance"
        );
    }

    #[test]
    fn notification_body_click_excludes_dismiss_action_controls_and_stack_gaps() {
        use pixelkit_raster::Rect;
        use pixelkit_shell::{Input, MouseButton};
        let layout = pixelkit_ui::toast::ToastLayout {
            height: 120,
            dismiss: Rect::new(340, 10, 28, 28),
            actions: vec![Rect::new(12, 90, 80, 26)],
            body_lines: 1,
            text_width: 310,
        };
        let card = Rect::new(0, 0, 380, 120);
        for (point, expected) in [
            ((20_i16, 50_i16), true),
            ((350, 20), false),
            ((30, 100), false),
            ((20, 125), false),
        ] {
            let mut input = Input::new();
            input.cursor_moved(f32::from(point.0), f32::from(point.1));
            input.mouse(MouseButton::Left, true);
            assert_eq!(take_card_body_click(&mut input, card, &layout), expected);
            if expected {
                assert!(
                    !take_card_body_click(&mut input, card, &layout),
                    "one dispatch per press"
                );
            }
        }
    }

    #[test]
    fn parse_defaults_everything_but_the_id() {
        let row = ToastPresentation::parse_active_json(r#"{"id": 3}"#).expect("minimal row parses");
        assert_eq!(row.app_name, "Application");
        assert_eq!(row.summary, "Notification");
        assert_eq!(row.urgency, 1);
        assert!(row.body.is_empty() && row.actions.is_empty());
        assert_eq!(row.expires_ms, None);
        assert!(ToastPresentation::parse_active_json(r#"{"summary": "no id"}"#).is_none());
        assert!(ToastPresentation::parse_active_json("not json").is_none());
        // A null app id is absence, not the string "null".
        let row = ToastPresentation::parse_active_json(r#"{"id": 4, "app_id": null}"#).unwrap();
        assert_eq!(row.app_id, None);
    }

    #[test]
    fn identical_content_replacement_is_a_new_generation() {
        let now = Instant::now();
        let mut stack = stack_with(1, now);
        let mut row = presentation(1);
        row.event_id = "replacement".into();
        row.expires_ms = Some(now_ms() + 60_000);
        let later = now + Duration::from_secs(3);
        stack.note_active(&[row.clone()], later, now_ms());
        assert_eq!(stack.visible()[0].presentation, row);
        assert_eq!(stack.visible()[0].first_presented, later);
    }

    #[test]
    fn duration_classes_default_to_info_when_unknown() {
        assert_eq!(DurationClass::classify(1, "", false), DurationClass::Info);
        assert_eq!(
            DurationClass::classify(1, "some.new.producer", false),
            DurationClass::Info,
            "an unrecognised category is informational, never trusted success"
        );
        assert_eq!(
            DurationClass::classify(2, "", false),
            DurationClass::Failure
        );
        assert_eq!(
            DurationClass::classify(1, "transfer.error", false),
            DurationClass::Failure
        );
        assert_eq!(
            DurationClass::classify(1, "job.failed", false),
            DurationClass::Failure
        );
        assert_eq!(
            DurationClass::classify(1, "", true),
            DurationClass::Failure,
            "actionable rows hold still long enough to read the choices"
        );
        assert_eq!(
            DurationClass::classify(1, "transfer.complete", false),
            DurationClass::Success
        );
        assert_eq!(
            DurationClass::classify(0, "job.success", false),
            DurationClass::Success
        );
        // Failure wins over success markers: an error with actions is still an error.
        assert_eq!(
            DurationClass::classify(1, "job.success", true),
            DurationClass::Failure
        );
    }

    #[test]
    fn dwell_is_three_four_five_seconds_by_class() {
        assert_eq!(DurationClass::Success.dwell(), Duration::from_secs(3));
        assert_eq!(DurationClass::Info.dwell(), Duration::from_secs(4));
        assert_eq!(DurationClass::Failure.dwell(), Duration::from_secs(5));
    }

    #[test]
    fn the_startup_snapshot_adopts_without_presenting() {
        let mut stack = ToastStack::default();
        let active = vec![presentation(1), presentation(2)];
        stack.adopt_snapshot(&active);
        assert!(stack.is_empty(), "old news is the history tile's business");
        // And the same rows arriving again are already-seen, not fresh.
        let now = Instant::now();
        stack.note_active(&active, now, now_ms());
        stack.promote(now, now_ms(), "HDMI-1", false);
        assert!(stack.is_empty(), "adopted rows never toast");
    }

    #[test]
    fn arrivals_enqueue_oldest_first_and_close_cancels_at_once() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.adopt_snapshot(&[]);
        // Newest first, as ListActive orders them.
        let active = vec![presentation(3), presentation(2), presentation(1)];
        stack.note_active(&active, now, now_ms());
        stack.promote(now, now_ms(), "HDMI-1", false);
        let ids: Vec<u32> = stack
            .visible()
            .iter()
            .map(|toast| toast.presentation.id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3], "the queue holds arrival order");
        // The server closed id 2: its card leaves without waiting out its dwell.
        stack.note_active(&[presentation(3), presentation(1)], now, now_ms());
        let ids: Vec<u32> = stack
            .visible()
            .iter()
            .map(|toast| toast.presentation.id)
            .collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn same_id_replacement_updates_in_place_and_restarts_dwell() {
        let first = Instant::now();
        let mut stack = stack_with(1, first);
        assert_eq!(stack.visible_len(), 1);
        let later = first + Duration::from_secs(3);
        let mut replaced = presentation(1);
        replaced.body = "elapsed 15s, still going".to_owned();
        replaced.event_id = "replacement".into();
        stack.note_active(&[replaced.clone()], later, now_ms());
        assert_eq!(stack.visible_len(), 1, "no second card for a replacement");
        assert_eq!(stack.waiting_len(), 0);
        let shown = &stack.visible()[0];
        assert_eq!(shown.presentation.body, "elapsed 15s, still going");
        assert_eq!(
            shown.first_presented, later,
            "new content's dwell starts now"
        );
        // An unchanged re-fetch touches nothing.
        stack.note_active(&[replaced], later + Duration::from_secs(1), now_ms());
        assert_eq!(stack.visible()[0].first_presented, later);
    }

    #[test]
    fn the_stack_bounds_visible_and_waiting_with_a_compact_count() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.adopt_snapshot(&[]);
        // Newest first, as ListActive orders them.
        let newest_first: Vec<ToastPresentation> = (1..=30).rev().map(presentation).collect();
        stack.note_active(&newest_first, now, now_ms());
        assert_eq!(stack.waiting_len(), MAX_WAITING_TOASTS + MAX_VISIBLE_TOASTS);
        assert_eq!(
            stack.overflow(),
            7,
            "arrivals beyond visible and waiting capacity stay in history"
        );
        stack.promote(now, now_ms(), "HDMI-1", false);
        assert_eq!(stack.visible_len(), MAX_VISIBLE_TOASTS);
        assert_eq!(stack.waiting_len(), MAX_WAITING_TOASTS);
        // Retiring one card promotes the oldest waiter, in place.
        stack.dismiss(stack.visible()[0].presentation.id);
        stack.promote(now, now_ms(), "HDMI-1", false);
        assert_eq!(stack.visible_len(), MAX_VISIBLE_TOASTS);
        assert_eq!(stack.waiting_len(), MAX_WAITING_TOASTS - 1);
    }

    #[test]
    fn server_expiry_is_never_resurrected_for_dwell() {
        let now = Instant::now();
        let wall = now_ms();
        let mut stack = ToastStack::default();
        stack.adopt_snapshot(&[]);
        let mut expired = presentation(1);
        expired.expires_ms = Some(wall - 1);
        stack.note_active(&[expired], now, wall);
        stack.promote(now, wall, "HDMI-1", false);
        assert!(stack.is_empty(), "already-expired rows never present");
        // And an expiry that lands mid-dwell cancels the card early.
        let mut live = presentation(2);
        live.expires_ms = Some(wall + 60_000);
        stack.note_active(&[live.clone()], now, wall);
        stack.promote(now, wall, "HDMI-1", false);
        assert_eq!(stack.visible_len(), 1);
        live.expires_ms = Some(wall - 1);
        stack.note_active(&[live], now, wall);
        assert!(
            stack.is_empty(),
            "server expiry cancels presentation at once"
        );
    }

    #[test]
    fn no_server_expiry_means_dwell_alone() {
        let now = Instant::now();
        let wall = now_ms();
        let mut row = presentation(1);
        row.expires_ms = None;
        let mut stack = ToastStack::default();
        stack.adopt_snapshot(&[]);
        stack.note_active(&[row], now, wall);
        stack.promote(now, wall, "HDMI-1", false);
        assert_eq!(
            stack.next_deadline(now, wall),
            Some(now + Duration::from_secs(4)),
            "persistent rows still retire their popup on the Info dwell"
        );
        assert!(!stack.retire_expired(now + Duration::from_secs(3), wall));
        assert!(stack.retire_expired(now + Duration::from_secs(4), wall));
    }

    #[test]
    fn next_deadline_is_the_earliest_unpaused_card() {
        let first = Instant::now();
        let mut stack = stack_with(2, first);
        let wall = now_ms();
        assert_eq!(
            stack.next_deadline(first, wall),
            Some(first + Duration::from_secs(4))
        );
        // Pausing the earliest card moves the deadline to the next one.
        let earliest = stack.visible()[0].presentation.id;
        assert!(stack.pause(earliest, first + Duration::from_secs(1)));
        assert_eq!(
            stack.next_deadline(first + Duration::from_secs(1), wall),
            Some(first + Duration::from_secs(4)),
            "the unpaused card still retires on time"
        );
        assert!(stack.retire_expired(first + Duration::from_secs(4), wall));
        assert_eq!(stack.visible_len(), 1, "the paused card holds");
        assert!(
            stack
                .next_deadline(first + Duration::from_secs(4), wall)
                .is_none(),
            "nothing unpaused, nothing scheduled"
        );
    }

    #[test]
    fn pause_freezes_dwell_and_resume_continues_the_remainder() {
        let first = Instant::now();
        let mut stack = stack_with(1, first);
        let id = stack.visible()[0].presentation.id;
        let wall = now_ms();
        assert!(stack.pause(id, first + Duration::from_secs(3)));
        assert!(
            !stack.retire_expired(first + Duration::from_secs(30), wall),
            "a hovered card does not retire under the pointer"
        );
        assert!(stack.resume(id, first + Duration::from_secs(30)));
        assert!(
            !stack.retire_expired(first + Duration::from_secs(30), wall),
            "one second of dwell remains after the resume"
        );
        assert!(stack.retire_expired(first + Duration::from_secs(31), wall));
        assert!(!stack.pause(999, first), "unknown ids pause nothing");
        assert!(!stack.resume(999, first), "unknown ids resume nothing");
    }

    #[test]
    fn waiters_that_outlive_their_moment_never_present() {
        let first = Instant::now();
        let wall = now_ms();
        let mut stack = ToastStack::default();
        stack.adopt_snapshot(&[]);
        // Fill the visible slots, then queue one more behind them.
        let active: Vec<ToastPresentation> = (1..=4).map(presentation).collect();
        let mut newest_first = active;
        newest_first.reverse();
        stack.note_active(&newest_first, first, wall);
        stack.promote(first, wall, "HDMI-1", false);
        assert_eq!(stack.waiting_len(), 1);
        // The visible cards hold (paused); the waiter ages out unseen.
        for toast in stack
            .visible()
            .iter()
            .map(|toast| toast.presentation.id)
            .collect::<Vec<_>>()
        {
            stack.pause(toast, first);
        }
        let late = first + MAX_QUEUE_AGE;
        assert!(stack.retire_expired(late, wall));
        assert_eq!(
            stack.waiting_len(),
            0,
            "a stale waiter drops, never presents"
        );
        assert_eq!(stack.visible_len(), 3);
    }

    #[test]
    fn lock_hides_everything_and_unlock_replays_nothing() {
        let first = Instant::now();
        let wall = now_ms();
        let mut stack = ToastStack::default();
        stack.adopt_snapshot(&[]);
        let active: Vec<ToastPresentation> = (1..=5).map(presentation).collect();
        let mut newest_first = active.clone();
        newest_first.reverse();
        stack.note_active(&newest_first, first, wall);
        stack.promote(first, wall, "HDMI-1", false);
        assert!(!stack.is_empty());
        assert!(stack.hide_all(), "hiding reports that it hid something");
        assert!(stack.is_empty());
        assert_eq!(stack.overflow(), 0);
        // The same live set after unlock: remembered, not replayed.
        stack.note_active(&newest_first, first + Duration::from_secs(60), wall);
        stack.promote(first + Duration::from_secs(60), wall, "HDMI-1", false);
        assert!(stack.is_empty(), "no stale replay after unlock");
        // But genuinely new arrivals still present.
        let mut fresh = active;
        fresh.push(presentation(99));
        let mut newest_first = fresh;
        newest_first.reverse();
        stack.note_active(&newest_first, first + Duration::from_secs(61), wall);
        stack.promote(first + Duration::from_secs(61), wall, "HDMI-1", false);
        assert_eq!(stack.visible_len(), 1);
        assert_eq!(stack.visible()[0].presentation.id, 99);
    }

    #[test]
    fn fullscreen_defers_ordinary_cards_and_keeps_critical_ones() {
        let first = Instant::now();
        let wall = now_ms();
        let mut stack = ToastStack::default();
        stack.adopt_snapshot(&[]);
        let mut critical = presentation(1);
        critical.urgency = 2;
        let ordinary = presentation(2);
        stack.note_active(&[ordinary, critical], first, wall);
        stack.promote(first, wall, "HDMI-1", false);
        assert_eq!(stack.visible_len(), 2);
        stack.hide_ordinary_for_fullscreen();
        assert_eq!(stack.visible_len(), 1);
        assert_eq!(stack.visible()[0].presentation.id, 1, "critical stays");
        assert_eq!(stack.waiting_len(), 1, "ordinary defers");
        stack.hide_ordinary_for_fullscreen();
        assert_eq!(stack.visible_len(), 1, "the second call moves nothing");
        // Fullscreen exits quickly: the deferred card presents.
        stack.promote(first + Duration::from_secs(1), wall, "HDMI-1", false);
        assert_eq!(stack.visible_len(), 2);
    }

    #[test]
    fn a_long_fullscreen_stales_deferred_cards_instead_of_replaying() {
        let first = Instant::now();
        let wall = now_ms();
        let mut stack = stack_with(1, first);
        stack.hide_ordinary_for_fullscreen();
        assert_eq!(stack.waiting_len(), 1);
        let late = first + MAX_QUEUE_AGE;
        stack.retire_expired(late, wall);
        stack.promote(late, wall, "HDMI-1", false);
        assert!(stack.is_empty(), "no stale replay after fullscreen");
    }

    #[test]
    fn service_restart_replays_neither_same_rows_nor_same_content() {
        let first = Instant::now();
        let wall = now_ms();
        let mut stack = stack_with(2, first);
        // The service restarts and re-lists the same rows: already visible,
        // so nothing new enqueues.
        let active = vec![presentation(2), presentation(1)];
        stack.note_active(&active, first + Duration::from_secs(1), wall);
        assert_eq!(stack.waiting_len(), 0);
        // Retired rows that the restarted service still lists stay retired.
        stack.retire_expired(first + Duration::from_secs(30), wall);
        assert!(stack.is_empty());
        stack.note_active(&active, first + Duration::from_secs(31), wall);
        stack.promote(first + Duration::from_secs(31), wall, "HDMI-1", false);
        assert!(stack.is_empty(), "retired rows do not toast twice");
    }

    #[test]
    fn dismiss_and_invoke_clear_presentation_only() {
        let now = Instant::now();
        let mut stack = stack_with(2, now);
        let id = stack.visible()[0].presentation.id;
        assert!(stack.dismiss(id));
        assert!(!stack.dismiss(id), "dismissing twice finds nothing");
        let id = stack.visible()[0].presentation.id;
        assert!(stack.invoke(id));
        assert!(stack.is_empty());
    }

    #[test]
    fn only_fullscreen_defers_and_home_reveal_lifts_the_deferral() {
        assert!(
            !fullscreen_defers(&[], false),
            "a bare desktop defers nothing"
        );
        assert!(fullscreen_defers(&[toplevel(true, false, false)], false));
        assert!(
            !fullscreen_defers(&[toplevel(true, false, false)], true),
            "a revealed Home lifts over the fullscreen window"
        );
        assert!(
            !fullscreen_defers(&[toplevel(false, true, false)], false),
            "maximized is ordinary work, not fullscreen"
        );
        assert!(
            !fullscreen_defers(&[toplevel(true, false, true)], false),
            "a shelved window covers nothing"
        );
        assert!(!fullscreen_defers(&[toplevel(false, false, false)], false));
    }

    #[test]
    fn the_host_rect_sits_bottom_right_clear_of_the_bar() {
        let usable = WindowRect::new(0, 0, 1920, 1080);
        let rect = notification_host_rect(
            usable,
            300,
            TOAST_EDGE_MARGIN,
            notification_bottom_inset(true),
        );
        assert_eq!(rect.width, TOAST_CARD_WIDTH);
        assert_eq!(rect.height, 300);
        assert_eq!(rect.x + rect.width, 1920 - TOAST_EDGE_MARGIN);
        assert_eq!(
            rect.y + rect.height,
            1080 - BAR_HEIGHT - TOAST_EDGE_MARGIN,
            "the stack ends above the bar strip, not in it"
        );
        assert_eq!(
            notification_bottom_inset(false),
            TOAST_EDGE_MARGIN,
            "without a bar only the edge margin remains"
        );
    }

    #[test]
    fn the_host_rect_clamps_onto_tiny_outputs() {
        let usable = WindowRect::new(0, 0, 200, 100);
        let rect = notification_host_rect(usable, 900, TOAST_EDGE_MARGIN, TOAST_EDGE_MARGIN);
        assert!(rect.width <= 200 && rect.height <= 100);
        assert!(rect.x >= 0 && rect.y >= 0);
        assert!(rect.x + rect.width <= 200 && rect.y + rect.height <= 100);
    }

    #[test]
    fn stack_height_counts_cards_gaps_and_the_overflow_strip() {
        assert_eq!(stack_height(&[], 0), 0);
        assert_eq!(stack_height(&[100], 0), 100);
        assert_eq!(stack_height(&[100, 120], 0), 100 + TOAST_GAP + 120);
        assert_eq!(
            stack_height(&[100, 120], 7),
            100 + TOAST_GAP + 120 + TOAST_GAP + OVERFLOW_STRIP_HEIGHT
        );
    }

    fn outputs() -> Vec<Output> {
        vec![
            Output::new("HDMI-1", WindowRect::new(0, 0, 1920, 1080), 1000),
            Output::new("DP-1", WindowRect::new(1920, 0, 1920, 1080), 1000),
        ]
    }

    #[test]
    fn output_choice_prefers_focus_then_pointer() {
        let outputs = outputs();
        let focused = WindowRect::new(2000, 100, 800, 600);
        assert_eq!(
            choose_output(&outputs, Some(focused), Some((100, 100))).map(|o| o.name.as_str()),
            Some("DP-1"),
            "the focused window's output wins over the pointer's"
        );
        assert_eq!(
            choose_output(&outputs, None, Some((100, 100))).map(|o| o.name.as_str()),
            Some("HDMI-1"),
            "without focus the pointer's output answers"
        );
        assert_eq!(
            choose_output(&outputs, None, None),
            None,
            "without either the host falls back to its own monitor"
        );
        assert_eq!(choose_output(&[], Some(focused), Some((0, 0))), None);
    }

    #[test]
    fn sticky_output_holds_by_name_and_hotplug_falls_back() {
        let outputs = outputs();
        assert_eq!(
            sticky_output(&outputs, "DP-1").map(|o| o.name.as_str()),
            Some("DP-1")
        );
        let unplugged: Vec<Output> = outputs.into_iter().take(1).collect();
        assert_eq!(
            sticky_output(&unplugged, "DP-1"),
            None,
            "an unplugged output makes the host re-choose"
        );
    }

    #[test]
    fn reclamp_keeps_the_stack_on_screen_after_hotplug_or_scale() {
        let usable = WindowRect::new(0, 0, 1280, 720);
        let rect = reclamp(WindowRect::new(1500, 900, 380, 300), usable);
        assert!(rect.x + rect.width <= 1280 && rect.y + rect.height <= 720);
        assert!(rect.x >= 0 && rect.y >= 0);
        let kept = reclamp(WindowRect::new(100, 100, 380, 300), usable);
        assert_eq!(
            kept,
            WindowRect::new(100, 100, 380, 300),
            "inside stays put"
        );
    }

    #[test]
    fn the_notification_layer_sits_above_apps_below_modals() {
        use std::cmp::Ordering;
        // Lesser draws above: this is the `Layer::depth` order, not the wire value.
        assert_eq!(Layer::Notification.cmp(&Layer::Normal), Ordering::Less);
        assert_eq!(Layer::Notification.cmp(&Layer::AppOnTop), Ordering::Less);
        assert_eq!(Layer::Notification.cmp(&Layer::HomeSurface), Ordering::Less);
        assert_eq!(Layer::SecureLock.cmp(&Layer::Notification), Ordering::Less);
        assert_eq!(Layer::Recovery.cmp(&Layer::Notification), Ordering::Less);
        assert_eq!(Layer::SystemModal.cmp(&Layer::Notification), Ordering::Less);
        assert_eq!(
            Layer::Presentation.cmp(&Layer::Notification),
            Ordering::Less
        );
        assert_eq!(
            Layer::Notification.cmp(&Layer::ShellOverlay),
            Ordering::Less
        );
        assert!(notification_layer_is_ordered());
    }

    #[test]
    fn card_width_matches_the_pixelkit_compact_card() {
        assert_eq!(TOAST_CARD_WIDTH, pixelkit_ui::toast::TOAST_WIDTH);
    }

    /// Headless render: a three-card stack plus the overflow strip draws
    /// into an ordinary buffer with every card inside it.
    #[test]
    fn headless_stack_renders_every_card_inside_the_host() {
        use pixelkit_raster::{Painter, RasterKernel, WindowBuffer};
        use pixelkit_shell::{Input, Scale};

        let now = Instant::now();
        let wall = now_ms();
        let mut stack = ToastStack::default();
        stack.adopt_snapshot(&[]);
        let mut active: Vec<ToastPresentation> = (1..=30).map(presentation).collect();
        active[0].actions = vec![("open".to_owned(), "Open".to_owned())];
        active[1].body = "a longer body with several words to wrap".to_owned();
        active.reverse();
        stack.note_active(&active, now, wall);
        stack.promote(now, wall, "HDMI-1", false);
        assert_eq!(stack.visible_len(), 3);
        assert!(stack.overflow() > 0);

        let mut buffer = WindowBuffer::new(380, 800);
        let mut text = pixelkit_text::TextCache::new(crate::surface::fonts::set());
        let mut input = Input::new();
        let mut kernel = RasterKernel::new();
        let mut heights = Vec::new();
        let mut ui = pixelkit_ui::Ui::new(
            Painter::new(&mut buffer),
            &mut text,
            &mut input,
            crate::surface::design::theme(),
            Scale::ONE,
            &mut kernel,
        );
        let mut y = 0;
        for toast in stack.visible() {
            let row = &toast.presentation;
            let actions: Vec<pixelkit_ui::toast::ToastAction> = row
                .actions
                .iter()
                .map(|(key, label)| pixelkit_ui::toast::ToastAction { key, label })
                .collect();
            let card = pixelkit_ui::toast::ToastCard {
                app: &row.app_name,
                summary: &row.summary,
                body: &row.body,
                icon: None,
                actions: &actions,
                dismiss_label: "Dismiss",
            };
            let (outcome, layout) = ui.toast((0, y), 380, &card);
            assert!(outcome.is_none(), "a render with no input asks for nothing");
            heights.push(layout.height);
            y += layout.height + TOAST_GAP;
        }
        let host = stack_height(&heights, stack.overflow());
        assert!(host <= 800, "the measured stack fits its host: {host}px");
        drop(ui);
        let ink = buffer.pixels.iter().filter(|pixel| **pixel != 0).count();
        assert!(ink > 5_000, "three cards paint their summaries and bodies");
    }

    #[test]
    fn large_snapshot_and_overflow_are_never_replayed() {
        let now = Instant::now();
        let active: Vec<_> = (1..=100).rev().map(presentation).collect();
        let mut stack = ToastStack::default();
        stack.adopt_snapshot(&active);
        stack.note_active(&active, now, now_ms());
        stack.promote(now, now_ms(), "HDMI-1", false);
        assert!(
            stack.is_empty(),
            "snapshot adoption must not forget rows beyond 64"
        );
        stack.adopt_snapshot(&[]);
        stack.note_active(&active, now, now_ms());
        let overflow = stack.overflow();
        stack.note_active(&active, now, now_ms());
        assert_eq!(
            stack.overflow(),
            overflow,
            "a replay cannot inflate overflow"
        );
        stack.hide_all();
        stack.note_active(&active, now, now_ms());
        stack.promote(now, now_ms(), "HDMI-1", false);
        assert!(stack.is_empty(), "overflow remains history after unlock");
    }

    #[test]
    fn replacement_of_a_retired_id_is_a_fresh_arrival() {
        let now = Instant::now();
        let mut stack = stack_with(1, now);
        stack.retire_expired(now + Duration::from_secs(5), now_ms());
        let mut replacement = presentation(1);
        replacement.event_id = "next-generation".into();
        stack.note_active(&[replacement], now + Duration::from_secs(6), now_ms());
        stack.promote(now + Duration::from_secs(6), now_ms(), "HDMI-1", false);
        assert_eq!(stack.visible_len(), 1);
    }

    #[test]
    fn fullscreen_waiters_have_a_deadline_without_a_host() {
        let now = Instant::now();
        let mut stack = ToastStack::default();
        stack.note_active(&[presentation(1)], now, now_ms());
        stack.promote(now, now_ms(), "HDMI-1", true);
        assert_eq!(stack.visible_len(), 0);
        let deadline = now + MAX_QUEUE_AGE;
        assert_eq!(stack.next_deadline(now, now_ms()), Some(deadline));
        assert!(!stack.retire_expired(deadline - Duration::from_millis(1), now_ms()));
        let mut just_in_time = stack.clone();
        just_in_time.promote(
            deadline - Duration::from_millis(1),
            now_ms(),
            "HDMI-1",
            false,
        );
        assert_eq!(
            just_in_time.visible_len(),
            1,
            "fresh at the last instant before the bound"
        );
        assert_eq!(
            just_in_time.next_deadline(deadline, now_ms()),
            Some(deadline - Duration::from_millis(1) + DurationClass::Info.dwell()),
            "a fresh promotion gets its full presentation dwell, not the queue remainder",
        );
        stack.promote(deadline, now_ms(), "HDMI-1", false);
        assert!(stack.is_empty());
        assert_eq!(stack.next_deadline(now, now_ms()), None);
    }

    #[test]
    fn an_atomic_full_burst_drains_in_order_with_full_first_presentation_dwell() {
        let first = Instant::now();
        let wall = now_ms();
        let count = (MAX_VISIBLE_TOASTS + MAX_WAITING_TOASTS) as u32;
        let active: Vec<_> = (1..=count)
            .rev()
            .map(|id| {
                let mut row = presentation(id);
                row.category = "job.failed".into();
                row
            })
            .collect();
        let mut stack = ToastStack::default();
        stack.note_active(&active, first, wall);
        stack.promote(first, wall, "HDMI-1", false);
        assert_eq!(stack.visible_len(), MAX_VISIBLE_TOASTS);
        assert_eq!(stack.waiting_len(), MAX_WAITING_TOASTS);
        assert_eq!(stack.overflow(), 0);
        let mut presented = Vec::new();
        let mut now = first;
        while !stack.is_empty() {
            for toast in stack.visible() {
                assert_eq!(toast.first_presented, now);
                presented.push(toast.presentation.id);
            }
            let end = now + DurationClass::Failure.dwell();
            let before_end = end - Duration::from_millis(1);
            assert!(!stack.retire_expired(before_end, wall));
            assert!(stack.retire_expired(end, wall));
            stack.promote(end, wall, "HDMI-1", false);
            now = end;
        }
        assert_eq!(presented, (1..=count).collect::<Vec<_>>());
        assert_eq!(stack.next_deadline(now, wall), None);
    }

    #[test]
    fn queued_server_expiry_and_freshness_use_the_earlier_boundary() {
        let first = Instant::now();
        let wall = now_ms();
        for server_lifetime in [Duration::from_secs(5), Duration::from_secs(60)] {
            let mut row = presentation(1);
            row.expires_ms = Some(wall + server_lifetime.as_millis() as i64);
            let mut stack = ToastStack::default();
            stack.note_active(&[row], first, wall);
            stack.promote(first, wall, "HDMI-1", true);
            let age = server_lifetime.min(MAX_QUEUE_AGE);
            let deadline = first + age;
            let deadline_wall = wall + age.as_millis() as i64;
            assert_eq!(stack.next_deadline(first, wall), Some(deadline));
            assert!(!stack.retire_expired(deadline - Duration::from_millis(1), deadline_wall - 1));
            assert!(stack.retire_expired(deadline, deadline_wall));
            stack.promote(deadline, deadline_wall, "HDMI-1", false);
            assert!(
                stack.is_empty(),
                "neither stale nor expired queued content may replay"
            );
            assert_eq!(stack.next_deadline(deadline, deadline_wall), None);
        }
    }
}

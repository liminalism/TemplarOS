//! Compositor-drawn chrome: a window's name and control tags, the resource-pressure overlay, the
//! "show every window" scatter labels, the fallback switcher and the lock screen, rendered with
//! `pixelkit-raster`/`pixelkit-text` (root `Cargo.toml`'s `[workspace.dependencies]` comment on
//! why those two crates specifically are safe to link into the render thread: no dependencies of
//! their own).
//!
//! Nothing here decides layout policy — `crates/templar-window` already computed every rectangle
//! ([`templar_window::WindowLayout`], `scatter_layout`) before this module ever runs; this module
//! only turns those rectangles into pixels and positions them on screen. `chrome::layout` holds
//! the pure chrome-specific geometry (tab hit-testing, the switcher panel) that *is* decided
//! here; `chrome::text` finds a font without ever being able to panic; `chrome::paint` draws
//! with one; `chrome::cache` is what stops a repaint from happening on every frame regardless of
//! whether anything changed.
//!
//! There are two entry points, and the split is the whole point of them. [`collect_output_chrome`]
//! paints what belongs to the *output* — the lock screen, the pressure overlay, the fallback
//! switcher, the "show every window" scatter — which sits above every window there is; its own
//! internal order makes the lock screen win over the pressure overlay and the switcher win over
//! the scatter, matching `templar_window::Layer`'s ordering (`SecureLock` < `Recovery` <
//! `ShellOverlay`). [`collect_window_chrome`] paints what belongs to *one window* — its title
//! tab and its controls, at its active corner — and that is emitted per window, interleaved with
//! that window's own surface, by `src/scene.rs`. A tab is part of its window, not of the screen:
//! emitting every tab ahead of every surface (which is what a single flat list did) painted a
//! buried window's title over the translucent terminal in front of it.

pub mod cache;
pub mod layout;
pub mod paint;
pub mod text;
mod identification;

use cache::{CachedWidget, WidgetKey};
use smithay::{
    backend::renderer::{element::memory::MemoryRenderBufferRenderElement, ImportMem},
    desktop::{Space, Window},
    output::Output,
    utils::{Logical, Physical, Point},
};
use std::collections::HashMap;
use templar_comp_integration::DbusIdentityCache;
use templar_window::{Rect, WindowId, WindowService, WindowStacking};
use text::ChromeFonts;

/// Logical-pixel height of one fallback-switcher row.
const SWITCHER_ROW_HEIGHT: i32 = 30;
/// Logical-pixel height of a scatter tile's title strip (`chrome::layout::scatter_label_band`).
const SCATTER_LABEL_HEIGHT: i32 = 26;
/// Logical-pixel thickness of a window group's outline (planning doc §2 rule 7: "two logical
/// pixels in the group's accent colour").
const GROUP_OUTLINE_THICKNESS: i32 = 2;

/// The compositor-drawn fallback switcher's transient state (`TemplarCompState::step_switcher`,
/// `src/keybindings.rs`): what to list, in display order, and which entry is selected. It lives
/// exactly as long as the frozen list it draws — until the modifier comes up (which activates
/// the selection), or until Escape, a click or any other action ends the walk choosing nothing
/// (`src/input.rs`). A window closing mid-walk is pruned from the list and the HUD rebuilt. This
/// module owns no clock, so there is no timed fade.
///
/// Two chords share it. `Alt+Tab` lists every open application window, parked ones included,
/// most-recently-activated first; `` Alt+` `` lists the focused window's unit under the unit's
/// name. One HUD, because they are the same gesture at two scales, and because a second panel
/// widget would be a second thing to keep alive with the shell dead.
#[derive(Clone, Debug)]
pub struct SwitcherState {
    /// A title row above the entries: the unit's name. `None` for the plain `Alt+Tab` list.
    pub heading: Option<String>,
    /// The group's accent, painted behind the heading so the HUD and the band agree at a glance.
    pub accent: Option<u32>,
    pub entries: Vec<SwitcherEntry>,
    pub selected: WindowId,
}

/// One switcher row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitcherEntry {
    pub window_id: WindowId,
    /// Never empty: the title, else the application's short name, else "(untitled)".
    pub label: String,
    /// Parked on Home's shelf: drawn in muted ink, so the HUD says choosing it brings it back.
    pub parked: bool,
}

/// Compositor title menu. The captured WindowId never follows keyboard focus.
#[derive(Debug, Default)]
pub struct NameTabMenu {
    pub open: Option<NameTabMenuTarget>,
    pressed: Option<(u32, Option<WindowStacking>)>,
}

#[derive(Clone, Copy, Debug)]
pub struct NameTabMenuTarget {
    pub window_id: WindowId,
    pub panel: Rect,
}

impl NameTabMenuTarget {
    pub const ROWS: [(WindowStacking, &'static str); 3] = [
        (WindowStacking::AlwaysOnTop, "Always on top"),
        (WindowStacking::Normal, "Normal"),
        (WindowStacking::AlwaysOnBottom, "Always on bottom"),
    ];

    pub fn row(&self, index: usize) -> Rect {
        let top = self.panel.height * index as i32 / 3;
        let bottom = self.panel.height * (index as i32 + 1) / 3;
        Rect::new(self.panel.x, self.panel.y + top, self.panel.width, bottom - top)
    }

    pub fn choice_at(&self, pos: templar_window::Point) -> Option<WindowStacking> {
        Self::ROWS.iter().enumerate().find_map(|(index, (choice, _))| {
            let row = self.row(index);
            (pos.x >= row.x && pos.x < row.right() && pos.y >= row.y && pos.y < row.bottom())
                .then_some(*choice)
        })
    }
}

impl NameTabMenu {
    pub fn show(&mut self, window_id: WindowId, pos: templar_window::Point, output: Rect, button: u32) {
        let width = 200.min(output.width);
        let height = 90.min(output.height);
        self.open = Some(NameTabMenuTarget {
            window_id,
            panel: Rect::new(pos.x.clamp(output.x, output.right() - width),
                pos.y.clamp(output.y, output.bottom() - height), width, height),
        });
        // Own the opening secondary-button release, without starting a primary drag/pin.
        self.pressed = Some((button, None));
    }

    pub fn dismiss(&mut self) {
        self.open = None;
    }

    /// Claim complete button pairs, committing only a primary release on its pressed row.
    pub fn button<C: templar_window::IdentityCache>(&mut self, service: &mut WindowService<C>,
        pos: templar_window::Point, button: u32, pressed: bool) -> bool
    {
        if !pressed {
            let Some((held, choice)) = self.pressed else { return self.open.is_some(); };
            if held != button { return true; }
            self.pressed = None;
            if let Some(choice) = choice {
                if let Some(target) = self.open
                    && target.choice_at(pos) == Some(choice)
                    && service.is_visible(target.window_id)
                {
                    service.set_window_stacking(target.window_id, choice);
                }
                self.dismiss();
            }
            return true;
        }
        let Some(target) = self.open else { return self.pressed.is_some(); };
        let choice = target.choice_at(pos);
        self.pressed = Some((button, (button == 0x110).then_some(choice).flatten()));
        if choice.is_none() || !service.is_visible(target.window_id) {
            self.dismiss();
        }
        true
    }
}

struct IdentificationWidget {
    number: usize,
    label: String,
    calibration: bool,
    margins: templar_types::display::Insets,
    widget: CachedWidget,
}

/// Long-lived chrome state: the discovered font (if any) and every widget's cached painted
/// bitmap, keyed so a window closing or the pressure overlay hiding can drop its cache entry
/// rather than accumulate forever. Constructed once per compositor run (`src/backend/winit.rs`,
/// `src/backend/udev.rs`), never per-frame.
pub struct ChromeState {
    appearance: templar_types::appearance::Appearance,
    fonts: Option<ChromeFonts>,
    scratch: pixelkit_raster::WindowBuffer,
    name_tags: HashMap<WindowId, CachedWidget>,
    control_tags: HashMap<WindowId, CachedWidget>,
    pressure: CachedWidget,
    display_recovery: Option<String>,
    display_recovery_widget: CachedWidget,
    lock: CachedWidget,
    identification: HashMap<Output, IdentificationWidget>,
    /// One title band per card in the window overview.
    taskfield_labels: HashMap<WindowId, CachedWidget>,
    /// One band per section header, by position: sections have no stable identity across a
    /// re-entry and the layout is frozen for the length of the mode, so the index *is* the key.
    taskfield_headers: Vec<CachedWidget>,
    /// The scrim the overview's cards sit on, and the four bars ringing the selected one.
    taskfield_scrim: CachedWidget,
    taskfield_ring: Vec<CachedWidget>,
    switcher_rows: HashMap<WindowId, CachedWidget>,
    switcher_heading: CachedWidget,
    name_tab_menu_rows: [CachedWidget; 3],
    /// The two-pixel corner mark that says where Peek is (`peek_affordance`).
    peek: CachedWidget,
    /// The four bars of the gesture gate's learning outline (`collect_gesture_gate`).
    gate_outline: Vec<CachedWidget>,
    /// The four bars of one group's outline, kept against the member that draws them.
    outlines: HashMap<WindowId, Vec<CachedWidget>>,
}

impl std::fmt::Debug for ChromeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromeState")
            .field("has_font", &self.fonts.is_some())
            .field("name_tags", &self.name_tags.len())
            .field("taskfield_labels", &self.taskfield_labels.len())
            .finish_non_exhaustive()
    }
}

impl Default for ChromeState {
    fn default() -> Self {
        Self::new()
    }
}

impl ChromeState {
    /// Discovers a font once (never fatal — see `chrome::text`'s doc) and starts with every
    /// widget cache empty.
    pub fn new() -> Self {
        let fonts = ChromeFonts::discover();
        if fonts.is_none() {
            tracing::warn!(
                "chrome: no usable font found under the usual font directories; drawing plain \
                 rectangles with no text until one is installed"
            );
        }
        Self {
            appearance: templar_types::appearance::Appearance::default(),
            fonts,
            scratch: pixelkit_raster::WindowBuffer::new(1, 1),
            name_tags: HashMap::new(),
            control_tags: HashMap::new(),
            pressure: CachedWidget::new(),
            display_recovery: None,
            display_recovery_widget: CachedWidget::new(),
            lock: CachedWidget::new(),
            identification: HashMap::new(),
            taskfield_labels: HashMap::new(),
            taskfield_headers: Vec::new(),
            taskfield_scrim: CachedWidget::new(),
            taskfield_ring: Vec::new(),
            switcher_rows: HashMap::new(),
            switcher_heading: CachedWidget::new(),
            name_tab_menu_rows: std::array::from_fn(|_| CachedWidget::new()),
            peek: CachedWidget::new(),
            gate_outline: Vec::new(),
            outlines: HashMap::new(),
        }
    }

    pub fn set_display_recovery(&mut self, notice: String) {
        self.display_recovery = (!notice.is_empty()).then_some(notice);
        self.display_recovery_widget.invalidate();
    }

    /// Repaint owned chrome without replacing window cards, output placement, or scene state.
    pub fn update_appearance(&mut self, appearance: templar_types::appearance::Appearance) -> bool {
        if self.appearance == appearance { return false; }
        self.appearance = appearance;
        for widget in self.name_tags.values_mut()
            .chain(self.control_tags.values_mut())
            .chain(self.identification.values_mut().map(|identification| &mut identification.widget))
            .chain(self.taskfield_labels.values_mut())
            .chain(self.taskfield_headers.iter_mut())
            .chain(self.taskfield_ring.iter_mut())
            .chain(self.switcher_rows.values_mut())
            .chain(self.name_tab_menu_rows.iter_mut())
            .chain(self.gate_outline.iter_mut())
            .chain(self.outlines.values_mut().flat_map(|widgets| widgets.iter_mut()))
            .chain([&mut self.pressure, &mut self.lock, &mut self.taskfield_scrim, &mut self.switcher_heading, &mut self.peek])
        {
            widget.invalidate();
        }
        true
    }
    pub fn forget_output(&mut self, output: &Output) {
        self.identification.remove(output);
    }

    /// Whether a font was found — exposed for `main.rs`/logging and tests, not policy.
    pub fn has_font(&self) -> bool {
        self.fonts.is_some()
    }

    /// Drop the band and grip bitmaps of every window not in `live` — the retention half of the
    /// cache, run once per frame by `src/scene.rs` after it has walked the stacking order, so a
    /// window that closed or was shelved stops holding a GPU texture. `chrome::cache`'s doc: a
    /// widget that no longer applies is discarded rather than kept "just in case".
    pub fn retain_window_chrome(&mut self, live: &std::collections::HashSet<WindowId>) {
        self.name_tags.retain(|id, _| live.contains(id));
        self.control_tags.retain(|id, _| live.contains(id));
        self.outlines.retain(|id, _| live.contains(id));
    }

    /// Drop every window's tags: the lock screen or the pressure overlay owns the screen, so no
    /// window chrome is drawn at all this frame.
    fn forget_window_chrome(&mut self) {
        self.name_tags.clear();
        self.control_tags.clear();
        self.outlines.clear();
    }
}

/// Everything [`collect_output_chrome`] and `src/scene.rs` read from `TemplarCompState` — bundled so the function itself
/// takes individually-borrowed fields rather than the whole state, which is what lets
/// `src/backend/udev.rs` call it while a device's `GlesRenderer` (borrowed from
/// `state.backend_data`, a different field) is simultaneously held mutably.
pub struct ChromeInputs<'a> {
    pub space: &'a Space<Window>,
    pub window_service: &'a WindowService<DbusIdentityCache>,
    pub tab_counts: &'a HashMap<WindowId, templar_window::TabCount>,
    pub recovery_protocol: &'a crate::protocols_recovery::RecoveryProtocolState,
    pub session_lock: &'a crate::session_lock::SessionLockState,
    pub switcher: Option<&'a SwitcherState>,
    pub name_tab_menu: &'a NameTabMenu,
    /// The window whose title a title drag is currently hovering, if any: it draws as a drop
    /// target, which is the whole visible difference between "moving a window over another" and
    /// "about to group them" (`src/input.rs`, planning doc §2 rule 6).
    pub drop_target: Option<WindowId>,
    /// The bottom-centre gesture gate: whether Peek's mark is drawn at all, and the learning
    /// outline (`src/gesture_gate.rs`).
    pub gesture_gate: &'a crate::gesture_gate::GestureGate,
    /// The park/unpark motions (`src/motion.rs`), drawn by `src/scene.rs` at `now_ns`.
    pub motions: &'a crate::motion::MotionState,
    /// This frame's monotonic time (`window_bridge::now_monotonic_ns`), the one clock reading
    /// every motion in the frame is drawn at.
    pub now_ns: u64,
}

/// The window-independent chrome to draw over `output` this frame, front-to-back (index 0
/// topmost): the lock screen alone; else the pressure overlay alone; else the fallback switcher
/// and the "show every window" scatter. Everything here sits **above every window**, which is
/// what makes it output chrome rather than window chrome.
///
/// The returned flag says whether windows still get their own band and grip this frame. The lock
/// screen and the pressure overlay own the screen outright: drawing a title bar into either of
/// them would be drawing a control the user is not being offered.
pub fn collect_output_chrome<R: ImportMem>(
    chrome: &mut ChromeState,
    inputs: &ChromeInputs<'_>,
    output: &Output,
    renderer: &mut R,
    background: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) -> (Vec<MemoryRenderBufferRenderElement<R>>, bool) where R::TextureId: Clone + Send + 'static {
    let Some(output_geometry) = crate::viewport::output_geometry(inputs.space, output) else {
        return (Vec::new(), false);
    };
    let output_scale = output.current_scale().fractional_scale();
    let output_rect = Rect::new(
        output_geometry.loc.x,
        output_geometry.loc.y,
        output_geometry.size.w,
        output_geometry.size.h,
    );
    // The rect every "whole output" widget (lock screen, pressure overlay, switcher) is drawn
    // and positioned against, local to this output — so the same `chrome::layout` functions used
    // for hit-testing in `src/input.rs` (which works in *global* logical coordinates) are called
    // there with the output's real global rect, and here with this zeroed one; both derive the
    // same relative geometry by construction.
    let local_rect = Rect::new(0, 0, output_rect.width, output_rect.height);
    let origin_physical: Point<f64, Physical> = Point::from((0.0, 0.0));

    let mut elements = Vec::new();

    if inputs.session_lock.locked {
        if let Some(element) =
            paint_lock_screen(chrome, local_rect, output_scale, origin_physical, renderer)
        {
            elements.push(element);
        }
        chrome.forget_window_chrome();
        return (elements, false);
    }

    if let Some(text) = chrome.display_recovery.as_ref() {
        let width = (local_rect.width - 24).max(1);
        let colour = paint::opaque(chrome.appearance.palette.shell_fill);
        let fonts = chrome.fonts.as_mut().map(|fonts| (&mut fonts.cache, fonts.face));
        if let Some(element) = chrome.display_recovery_widget.ensure(
            WidgetKey::new(text, colour), (width, 42), output_scale,
            Point::from((12.0 * output_scale, (local_rect.height - 54).max(0) as f64 * output_scale)),
            &mut chrome.scratch, renderer, |painter, pixel_w, pixel_h| {
                paint::scatter_label(painter, pixel_w, pixel_h, text, colour, fonts,
                    scale_factor(pixel_w, width));
            },
        ) { elements.push(element); }
    }

    if let Some(overlay) = &inputs.recovery_protocol.pressure_overlay {
        if let Some(element) = paint_pressure_overlay(
            chrome,
            overlay,
            local_rect,
            output_scale,
            origin_physical,
            renderer,
        ) {
            elements.push(element);
        }
        chrome.forget_window_chrome();
        return (elements, false);
    }

    // A *visible* Presentation surface is the shell's actual renderer, not merely a connection
    // or a mapped buffer: the same `is_visible` verdict that puts it in `Space`. Destroying that
    // surface restores the unchanged text HUD, including mid-walk.
    let shell_presentation = inputs.window_service.windows().any(|window| {
        inputs.window_service.layer_of(window.id) == Some(templar_window::Layer::Presentation)
            && inputs.window_service.is_visible(window.id)
    });
    if let Some(switcher) = inputs.switcher.filter(|_| !shell_presentation) {
        collect_switcher(
            chrome,
            switcher,
            local_rect,
            output_scale,
            renderer,
            &mut elements,
        );
    } else {
        chrome.switcher_rows.clear();
    }

    if let templar_window::Presentation::Taskfield(taskfield) = inputs.window_service.presentation()
    {
        collect_taskfield(
            chrome,
            inputs.window_service,
            taskfield,
            output_rect,
            output_scale,
            renderer,
            &mut elements,
            background,
        );
        // Windows wear no bands in the overview: a card is a picture of a window, and a title bar
        // painted across it at its real size would be the one thing on screen not pulled back.
        // The overview draws its own title under each card instead.
        return (elements, false);
    }
    chrome.taskfield_labels.clear();
    chrome.taskfield_headers.clear();
    chrome.taskfield_ring.clear();

    if let Some(menu) = inputs.name_tab_menu.open
        .filter(|menu| inputs.window_service.is_visible(menu.window_id))
    {
        collect_name_tab_menu(chrome, inputs.window_service, menu, output_rect, output_scale,
            renderer, &mut elements);
    }

    collect_peek_affordance(
        chrome,
        inputs.window_service,
        inputs.gesture_gate,
        output_rect,
        local_rect,
        output_scale,
        renderer,
        &mut elements,
    );
    collect_gesture_gate(
        chrome,
        inputs.window_service,
        inputs.gesture_gate,
        output_rect,
        local_rect,
        output_scale,
        renderer,
        &mut elements,
    );

    (elements, true)
}

/// Whether `output_rect` is filled by a visible client window — maximized or fullscreen
/// (`WindowRecord::covers_screen`) — which is exactly when the shell's island is not on screen
/// and Peek is the only pointer route to it.
///
/// The same condition `input::dwell_intent_at` computes for its `covering` window, spelled once
/// here for the mark and once there for the region, because chrome takes the pieces of
/// `TemplarCompState` it needs rather than the state itself and cannot call it.
fn screen_filled_by_a_window(
    window_service: &WindowService<DbusIdentityCache>,
    output_rect: Rect,
) -> bool {
    window_service
        .stacking_order()
        .into_iter()
        .any(|window_id| {
            window_service.is_client_window(window_id)
                && window_service.is_visible(window_id)
                && window_service.window(window_id).is_some_and(|window| {
                    window.covers_screen() && window.geometry.intersection_area(output_rect) > 0
                })
        })
}

/// The corner mark for Peek: two logical pixels in the recovery tab's colour at the bottom-left of
/// an output whose screen a window is filling (planning doc §6, stage 0).
///
/// Drawn by the compositor rather than the shell for the same reason the recovery band is: it has
/// to survive the shell dying, and under a fullscreen window the shell is not on screen to draw
/// anything at all. Drawn *only* then: on an ordinary desktop the island is right there, and a
/// permanent mark under it would be furniture for a route nobody needs. It also goes while the
/// window's own chrome is showing (those tabs cover this corner) and while a peek is already up
/// (the island is what is being reached for now, not the mark). And never while the gesture gate
/// is enabled: Peek itself is off then, and a mark for it would point at nothing.
#[allow(clippy::too_many_arguments)]
fn collect_peek_affordance<R: ImportMem>(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    gesture_gate: &crate::gesture_gate::GestureGate,
    output_rect: Rect,
    local_rect: Rect,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    if gesture_gate.config().enabled
        || window_service.peek_output().is_some()
        || !screen_filled_by_a_window(window_service, output_rect)
    {
        return;
    }
    let mark = crate::input::peek_affordance(local_rect);
    if mark.width <= 0 || mark.height <= 0 {
        return;
    }
    let colour = paint::opaque(chrome.appearance.palette.activity);
    if let Some(element) = chrome.peek.ensure(
        WidgetKey::new("peek-affordance", colour),
        (mark.width, mark.height),
        output_scale,
        local_physical(mark, output_scale),
        &mut chrome.scratch,
        renderer,
        move |painter, pixel_w, pixel_h| {
            paint::group_outline_bar(painter, pixel_w, pixel_h, colour);
        },
    ) {
        elements.push(element);
    }
}

/// The gesture gate's outline, in learning mode only: one logical pixel just inside the gate on
/// every output, faint while idle and in the focus accent while it is armed (or has a rail open)
/// on this output — so what arms and what does not can be seen while the gesture is being
/// learnt. Opaque like every chrome widget, so "faint" is a dim colour rather than alpha.
#[allow(clippy::too_many_arguments)]
fn collect_gesture_gate<R: ImportMem>(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    gesture_gate: &crate::gesture_gate::GestureGate,
    output_rect: Rect,
    local_rect: Rect,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    let config = gesture_gate.config();
    if !config.enabled || !config.learning {
        chrome.gate_outline.clear();
        return;
    }
    // The recognizer names outputs by their index in `WindowService::outputs`, which is also
    // what the pointer paths feed it.
    let this_output = window_service
        .outputs()
        .iter()
        .position(|output| output.logical == output_rect);
    let active = match gesture_gate.phase() {
        crate::gesture_gate::Phase::Idle => None,
        crate::gesture_gate::Phase::Armed { output, .. }
        | crate::gesture_gate::Phase::Open { output, .. } => Some(output),
    };
    let colour = if active.is_some() && active == this_output {
        paint::opaque(chrome.appearance.palette.accent)
    } else {
        paint::opaque(chrome.appearance.palette.edge)
    };
    let area = gesture_gate.area(crate::gesture_gate::Area {
        x: f64::from(local_rect.x),
        y: f64::from(local_rect.y),
        width: f64::from(local_rect.width),
        height: f64::from(local_rect.height),
    });
    // `outline_bars` draws outside the rectangle it is given; shrinking it by the thickness puts
    // the outline on the gate's own edge, the bottom one included, rather than off the screen.
    let thickness = 1;
    let inner = Rect::new(
        area.x.round() as i32 + thickness,
        area.y.round() as i32 + thickness,
        area.width.round() as i32 - 2 * thickness,
        area.height.round() as i32 - 2 * thickness,
    );
    if inner.width <= 0 || inner.height <= 0 {
        return;
    }
    chrome.gate_outline.resize_with(4, CachedWidget::new);
    for (widget, bar) in chrome
        .gate_outline
        .iter_mut()
        .zip(layout::outline_bars(inner, thickness))
    {
        if let Some(element) = widget.ensure(
            WidgetKey::new("gesture-gate", colour),
            (bar.width, bar.height),
            output_scale,
            local_physical(bar, output_scale),
            &mut chrome.scratch,
            renderer,
            move |painter, pixel_w, pixel_h| {
                paint::group_outline_bar(painter, pixel_w, pixel_h, colour);
            },
        ) {
            elements.push(element);
        }
    }
}

/// One window's own chrome, front-to-back: its title tab then its controls, both laid out
/// by `WindowService::window_layout` at the window's active corner, then its group outline,
/// appended to `elements`. The title draws while its window is visible; the controls draw only
/// while revealed (or always, under the always-controls policy) — and a hidden control takes
/// no input, so what is seen is exactly what answers.
///
/// Both belong to `window_id` and to nothing else, so `src/scene.rs` emits them immediately
/// *above that window's surface and below every window in front of it* — never as part of one
/// flat chrome list, which is what used to paint a buried window's title across the translucent
/// terminal on top of it.
///
/// The resize squares need no paint: they are the bare corner grabs around the client, and a
/// window whose client draws no frame — most XWayland windows, anything borderless — is still
/// resizable from all four by the same geometry the tabs avoid.
///
/// A group's outline is window chrome too, and for the same reason: it must survive the shell
/// dying, and it must never float above a window that has nothing to do with the group. It is
/// emitted with the group's **bottom-most** visible member, so anything stacked above that member
/// — including an unrelated window sitting between two members — occludes it exactly as it
/// occludes that member's own tabs.
#[allow(clippy::too_many_arguments)]
pub fn collect_window_chrome<R: ImportMem>(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    window_id: WindowId,
    tab_count: Option<u32>,
    drop_target: Option<WindowId>,
    output_rect: Rect,
    output_scale: f64,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
    renderer: &mut R,
) where R::TextureId: Clone + Send + 'static {
    // All client windows share one visibility policy, including fullscreen overlays.
    if window_service.is_client_window(window_id) {
        if window_service.title_revealed(window_id) {
            collect_name_tag(
                chrome,
                window_service,
                window_id,
                tab_count,
                drop_target == Some(window_id),
                output_rect,
                output_scale,
                renderer,
                elements,
            );
        }
        if window_service.controls_revealed(window_id) {
            collect_control_tags(
                chrome,
                window_service,
                window_id,
                output_rect,
                output_scale,
                renderer,
                elements,
            );
        }
    }
    collect_group_outline(
        chrome,
        window_service,
        window_id,
        output_rect,
        output_scale,
        renderer,
        elements,
    );
}

fn to_physical(rect: Rect, output_origin: Point<i32, Logical>, scale: f64) -> Point<f64, Physical> {
    let relative: Point<i32, Logical> = Point::from((rect.x, rect.y)) - output_origin;
    relative.to_f64().to_physical(scale)
}

/// [`to_physical`] for a rect that is already local to the output (its own `(0, 0)` origin) —
/// the fallback switcher's rows, laid out against `local_rect` rather than global coordinates.
fn local_physical(rect: Rect, scale: f64) -> Point<f64, Physical> {
    let point: Point<i32, Logical> = Point::from((rect.x, rect.y));
    point.to_f64().to_physical(scale)
}

fn paint_lock_screen<R: ImportMem>(
    chrome: &mut ChromeState,
    local_rect: Rect,
    output_scale: f64,
    location: Point<f64, Physical>,
    renderer: &mut R,
) -> Option<MemoryRenderBufferRenderElement<R>> where R::TextureId: Clone + Send + 'static {
    let palette = &chrome.appearance.palette;
    let key = WidgetKey::new("locked", paint::opaque(palette.ground));
    let fonts = chrome.fonts.as_mut().map(|f| (&mut f.cache, f.face));
    chrome.lock.ensure(
        key,
        (local_rect.width, local_rect.height),
        output_scale,
        location,
        &mut chrome.scratch,
        renderer,
        move |painter, pixel_w, pixel_h| {
            paint::lock_screen(
                painter,
                pixel_w,
                pixel_h,
                fonts,
                scale_factor(pixel_w, local_rect.width),
                palette,
            );
        },
    )
}

fn paint_pressure_overlay<R: ImportMem>(
    chrome: &mut ChromeState,
    overlay: &crate::protocols_recovery::PressureOverlay,
    local_rect: Rect,
    output_scale: f64,
    location: Point<f64, Physical>,
    renderer: &mut R,
) -> Option<MemoryRenderBufferRenderElement<R>> where R::TextureId: Clone + Send + 'static {
    let palette = &chrome.appearance.palette;
    let key = WidgetKey::new(overlay.message.clone(), paint::opaque(palette.danger_soft))
        .with_secondary(overlay.offender_app_id.clone().unwrap_or_default())
        .with_variant(overlay.selected.index() as u32);
    let buttons_logical = layout::pressure_overlay_buttons(local_rect);
    let message = overlay.message.clone();
    let offender = overlay.offender_app_id.clone();
    let selected = overlay.selected;
    let fonts = chrome.fonts.as_mut().map(|f| (&mut f.cache, f.face));

    chrome.pressure.ensure(
        key,
        (local_rect.width, local_rect.height),
        output_scale,
        location,
        &mut chrome.scratch,
        renderer,
        move |painter, pixel_w, pixel_h| {
            let scale = scale_factor(pixel_w, local_rect.width);
            let buttons_px: [pixelkit_raster::Rect; 4] =
                std::array::from_fn(|i| paint::scaled_px(buttons_logical[i], scale));
            paint::pressure_overlay(
                painter,
                pixel_w,
                pixel_h,
                &message,
                offender.as_deref(),
                &buttons_px,
                selected,
                fonts,
                scale,
                palette,
            );
        },
    )
}

fn collect_name_tab_menu<R: ImportMem>(
    chrome: &mut ChromeState, service: &WindowService<DbusIdentityCache>,
    menu: NameTabMenuTarget, output: Rect, scale: f64, renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    if menu.panel.intersection_area(output) == 0 { return; }
    for (index, (choice, label)) in NameTabMenuTarget::ROWS.into_iter().enumerate() {
        let row = menu.row(index);
        let selected = service.window(menu.window_id).is_some_and(|window| window.stacking == choice);
        let palette = &chrome.appearance.palette;
        let background = paint::opaque(if selected { palette.accent_soft } else { palette.system_shell });
        let fonts = chrome.fonts.as_mut().map(|fonts| (&mut fonts.cache, fonts.face));
        if let Some(element) = chrome.name_tab_menu_rows[index].ensure(
            WidgetKey::new(label, background).with_variant(u32::from(selected)),
            (row.width, row.height), scale,
            to_physical(row, Point::from((output.x, output.y)), scale),
            &mut chrome.scratch, renderer,
            move |painter, width, height| paint::switcher_row(painter, width, height, label,
                selected, false, fonts, scale_factor(width, row.width), palette),
        ) {
            elements.push(element);
        }
    }
}

fn collect_switcher<R: ImportMem>(
    chrome: &mut ChromeState,
    switcher: &SwitcherState,
    local_rect: Rect,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    let heading_rows = usize::from(switcher.heading.is_some());
    // More windows than the output has rows for: show the stretch around the selection rather
    // than letting the panel's clamp cut rows off the bottom, where the walk would vanish.
    let fits = usize::try_from(local_rect.height / SWITCHER_ROW_HEIGHT.max(1)).unwrap_or(0);
    let selected = switcher
        .entries
        .iter()
        .position(|entry| entry.window_id == switcher.selected)
        .unwrap_or(0);
    let shown = layout::switcher_visible_rows(
        switcher.entries.len(),
        selected,
        fits.saturating_sub(heading_rows).max(1),
    );
    let panel = layout::switcher_panel(local_rect, shown.len() + heading_rows, SWITCHER_ROW_HEIGHT);
    let mut live = std::collections::HashSet::new();

    if let Some(heading) = &switcher.heading {
        collect_switcher_heading(
            chrome,
            heading,
            switcher.accent,
            layout::switcher_row(panel, 0, SWITCHER_ROW_HEIGHT),
            output_scale,
            renderer,
            elements,
        );
    }

    for (slot, entry) in switcher.entries[shown].iter().enumerate() {
        live.insert(entry.window_id);
        let row = layout::switcher_row(panel, slot + heading_rows, SWITCHER_ROW_HEIGHT);
        let selected = entry.window_id == switcher.selected;
        let background = if selected {
            paint::opaque(chrome.appearance.palette.accent_soft)
        } else {
            paint::opaque(chrome.appearance.palette.system_shell)
        };
        let key = WidgetKey::new(entry.label.clone(), background)
            .with_variant(u32::from(selected) | (u32::from(entry.parked) << 1));
        let label = entry.label.clone();
        let palette = &chrome.appearance.palette;
        let parked = entry.parked;
        let fonts = chrome.fonts.as_mut().map(|f| (&mut f.cache, f.face));
        let location = local_physical(row, output_scale);
        let widget = chrome.switcher_rows.entry(entry.window_id).or_default();
        if let Some(element) = widget.ensure(
            key,
            (row.width, row.height),
            output_scale,
            location,
            &mut chrome.scratch,
            renderer,
            move |painter, pixel_w, pixel_h| {
                paint::switcher_row(
                    painter,
                    pixel_w,
                    pixel_h,
                    &label,
                    selected,
                    parked,
                    fonts,
                    scale_factor(pixel_w, row.width),
                    palette,
                );
            },
        ) {
            elements.push(element);
        }
    }
    chrome.switcher_rows.retain(|id, _| live.contains(id));
}

/// The switcher's title row: the group's name in its accent, or "Groups" on the panel colour.
/// One row-shaped widget of its own — it belongs to no window, so it cannot live in the
/// per-window row cache.
#[allow(clippy::too_many_arguments)]
fn collect_switcher_heading<R: ImportMem>(
    chrome: &mut ChromeState,
    heading: &str,
    accent: Option<u32>,
    row: Rect,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    let background = accent.map_or(paint::opaque(chrome.appearance.palette.system_shell), paint::opaque);
    let key = WidgetKey::new(heading.to_owned(), background);
    let heading = heading.to_owned();
    let fonts = chrome.fonts.as_mut().map(|f| (&mut f.cache, f.face));
    let location = local_physical(row, output_scale);
    if let Some(element) = chrome.switcher_heading.ensure(
        key,
        (row.width, row.height),
        output_scale,
        location,
        &mut chrome.scratch,
        renderer,
        move |painter, pixel_w, pixel_h| {
            paint::switcher_heading(
                painter,
                pixel_w,
                pixel_h,
                &heading,
                background,
                fonts,
                scale_factor(pixel_w, row.width),
            );
        },
    ) {
        elements.push(element);
    }
}

/// The window overview's own chrome (`templar_window::Presentation::Taskfield`): the scrim the
/// cards sit on, one header per section, a title band under each card, and the four bars ringing
/// the selected one.
///
/// Everything but the scrim goes in `elements`, above every window, and the scrim goes in
/// `background`, which `src/scene.rs` appends *last* — beneath the cards, because a card is the
/// window's own live surface drawn small (`WindowService::presentation_rect`) and not a
/// placeholder. That is the whole difference between this and the recovery scatter it replaces:
/// there the tiles were coloured rectangles, here they are the windows.
///
/// Drawn by the compositor whether or not a shell is running. When one is, it puts icons and
/// richer labels over the same rectangles from its own `Layer::Presentation` surface, having run
/// `templar_window::taskfield::layout` over the same bytes.
#[allow(clippy::too_many_arguments)]
fn collect_taskfield<R: ImportMem>(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    taskfield: &templar_window::TaskfieldState,
    output_rect: Rect,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
    background: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    let origin = Point::from((output_rect.x, output_rect.y));

    // The scrim, over this whole output. It is what makes the mode read as a mode: the desktop
    // behind the cards is still being drawn, and without it a card would sit on whatever window
    // happened to be underneath.
    let scrim = paint::opaque(chrome.appearance.palette.ground);
    if let Some(element) = chrome.taskfield_scrim.ensure(
        WidgetKey::new("taskfield-scrim", scrim),
        (output_rect.width, output_rect.height),
        output_scale,
        to_physical(output_rect, origin, output_scale),
        &mut chrome.scratch,
        renderer,
        |painter, pixel_w, pixel_h| {
            paint::group_outline_bar(painter, pixel_w, pixel_h, scrim);
        },
    ) {
        background.push(element);
    }

    // The query, when there is one, in the band the first section header would otherwise start
    // at. `taskfield::layout` never re-runs for a query — a card must not move out from under the
    // pointer as the user types — so this says what was typed and nothing moves.
    //
    // ponytail: the compositor shows the query but does not dim the non-matches
    // (`templar_window::task::matches`); dimming a live surface needs a per-element alpha the
    // widget cache does not carry. The shell's `Layer::Presentation` surface is where the dim
    // belongs anyway. Add the alpha to `CachedWidget::ensure` if the shell-dead case ever needs it.
    let mut sections: Vec<(Rect, String, Option<u32>)> = Vec::new();
    if !taskfield.query.is_empty() {
        // Along the bottom edge, which `taskfield::layout` leaves clear by its own margin — and
        // where a person looks for what they are typing.
        let band = Rect::new(
            output_rect.x,
            output_rect.bottom() - SCATTER_LABEL_HEIGHT,
            output_rect.width,
            SCATTER_LABEL_HEIGHT,
        );
        sections.push((band, format!("“{}”", taskfield.query), None));
    }
    for section in &taskfield.sections {
        if output_rect.intersection(section.header) != Some(section.header) {
            continue;
        }
        let overflow = if section.overflow > 0 {
            format!(" (+{} more)", section.overflow)
        } else {
            String::new()
        };
        sections.push((
            section.header,
            format!("{} — {}{overflow}", section.label, section.cards.len()),
            section.accent,
        ));
    }

    taskfield_headers(chrome, &sections, origin, output_scale, renderer, elements);
    taskfield_labels(
        chrome,
        window_service,
        taskfield,
        output_rect,
        origin,
        output_scale,
        renderer,
        elements,
    );
    taskfield_ring(
        chrome,
        taskfield,
        output_rect,
        origin,
        output_scale,
        renderer,
        elements,
    );
}

/// One band per section header, and the query when there is one, in the section's own accent.
fn taskfield_headers<R: ImportMem>(
    chrome: &mut ChromeState,
    sections: &[(Rect, String, Option<u32>)],
    origin: Point<i32, Logical>,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    chrome
        .taskfield_headers
        .resize_with(sections.len(), CachedWidget::new);
    for (widget, (rect, label, accent)) in chrome.taskfield_headers.iter_mut().zip(sections) {
        let colour = accent.map_or(paint::opaque(chrome.appearance.palette.shell_fill), paint::opaque);
        let fonts = chrome.fonts.as_mut().map(|f| (&mut f.cache, f.face));
        let text = label.clone();
        if let Some(element) = widget.ensure(
            WidgetKey::new(label.clone(), colour),
            (rect.width, rect.height),
            output_scale,
            to_physical(*rect, origin, output_scale),
            &mut chrome.scratch,
            renderer,
            move |painter, pixel_w, pixel_h| {
                paint::switcher_heading(
                    painter,
                    pixel_w,
                    pixel_h,
                    &text,
                    colour,
                    fonts,
                    scale_factor(pixel_w, rect.width),
                );
            },
        ) {
            elements.push(element);
        }
    }
}

/// One title band per card, inside the bottom of the card itself.
#[allow(clippy::too_many_arguments)]
fn taskfield_labels<R: ImportMem>(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    taskfield: &templar_window::TaskfieldState,
    output_rect: Rect,
    origin: Point<i32, Logical>,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    // One title band per card, inside the bottom of the card itself: the card *is* the window, so
    // there is no tile below it to hang a strip off, and a strip outside it would overlap the next
    // row. `scatter_label_band` splits the rectangle the same way it always did.
    for card in taskfield.cards() {
        if output_rect.intersection(card.rect) != Some(card.rect) {
            continue;
        }
        let selected = taskfield.selected == Some(card.window);
        let title = window_service
            .window(card.window)
            .map(|window| window.title.clone())
            .unwrap_or_default();
        let (_, label) = layout::scatter_label_band(card.rect, SCATTER_LABEL_HEIGHT);
        let background_colour = if selected {
            paint::opaque(chrome.appearance.palette.accent)
        } else {
            paint::opaque(chrome.appearance.palette.shell_fill)
        };
        let fonts = chrome.fonts.as_mut().map(|f| (&mut f.cache, f.face));
        let widget = chrome.taskfield_labels.entry(card.window).or_default();
        let title_for_paint = title.clone();
        if let Some(element) = widget.ensure(
            WidgetKey::new(title, background_colour),
            (label.width, label.height),
            output_scale,
            to_physical(label, origin, output_scale),
            &mut chrome.scratch,
            renderer,
            move |painter, pixel_w, pixel_h| {
                paint::scatter_label(
                    painter,
                    pixel_w,
                    pixel_h,
                    &title_for_paint,
                    background_colour,
                    fonts,
                    scale_factor(pixel_w, label.width),
                );
            },
        ) {
            elements.push(element);
        }
    }
    // Retain every card in the mode, not only the ones on this output: a per-output retain
    // dropped the other output's label caches and forced a rerasterize when that output drew.
    let live: std::collections::HashSet<_> = taskfield.cards().map(|card| card.window).collect();
    chrome.taskfield_labels.retain(|id, _| live.contains(id));
}

/// The four bars ringing the selected card.
#[allow(clippy::too_many_arguments)]
fn taskfield_ring<R: ImportMem>(
    chrome: &mut ChromeState,
    taskfield: &templar_window::TaskfieldState,
    output_rect: Rect,
    origin: Point<i32, Logical>,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    // The selection ring, four bars just *outside* the selected card so none of the window's own
    // pixels are covered — the same construction, and the same reason, as a group's outline.
    let ring = taskfield
        .selected
        .and_then(|id| taskfield.card(id))
        .map(|card| card.rect)
        .filter(|rect| output_rect.intersection(*rect).is_some());
    let Some(ring) = ring else {
        chrome.taskfield_ring.clear();
        return;
    };
    chrome.taskfield_ring.resize_with(4, CachedWidget::new);
    let ring_colour = paint::opaque(chrome.appearance.palette.accent);
    for (widget, bar) in chrome
        .taskfield_ring
        .iter_mut()
        .zip(layout::outline_bars(ring, GROUP_OUTLINE_THICKNESS))
    {
        if let Some(element) = widget.ensure(
            WidgetKey::new("taskfield-ring", ring_colour),
            (bar.width, bar.height),
            output_scale,
            to_physical(bar, origin, output_scale),
            &mut chrome.scratch,
            renderer,
            |painter, pixel_w, pixel_h| {
                paint::group_outline_bar(painter, pixel_w, pixel_h, ring_colour);
            },
        ) {
            elements.push(element);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_name_tag<R: ImportMem>(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    window_id: WindowId,
    tab_count: Option<u32>,
    drop_target: bool,
    output_rect: Rect,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    let Some(window) = window_service.window(window_id) else {
        return;
    };
    let Some(layout) = window_service.window_layout(window_id) else {
        return;
    };
    let band = layout.title;
    if output_rect.intersection(band).is_none() {
        return;
    }
    let requires_compositor_paint = window_service
        .recovery_affordance(window_id)
        .is_some_and(|affordance| affordance.requires_compositor_paint);
    let background = if drop_target {
        // A title tab being hovered by another window's title drag: releasing here groups the
        // two, and saying so before the release is the difference between a gesture and an
        // accident.
        paint::opaque(chrome.appearance.palette.accent_soft)
    } else if requires_compositor_paint {
        paint::opaque(chrome.appearance.palette.activity)
    } else {
        paint::opaque(chrome.appearance.palette.shell_low)
    };
    // The group's stripe and name, if this window is in one: the same accent the outline and the
    // Home tile use, so one group looks like one thing wherever it appears (§2 rule 7).
    let group = window_service
        .group_of(window_id)
        .and_then(|group| window_service.group_record(group))
        .map(|record| (record.accent, record.display_name().to_owned()));
    // Pinned above the other windows: its name is drawn in italic, so the state is legible from
    // the tab itself rather than from watching what the stacking order does.
    let pinned = window_service.is_always_on_top(window_id);
    // The corner, overlay and placement join the key: a tab that moved corners repaints rather
    // than reusing the bitmap it wore elsewhere, and only the old and new regions damage.
    let geometry_flags =
        u32::from(pinned) | (layout.corner as u32) << 1 | u32::from(layout.overlay) << 3;
    let mut key = WidgetKey::new(window.title.clone(), background)
        .with_secondary(group.clone().map(|(_, name)| name).unwrap_or_default())
        .with_variant(group.as_ref().map_or(0, |(accent, _)| *accent))
        .with_flags(geometry_flags);
    key.tab_count = tab_count;
    let title = window.title.clone();
    let location = to_physical(
        band,
        Point::from((output_rect.x, output_rect.y)),
        output_scale,
    );
    let fonts = chrome
        .fonts
        .as_mut()
        .map(|f| (&mut f.cache, if pinned { f.italic } else { f.face }));
    let widget = chrome.name_tags.entry(window_id).or_default();
    if let Some(element) = widget.ensure(
        key,
        (band.width, band.height),
        output_scale,
        location,
        &mut chrome.scratch,
        renderer,
        move |painter, pixel_w, pixel_h| {
            let scale = scale_factor(pixel_w, band.width);
            paint::name_tag(
                painter,
                pixel_w,
                pixel_h,
                background,
                &title,
                group
                    .as_ref()
                    .map(|(accent, name)| (*accent, name.as_str())),
                tab_count,
                fonts,
                scale,
            );
        },
    ) {
        elements.push(element);
    }
}

/// The 2-logical-pixel outline around the union of a group's unshelved members
/// (`WindowService::group_outline`, §2 rule 7), drawn as four bars *just outside* that union so
/// it never covers a member's own pixels — and drawn by the group's bottom-most visible member,
/// so an unrelated window above it covers the outline rather than the other way round.
fn collect_group_outline<R: ImportMem>(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    window_id: WindowId,
    output_rect: Rect,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    let outline = window_service
        .group_of(window_id)
        .filter(|group| {
            // Exactly one member draws it: the last one in stacking order that is on screen.
            window_service
                .group_stacking(*group)
                .into_iter()
                .rfind(|id| window_service.is_visible(*id))
                == Some(window_id)
        })
        .and_then(|group| {
            let accent = window_service.group_record(group)?.accent;
            Some((window_service.group_outline(group)?, accent))
        });
    let Some((outline, accent)) = outline else {
        chrome.outlines.remove(&window_id);
        return;
    };
    let colour = paint::opaque(accent);
    let widgets = chrome
        .outlines
        .entry(window_id)
        .or_insert_with(|| (0..4).map(|_| CachedWidget::new()).collect());
    for (widget, bar) in widgets
        .iter_mut()
        .zip(layout::outline_bars(outline, GROUP_OUTLINE_THICKNESS))
    {
        if output_rect.intersection(bar).is_none() {
            continue;
        }
        let location = to_physical(
            bar,
            Point::from((output_rect.x, output_rect.y)),
            output_scale,
        );
        if let Some(element) = widget.ensure(
            WidgetKey::new("group-outline", colour),
            (bar.width, bar.height),
            output_scale,
            location,
            &mut chrome.scratch,
            renderer,
            move |painter, pixel_w, pixel_h| {
                paint::group_outline_bar(painter, pixel_w, pixel_h, colour);
            },
        ) {
            elements.push(element);
        }
    }
}

/// The three controls — park, maximize, close — from the shared `window_layout` (the same
/// rectangles `src/input.rs` hit-tests), painted as one widget over their union: a vertical
/// strip for the same-corner stack, a horizontal one for the opposite-end row.
fn collect_control_tags<R: ImportMem>(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    window_id: WindowId,
    output_rect: Rect,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<MemoryRenderBufferRenderElement<R>>,
) where R::TextureId: Clone + Send + 'static {
    let Some(layout) = window_service.window_layout(window_id) else {
        return;
    };
    let tags = layout::tab_control_tags(&layout);
    let x = tags.iter().map(|(_, rect)| rect.x).min().unwrap_or(0);
    let y = tags.iter().map(|(_, rect)| rect.y).min().unwrap_or(0);
    let union = Rect::new(
        x,
        y,
        tags.iter().map(|(_, rect)| rect.right()).max().unwrap_or(x) - x,
        tags.iter().map(|(_, rect)| rect.bottom()).max().unwrap_or(y) - y,
    );
    if output_rect.intersection(union).is_none() {
        return;
    }
    let requires_compositor_paint = window_service
        .recovery_affordance(window_id)
        .is_some_and(|affordance| affordance.requires_compositor_paint);
    let background = if requires_compositor_paint {
        paint::opaque(chrome.appearance.palette.activity)
    } else {
        paint::opaque(chrome.appearance.palette.shell_low)
    };
    // The arrangement joins the key with the count: a stack and a row are different widgets
    // even where their unions coincide.
    let key = WidgetKey::new("control-tags", background).with_variant(
        u32::try_from(tags.len()).unwrap_or(0)
            | (layout.corner as u32) << 8
            | u32::from(layout.overlay) << 10
            | (layout.placement as u32) << 11,
    );
    let location = to_physical(
        union,
        Point::from((output_rect.x, output_rect.y)),
        output_scale,
    );
    let local = tags.map(|(tag, rect)| {
        (tag, Rect::new(rect.x - union.x, rect.y - union.y, rect.width, rect.height))
    });
    let widget = chrome.control_tags.entry(window_id)
        .or_insert_with(CachedWidget::new_transparent);
    if let Some(element) = widget.ensure(
        key,
        (union.width, union.height),
        output_scale,
        location,
        &mut chrome.scratch,
        renderer,
        move |painter, pixel_w, pixel_h| {
            let scale = scale_factor(pixel_w, union.width);
            let tags = local.map(|(tag, rect)| (tag, paint::scaled_px(rect, scale)));
            paint::control_tags(painter, pixel_w, pixel_h, background, &tags, scale);
        },
    ) {
        elements.push(element);
    }
}

/// The real per-axis scale a widget's buffer was actually rasterised at (`pixel_w / logical_w`),
/// recovered inside a paint closure rather than recomputed independently — see
/// `chrome::cache::CachedWidget::ensure`'s doc for why this must match exactly, not merely
/// approximately.
fn scale_factor(pixel_dimension: i32, logical_dimension: i32) -> f32 {
    pixel_dimension as f32 / logical_dimension.max(1) as f32
}

pub fn clear_identification(chrome: &mut ChromeState) { chrome.identification.clear(); }

pub fn collect_identification<R: ImportMem>(
    chrome: &mut ChromeState,
    output: &Output,
    number: usize,
    label: &str,
    calibration: bool,
    renderer: &mut R,
) -> Option<smithay::backend::renderer::element::utils::RescaleRenderElement<MemoryRenderBufferRenderElement<R>>>
where R::TextureId: Clone + Send + 'static {
    let signal = output.current_transform().transform_size(output.current_mode()?.size);
    let margins = if crate::viewport::hardware(output) { templar_types::display::Insets::default() } else { crate::viewport::margins(output) };
    let palette = &chrome.appearance.palette;
    let fonts = chrome.fonts.as_mut().map(|fonts| (&mut fonts.cache, fonts.face));
    let identification = chrome.identification.entry(output.clone()).or_insert_with(|| IdentificationWidget { number, label: label.to_owned(), calibration, margins, widget: CachedWidget::new_transparent() });
    if identification.number != number || identification.label != label || identification.calibration != calibration || identification.margins != margins {
        identification.number = number;
        identification.label.clear(); identification.label.push_str(label);
        identification.calibration = calibration; identification.margins = margins;
        identification.widget.invalidate();
    }
    let element = identification.widget.ensure(
        WidgetKey::new("", 0), (signal.w, signal.h), 1.0, (0.0, 0.0).into(), &mut chrome.scratch, renderer,
        move |painter, width, height| identification::paint(painter, width, height, number, label, calibration, margins, fonts, palette),
    )?;
    Some(smithay::backend::renderer::element::utils::RescaleRenderElement::from_element(element, (0, 0).into(), 1.0 / output.current_scale().fractional_scale()))
}

#[cfg(test)]
mod name_tab_menu_tests {
    use super::*;
    use templar_window::{CachedIdentity, ClientKind, IdentityCache, Output, Point};
    use templar_types::WindowClaims;

    struct UnknownIdentity;
    impl IdentityCache for UnknownIdentity {
        fn resolve_cached(&self, _: u32, _: &WindowClaims) -> Option<CachedIdentity> { None }
        fn request_resolution(&self, _: WindowId, _: u32, _: WindowClaims) {}
    }

    fn windows() -> (WindowService<UnknownIdentity>, WindowId, WindowId) {
        let output = Output::new("test", Rect::new(0, 0, 800, 600), 1000);
        let mut service = WindowService::new(UnknownIdentity, vec![output]);
        let first = service.on_toplevel_mapped(1, ClientKind::Application,
            WindowClaims::default(), "first".into(), Rect::new(100, 100, 300, 200));
        let second = service.on_toplevel_mapped(2, ClientKind::Application,
            WindowClaims::default(), "second".into(), Rect::new(200, 200, 300, 200));
        (service, first, second)
    }

    fn row_point(menu: &NameTabMenu, index: usize) -> Point {
        let row = menu.open.unwrap().row(index);
        Point { x: row.x + row.width / 2, y: row.y + row.height / 2 }
    }

    #[test]
    fn title_menu_commits_captured_window_across_focus_changes_and_reverses_layers() {
        let (mut service, first, second) = windows();
        let mut menu = NameTabMenu::default();
        for (index, preference) in [
            (0, WindowStacking::AlwaysOnTop), (1, WindowStacking::Normal),
            (2, WindowStacking::AlwaysOnBottom), (1, WindowStacking::Normal),
        ] {
            menu.show(first, Point { x: 790, y: 590 }, Rect::new(0, 0, 800, 600), 0x111);
            let point = row_point(&menu, index);
            assert!(menu.button(&mut service, point, 0x111, false));
            service.activate(second);
            let before_press = service.window(first).unwrap().stacking;
            assert!(menu.button(&mut service, point, 0x110, true));
            assert_eq!(service.window(first).unwrap().stacking, before_press, "a press must not commit");
            assert!(menu.button(&mut service, point, 0x110, false));
            assert!(menu.open.is_none());
            assert_eq!(service.window(first).unwrap().stacking, preference);
            assert_eq!(service.window(second).unwrap().stacking, WindowStacking::Normal);
            assert!(service.window(second).unwrap().activated);
            assert!(!service.window(first).unwrap().activated);
            assert_eq!(service.stacking_order().first().copied(),
                Some(if preference == WindowStacking::AlwaysOnTop { first } else { second }));
        }
    }

    #[test]
    fn title_menu_dismissal_and_disappearing_target_never_retarget_or_leak_release() {
        let (mut service, first, second) = windows();
        let mut menu = NameTabMenu::default();
        let output = Rect::new(0, 0, 800, 600);
        menu.show(first, Point { x: 100, y: 100 }, output, 0x111);
        let point = row_point(&menu, 0);
        assert!(menu.button(&mut service, point, 0x111, false));
        assert!(menu.button(&mut service, point, 0x110, true));
        service.on_toplevel_destroyed(first);
        assert!(menu.button(&mut service, point, 0x110, false));
        assert!(menu.open.is_none());
        assert_eq!(service.window(second).unwrap().stacking, WindowStacking::Normal);
        menu.show(second, Point { x: 100, y: 100 }, output, 0x111);
        assert!(menu.button(&mut service, point, 0x111, false));
        let outside = Point { x: 0, y: 0 };
        assert!(menu.button(&mut service, outside, 0x110, true));
        assert!(menu.open.is_none());
        assert!(menu.button(&mut service, outside, 0x110, false));
        assert!(!menu.button(&mut service, outside, 0x110, true));
        menu.show(second, Point { x: 100, y: 100 }, output, 0x111);
        menu.dismiss();
        assert!(menu.button(&mut service, point, 0x111, false),
            "Escape/destroy dismissal still owns the secondary opening release");
        assert_eq!(service.window(second).unwrap().stacking, WindowStacking::Normal);
    }
}

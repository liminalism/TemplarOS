//! Compositor-drawn chrome: a window's name and control tags, the resource-pressure overlay, the
//! "show every window" scatter labels, the fallback switcher and the lock screen, rendered with
//! `pixelkit-raster`/`pixelkit-text` (root `Cargo.toml`'s `[workspace.dependencies]` comment on
//! why those two crates specifically are safe to link into the render thread: no dependencies of
//! their own).
//!
//! Nothing here decides layout policy — `crates/templar-window` already computed every rectangle
//! (`tag_strip`, `scatter_layout`) before this module ever runs; this module only turns those
//! rectangles into pixels and positions them on screen. `chrome::layout` holds the pure
//! chrome-specific geometry (button rects, the switcher panel) that *is* decided here;
//! `chrome::text` finds a font without ever being able to panic; `chrome::paint` draws with one;
//! `chrome::cache` is what stops a repaint from happening on every frame regardless of whether
//! anything changed.
//!
//! There are two entry points, and the split is the whole point of them. [`collect_output_chrome`]
//! paints what belongs to the *output* — the lock screen, the pressure overlay, the fallback
//! switcher, the "show every window" scatter — which sits above every window there is; its own
//! internal order makes the lock screen win over the pressure overlay and the switcher win over
//! the scatter, matching `templar_window::Layer`'s ordering (`SecureLock` < `Recovery` <
//! `ShellOverlay`). [`collect_window_chrome`] paints what belongs to *one window* — its name tag
//! and its control tags, along its bottom edge — and that is emitted per window, interleaved with
//! that window's own surface, by `src/scene.rs`. A tag is part of its window, not of the screen:
//! emitting every band ahead of every surface (which is what a single flat list did) painted a
//! buried window's title over the translucent terminal in front of it.

pub mod cache;
pub mod layout;
pub mod paint;
pub mod text;

use cache::{CachedWidget, WidgetKey};
use smithay::{
    backend::renderer::{element::memory::MemoryRenderBufferRenderElement, gles::GlesRenderer},
    desktop::{Space, Window},
    output::Output,
    utils::{Logical, Physical, Point},
};
use std::collections::HashMap;
use templar_comp_integration::DbusIdentityCache;
use templar_window::{Rect, WindowId, WindowService};
use text::ChromeFonts;

/// Logical-pixel height of one fallback-switcher row.
const SWITCHER_ROW_HEIGHT: i32 = 30;
/// Logical-pixel height of a scatter tile's title strip (`chrome::layout::scatter_label_band`).
const SCATTER_LABEL_HEIGHT: i32 = 26;
/// Logical-pixel thickness of a window group's outline (planning doc §2 rule 7: "two logical
/// pixels in the group's accent colour").
const GROUP_OUTLINE_THICKNESS: i32 = 2;

/// The compositor-drawn fallback switcher's transient state (`TemplarCompState::step_switcher`,
/// `src/keybindings.rs`): what to list, in display order, and which entry was just selected. It
/// lives exactly as long as the frozen list it draws — until the modifier comes up, or until any
/// other action ends the walk (`src/input.rs`). This module owns no clock, so there is no timed
/// fade.
///
/// Two switchers share it. `Alt+Tab` lists every unshelved window, most-recently-activated first;
/// `Super+Tab` lists the groups themselves with their member counts under "Groups".
/// One HUD, because they are the same gesture at two scales, and because a second panel widget
/// would be a second thing to keep alive with the shell dead.
#[derive(Clone, Debug)]
pub struct SwitcherState {
    /// A title row above the entries: the group's name, or "Groups". `None` for the plain,
    /// groupless list — a person with no groups sees exactly the switcher they always had.
    pub heading: Option<String>,
    /// The group's accent, painted behind the heading so the HUD and the band agree at a glance.
    pub accent: Option<u32>,
    pub entries: Vec<(WindowId, String)>,
    pub selected: WindowId,
}

/// Long-lived chrome state: the discovered font (if any) and every widget's cached painted
/// bitmap, keyed so a window closing or the pressure overlay hiding can drop its cache entry
/// rather than accumulate forever. Constructed once per compositor run (`src/backend/winit.rs`,
/// `src/backend/udev.rs`), never per-frame.
pub struct ChromeState {
    fonts: Option<ChromeFonts>,
    scratch: pixelkit_raster::WindowBuffer,
    name_tags: HashMap<WindowId, CachedWidget>,
    control_tags: HashMap<WindowId, CachedWidget>,
    pressure: CachedWidget,
    lock: CachedWidget,
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
    /// The two-pixel corner mark that says where Peek is (`peek_affordance`).
    peek: CachedWidget,
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
            fonts,
            scratch: pixelkit_raster::WindowBuffer::new(1, 1),
            name_tags: HashMap::new(),
            control_tags: HashMap::new(),
            pressure: CachedWidget::new(),
            lock: CachedWidget::new(),
            taskfield_labels: HashMap::new(),
            taskfield_headers: Vec::new(),
            taskfield_scrim: CachedWidget::new(),
            taskfield_ring: Vec::new(),
            switcher_rows: HashMap::new(),
            switcher_heading: CachedWidget::new(),
            peek: CachedWidget::new(),
            outlines: HashMap::new(),
        }
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
    pub recovery_protocol: &'a crate::protocols_recovery::RecoveryProtocolState,
    pub session_lock: &'a crate::session_lock::SessionLockState,
    pub switcher: Option<&'a SwitcherState>,
    /// The window whose band a band drag is currently hovering, if any: it draws as a drop
    /// target, which is the whole visible difference between "moving a window over another" and
    /// "about to group them" (`src/input.rs`, planning doc §2 rule 6).
    pub drop_target: Option<WindowId>,
}

/// The window-independent chrome to draw over `output` this frame, front-to-back (index 0
/// topmost): the lock screen alone; else the pressure overlay alone; else the fallback switcher
/// and the "show every window" scatter. Everything here sits **above every window**, which is
/// what makes it output chrome rather than window chrome.
///
/// The returned flag says whether windows still get their own band and grip this frame. The lock
/// screen and the pressure overlay own the screen outright: drawing a title bar into either of
/// them would be drawing a control the user is not being offered.
pub fn collect_output_chrome(
    chrome: &mut ChromeState,
    inputs: &ChromeInputs<'_>,
    output: &Output,
    renderer: &mut GlesRenderer,
    background: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) -> (Vec<MemoryRenderBufferRenderElement<GlesRenderer>>, bool) {
    let Some(output_geometry) = inputs.space.output_geometry(output) else {
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

    if let Some(switcher) = inputs.switcher {
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

    collect_peek_affordance(
        chrome,
        inputs.window_service,
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
/// window's own chrome is showing (that strip covers this corner) and while a peek is already up
/// (the island is what is being reached for now, not the mark).
#[allow(clippy::too_many_arguments)]
fn collect_peek_affordance(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    output_rect: Rect,
    local_rect: Rect,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
    if window_service.covering_chrome().is_some()
        || window_service.peek_output().is_some()
        || !screen_filled_by_a_window(window_service, output_rect)
    {
        return;
    }
    let mark = crate::input::peek_affordance(local_rect);
    if mark.width <= 0 || mark.height <= 0 {
        return;
    }
    let colour = paint::RECOVERY_TAB_COLOR;
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

/// One window's own chrome, front-to-back: its name tag then its control tags, both laid out on
/// `WindowService::recovery_affordance`'s strip along the window's bottom edge, then its group
/// outline, appended to `elements`. Nothing for a window without a frame (`WindowService::has_frame`):
/// a fullscreen window, or a maximized one whose controls the shell's bar holds.
///
/// Both belong to `window_id` and to nothing else, so `src/scene.rs` emits them immediately
/// *above that window's surface and below every window in front of it* — never as part of one
/// flat chrome list, which is what used to paint a buried window's title across the translucent
/// terminal on top of it.
///
/// The grip is drawn for the same reason the band is: a window whose client draws no frame — most
/// XWayland windows, anything borderless — would otherwise have no pointer route to a resize at
/// all, and an affordance nobody can see is not one. The service decides which windows have a
/// grip (not maximized, not fullscreen, tall enough); this only paints what it is told.
///
/// A group's outline is window chrome too, and for the same reason: it must survive the shell
/// dying, and it must never float above a window that has nothing to do with the group. It is
/// emitted with the group's **bottom-most** visible member, so anything stacked above that member
/// — including an unrelated window sitting between two members — occludes it exactly as it
/// occludes that member's own band.
#[allow(clippy::too_many_arguments)]
pub fn collect_window_chrome(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    window_id: WindowId,
    drop_target: Option<WindowId>,
    output_rect: Rect,
    output_scale: f64,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
    renderer: &mut GlesRenderer,
) {
    // A window filling the screen wears no frame, but it can be *asked* for one: resting the
    // pointer in either bottom corner shows its name tag at the left and its controls at the
    // right, drawn over the window rather than beside it. Its geometry is the whole output, so
    // the strip `recovery_affordance` already computes lands exactly on the bottom edge of the
    // screen — which is where the user asked for them, and why this needs no geometry of its own.
    if window_service.has_frame(window_id) || window_service.covering_chrome() == Some(window_id) {
        collect_name_tag(
            chrome,
            window_service,
            window_id,
            drop_target == Some(window_id),
            output_rect,
            output_scale,
            renderer,
            elements,
        );
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

fn paint_lock_screen(
    chrome: &mut ChromeState,
    local_rect: Rect,
    output_scale: f64,
    location: Point<f64, Physical>,
    renderer: &mut GlesRenderer,
) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
    let key = WidgetKey::new("locked", paint::LOCK_SCREEN_COLOR);
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
            );
        },
    )
}

fn paint_pressure_overlay(
    chrome: &mut ChromeState,
    overlay: &crate::protocols_recovery::PressureOverlay,
    local_rect: Rect,
    output_scale: f64,
    location: Point<f64, Physical>,
    renderer: &mut GlesRenderer,
) -> Option<MemoryRenderBufferRenderElement<GlesRenderer>> {
    let key = WidgetKey::new(overlay.message.clone(), paint::PRESSURE_OVERLAY_COLOR)
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
            );
        },
    )
}

fn collect_switcher(
    chrome: &mut ChromeState,
    switcher: &SwitcherState,
    local_rect: Rect,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
    let heading_rows = usize::from(switcher.heading.is_some());
    let panel = layout::switcher_panel(
        local_rect,
        switcher.entries.len() + heading_rows,
        SWITCHER_ROW_HEIGHT,
    );
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

    for (index, (window_id, title)) in switcher.entries.iter().enumerate() {
        live.insert(*window_id);
        let row = layout::switcher_row(panel, index + heading_rows, SWITCHER_ROW_HEIGHT);
        let selected = *window_id == switcher.selected;
        let background = if selected {
            paint::SWITCHER_SELECTED_COLOR
        } else {
            paint::SWITCHER_PANEL_COLOR
        };
        let key = WidgetKey::new(title.clone(), background).with_variant(u32::from(selected));
        let title = title.clone();
        let fonts = chrome.fonts.as_mut().map(|f| (&mut f.cache, f.face));
        let location = local_physical(row, output_scale);
        let widget = chrome.switcher_rows.entry(*window_id).or_default();
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
                    &title,
                    selected,
                    fonts,
                    scale_factor(pixel_w, row.width),
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
fn collect_switcher_heading(
    chrome: &mut ChromeState,
    heading: &str,
    accent: Option<u32>,
    row: Rect,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
    let background = accent.map_or(paint::SWITCHER_PANEL_COLOR, paint::opaque);
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
fn collect_taskfield(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    taskfield: &templar_window::TaskfieldState,
    output_rect: Rect,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
    background: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
    let origin = Point::from((output_rect.x, output_rect.y));

    // The scrim, over this whole output. It is what makes the mode read as a mode: the desktop
    // behind the cards is still being drawn, and without it a card would sit on whatever window
    // happened to be underneath.
    if let Some(element) = chrome.taskfield_scrim.ensure(
        WidgetKey::new("taskfield-scrim", paint::TASKFIELD_SCRIM_COLOR),
        (output_rect.width, output_rect.height),
        output_scale,
        to_physical(output_rect, origin, output_scale),
        &mut chrome.scratch,
        renderer,
        |painter, pixel_w, pixel_h| {
            paint::group_outline_bar(painter, pixel_w, pixel_h, paint::TASKFIELD_SCRIM_COLOR);
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
fn taskfield_headers(
    chrome: &mut ChromeState,
    sections: &[(Rect, String, Option<u32>)],
    origin: Point<i32, Logical>,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
    chrome
        .taskfield_headers
        .resize_with(sections.len(), CachedWidget::new);
    for (widget, (rect, label, accent)) in chrome.taskfield_headers.iter_mut().zip(sections) {
        let colour = accent.map_or(paint::SCATTER_LABEL_COLOR, paint::opaque);
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
fn taskfield_labels(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    taskfield: &templar_window::TaskfieldState,
    output_rect: Rect,
    origin: Point<i32, Logical>,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
    // One title band per card, inside the bottom of the card itself: the card *is* the window, so
    // there is no tile below it to hang a strip off, and a strip outside it would overlap the next
    // row. `scatter_label_band` splits the rectangle the same way it always did.
    let mut live = std::collections::HashSet::new();
    for card in taskfield.cards() {
        if output_rect.intersection(card.rect) != Some(card.rect) {
            continue;
        }
        live.insert(card.window);
        let selected = taskfield.selected == Some(card.window);
        let title = window_service
            .window(card.window)
            .map(|window| window.title.clone())
            .unwrap_or_default();
        let (_, label) = layout::scatter_label_band(card.rect, SCATTER_LABEL_HEIGHT);
        let background_colour = if selected {
            paint::SCATTER_FOCUS_ACCENT
        } else {
            paint::SCATTER_LABEL_COLOR
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
                    selected,
                    fonts,
                    scale_factor(pixel_w, label.width),
                );
            },
        ) {
            elements.push(element);
        }
    }
    chrome.taskfield_labels.retain(|id, _| live.contains(id));
}

/// The four bars ringing the selected card.
#[allow(clippy::too_many_arguments)]
fn taskfield_ring(
    chrome: &mut ChromeState,
    taskfield: &templar_window::TaskfieldState,
    output_rect: Rect,
    origin: Point<i32, Logical>,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
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
    for (widget, bar) in chrome
        .taskfield_ring
        .iter_mut()
        .zip(layout::outline_bars(ring, GROUP_OUTLINE_THICKNESS))
    {
        if let Some(element) = widget.ensure(
            WidgetKey::new("taskfield-ring", paint::SCATTER_FOCUS_ACCENT),
            (bar.width, bar.height),
            output_scale,
            to_physical(bar, origin, output_scale),
            &mut chrome.scratch,
            renderer,
            |painter, pixel_w, pixel_h| {
                paint::group_outline_bar(painter, pixel_w, pixel_h, paint::SCATTER_FOCUS_ACCENT);
            },
        ) {
            elements.push(element);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_name_tag(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    window_id: WindowId,
    drop_target: bool,
    output_rect: Rect,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
    let Some(window) = window_service.window(window_id) else {
        return;
    };
    let Some(affordance) = window_service.recovery_affordance(window_id) else {
        return;
    };
    let band = layout::name_tag(affordance.strip);
    if output_rect.intersection(band).is_none() {
        return;
    }
    let background = if drop_target {
        // A name tag being hovered by another window's name-tag drag: releasing here groups the
        // two, and saying so before the release is the difference between a gesture and an
        // accident.
        paint::BAND_DROP_TARGET_COLOR
    } else if affordance.requires_compositor_paint {
        paint::RECOVERY_TAB_COLOR
    } else {
        paint::RECOVERY_BAND_COLOR
    };
    // The group's stripe and name, if this window is in one: the same accent the outline and the
    // Home tile use, so one group looks like one thing wherever it appears (§2 rule 7).
    let group = window_service
        .group_of(window_id)
        .and_then(|group| window_service.group_record(group))
        .map(|record| (record.accent, record.display_name().to_owned()));
    // Pinned above the other windows: its name is drawn in italic, so the state is legible from
    // the tag itself rather than from watching what the stacking order does.
    let pinned = window_service.is_always_on_top(window_id);
    let key = WidgetKey::new(window.title.clone(), background)
        .with_secondary(group.clone().map(|(_, name)| name).unwrap_or_default())
        .with_variant(group.as_ref().map_or(0, |(accent, _)| *accent))
        .with_flags(u32::from(pinned));
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
fn collect_group_outline(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    window_id: WindowId,
    output_rect: Rect,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
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

/// The control tags — park, maximize, close — packed against the right end of the window's tag
/// strip (`layout::control_tags`, the same rectangles `src/input.rs` hit-tests), painted as one
/// widget over their union.
fn collect_control_tags(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    window_id: WindowId,
    output_rect: Rect,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
    let Some(affordance) = window_service.recovery_affordance(window_id) else {
        return;
    };
    let strip = affordance.strip;
    let tags = layout::control_tags(strip);
    let Some((_, first)) = tags.first() else {
        return;
    };
    let union = Rect::new(first.x, strip.y, strip.right() - first.x, strip.height);
    if output_rect.intersection(union).is_none() {
        return;
    }
    let background = if affordance.requires_compositor_paint {
        paint::RECOVERY_TAB_COLOR
    } else {
        paint::RECOVERY_BAND_COLOR
    };
    // Nothing about the tags varies with the window but how many fit, and `ensure` already keys
    // on the size: the count is the whole key.
    let key = WidgetKey::new("control-tags", background)
        .with_variant(u32::try_from(tags.len()).unwrap_or(0));
    let location = to_physical(
        union,
        Point::from((output_rect.x, output_rect.y)),
        output_scale,
    );
    let local: Vec<(layout::ControlTag, Rect)> = tags
        .into_iter()
        .map(|(tag, rect)| {
            (
                tag,
                Rect::new(rect.x - union.x, rect.y - union.y, rect.width, rect.height),
            )
        })
        .collect();
    let widget = chrome.control_tags.entry(window_id).or_default();
    if let Some(element) = widget.ensure(
        key,
        (union.width, union.height),
        output_scale,
        location,
        &mut chrome.scratch,
        renderer,
        move |painter, pixel_w, pixel_h| {
            let scale = scale_factor(pixel_w, union.width);
            let tags: Vec<(layout::ControlTag, pixelkit_raster::Rect)> = local
                .iter()
                .map(|(tag, rect)| (*tag, paint::scaled_px(*rect, scale)))
                .collect();
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

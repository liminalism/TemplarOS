//! Compositor-drawn chrome: the recovery band/tab, the resource-pressure overlay, the
//! "show every window" scatter labels, the fallback switcher and the lock screen, rendered with
//! `pixelkit-raster`/`pixelkit-text` (root `Cargo.toml`'s `[workspace.dependencies]` comment on
//! why those two crates specifically are safe to link into the render thread: no dependencies of
//! their own).
//!
//! Nothing here decides layout policy — `crates/lege-window` already computed every rectangle
//! (`recovery_band`, `scatter_layout`) before this module ever runs; this module only turns those
//! rectangles into pixels and positions them on screen. `chrome::layout` holds the pure
//! chrome-specific geometry (button rects, the switcher panel) that *is* decided here;
//! `chrome::text` finds a font without ever being able to panic; `chrome::paint` draws with one;
//! `chrome::cache` is what stops a repaint from happening on every frame regardless of whether
//! anything changed.
//!
//! [`collect_elements`] is `src/backend/winit.rs` and `src/backend/udev.rs`'s shared entry point,
//! called once per output per frame — both backends already keep their `custom_elements` ordered
//! ahead of `Space`'s own content so chrome always wins over client surfaces
//! (`src/backend/udev.rs`'s module-level comment); this module's own internal order additionally
//! makes the lock screen win over the pressure overlay, and the fallback switcher win over the
//! "show every window" scatter, matching `lege_window::Layer`'s ordering
//! (`SecureLock` < `Recovery` < `ShellOverlay`).

pub mod cache;
pub mod layout;
pub mod paint;
pub mod text;

use cache::{CachedWidget, WidgetKey};
use lege_comp_integration::DbusIdentityCache;
use lege_window::{Rect, WindowId, WindowService};
use smithay::{
    backend::renderer::{element::memory::MemoryRenderBufferRenderElement, gles::GlesRenderer},
    desktop::{Space, Window},
    output::Output,
    utils::{Logical, Physical, Point},
};
use std::collections::HashMap;
use text::ChromeFonts;

/// Logical-pixel height of one fallback-switcher row.
const SWITCHER_ROW_HEIGHT: i32 = 30;
/// Logical-pixel height of a scatter tile's title strip (`chrome::layout::scatter_label_band`).
const SCATTER_LABEL_HEIGHT: i32 = 26;

/// The compositor-drawn fallback switcher's transient state (`WindowService::cycle_focus`,
/// `src/keybindings.rs`): which windows to list, in display order, and which one was just
/// selected. Cleared by `src/input.rs` on the next input that is not itself another switcher step
/// — this module owns no clock, so there is no timed fade, only "until you do something else".
#[derive(Clone, Debug)]
pub struct SwitcherState {
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
    bands: HashMap<WindowId, CachedWidget>,
    pressure: CachedWidget,
    lock: CachedWidget,
    scatter_cards: HashMap<WindowId, CachedWidget>,
    scatter_labels: HashMap<WindowId, CachedWidget>,
    switcher_rows: HashMap<WindowId, CachedWidget>,
}

impl std::fmt::Debug for ChromeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromeState")
            .field("has_font", &self.fonts.is_some())
            .field("bands", &self.bands.len())
            .field("scatter_cards", &self.scatter_cards.len())
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
            bands: HashMap::new(),
            pressure: CachedWidget::new(),
            lock: CachedWidget::new(),
            scatter_cards: HashMap::new(),
            scatter_labels: HashMap::new(),
            switcher_rows: HashMap::new(),
        }
    }

    /// Whether a font was found — exposed for `main.rs`/logging and tests, not policy.
    pub fn has_font(&self) -> bool {
        self.fonts.is_some()
    }
}

/// Everything [`collect_elements`] reads from `LegeCompState` — bundled so the function itself
/// takes individually-borrowed fields rather than the whole state, which is what lets
/// `src/backend/udev.rs` call it while a device's `GlesRenderer` (borrowed from
/// `state.backend_data`, a different field) is simultaneously held mutably.
pub struct ChromeInputs<'a> {
    pub space: &'a Space<Window>,
    pub window_service: &'a WindowService<DbusIdentityCache>,
    pub recovery_protocol: &'a crate::protocols_recovery::RecoveryProtocolState,
    pub show_all_windows: Option<&'a [(WindowId, Rect)]>,
    pub session_lock: &'a crate::session_lock::SessionLockState,
    pub switcher: Option<&'a SwitcherState>,
}

/// Every chrome element to draw over `output` this frame, front-to-back (index 0 topmost) —
/// matching `render_output`/`UdevRenderElement`'s existing convention (`src/backend/udev.rs`'s
/// module comment). Priority, top to bottom: the lock screen alone; else the pressure overlay
/// alone; else the fallback switcher, the "show every window" scatter, and every window's
/// recovery band/tab, together.
pub fn collect_elements(
    chrome: &mut ChromeState,
    inputs: &ChromeInputs<'_>,
    output: &Output,
    renderer: &mut GlesRenderer,
) -> Vec<MemoryRenderBufferRenderElement<GlesRenderer>> {
    let Some(output_geometry) = inputs.space.output_geometry(output) else {
        return Vec::new();
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
        return elements;
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
        return elements;
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

    if let Some(layout) = inputs.show_all_windows {
        collect_scatter(
            chrome,
            inputs.window_service,
            layout,
            output_rect,
            output_scale,
            renderer,
            &mut elements,
        );
    } else {
        chrome.scatter_cards.clear();
        chrome.scatter_labels.clear();
    }

    collect_bands(
        chrome,
        inputs.window_service,
        output_rect,
        output_scale,
        renderer,
        &mut elements,
    );

    elements
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
    let panel = layout::switcher_panel(local_rect, switcher.entries.len(), SWITCHER_ROW_HEIGHT);
    let mut live = std::collections::HashSet::new();

    for (index, (window_id, title)) in switcher.entries.iter().enumerate() {
        live.insert(*window_id);
        let row = layout::switcher_row(panel, index, SWITCHER_ROW_HEIGHT);
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

#[allow(clippy::too_many_arguments)]
fn collect_scatter(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    layout_rects: &[(WindowId, Rect)],
    output_rect: Rect,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
    let mut live = std::collections::HashSet::new();
    for (window_id, tile) in layout_rects {
        // `scatter_layout` guarantees every tile is fully contained in the output it was
        // assigned to (its own doc/proptest) — this output pass only draws the tiles that landed
        // on *this* output, so a multi-monitor "show every window" does not duplicate a tile onto
        // every screen.
        if output_rect.intersection(*tile) != Some(*tile) {
            continue;
        }
        live.insert(*window_id);
        let focused = window_service
            .window(*window_id)
            .is_some_and(|w| w.activated);
        let title = window_service
            .window(*window_id)
            .map(|w| w.title.clone())
            .unwrap_or_default();
        let (card, label) = layout::scatter_label_band(*tile, SCATTER_LABEL_HEIGHT);

        let card_key = WidgetKey::new(String::new(), paint::SCATTER_CARD_COLOR)
            .with_variant(u32::from(focused));
        let card_location = to_physical(
            card,
            Point::from((output_rect.x, output_rect.y)),
            output_scale,
        );
        let card_widget = chrome.scatter_cards.entry(*window_id).or_default();
        if let Some(element) = card_widget.ensure(
            card_key,
            (card.width, card.height),
            output_scale,
            card_location,
            &mut chrome.scratch,
            renderer,
            move |painter, pixel_w, pixel_h| {
                paint::scatter_card(
                    painter,
                    pixel_w,
                    pixel_h,
                    focused,
                    scale_factor(pixel_w, card.width),
                );
            },
        ) {
            elements.push(element);
        }

        let label_background = if focused {
            paint::SCATTER_FOCUS_ACCENT
        } else {
            paint::SCATTER_LABEL_COLOR
        };
        let label_key = WidgetKey::new(title.clone(), label_background);
        let label_location = to_physical(
            label,
            Point::from((output_rect.x, output_rect.y)),
            output_scale,
        );
        let fonts = chrome.fonts.as_mut().map(|f| (&mut f.cache, f.face));
        let label_widget = chrome.scatter_labels.entry(*window_id).or_default();
        let title_for_paint = title.clone();
        if let Some(element) = label_widget.ensure(
            label_key,
            (label.width, label.height),
            output_scale,
            label_location,
            &mut chrome.scratch,
            renderer,
            move |painter, pixel_w, pixel_h| {
                paint::scatter_label(
                    painter,
                    pixel_w,
                    pixel_h,
                    &title_for_paint,
                    focused,
                    fonts,
                    scale_factor(pixel_w, label.width),
                );
            },
        ) {
            elements.push(element);
        }
    }
    chrome.scatter_cards.retain(|id, _| live.contains(id));
    chrome.scatter_labels.retain(|id, _| live.contains(id));
}

fn collect_bands(
    chrome: &mut ChromeState,
    window_service: &WindowService<DbusIdentityCache>,
    output_rect: Rect,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
) {
    let mut live = std::collections::HashSet::new();
    for window in window_service.windows() {
        if window.shelved {
            continue;
        }
        let Some(affordance) = window_service.recovery_affordance(window.id) else {
            continue;
        };
        let band = affordance.band;
        if output_rect.intersection(band).is_none() {
            continue;
        }
        live.insert(window.id);
        let background = if affordance.requires_compositor_paint {
            paint::RECOVERY_TAB_COLOR
        } else {
            paint::RECOVERY_BAND_COLOR
        };
        let key = WidgetKey::new(window.title.clone(), background);
        let title = window.title.clone();
        let location = to_physical(
            band,
            Point::from((output_rect.x, output_rect.y)),
            output_scale,
        );
        let fonts = chrome.fonts.as_mut().map(|f| (&mut f.cache, f.face));
        let widget = chrome.bands.entry(window.id).or_default();
        if let Some(element) = widget.ensure(
            key,
            (band.width, band.height),
            output_scale,
            location,
            &mut chrome.scratch,
            renderer,
            move |painter, pixel_w, pixel_h| {
                paint::recovery_band(
                    painter,
                    pixel_w,
                    pixel_h,
                    background,
                    &title,
                    fonts,
                    scale_factor(pixel_w, band.width),
                );
            },
        ) {
            elements.push(element);
        }
    }
    chrome.bands.retain(|id, _| live.contains(id));
}

/// The real per-axis scale a widget's buffer was actually rasterised at (`pixel_w / logical_w`),
/// recovered inside a paint closure rather than recomputed independently — see
/// `chrome::cache::CachedWidget::ensure`'s doc for why this must match exactly, not merely
/// approximately.
fn scale_factor(pixel_dimension: i32, logical_dimension: i32) -> f32 {
    pixel_dimension as f32 / logical_dimension.max(1) as f32
}

//! One frame's render elements, in the order they must be drawn.
//!
//! Both backends (`src/backend/udev.rs`, `src/backend/winit.rs`) hand Smithay a single flat
//! `&[RenderElement]` ordered **front-to-back**: index 0 is topmost. Getting that order right is
//! the whole of this module, and it is a correctness question rather than a cosmetic one — a
//! translucent client surface blends with whatever was drawn *under* it, so an element in the
//! wrong half of the list is not merely out of place, it is composited into the wrong pixels.
//!
//! The order is:
//!
//! 1. the pointer cursor (the backend prepends it; it is above everything by definition);
//! 2. **output chrome** — the lock screen, the pressure overlay, the fallback switcher, the "show
//!    every window" scatter (`chrome::collect_output_chrome`). These belong to the screen, not to
//!    any window, and stay above every window there is;
//! 3. then, for each window in `WindowService::stacking_order()` **top first**: that window's own
//!    band and grip (`chrome::collect_window_chrome`), immediately followed by that window's
//!    surface elements.
//!
//! Step 3 is the fix for the fault this module was extracted for. Chrome used to be collected as
//! one flat list and pushed ahead of *all* of `space_render_elements`, which put every window's
//! title band above every window's content: the band of a window buried three deep painted itself
//! across the translucent terminal in front of it, at full opacity, because it was drawn later —
//! that is, higher. A band is part of its window. Interleaving it with that window's own surface
//! is what makes a higher window occlude a lower window's title, exactly as it occludes a lower
//! window's content.
//!
//! [`window_scene_order`] is the pure statement of that rule, testable with no renderer, no
//! display and no `Space`; [`collect`] is the same rule with real pixels hung off it.

use crate::chrome::{ChromeInputs, ChromeState};
use smithay::{
    backend::renderer::{
        element::{
            AsRenderElements, memory::MemoryRenderBufferRenderElement,
            surface::WaylandSurfaceRenderElement,
        },
        gles::GlesRenderer,
    },
    desktop::{Space, Window},
    output::Output,
    utils::Scale,
};
use std::collections::HashSet;
use templar_window::{Rect, WindowId};

// The one element type both backends draw a frame out of. `render_elements!` (Smithay macro,
// `#[macro_export]`'d — not doc-commentable, hence `//` rather than `///`) generates
// `SceneElement` and its `RenderElement<GlesRenderer>` impl.
//
// There is deliberately no `Space` variant: this module never calls `space_render_elements`,
// because that function returns one flat list for the whole space and the whole point here is to
// interleave per window. `Space` is still where a window's *position* comes from — it remains the
// render mirror `src/space_sync.rs` maintains — only the flattening is gone.
smithay::backend::renderer::element::render_elements! {
    pub SceneElement<=GlesRenderer>;
    Cursor=crate::cursor::CursorRenderElement,
    Chrome=MemoryRenderBufferRenderElement<GlesRenderer>,
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
}

/// One thing to draw for one window, in front-to-back order within that window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScenePiece {
    /// The window's compositor-drawn chrome: its title band and its resize grip.
    Chrome(WindowId),
    /// The window's own surface (and its popups).
    Surface(WindowId),
}

/// The per-window half of the frame's draw order, front-to-back, from a stacking order that is
/// already top-first (`WindowService::stacking_order`).
///
/// Each window contributes its chrome and then its surface, and the *next* window down starts
/// only after both — so window N's surface is drawn above window N+1's band. `visible` is
/// `WindowService::is_visible`'s verdict and nothing else (COMPOSITOR.md property 7: a hidden
/// window neither renders, takes clicks nor paints a band), and `chrome` says whether this
/// window has any band or grip to draw at all — false for a fullscreen window, for a shell
/// surface, and for every window while the lock screen or the pressure overlay owns the screen.
///
/// Pure: no renderer, no `Space`, no display. This is the statement of the rule; [`collect`] is
/// its application.
pub fn window_scene_order(windows: &[(WindowId, bool, bool)]) -> Vec<ScenePiece> {
    let mut pieces = Vec::with_capacity(windows.len() * 2);
    for (window_id, visible, chrome) in windows.iter().copied() {
        if !visible {
            continue;
        }
        if chrome {
            pieces.push(ScenePiece::Chrome(window_id));
        }
        pieces.push(ScenePiece::Surface(window_id));
    }
    pieces
}

/// Every element to draw over `output` this frame except the cursor, front-to-back — the module
/// doc's order, with pixels.
///
/// Takes `chrome`/`windows`/`inputs` as separately-borrowed pieces rather than `&mut
/// TemplarCompState<B>` for the reason `ChromeInputs` exists: `src/backend/udev.rs` calls this while
/// a device's `GlesRenderer`, borrowed out of `state.backend_data`, is held mutably.
pub fn collect(
    chrome: &mut ChromeState,
    windows: &crate::window_bridge::WindowIndex,
    inputs: &ChromeInputs<'_>,
    output: &Output,
    renderer: &mut GlesRenderer,
) -> Vec<SceneElement> {
    let Some(output_geometry) = inputs.space.output_geometry(output) else {
        return Vec::new();
    };
    let output_rect = Rect::new(
        output_geometry.loc.x,
        output_geometry.loc.y,
        output_geometry.size.w,
        output_geometry.size.h,
    );
    let output_scale = output.current_scale().fractional_scale();

    let (overlay, windows_get_chrome) =
        crate::chrome::collect_output_chrome(chrome, inputs, output, renderer);
    let mut elements: Vec<SceneElement> = overlay.into_iter().map(SceneElement::Chrome).collect();

    let plan: Vec<(WindowId, bool, bool)> = inputs
        .window_service
        .stacking_order()
        .into_iter()
        .map(|window_id| {
            (
                window_id,
                inputs.window_service.is_visible(window_id),
                windows_get_chrome,
            )
        })
        .collect();

    let mut live_chrome = HashSet::new();
    // A `Vec` rather than a set: `smithay::desktop::Window` has interior mutability and so cannot
    // be a hash key (clippy's `mutable_key_type`), and a scene has a handful of windows in it,
    // not thousands.
    let mut drawn: Vec<Window> = inputs
        .window_service
        .stacking_order()
        .into_iter()
        .filter(|id| !inputs.window_service.is_visible(*id))
        .filter_map(|id| windows.element_of(id).cloned())
        .collect();
    let mut chrome_elements = Vec::new();
    for piece in window_scene_order(&plan) {
        match piece {
            ScenePiece::Chrome(window_id) => {
                chrome_elements.clear();
                crate::chrome::collect_window_chrome(
                    chrome,
                    inputs.window_service,
                    window_id,
                    inputs.drop_target,
                    output_rect,
                    output_scale,
                    &mut chrome_elements,
                    renderer,
                );
                if !chrome_elements.is_empty() {
                    live_chrome.insert(window_id);
                }
                elements.extend(chrome_elements.drain(..).map(SceneElement::Chrome));
            }
            ScenePiece::Surface(window_id) => {
                let Some(window) = windows.element_of(window_id) else {
                    continue;
                };
                drawn.push(window.clone());
                surface_elements(
                    window,
                    inputs.space,
                    output_geometry,
                    output_scale,
                    renderer,
                    &mut elements,
                );
            }
        }
    }

    // A safety net, not a second policy: anything `Space` is showing that the stacking order did
    // not account for is drawn beneath everything the order did, rather than silently vanishing.
    // `WindowService` is the sole z-order authority (COMPOSITOR.md property 4), so this list is
    // normally empty — but a window that is on screen must not disappear because two mirrors of
    // the same fact briefly disagreed. `Space::elements()` is bottom-first, so `rev()` here keeps
    // this tail front-to-back like the rest.
    let leftovers: Vec<Window> = inputs
        .space
        .elements()
        .rev()
        .filter(|window| !drawn.contains(window))
        .cloned()
        .collect();
    for window in &leftovers {
        surface_elements(
            window,
            inputs.space,
            output_geometry,
            output_scale,
            renderer,
            &mut elements,
        );
    }

    chrome.retain_window_chrome(&live_chrome);
    elements
}

/// One window's surface elements, positioned exactly as `Space` would have positioned them.
///
/// This is `Space::render_elements_for_region`'s per-element body, spelled out for one element:
/// a window's render origin is its `Space` location minus its own `geometry().loc` (the client's
/// shadow/decoration offset inside its surface), and the region it is drawn against is the
/// output's rectangle. Deriving it here rather than calling `space_render_elements` is what
/// allows the interleaving in [`collect`]; deriving it the *same* way is what keeps a window in
/// exactly the pixels it was in before.
fn surface_elements(
    window: &Window,
    space: &Space<Window>,
    output_geometry: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
    output_scale: f64,
    renderer: &mut GlesRenderer,
    elements: &mut Vec<SceneElement>,
) {
    let Some(location) = space.element_location(window) else {
        return;
    };
    let render_location = location - window.geometry().loc;
    let mut bbox = window.bbox();
    bbox.loc += render_location;
    if !output_geometry.overlaps(bbox) {
        return;
    }
    let scale = Scale::from(output_scale);
    let physical = (render_location - output_geometry.loc).to_physical_precise_round(scale);
    elements.extend(
        AsRenderElements::<GlesRenderer>::render_elements::<
            WaylandSurfaceRenderElement<GlesRenderer>,
        >(window, renderer, physical, scale, 1.0)
        .into_iter()
        .map(SceneElement::Surface),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: u64) -> WindowId {
        WindowId(raw)
    }

    /// The fault the user reported: a buried window's title bar painted over the translucent
    /// terminal in front of it. In draw order that is "every chrome element before every
    /// surface"; the fix is that each window's chrome sits between its own surface and the
    /// surface of the window above it.
    ///
    /// Read front-to-back: the top window's surface must come *before* — that is, above — the
    /// window beneath it gets its band.
    #[test]
    fn a_higher_window_is_drawn_over_the_window_below_its_title_band() {
        let top = id(1);
        let bottom = id(2);
        let order = window_scene_order(&[(top, true, true), (bottom, true, true)]);
        assert_eq!(
            order,
            vec![
                ScenePiece::Chrome(top),
                ScenePiece::Surface(top),
                ScenePiece::Chrome(bottom),
                ScenePiece::Surface(bottom),
            ]
        );

        let top_surface = order
            .iter()
            .position(|piece| *piece == ScenePiece::Surface(top))
            .expect("the top window's surface is drawn");
        let bottom_band = order
            .iter()
            .position(|piece| *piece == ScenePiece::Chrome(bottom))
            .expect("the bottom window's band is drawn");
        assert!(
            top_surface < bottom_band,
            "the window on top must be drawn above the band of the window beneath it, or a \
             translucent surface blends with the wallpaper and then has a title bar stamped on \
             top of it at full opacity"
        );
    }

    /// A window's band belongs to that window, so it is never drawn without it: a hidden window
    /// (shelved, or behind a "go home" reveal) contributes nothing at all — COMPOSITOR.md's
    /// property 7, "a hidden window neither renders, takes clicks nor paints a band".
    #[test]
    fn a_hidden_window_contributes_neither_tags_nor_surface() {
        let visible = id(1);
        let hidden = id(2);
        let order = window_scene_order(&[(visible, true, true), (hidden, false, true)]);
        assert_eq!(
            order,
            vec![ScenePiece::Chrome(visible), ScenePiece::Surface(visible)]
        );
    }

    /// A fullscreen window and every shell surface have no frame (COMPOSITOR.md property 2), and
    /// no window has one while the lock screen owns the screen — the surface is still drawn.
    #[test]
    fn a_window_with_no_chrome_still_draws_its_surface() {
        let framed = id(1);
        let bare = id(2);
        let order = window_scene_order(&[(bare, true, false), (framed, true, true)]);
        assert_eq!(
            order,
            vec![
                ScenePiece::Surface(bare),
                ScenePiece::Chrome(framed),
                ScenePiece::Surface(framed),
            ]
        );
    }

    /// The order is the stacking order and nothing else: three windows keep their relative
    /// positions, and each one's chrome stays with its own surface.
    #[test]
    fn every_window_keeps_its_own_chrome_beside_its_own_surface() {
        let windows: Vec<(WindowId, bool, bool)> =
            (1..=3).map(|raw| (id(raw), true, true)).collect();
        let order = window_scene_order(&windows);
        for (index, (window_id, _, _)) in windows.iter().enumerate() {
            assert_eq!(order[index * 2], ScenePiece::Chrome(*window_id));
            assert_eq!(order[index * 2 + 1], ScenePiece::Surface(*window_id));
        }
    }
}

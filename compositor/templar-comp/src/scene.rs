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
//! 2. **output chrome** — the lock screen, the pressure overlay, the fallback switcher, the window
//!    overview's headers and labels (`chrome::collect_output_chrome`). These belong to the screen,
//!    not to any window, and stay above every window there is;
//! 3. then, for each window in `WindowService::stacking_order()` **top first**: that window's own
//!    band and grip (`chrome::collect_window_chrome`), immediately followed by that window's
//!    surface elements;
//! 4. finally the output chrome's **background** — today just the window overview's scrim — which
//!    is the one piece of chrome that belongs *under* the windows, because in that mode the
//!    windows are the interface and the scrim is the ground they stand on.
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
use crate::wallpaper::WallpaperRenderer;
use smithay::{
    backend::renderer::{
        ImportAll, ImportMem,
        element::{
            AsRenderElements, memory::MemoryRenderBufferRenderElement,
            surface::WaylandSurfaceRenderElement,
        },
    },
    desktop::{Space, Window},
    output::Output,
    utils::{Logical, Rectangle, Scale},
};
use std::collections::HashSet;
use templar_window::{Rect, WindowId};

// The one element type both backends draw a frame out of. `render_elements!` (Smithay macro,
// `#[macro_export]`'d — not doc-commentable, hence `//` rather than `///`) generates
// `SceneElement` and its renderer-generic `RenderElement` impl.
//
// There is deliberately no `Space` variant: this module never calls `space_render_elements`,
// because that function returns one flat list for the whole space and the whole point here is to
// interleave per window. `Space` is still where a window's *position* comes from — it remains the
// render mirror `src/space_sync.rs` maintains — only the flattening is gone.
smithay::backend::renderer::element::render_elements! {
    pub SceneElement<R> where R: ImportAll + ImportMem + WallpaperRenderer;
    Cursor=crate::cursor::CursorRenderElement<R>,
    Chrome=MemoryRenderBufferRenderElement<R>,
    Surface=WaylandSurfaceRenderElement<R>,
    Wallpaper=crate::wallpaper::WallpaperElement,
    Essence=crate::essence_renderer::EssenceElement,
    Ground=smithay::backend::renderer::element::solid::SolidColorRenderElement,
}

// Native composition keeps the zero-inset path unwrapped. Calibrated elements are constrained
// after the complete scene, including the cursor, so damage/capture/hit geometry agree.
smithay::backend::renderer::element::render_elements! {
    pub NativeSceneElement<R> where R: ImportAll + ImportMem + WallpaperRenderer;
    Plain=SceneElement<R>,
    Calibrated=smithay::backend::renderer::element::utils::CropRenderElement<smithay::backend::renderer::element::utils::RelocateRenderElement<smithay::backend::renderer::element::utils::RescaleRenderElement<SceneElement<R>>>>,
    Identification=smithay::backend::renderer::element::utils::RescaleRenderElement<MemoryRenderBufferRenderElement<R>>,
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
/// a native GPU-manager renderer, borrowed out of `state.backend_data`, is held mutably.
pub fn collect<R: ImportAll + ImportMem + WallpaperRenderer>(
    chrome: &mut ChromeState,
    wallpaper: &mut crate::wallpaper::WallpaperState,
    essence: &mut crate::essence_renderer::EssenceRenderer,
    windows: &crate::window_bridge::WindowIndex,
    inputs: &ChromeInputs<'_>,
    output: &Output,
    renderer: &mut R,
) -> Vec<SceneElement<R>>
where
    R::TextureId: Clone + Send + 'static,
{
    let Some(output_geometry) = crate::viewport::output_geometry(&inputs.space, output) else {
        return Vec::new();
    };
    let output_rect = Rect::new(
        output_geometry.loc.x,
        output_geometry.loc.y,
        output_geometry.size.w,
        output_geometry.size.h,
    );
    let output_scale = output.current_scale().fractional_scale();

    let mut background = Vec::new();
    let (overlay, windows_get_chrome) =
        crate::chrome::collect_output_chrome(chrome, inputs, output, renderer, &mut background);
    let mut elements: Vec<SceneElement<R>> =
        overlay.into_iter().map(SceneElement::Chrome).collect();

    essence.retire(renderer.wallpaper_gles(), |id| {
        !inputs.session_lock.locked
            && drawn_motion(inputs, id).is_some()
            && inputs.motions.essence(id, inputs.now_ns).is_some()
    });
    if inputs.session_lock.locked {
        // Never import or render ordinary surfaces while locked, even if fallback chrome fails.
        // A transparent lock surface composites over the cleared frame, not the user's work.
        if let Some(surface) = inputs.session_lock.surfaces.get(&output.name()) {
            let mut lock_elements: Vec<SceneElement<R>> =
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                    renderer,
                    surface.wl_surface(),
                    (0, 0),
                    Scale::from(output_scale),
                    1.0,
                    smithay::backend::renderer::element::Kind::Unspecified,
                );
            lock_elements.append(&mut elements);
            return lock_elements;
        }
        return elements;
    }

    let plan: Vec<(WindowId, bool, bool)> = inputs
        .window_service
        .stacking_order()
        .into_iter()
        .map(|window_id| {
            let moving = drawn_motion(inputs, window_id).is_some();
            (
                window_id,
                inputs.window_service.is_visible(window_id) || moving,
                windows_get_chrome && !moving,
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
                    inputs.tab_counts.get(&window_id).map(|tabs| tabs.count),
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
                if drawn_motion(inputs, window_id).is_some()
                    && let Some(motion) = inputs.motions.essence(window_id, inputs.now_ns)
                    && let Some(effect) = essence.element(
                        renderer,
                        window_id,
                        window,
                        motion,
                        inputs.motions.style(),
                        inputs.now_ns,
                        output_rect,
                        output_scale,
                    )
                {
                    elements.push(SceneElement::Essence(effect));
                    continue;
                }
                let (presentation, alpha) = surface_placement(inputs, window, window_id);
                surface_elements(
                    window,
                    inputs.space,
                    output_geometry,
                    output_scale,
                    presentation,
                    alpha,
                    renderer,
                    &mut elements,
                );
            }
        }
    }

    append_leftovers(
        inputs,
        &drawn,
        output_geometry,
        output_scale,
        renderer,
        &mut elements,
    );

    // Last, so it is bottom-most: the overview's scrim is the only chrome the windows are drawn
    // *on top of*. Home is already in the window loop (bottom of stacking) and is visible in
    // Taskfield so it stays mapped; inserting the scrim after the cards puts it above Home and
    // below the cards, which is the ground they stand on.
    elements.extend(background.into_iter().map(SceneElement::Chrome));
    elements.extend(wallpaper.elements(renderer, output).into_iter().map(
        |element| match element {
            crate::wallpaper::WallpaperScene::Image(image) => SceneElement::Wallpaper(image),
            crate::wallpaper::WallpaperScene::Ground(ground) => SceneElement::Ground(ground),
        },
    ));

    retain_visible_chrome(chrome, inputs, live_chrome);
    elements
}

/// The park/unpark motion `window_id` is drawn by this frame, if any (`src/motion.rs`).
///
/// A window parking into its card is drawn for the motion's lifetime although it is already
/// shelved — at its own place in the stacking order, which shelving does not change, with no band
/// or grip. It takes no input: `Space` and the hit test know it only as shelved. An unparking
/// window is already where it belongs and is only drawn somewhere else until the motion ends, its
/// chrome held back until then. A park whose window is visible again, or an unpark whose window
/// is hidden again, draws nothing; neither does anything in Taskfield, where every window is a
/// card already.
fn drawn_motion<'a>(
    inputs: &ChromeInputs<'a>,
    window_id: WindowId,
) -> Option<&'a crate::motion::WindowMotion> {
    if matches!(
        inputs.window_service.presentation(),
        templar_window::Presentation::Taskfield(_)
    ) {
        return None;
    }
    let visible = inputs.window_service.is_visible(window_id);
    inputs
        .motions
        .active(window_id, inputs.now_ns)
        .filter(|motion| match motion.kind {
            crate::motion::MotionKind::Park => !visible,
            crate::motion::MotionKind::Unpark => visible,
        })
}

/// Where `window`'s surface tree is drawn this frame instead of its real place (`None`: its real
/// place), and how opaque: its motion's interpolated rectangle — the content fitted into the
/// middle of it, since a card rarely has the window's aspect — or else its Taskfield card.
fn surface_placement(
    inputs: &ChromeInputs<'_>,
    window: &Window,
    window_id: WindowId,
) -> (Option<Rect>, f32) {
    match drawn_motion(inputs, window_id) {
        Some(motion) => {
            let content = window.geometry().size;
            (
                Some(crate::motion::fit_centered(
                    motion.rect_at(inputs.now_ns),
                    content.w,
                    content.h,
                )),
                motion.alpha_at(inputs.now_ns),
            )
        }
        None => (
            inputs.window_service.presentation_rect(window_id),
            inputs.window_service.taskfield_card_alpha(window_id),
        ),
    }
}

/// Anything `Space` is showing that the stacking order did not account for, drawn beneath
/// everything the order did. Not in Taskfield: a leftover without a card used to be drawn at its
/// real size, which is how a filter left full-size windows sitting on the overview.
fn append_leftovers<R: ImportAll + ImportMem + WallpaperRenderer>(
    inputs: &ChromeInputs<'_>,
    drawn: &[Window],
    output_geometry: Rectangle<i32, Logical>,
    output_scale: f64,
    renderer: &mut R,
    elements: &mut Vec<SceneElement<R>>,
) where
    R::TextureId: Clone + Send + 'static,
{
    if matches!(
        inputs.window_service.presentation(),
        templar_window::Presentation::Taskfield(_)
    ) {
        return;
    }
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
            None,
            1.0,
            renderer,
            elements,
        );
    }
}

/// Retain every currently visible window's chrome, not only the ones drawn on this output: a
/// per-output retain dropped the other output's caches and forced a rerasterize next frame.
fn retain_visible_chrome(
    chrome: &mut ChromeState,
    inputs: &ChromeInputs<'_>,
    mut live_chrome: HashSet<WindowId>,
) {
    live_chrome.extend(
        inputs
            .window_service
            .stacking_order()
            .into_iter()
            .filter(|id| inputs.window_service.is_visible(*id)),
    );
    chrome.retain_window_chrome(&live_chrome);
}

/// One window's surface elements, positioned exactly as `Space` would have positioned them —
/// unless `presentation` says this window is being drawn somewhere else at some other size.
///
/// This is `Space::render_elements_for_region`'s per-element body, spelled out for one element:
/// a window's render origin is its `Space` location minus its own `geometry().loc` (the client's
/// shadow/decoration offset inside its surface), and the region it is drawn against is the
/// output's rectangle. Deriving it here rather than calling `space_render_elements` is what
/// allows the interleaving in [`collect`]; deriving it the *same* way is what keeps a window in
/// exactly the pixels it was in before.
///
/// `presentation` is `WindowService::presentation_rect` — `Some` only in the window overview,
/// where the whole surface tree is drawn into its card. That is a *render* change and nothing
/// else: no `xdg_toplevel.configure` is sent, `Space` is not touched, and the client goes on
/// painting at its real size, which is why leaving the mode restores the previous frame exactly
/// and why an unresponsive client's card is as live as anyone else's. The arithmetic is
/// `src/screencopy.rs`'s `render_one`: one uniform scale, and the content origin moved to the
/// card. XWayland comes free, because a `smithay::desktop::Window` is a surface tree either way.
/// Where a window's surface tree starts, in this output's physical pixels, and by how much it is
/// shrunk, when it is being drawn into `card` instead of at its real geometry.
///
/// `k` is the smaller of the two ratios so the card never crops — the card rectangle already
/// carries the window's own aspect (`taskfield::card_in`), so the two agree to within a rounding
/// pixel. The origin is the card's position relative to the output in physical pixels, less the
/// client's own `geometry().loc` (its shadow/decoration offset inside the surface) at the *scaled*
/// size, which is `src/screencopy.rs`'s `render_one` arithmetic: the content, not the buffer,
/// lands on the card.
///
/// Pure, so the one piece of this that can be wrong by a pixel can be exercised without a
/// renderer, a display or a `Space`.
fn card_placement(
    card: Rect,
    geometry: Rect,
    output_loc: (i32, i32),
    output_scale: f64,
) -> ((f64, f64), f64) {
    let k = (f64::from(card.width) / f64::from(geometry.width.max(1)))
        .min(f64::from(card.height) / f64::from(geometry.height.max(1)));
    (
        (
            f64::from(card.x - output_loc.0) * output_scale
                - f64::from(geometry.x) * output_scale * k,
            f64::from(card.y - output_loc.1) * output_scale
                - f64::from(geometry.y) * output_scale * k,
        ),
        k,
    )
}

fn surface_elements<R: ImportAll + ImportMem + WallpaperRenderer>(
    window: &Window,
    space: &Space<Window>,
    output_geometry: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
    output_scale: f64,
    presentation: Option<Rect>,
    alpha: f32,
    renderer: &mut R,
    elements: &mut Vec<SceneElement<R>>,
) where
    R::TextureId: Clone + Send + 'static,
{
    let geometry = window.geometry();
    if let Some(card) = presentation {
        let (origin, k) = card_placement(
            card,
            Rect::new(
                geometry.loc.x,
                geometry.loc.y,
                geometry.size.w,
                geometry.size.h,
            ),
            (output_geometry.loc.x, output_geometry.loc.y),
            output_scale,
        );
        elements.extend(
            AsRenderElements::<R>::render_elements::<WaylandSurfaceRenderElement<R>>(
                window,
                renderer,
                smithay::utils::Point::from((origin.0.round() as i32, origin.1.round() as i32)),
                Scale::from(output_scale * k),
                alpha,
            )
            .into_iter()
            .map(SceneElement::Surface),
        );
        return;
    }
    // A window drawn somewhere else may not be in `Space` at all (a parking window is already
    // shelved), so only its real placement needs a `Space` location.
    let Some(location) = space.element_location(window) else {
        return;
    };
    let render_location = location - geometry.loc;
    let mut bbox = window.bbox();
    bbox.loc += render_location;
    if !output_geometry.overlaps(bbox) {
        return;
    }
    let scale = Scale::from(output_scale);
    let physical = (render_location - output_geometry.loc).to_physical_precise_round(scale);
    elements.extend(
        AsRenderElements::<R>::render_elements::<WaylandSurfaceRenderElement<R>>(
            window, renderer, physical, scale, alpha,
        )
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

    /// A card is the window shrunk to fit and put where the layout said, in this output's own
    /// pixels — and nothing about the client changes to make it happen.
    #[test]
    fn a_card_places_the_window_content_exactly_where_the_layout_put_it() {
        // A 1600x900 window with no shadow offset, on the second output, into a 320x180 card.
        let (origin, k) = card_placement(
            Rect::new(2020, 130, 320, 180),
            Rect::new(0, 0, 1600, 900),
            (1920, 0),
            1.0,
        );
        assert!((k - 0.2).abs() < 1e-9, "uniform fifth-size, got {k}");
        assert_eq!(
            origin,
            (100.0, 130.0),
            "card position, less the output's origin"
        );

        // The client's own shadow offset is subtracted at the *scaled* size, so its content —
        // not its buffer — lands on the card.
        let (shadowed, _) = card_placement(
            Rect::new(2020, 130, 320, 180),
            Rect::new(50, 20, 1600, 900),
            (1920, 0),
            1.0,
        );
        assert_eq!(shadowed, (100.0 - 10.0, 130.0 - 4.0));

        // On a 2x output every physical number doubles and the scale does not: `k` is the
        // window's shrink, `output_scale * k` is what the renderer is handed.
        let (hidpi, k2) = card_placement(
            Rect::new(20, 30, 320, 180),
            Rect::new(0, 0, 1600, 900),
            (0, 0),
            2.0,
        );
        assert_eq!(hidpi, (40.0, 60.0));
        assert!((k2 - 0.2).abs() < 1e-9);

        // The smaller ratio wins, so a card that is proportionally wider than the window still
        // fits inside it rather than cropping.
        let (_, squat) = card_placement(
            Rect::new(0, 0, 400, 100),
            Rect::new(0, 0, 1600, 900),
            (0, 0),
            1.0,
        );
        assert!((squat - 100.0 / 900.0).abs() < 1e-9);
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

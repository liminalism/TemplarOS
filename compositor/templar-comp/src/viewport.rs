//! Native HDMI calibration keeps the signal mode unchanged while changing usable desktop geometry.
use smithay::{
    desktop::{Space, Window},
    output::Output,
    utils::{Logical, Physical, Point, Rectangle, Size, Transform},
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use templar_types::display::Insets;

#[derive(Default)]
struct Viewport {
    insets: AtomicU64,
    hardware: AtomicBool,
}
pub fn configure(output: &Output, insets: Insets, hardware: bool) {
    output
        .user_data()
        .insert_if_missing_threadsafe(Viewport::default);
    let viewport = output
        .user_data()
        .get::<Viewport>()
        .expect("inserted viewport");
    let packed = u64::from(insets.left)
        | (u64::from(insets.right) << 16)
        | (u64::from(insets.top) << 32)
        | (u64::from(insets.bottom) << 48);
    viewport.hardware.store(hardware, Ordering::Release);
    viewport.insets.store(packed, Ordering::Release);
}
pub fn insets(output: &Output) -> Insets {
    let Some(viewport) = output.user_data().get::<Viewport>() else {
        return Insets::default();
    };
    let packed = viewport.insets.load(Ordering::Acquire);
    Insets {
        left: (packed & 0xffff) as u32,
        right: ((packed >> 16) & 0xffff) as u32,
        top: ((packed >> 32) & 0xffff) as u32,
        bottom: ((packed >> 48) & 0xffff) as u32,
    }
}
pub fn hardware(output: &Output) -> bool {
    output
        .user_data()
        .get::<Viewport>()
        .is_some_and(|viewport| viewport.hardware.load(Ordering::Acquire))
}
fn rotated(insets: Insets, transform: Transform) -> Insets {
    match transform {
        Transform::_90 => Insets {
            left: insets.bottom,
            right: insets.top,
            top: insets.left,
            bottom: insets.right,
        },
        Transform::_180 => Insets {
            left: insets.right,
            right: insets.left,
            top: insets.bottom,
            bottom: insets.top,
        },
        Transform::_270 => Insets {
            left: insets.top,
            right: insets.bottom,
            top: insets.right,
            bottom: insets.left,
        },
        _ => insets,
    }
}
pub fn margins(output: &Output) -> Insets {
    rotated(insets(output), output.current_transform())
}
pub fn visible_physical_size(output: &Output) -> Option<Size<i32, Physical>> {
    let mode = output.current_mode()?;
    let (width, height) =
        insets(output).visible_size(mode.size.w.try_into().ok()?, mode.size.h.try_into().ok()?)?;
    let size = Size::<i32, Physical>::from((width as i32, height as i32));
    Some(output.current_transform().transform_size(size))
}
/// Every compositor work-area/hit-test caller uses this; wl_output still advertises the real signal mode.
pub fn output_geometry(space: &Space<Window>, output: &Output) -> Option<Rectangle<i32, Logical>> {
    let mut geometry = space.output_geometry(output)?;
    if !insets(output).is_zero() {
        geometry.size = visible_physical_size(output)?
            .to_f64()
            .to_logical(output.current_scale().fractional_scale())
            .to_i32_round();
    }
    Some(geometry)
}
/// A physical absolute device mapped to this output is normalized over the full HDMI signal.
/// Inverse inset mapping clamps black margins to the nearest usable desktop edge.
pub fn absolute_position(
    space: &Space<Window>,
    output: &Output,
    normalized: Point<f64, Logical>,
    input_transform: Transform,
) -> Option<Point<f64, Logical>> {
    let geometry = output_geometry(space, output)?;
    let mode = output.current_mode()?;
    let physical = mode.size.to_f64();
    let point: Point<f64, Physical> = (normalized.x * physical.w, normalized.y * physical.h).into();
    let point = input_transform.transform_point_in(point, &physical);
    let margins = margins(output);
    let scale = output.current_scale().fractional_scale();
    let x = (point.x - f64::from(margins.left)) / scale;
    let y = (point.y - f64::from(margins.top)) / scale;
    let (left, top) = (f64::from(geometry.loc.x), f64::from(geometry.loc.y));
    Some(Point::from((
        (left + x).clamp(
            left,
            (left + f64::from(geometry.size.w)).next_down().max(left),
        ),
        (top + y).clamp(top, (top + f64::from(geometry.size.h)).next_down().max(top)),
    )))
}

/// Software insets clip and relocate the final scene. Usable hardware underscan instead gets a
/// full-signal framebuffer, stretched from that same visible scene before KMS adds its margins.

/// Explicit mirror source pixels are rescaled exactly once before target calibration/cropping.
pub fn constrain_elements_at_scale<E: smithay::backend::renderer::element::Element>(
    output: &Output,
    elements: impl IntoIterator<Item = E>,
    source_scale: f64,
) -> Option<
    impl Iterator<
        Item = smithay::backend::renderer::element::utils::CropRenderElement<
            smithay::backend::renderer::element::utils::RelocateRenderElement<
                smithay::backend::renderer::element::utils::RescaleRenderElement<E>,
            >,
        >,
    >,
> {
    use smithay::backend::renderer::element::utils::{
        CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
    };
    let visible = visible_physical_size(output)?;
    let signal = output
        .current_transform()
        .transform_size(output.current_mode()?.size);
    let margins = margins(output);
    let (crop, factor, offset) = if hardware(output) {
        (
            Rectangle::new((0, 0).into(), signal),
            signal.to_f64() / visible.to_f64(),
            Point::from((0, 0)),
        )
    } else {
        (
            Rectangle::new((margins.left as i32, margins.top as i32).into(), visible),
            smithay::utils::Scale::from(1.0),
            Point::from((margins.left as i32, margins.top as i32)),
        )
    };
    let scale = output.current_scale().fractional_scale();
    let factor = factor * (output.current_scale().fractional_scale() / source_scale);
    Some(
        elements
            .into_iter()
            .map(move |element| RescaleRenderElement::from_element(element, (0, 0).into(), factor))
            .map(move |element| {
                RelocateRenderElement::from_element(element, offset, Relocate::Relative)
            })
            .filter_map(move |element| CropRenderElement::from_element(element, scale, crop)),
    )
}

/// Emulate KMS underscan for capture: exported pixels include the physical black margins, not
/// the deliberately expanded scanout framebuffer which the kernel compresses afterwards.
pub fn hardware_capture_elements<E: smithay::backend::renderer::element::Element>(
    output: &Output,
    elements: impl IntoIterator<Item = E>,
) -> Option<
    impl Iterator<
        Item = smithay::backend::renderer::element::utils::CropRenderElement<
            smithay::backend::renderer::element::utils::RelocateRenderElement<
                smithay::backend::renderer::element::utils::RescaleRenderElement<E>,
            >,
        >,
    >,
> {
    use smithay::backend::renderer::element::utils::{
        CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
    };
    let visible = visible_physical_size(output)?;
    let signal = output
        .current_transform()
        .transform_size(output.current_mode()?.size);
    let factor = visible.to_f64() / signal.to_f64();
    let margins = margins(output);
    let offset = Point::from((margins.left as i32, margins.top as i32));
    let crop = Rectangle::new(offset, visible);
    let scale = output.current_scale().fractional_scale();
    Some(
        elements
            .into_iter()
            .map(move |element| RescaleRenderElement::from_element(element, (0, 0).into(), factor))
            .map(move |element| {
                RelocateRenderElement::from_element(element, offset, Relocate::Relative)
            })
            .filter_map(move |element| CropRenderElement::from_element(element, scale, crop)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotation_keeps_margins_tied_to_physical_signal_edges() {
        let insets = Insets {
            left: 1,
            right: 2,
            top: 3,
            bottom: 4,
        };
        assert_eq!(
            rotated(insets, Transform::_90),
            Insets {
                left: 4,
                right: 3,
                top: 1,
                bottom: 2
            }
        );
        assert_eq!(
            rotated(insets, Transform::_180),
            Insets {
                left: 2,
                right: 1,
                top: 4,
                bottom: 3
            }
        );
    }
    #[test]
    fn absolute_pointer_inverse_uses_the_visible_scaled_geometry() {
        use smithay::output::{Mode, PhysicalProperties, Scale, Subpixel};
        let output = Output::new(
            "pure-geometry".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
                serial_number: String::new(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: (1920, 1080).into(),
                refresh: 60000,
            }),
            Some(Transform::Normal),
            Some(Scale::Fractional(2.0)),
            Some((100, 200).into()),
        );
        configure(&output, Insets::symmetric(20, 10), false);
        let mut space = Space::<Window>::default();
        space.map_output(&output, (100, 200));
        assert_eq!(
            output_geometry(&space, &output).unwrap().size,
            Size::from((940, 530))
        );
        let point = absolute_position(
            &space,
            &output,
            (100.0 / 1920.0, 70.0 / 1080.0).into(),
            Transform::Normal,
        )
        .unwrap();
        assert!((point.x - 140.0).abs() < 1e-9 && (point.y - 230.0).abs() < 1e-9);
        assert_eq!(
            absolute_position(&space, &output, (0.0, 0.0).into(), Transform::Normal).unwrap(),
            Point::from((100.0, 200.0))
        );
        let far = absolute_position(&space, &output, (1.0, 1.0).into(), Transform::Normal).unwrap();
        assert!(far.x < 1040.0 && far.x > 1039.0 && far.y < 730.0 && far.y > 729.0);
    }
    #[test]
    fn nested_canvas_pointer_does_not_inherit_framebuffer_y_inversion() {
        use crate::state::Backend;
        use smithay::output::{Mode, PhysicalProperties, Scale, Subpixel};
        let output = Output::new(
            "nested-input".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
                serial_number: String::new(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: (1200, 800).into(),
                refresh: 60000,
            }),
            Some(Transform::Flipped180),
            Some(Scale::Fractional(1.25)),
            Some((100, 200).into()),
        );
        configure(&output, Insets::symmetric(20, 10), false);
        let mut space = Space::<Window>::default();
        space.map_output(&output, (100, 200));
        let transform =
            crate::backend::winit::WinitBackendData::default().absolute_input_transform(&output);
        let point = absolute_position(
            &space,
            &output,
            (842.0 / 1200.0, 653.0 / 800.0).into(),
            transform,
        )
        .unwrap();
        assert!((point.x - 757.6).abs() < 1e-9 && (point.y - 714.4).abs() < 1e-9);
        assert_eq!(
            absolute_position(&space, &output, (0.0, 0.0).into(), transform).unwrap(),
            Point::from((100.0, 200.0))
        );
    }
}

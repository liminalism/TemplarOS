//! Handler impls that are pure Smithay protocol plumbing: nothing here consults
//! `templar_window::WindowService` because nothing here carries window-management policy. Buffer
//! lifecycle, SHM, output-global bookkeeping, clipboard/primary-selection/DnD negotiation and
//! fractional-scale advertisement all have exactly one reasonable behaviour, which is "delegate
//! to Smithay's own default and expose the state it asks for" — every method below is either a
//! state accessor or accepts Smithay's provided default.
//!
//! `SelectionHandler`'s `SelectionUserData` is `()`: we attach no compositor-side metadata to a
//! clipboard/primary selection beyond what Smithay itself tracks.

use crate::state::{Backend, TemplarCompState};
use smithay::{
    output::Output,
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_output::WlOutput},
    wayland::{
        buffer::BufferHandler,
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::FractionalScaleHandler,
        output::OutputHandler,
        selection::primary_selection::PrimarySelectionHandler,
        shm::{ShmHandler, ShmState},
    },
};

impl<B: Backend + 'static> BufferHandler for TemplarCompState<B> {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl<B: Backend + 'static> ShmHandler for TemplarCompState<B> {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl<B: Backend + 'static> OutputHandler for TemplarCompState<B> {
    fn output_bound(&mut self, _output: Output, _wl_output: WlOutput) {}
}

// `SelectionHandler`, `WaylandDndGrabHandler`/`DndGrabHandler` and `DataDeviceHandler` live in
// `src/input.rs` instead of here, next to `SeatHandler` — all four traits share the seat/focus
// machinery input handling already needs in scope.

impl<B: Backend + 'static> PrimarySelectionHandler for TemplarCompState<B> {
    fn primary_selection_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::primary_selection::PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl<B: Backend + 'static> FractionalScaleHandler for TemplarCompState<B> {}

impl<B: Backend + 'static> DmabufHandler for TemplarCompState<B> {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Accept every import unconditionally: whether a given dmabuf/modifier combination is
        // actually renderable is backend/renderer-specific (winit vs. udev, GPU-dependent format
        // tables) and Smithay's own render path already falls back to skipping an unrenderable
        // surface rather than requiring an eager validation here. A client that produced buffers
        // its own GPU cannot read back is a pre-existing bug outside anything this compositor
        // could detect earlier than "the next render pass silently omits that surface".
        let _ = notifier.successful::<Self>();
    }
}

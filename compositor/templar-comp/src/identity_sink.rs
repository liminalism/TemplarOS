//! Deliver verified identity changes on the compositor thread, including an idle desktop.
//! The resolver never mutates Wayland state and no frame/timer has to poll its answers.

use crate::state::{Backend, TemplarCompState};
use smithay::reexports::calloop::{
    LoopHandle,
    channel::{self, Event},
};
use templar_window::{CachedIdentity, WindowId};

pub fn install<B: Backend + 'static>(
    loop_handle: &LoopHandle<'static, TemplarCompState<B>>,
) -> impl Fn(WindowId, CachedIdentity) + Send + 'static {
    let (sender, receiver) = channel::channel::<(WindowId, CachedIdentity)>();
    loop_handle
        .insert_source(receiver, |event, (), state: &mut TemplarCompState<B>| {
            if let Event::Msg((window_id, identity)) = event
                && state
                    .window_service
                    .window(window_id)
                    .is_some_and(|window| window.identity != identity)
            {
                state.window_service.update_identity(window_id, identity);
                // Publish canonical app/instance changes to Home and the governor, not only
                // to the local cache. Destroyed windows and unchanged answers do no work.
                crate::space_sync::sync_after_change(state);
            }
        })
        .expect("inserting the identity-answer channel source");
    move |window_id, identity| {
        let _ = sender.send((window_id, identity));
    }
}

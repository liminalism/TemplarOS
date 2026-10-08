//! Never invoke Smithay's untimed `SyncPoint::wait` on a render path.
use super::native_policy::FENCE_TIMEOUT;
use smithay::backend::{egl::fence::EGLFence, renderer::sync::SyncPoint};

pub fn bounded(sync: &SyncPoint) -> Result<(), String> {
    if !sync.contains_fence() {
        return Ok(());
    }
    let fence = sync.get::<EGLFence>().ok_or_else(|| {
        "The renderer returned a fence without a bounded wait contract".to_owned()
    })?;
    match fence.client_wait(Some(FENCE_TIMEOUT), false) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "Render fence timed out after {} ms; this frame was not queued",
            FENCE_TIMEOUT.as_millis()
        )),
        Err(error) => Err(format!("Bounded render fence wait failed: {error}")),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn already_signaled_frame_needs_no_driver_wait() {
        assert!(bounded(&SyncPoint::signaled()).is_ok());
    }
}

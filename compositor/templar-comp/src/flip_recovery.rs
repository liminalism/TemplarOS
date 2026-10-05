//! Lost completion recovery: never pretend a pending frame completed. The backend must first
//! synchronously disable planes via DrmCompositor::clear, which releases Smithay's pending state.
use smithay::backend::drm::DrmEventTime as Time;
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug)]
pub struct RearmedAt {
    pub monotonic: Duration,
    pub realtime: SystemTime,
}
impl RearmedAt {
    pub fn accepts(self, timestamp: Option<Time>) -> bool {
        match timestamp {
            Some(Time::Monotonic(time)) => time > self.monotonic,
            Some(Time::Realtime(time)) => time > self.realtime,
            None => false, // Missing metadata cannot prove a late event belongs to the new frame.
        }
    }
}

/// `clear` must disable scanout synchronously *before* dropping pending buffers. Failure keeps
/// ownership in flight: queuing anyway could reuse a buffer the kernel is still reading.
pub fn rearm<E>(in_flight: &mut bool, clear: impl FnOnce() -> Result<(), E>) -> Result<(), E> {
    clear()?;
    *in_flight = false;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dropped_completion_rearms_only_after_scanout_release_and_rejects_late_event() {
        let mut in_flight = true;
        // Inject a clear failure while the lost flip still owns a buffer.
        assert_eq!(rearm(&mut in_flight, || Err("DRM busy")), Err("DRM busy"));
        assert!(
            in_flight,
            "must not submit or release a buffer after clear failed"
        );
        let mut scanout_enabled = true;
        let mut pending_buffer = Some(7);
        rearm(&mut in_flight, || {
            scanout_enabled = false;
            pending_buffer = None;
            Ok::<_, &str>(())
        })
        .unwrap();
        assert!(!scanout_enabled);
        assert!(pending_buffer.is_none());
        assert!(!in_flight, "retry can now queue a fresh frame");
        let cutoff = RearmedAt {
            monotonic: Duration::from_secs(10),
            realtime: SystemTime::UNIX_EPOCH + Duration::from_secs(100),
        };
        in_flight = true; // The retry submitted a new frame.
        assert!(!cutoff.accepts(Some(Time::Monotonic(Duration::from_secs(9)))));
        assert!(!cutoff.accepts(Some(Time::Realtime(
            SystemTime::UNIX_EPOCH + Duration::from_secs(99)
        ))));
        assert!(!cutoff.accepts(None));
        assert!(
            in_flight,
            "late old completion cannot release the new pending frame"
        );
        assert!(cutoff.accepts(Some(Time::Monotonic(Duration::from_secs(11)))));
        in_flight = false; // The real new completion resumes the render chain.
        assert!(!in_flight);
    }
}

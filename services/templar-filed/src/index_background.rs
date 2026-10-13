//! Background acquisition and maintenance yield CPU and I/O to interactive applications.
//! Linux applies these settings to the calling thread, so browsing and query workers keep their
//! ordinary scheduling priority. Nothing changes the service's policy or the user's live units.

/// Set background worker priorities once, before starting its event/work loop. A quiet machine
/// may still finish a scan promptly, while another runnable application wins contention.
#[allow(unsafe_code)] // Narrow Linux scalar-only priority FFI; see each call's safety contract.
pub(crate) fn lower_thread_priority() {
    // SAFETY: getpriority and setpriority receive valid scalar selectors; who=0 is the calling
    // Linux thread. No pointers are involved. Never raise a priority already lower than ours.
    let priority = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    if priority < 10 && unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) } != 0 {
        tracing::info!(error = %std::io::Error::last_os_error(), "background index CPU priority unavailable");
    }

    // SAFETY: ioprio_set takes three scalar arguments. IOPRIO_WHO_PROCESS=1 with who=0 selects
    // the calling Linux thread; IOPRIO_CLASS_IDLE=3, shifted by IOPRIO_CLASS_SHIFT=13, asks the
    // I/O scheduler to serve this worker when other I/O is idle. Unsupported kernels merely fail.
    let result = unsafe { libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13) };
    if result < 0 {
        tracing::info!(error = %std::io::Error::last_os_error(), "background index I/O priority unavailable");
    }
}

#[cfg(test)]
mod tests {
    /// Query only this calling thread. Missing kernel I/O-priority support is an ordinary None.
    #[allow(unsafe_code)] // Test-only scalar Linux FFI for owned-thread measurements.
    fn calling_thread_priorities() -> (i32, Option<libc::c_long>) {
        // SAFETY: scalar query for the calling thread; no pointers are involved.
        let cpu = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        // SAFETY: IOPRIO_WHO_PROCESS=1 and who=0 select the calling Linux thread. The variadic
        // syscall has exactly its two scalar arguments, with no pointers or borrowed memory.
        let io = unsafe { libc::syscall(libc::SYS_ioprio_get, 1, 0) };
        (cpu, (io >= 0).then_some(io))
    }

    #[test]
    fn background_priority_applies_to_the_worker_only() {
        let before = calling_thread_priorities();
        let worker = std::thread::spawn(|| {
            super::lower_thread_priority();
            calling_thread_priorities()
        });
        let (cpu, io) = worker.join().unwrap();
        assert!(cpu >= 10);
        if let Some(io) = io {
            assert_eq!(io >> 13, 3, "background worker must request idle I/O");
        }
        assert_eq!(
            calling_thread_priorities(),
            before,
            "CPU and I/O priorities of the original thread must remain unchanged"
        );
    }
}

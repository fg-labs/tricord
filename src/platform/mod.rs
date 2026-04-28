//! Per-OS process-tree sampling.
//!
//! The trait [`ProcessSampler`] is implemented twice — once on top of `procfs`
//! for Linux, once on top of `libproc` (and `proc_pid_rusage`) for macOS — so
//! the sampler thread in [`crate::sampler`] can stay platform-agnostic.

use crate::sampler::ProcessSnapshot;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

/// Reads the OS resource counters of an entire process tree.
pub trait ProcessSampler: Send {
    /// Return one [`ProcessSnapshot`] for `root_pid` and each currently-live
    /// descendant. Processes that have exited (or that we cannot read) are
    /// silently skipped — the [`crate::sampler::SamplerState`] aggregator
    /// already keeps the last value seen for each PID, so an exited child's
    /// counters are not lost.
    fn sample_tree(&mut self, root_pid: i32) -> Vec<ProcessSnapshot>;
}

/// Construct the per-OS sampler.
#[must_use]
pub fn new_sampler() -> Box<dyn ProcessSampler> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxSampler::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosSampler::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        compile_error!("tricord supports Linux and macOS only");
    }
}

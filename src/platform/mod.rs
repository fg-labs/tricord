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

/// Read the system 1-minute load average, in "tasks ready to run" units.
///
/// Returns `None` if the platform read fails. Used by `run_command` to
/// snapshot system context at run start and end — the value frames the
/// per-process numbers ("peak CPU 800 % on an idle box vs the same peak
/// on a thrashing host").
#[must_use]
pub fn read_loadavg_1m() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        linux::read_loadavg_1m()
    }
    #[cfg(target_os = "macos")]
    {
        macos::read_loadavg_1m()
    }
}

/// Read the system page-cache size, in MiB.
///
/// Returns `None` if the platform read fails, or unconditionally on macOS,
/// which has no equivalent of Linux's `Cached` accounting — see
/// `macos::read_page_cache_mb`. Used by `run_command` to snapshot system
/// context at run start and end, the same way `read_loadavg_1m` frames CPU
/// pressure: a workload slowed by page-cache eviction from other work on the
/// host looks identical to a real regression without this.
#[must_use]
pub fn read_page_cache_mb() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        linux::read_page_cache_mb()
    }
    #[cfg(target_os = "macos")]
    {
        macos::read_page_cache_mb()
    }
}

//! macOS process sampler — uses `proc_pidinfo` and `proc_pid_rusage` (V4) via
//! the [`libproc`] crate.
//!
//! macOS does not provide a kernel-computed proportional set size (PSS), and
//! the closest equivalent of unique set size (USS) is `phys_footprint` —
//! the same number Activity Monitor reports as "Memory" and the value
//! recommended by Apple as "the memory that would be reclaimed if this task
//! exited". We populate both `max_uss` and `max_pss` with this value. For
//! benchmarking workloads (a single dominant child plus shared system
//! libraries) the two are typically within a few percent of each other.

use std::collections::HashMap;
use std::sync::OnceLock;

use libproc::libproc::{
    bsd_info::BSDInfo,
    pid_rusage::{RUsageInfoV4, pidrusage},
    proc_pid::pidinfo,
    task_info::TaskInfo,
};
use libproc::processes::{ProcFilter, pids_by_type};

use super::ProcessSampler;
use crate::sampler::ProcessSnapshot;

const NANOS_PER_SEC: f64 = 1_000_000_000.0;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

/// `(numer, denom)` from `mach_timebase_info`, cached once per process.
///
/// `pti_total_user` / `pti_total_system` are in Mach absolute time units,
/// not nanoseconds — on Apple Silicon the ratio is 125/3, on Intel Macs
/// it's 1/1. Without this conversion, `cpu_time` on Apple Silicon comes
/// in ~42× too small.
fn mach_timebase() -> (u64, u64) {
    static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: `mach_timebase_info` is always available on macOS and
        // only writes to the out-pointer we just allocated.
        unsafe { mach_timebase_info(&raw mut info) };
        // Pin a corrupt zero-denom result to 1 so a downstream divide
        // can't panic; the kernel never returns this in practice.
        let denom = if info.denom == 0 { 1 } else { info.denom };
        (u64::from(info.numer), u64::from(denom))
    })
}

#[allow(clippy::cast_precision_loss)]
fn mach_time_to_seconds(mach_time: u64) -> f64 {
    let (numer, denom) = mach_timebase();
    (mach_time as f64) * (numer as f64) / (denom as f64) / NANOS_PER_SEC
}

/// Process-tree sampler for macOS. Stateless across calls.
pub struct MacosSampler;

impl MacosSampler {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for MacosSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSampler for MacosSampler {
    fn sample_tree(&mut self, root_pid: i32) -> Vec<ProcessSnapshot> {
        let pids = collect_descendants(root_pid);
        pids.into_iter().filter_map(sample_process).collect()
    }
}

/// Build a parent → children map and DFS from `root`.
fn collect_descendants(root: i32) -> Vec<i32> {
    let mut by_parent: HashMap<i32, Vec<i32>> = HashMap::new();
    if let Ok(pids) = pids_by_type(ProcFilter::All) {
        for pid in pids {
            #[allow(clippy::cast_possible_wrap)]
            let pid_i32 = pid as i32;
            if let Ok(info) = pidinfo::<BSDInfo>(pid_i32, 0) {
                #[allow(clippy::cast_possible_wrap)]
                let ppid = info.pbi_ppid as i32;
                #[allow(clippy::cast_possible_wrap)]
                let proc_pid = info.pbi_pid as i32;
                by_parent.entry(ppid).or_default().push(proc_pid);
            }
        }
    }
    let mut out = vec![root];
    let mut stack = vec![root];
    while let Some(parent) = stack.pop() {
        if let Some(children) = by_parent.get(&parent) {
            for &child in children {
                out.push(child);
                stack.push(child);
            }
        }
    }
    out
}

fn sample_process(pid: i32) -> Option<ProcessSnapshot> {
    let task: TaskInfo = pidinfo(pid, 0).ok()?;
    let rss_bytes = task.pti_resident_size;
    let vms_bytes = task.pti_virtual_size;
    let cpu_time_seconds =
        mach_time_to_seconds(task.pti_total_user) + mach_time_to_seconds(task.pti_total_system);

    let rusage_result: Result<RUsageInfoV4, _> = pidrusage(pid);
    let (uss_bytes, pss_bytes, io_read_bytes, io_write_bytes) = match rusage_result {
        Ok(rusage) => {
            // ri_phys_footprint is Apple's per-task "owned memory that would
            // be reclaimed on exit" — the closest analog to USS. We populate
            // both USS and PSS with it; see module docs.
            let footprint = rusage.ri_phys_footprint;
            (
                Some(footprint),
                Some(footprint),
                Some(rusage.ri_diskio_bytesread),
                Some(rusage.ri_diskio_byteswritten),
            )
        }
        Err(_) => (None, None, None, None),
    };

    Some(ProcessSnapshot {
        pid,
        rss_bytes,
        vms_bytes,
        uss_bytes,
        pss_bytes,
        io_read_bytes,
        io_write_bytes,
        cpu_time_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_pid_produces_a_snapshot() {
        let mut sampler = MacosSampler::new();
        #[allow(clippy::cast_possible_wrap)]
        let pid = std::process::id() as i32;
        let snaps = sampler.sample_tree(pid);
        let me = snaps.iter().find(|s| s.pid == pid).expect("self snapshot");
        assert!(me.rss_bytes > 0, "expected non-zero RSS");
        assert!(me.vms_bytes > 0, "expected non-zero VMS");
    }
}

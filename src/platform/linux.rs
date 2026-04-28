//! Linux process sampler — reads `/proc/<pid>/{stat,status,smaps_rollup,io}`
//! via the [`procfs`] crate.

use std::collections::HashMap;

use procfs::{process, process::Process};

use super::ProcessSampler;
use crate::sampler::ProcessSnapshot;

/// Process-tree sampler for Linux. Stateless across calls.
pub struct LinuxSampler {
    ticks_per_second: f64,
}

impl LinuxSampler {
    /// Cache `_SC_CLK_TCK` once at construction; this never changes during the
    /// life of the process.
    #[must_use]
    pub fn new() -> Self {
        Self { ticks_per_second: procfs::ticks_per_second() as f64 }
    }
}

impl Default for LinuxSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSampler for LinuxSampler {
    fn sample_tree(&mut self, root_pid: i32) -> Vec<ProcessSnapshot> {
        let pids = collect_descendants(root_pid);
        pids.into_iter().filter_map(|pid| sample_process(pid, self.ticks_per_second)).collect()
    }
}

/// Build a parent → children map by walking `/proc` and return the PIDs of
/// `root` and all its transitive descendants. Returns `[root]` alone if the
/// `/proc` walk fails (which we don't expect on Linux).
fn collect_descendants(root: i32) -> Vec<i32> {
    let mut by_parent: HashMap<i32, Vec<i32>> = HashMap::new();
    if let Ok(iter) = process::all_processes() {
        for proc in iter.flatten() {
            if let Ok(stat) = proc.stat() {
                by_parent.entry(stat.ppid).or_default().push(stat.pid);
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

fn sample_process(pid: i32, ticks_per_second: f64) -> Option<ProcessSnapshot> {
    let proc = Process::new(pid).ok()?;
    let stat = proc.stat().ok()?;
    let status = proc.status().ok()?;

    // VmRSS / VmSize from /proc/PID/status come in kB; multiply to get bytes.
    let rss_bytes = status.vmrss.unwrap_or(0).saturating_mul(1024);
    let vms_bytes = status.vmsize.unwrap_or(0).saturating_mul(1024);

    let (uss_bytes, pss_bytes) = read_smaps_rollup(&proc);
    let (io_read_bytes, io_write_bytes) = match proc.io() {
        Ok(io) => (Some(io.read_bytes), Some(io.write_bytes)),
        Err(_) => (None, None),
    };

    let cpu_ticks = stat.utime.saturating_add(stat.stime);
    let cpu_time_seconds = (cpu_ticks as f64) / ticks_per_second;

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

/// Parse `/proc/<pid>/smaps_rollup` for USS and PSS. Returns `(None, None)` if
/// the file cannot be read (older kernels, restricted access).
fn read_smaps_rollup(proc: &Process) -> (Option<u64>, Option<u64>) {
    match proc.smaps_rollup() {
        Ok(rollup) => {
            let Some(map) = rollup.memory_map_rollup.0.first() else {
                return (None, None);
            };
            let extension = &map.extension.map;
            let private_clean = extension.get("Private_Clean").copied().unwrap_or(0);
            let private_dirty = extension.get("Private_Dirty").copied().unwrap_or(0);
            let uss = private_clean.saturating_add(private_dirty);
            let pss = extension.get("Pss").copied();
            (Some(uss), pss)
        }
        Err(_) => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_pid_produces_a_snapshot() {
        let mut sampler = LinuxSampler::new();
        let snaps = sampler.sample_tree(std::process::id() as i32);
        let me = snaps.iter().find(|s| s.pid == std::process::id() as i32).expect("self snapshot");
        assert!(me.rss_bytes > 0, "expected non-zero RSS");
        assert!(me.vms_bytes > 0, "expected non-zero VMS");
    }
}

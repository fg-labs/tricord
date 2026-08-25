//! Linux process sampler — reads `/proc/<pid>/{stat,status,smaps_rollup,io}`
//! via the [`procfs`] crate.

use std::collections::HashMap;

use procfs::{Current, Meminfo, process, process::Process};

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

    // `/proc/<pid>/stat` reports per-process page faults directly. The
    // `c{min,maj}flt` siblings count exited children — we already track each
    // PID in our own per-PID accumulator, so using the c-variants would
    // double-count. `minflt` and `majflt` are u64 in modern procfs.
    let major_page_faults = Some(stat.majflt);
    let minor_page_faults = Some(stat.minflt);

    // `/proc/<pid>/status` reports per-process context-switch counts under
    // `voluntary_ctxt_switches` / `nonvoluntary_ctxt_switches`. We use
    // "involuntary" as the public name (matches BSD `rusage` terminology).
    let voluntary_ctx_switches = status.voluntary_ctxt_switches;
    let involuntary_ctx_switches = status.nonvoluntary_ctxt_switches;

    // `/proc/<pid>/status` exposes the live thread count under `Threads:`,
    // surfaced as `status.threads` by procfs (`u64`).
    let thread_count = Some(status.threads);

    // `/proc/<pid>/status` reports `VmSwap:` in kB when the process has
    // anything swapped out; the field is absent on kernels without swap
    // accounting per-process, hence `Option`. Multiply to bytes for the
    // common sampler aggregation path.
    let swap_bytes = status.vmswap.map(|kb| kb.saturating_mul(1024));

    Some(ProcessSnapshot {
        pid,
        rss_bytes,
        vms_bytes,
        uss_bytes,
        pss_bytes,
        io_read_bytes,
        io_write_bytes,
        cpu_time_seconds,
        major_page_faults,
        minor_page_faults,
        voluntary_ctx_switches,
        involuntary_ctx_switches,
        thread_count,
        swap_bytes,
    })
}

/// Read the first whitespace-separated field of `/proc/loadavg` — the
/// kernel's exponentially-weighted 1-minute load average. Returns `None`
/// if the file is missing or unparseable.
#[must_use]
pub fn read_loadavg_1m() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().next()?.parse::<f64>().ok()
}

/// Read the system page-cache size (`Cached` in `/proc/meminfo`), in MiB.
///
/// `Cached` is in-memory storage for file contents read from disk — memory
/// the kernel reclaims under pressure before it would ever swap, so a value
/// that drops between two readings is a direct sign the run competed for
/// cache space with other work on the host. Returns `None` if `/proc/meminfo`
/// cannot be read or parsed.
#[must_use]
pub fn read_page_cache_mb() -> Option<f64> {
    let cached_bytes = Meminfo::current().ok()?.cached;
    Some(cached_bytes as f64 / (1024.0 * 1024.0))
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
        let pid = std::process::id().cast_signed();
        let snaps = sampler.sample_tree(pid);
        let me = snaps.iter().find(|s| s.pid == pid).expect("self snapshot");
        assert!(me.rss_bytes > 0, "expected non-zero RSS");
        assert!(me.vms_bytes > 0, "expected non-zero VMS");
    }
}

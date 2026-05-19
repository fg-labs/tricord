//! Periodic sampling of a process tree's resource use.
//!
//! The sampler runs on its own thread, polling the OS at a configurable
//! interval (default 0.5 s, matching Snakemake's `BENCHMARK_INTERVAL_SHORT`).
//! At each tick it asks the platform module ([`crate::platform`]) for a
//! snapshot of every live process in the tree rooted at the spawned child,
//! then folds the snapshot into a running [`SamplerState`]. When the sampler
//! is asked to stop (via [`SamplerHandle::stop`]) it returns the aggregated
//! [`BenchmarkRecord`].

// USS, PSS, RSS, VMS are domain acronyms for distinct memory metrics that
// happen to look similar to clippy's lexical similarity check. The
// distinctions matter, so we keep the names.
#![allow(clippy::similar_names)]

use std::{
    collections::HashMap,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    format, platform,
    record::{BenchmarkRecord, TRACE_TSV_HEADER, TickRecord},
};

/// Default sampling interval (matches `snakemake.benchmark.BENCHMARK_INTERVAL_SHORT`).
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(500);

/// One snapshot of resource use for one process at one moment in time.
///
/// All byte-valued fields are raw bytes (the [`SamplerState`] aggregator
/// converts to MiB when it produces the final record).
///
/// Page-fault counts are cumulative-since-process-birth integers, like the
/// I/O byte counters; per-tick deltas are computed downstream in
/// [`SamplerState::tick`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessSnapshot {
    pub pid: i32,
    pub rss_bytes: u64,
    pub vms_bytes: u64,
    pub uss_bytes: Option<u64>,
    pub pss_bytes: Option<u64>,
    pub io_read_bytes: Option<u64>,
    pub io_write_bytes: Option<u64>,
    pub cpu_time_seconds: f64,
    /// Cumulative major page faults for this process. `None` if the platform
    /// did not expose them this sample.
    pub major_page_faults: Option<u64>,
    /// Cumulative minor page faults for this process. `None` if the platform
    /// did not expose them this sample (always `None` on macOS via
    /// `proc_pid_rusage`).
    pub minor_page_faults: Option<u64>,
    /// Cumulative voluntary context switches for this process. `None` on
    /// macOS — `proc_pid_rusage` does not split context switches.
    pub voluntary_ctx_switches: Option<u64>,
    /// Cumulative involuntary context switches for this process. `None` on
    /// macOS.
    pub involuntary_ctx_switches: Option<u64>,
    /// Number of live threads in this process at sample time. `None` if the
    /// platform did not expose a thread count this sample. Used to derive
    /// per-tick `n_threads` (sum across PIDs) and aggregate `peak_n_threads`
    /// (max of those sums across ticks).
    pub thread_count: Option<u64>,
}

/// Per-PID accumulator; used to keep the latest seen value of monotonically-
/// increasing counters even after the process exits.
#[derive(Debug, Clone, Copy, Default)]
struct ProcessAccum {
    io_read_bytes: Option<u64>,
    io_write_bytes: Option<u64>,
    cpu_time_seconds: f64,
    /// Running max of cumulative major page faults (so summing across PIDs
    /// gives total tree faults, including exited children).
    major_page_faults: Option<u64>,
    minor_page_faults: Option<u64>,
    voluntary_ctx_switches: Option<u64>,
    involuntary_ctx_switches: Option<u64>,
}

/// Running aggregate of all snapshots taken during a benchmark run.
///
/// `Option<u64>` peak fields use `None` to mean "this metric was never observed
/// for any process in any tick" — used downstream to render the TSV column as
/// `-` rather than `0.00`.
#[derive(Debug, Default)]
pub struct SamplerState {
    max_rss_bytes: u64,
    max_vms_bytes: u64,
    max_uss_bytes: Option<u64>,
    max_pss_bytes: Option<u64>,
    per_pid: HashMap<i32, ProcessAccum>,
    /// Per-PID cumulative counters at the end of the last tick — used to
    /// compute per-tick *deltas* for the trace TSV. Distinct from the
    /// matching fields on [`ProcessAccum`] (those hold the running maximum
    /// used for aggregate totals; this is a point-in-time snapshot).
    prev_cumulative: HashMap<i32, PrevCumulative>,
    /// Peak instantaneous live-thread count across the tree (max over ticks
    /// of summed per-PID `thread_count`). `None` if no process ever exposed
    /// a thread count.
    max_n_threads: Option<u64>,
    /// Peak instantaneous live-process count across the tree (max over
    /// ticks of `snapshots.len()`).
    max_n_procs: u64,
    data_collected: bool,
}

/// Per-PID cumulative-counter snapshot used to compute per-tick deltas.
/// Each `tricord`-added per-tick metric needs a slot here; the type is
/// `u64` because `saturating_sub` against an `Option<u64>` would be uglier.
#[derive(Debug, Clone, Copy, Default)]
struct PrevCumulative {
    major_page_faults: u64,
    minor_page_faults: u64,
    voluntary_ctx_switches: u64,
    involuntary_ctx_switches: u64,
}

/// Per-tick deltas of cumulative counters, summed across observed PIDs.
/// `None` means no process exposed the counter this tick.
#[derive(Debug, Default)]
struct PerTickDeltas {
    major_page_faults: Option<u64>,
    minor_page_faults: Option<u64>,
    voluntary_ctx_switches: Option<u64>,
    involuntary_ctx_switches: Option<u64>,
}

/// Per-tick memory sums across the live tree. `uss` and `pss` are `None`
/// when no process in this tick exposed the metric.
struct MemorySums {
    rss: u64,
    vms: u64,
    uss: Option<u64>,
    pss: Option<u64>,
}

/// Cumulative I/O and CPU totals across every PID observed so far. `io_in`
/// and `io_out` are `None` when no process has ever exposed I/O counters;
/// `major_page_faults` and `minor_page_faults` likewise.
struct CumulativeTotals {
    io_in: Option<u64>,
    io_out: Option<u64>,
    cpu_time: f64,
    major_page_faults: Option<u64>,
    minor_page_faults: Option<u64>,
    voluntary_ctx_switches: Option<u64>,
    involuntary_ctx_switches: Option<u64>,
}

/// Sum memory across the snapshots in a single tick.
fn sum_memory(snapshots: &[ProcessSnapshot]) -> MemorySums {
    let mut rss: u64 = 0;
    let mut vms: u64 = 0;
    let mut uss: u64 = 0;
    let mut pss: u64 = 0;
    let mut any_uss = false;
    let mut any_pss = false;
    for snap in snapshots {
        rss = rss.saturating_add(snap.rss_bytes);
        vms = vms.saturating_add(snap.vms_bytes);
        if let Some(v) = snap.uss_bytes {
            uss = uss.saturating_add(v);
            any_uss = true;
        }
        if let Some(v) = snap.pss_bytes {
            pss = pss.saturating_add(v);
            any_pss = true;
        }
    }
    MemorySums {
        rss,
        vms,
        uss: if any_uss { Some(uss) } else { None },
        pss: if any_pss { Some(pss) } else { None },
    }
}

/// Sum the per-PID accumulators for I/O, CPU, and page faults. Includes PIDs
/// whose processes have already exited (last-observed value persists).
fn cumulative_totals(per_pid: &HashMap<i32, ProcessAccum>) -> CumulativeTotals {
    let mut io_in: u64 = 0;
    let mut io_out: u64 = 0;
    let mut any_io_in = false;
    let mut any_io_out = false;
    let mut cpu_time = 0.0_f64;
    let mut major_pf: u64 = 0;
    let mut minor_pf: u64 = 0;
    let mut any_major_pf = false;
    let mut any_minor_pf = false;
    let mut vol_cs: u64 = 0;
    let mut invol_cs: u64 = 0;
    let mut any_vol_cs = false;
    let mut any_invol_cs = false;
    for accum in per_pid.values() {
        if let Some(v) = accum.io_read_bytes {
            io_in = io_in.saturating_add(v);
            any_io_in = true;
        }
        if let Some(v) = accum.io_write_bytes {
            io_out = io_out.saturating_add(v);
            any_io_out = true;
        }
        cpu_time += accum.cpu_time_seconds;
        if let Some(v) = accum.major_page_faults {
            major_pf = major_pf.saturating_add(v);
            any_major_pf = true;
        }
        if let Some(v) = accum.minor_page_faults {
            minor_pf = minor_pf.saturating_add(v);
            any_minor_pf = true;
        }
        if let Some(v) = accum.voluntary_ctx_switches {
            vol_cs = vol_cs.saturating_add(v);
            any_vol_cs = true;
        }
        if let Some(v) = accum.involuntary_ctx_switches {
            invol_cs = invol_cs.saturating_add(v);
            any_invol_cs = true;
        }
    }
    CumulativeTotals {
        io_in: if any_io_in { Some(io_in) } else { None },
        io_out: if any_io_out { Some(io_out) } else { None },
        cpu_time,
        major_page_faults: if any_major_pf { Some(major_pf) } else { None },
        minor_page_faults: if any_minor_pf { Some(minor_pf) } else { None },
        voluntary_ctx_switches: if any_vol_cs { Some(vol_cs) } else { None },
        involuntary_ctx_switches: if any_invol_cs { Some(invol_cs) } else { None },
    }
}

fn bytes_to_mib(bytes: u64) -> f64 {
    (bytes as f64) / (1024.0 * 1024.0)
}

/// Sum the `thread_count` field across `snapshots`. Returns `None` when no
/// process in the tick exposed a thread count (older platforms, restricted
/// access) so the trace column renders `-` rather than `0` and aggregates
/// stay `None` for the run.
fn sum_thread_count(snapshots: &[ProcessSnapshot]) -> Option<u64> {
    let mut total: u64 = 0;
    let mut any = false;
    for snap in snapshots {
        if let Some(t) = snap.thread_count {
            total = total.saturating_add(t);
            any = true;
        }
    }
    if any { Some(total) } else { None }
}

impl SamplerState {
    /// Fold one tick's worth of snapshots into the running aggregate.
    pub fn absorb(&mut self, snapshots: &[ProcessSnapshot]) {
        if snapshots.is_empty() {
            return;
        }
        let sums = sum_memory(snapshots);
        self.max_rss_bytes = self.max_rss_bytes.max(sums.rss);
        self.max_vms_bytes = self.max_vms_bytes.max(sums.vms);
        if let Some(v) = sums.uss {
            self.max_uss_bytes = Some(self.max_uss_bytes.unwrap_or(0).max(v));
        }
        if let Some(v) = sums.pss {
            self.max_pss_bytes = Some(self.max_pss_bytes.unwrap_or(0).max(v));
        }
        // Instantaneous tree-wide thread + process counts at this tick.
        // Both follow the memory-style "max of per-tick sums" semantics —
        // they are not cumulative counters with deltas.
        if let Some(threads) = sum_thread_count(snapshots) {
            self.max_n_threads = Some(self.max_n_threads.unwrap_or(0).max(threads));
        }
        self.max_n_procs = self.max_n_procs.max(snapshots.len() as u64);
        for snap in snapshots {
            let entry = self.per_pid.entry(snap.pid).or_default();
            if let Some(io_in) = snap.io_read_bytes {
                entry.io_read_bytes = Some(io_in.max(entry.io_read_bytes.unwrap_or(0)));
            }
            if let Some(io_out) = snap.io_write_bytes {
                entry.io_write_bytes = Some(io_out.max(entry.io_write_bytes.unwrap_or(0)));
            }
            if snap.cpu_time_seconds > entry.cpu_time_seconds {
                entry.cpu_time_seconds = snap.cpu_time_seconds;
            }
            if let Some(v) = snap.major_page_faults {
                entry.major_page_faults = Some(v.max(entry.major_page_faults.unwrap_or(0)));
            }
            if let Some(v) = snap.minor_page_faults {
                entry.minor_page_faults = Some(v.max(entry.minor_page_faults.unwrap_or(0)));
            }
            if let Some(v) = snap.voluntary_ctx_switches {
                entry.voluntary_ctx_switches =
                    Some(v.max(entry.voluntary_ctx_switches.unwrap_or(0)));
            }
            if let Some(v) = snap.involuntary_ctx_switches {
                entry.involuntary_ctx_switches =
                    Some(v.max(entry.involuntary_ctx_switches.unwrap_or(0)));
            }
        }
        self.data_collected = true;
    }

    /// Build a per-tick [`TickRecord`] from `snapshots` (the just-sampled
    /// live processes) plus this state's running cumulative I/O and CPU
    /// totals. Returns `None` when `snapshots` is empty (nothing to record).
    ///
    /// Memory totals are instantaneous (summed across `snapshots`); I/O and
    /// CPU are cumulative across every PID observed so far, including
    /// children that have already exited. Page-fault columns are *deltas*
    /// for the current tick — for each PID, the difference between the
    /// current sample and the previous-tick observation, summed across all
    /// PIDs in this tick. The first observation of a PID is treated as a
    /// delta from zero, which is correct for PIDs born after `tricord`
    /// started (their counters started at zero).
    ///
    /// Takes `&mut self` because the per-tick delta requires snapshotting
    /// the current per-PID values into [`Self::prev_cumulative`] so the
    /// next tick can compute *its* delta.
    pub fn tick(
        &mut self,
        snapshots: &[ProcessSnapshot],
        elapsed_seconds: f64,
    ) -> Option<TickRecord> {
        if snapshots.is_empty() {
            return None;
        }
        let mem = sum_memory(snapshots);
        let cum = cumulative_totals(&self.per_pid);

        let deltas = self.compute_and_advance_per_tick_deltas(snapshots);

        Some(TickRecord {
            elapsed: elapsed_seconds,
            rss: bytes_to_mib(mem.rss),
            vms: bytes_to_mib(mem.vms),
            uss: mem.uss.map(bytes_to_mib),
            pss: mem.pss.map(bytes_to_mib),
            io_in: cum.io_in.map(bytes_to_mib),
            io_out: cum.io_out.map(bytes_to_mib),
            cpu_time: cum.cpu_time,
            n_procs: snapshots.len(),
            major_page_faults: deltas.major_page_faults,
            minor_page_faults: deltas.minor_page_faults,
            voluntary_ctx_switches: deltas.voluntary_ctx_switches,
            involuntary_ctx_switches: deltas.involuntary_ctx_switches,
            n_threads: sum_thread_count(snapshots),
        })
    }

    /// For each PID in `snapshots`, compute the delta from the previous-tick
    /// cumulative counters (zero if never seen) for every per-tick metric,
    /// summed across PIDs. Advance [`Self::prev_cumulative`] to the current
    /// values so the next call computes its own correct delta.
    ///
    /// Each metric returns `None` when no process exposed it this tick —
    /// same convention as the I/O fields, so the trace TSV renders `-`
    /// rather than `0` and consumers can tell "not observed" from "observed
    /// as zero." `saturating_sub` guards against PID reuse (a recycled PID
    /// starting at zero must not produce a phantom huge delta).
    fn compute_and_advance_per_tick_deltas(
        &mut self,
        snapshots: &[ProcessSnapshot],
    ) -> PerTickDeltas {
        let mut major_pf: u64 = 0;
        let mut minor_pf: u64 = 0;
        let mut vol_cs: u64 = 0;
        let mut invol_cs: u64 = 0;
        let mut any_major_pf = false;
        let mut any_minor_pf = false;
        let mut any_vol_cs = false;
        let mut any_invol_cs = false;
        for snap in snapshots {
            let prev = self.prev_cumulative.get(&snap.pid).copied().unwrap_or_default();
            if let Some(current) = snap.major_page_faults {
                major_pf = major_pf.saturating_add(current.saturating_sub(prev.major_page_faults));
                any_major_pf = true;
            }
            if let Some(current) = snap.minor_page_faults {
                minor_pf = minor_pf.saturating_add(current.saturating_sub(prev.minor_page_faults));
                any_minor_pf = true;
            }
            if let Some(current) = snap.voluntary_ctx_switches {
                vol_cs = vol_cs.saturating_add(current.saturating_sub(prev.voluntary_ctx_switches));
                any_vol_cs = true;
            }
            if let Some(current) = snap.involuntary_ctx_switches {
                invol_cs =
                    invol_cs.saturating_add(current.saturating_sub(prev.involuntary_ctx_switches));
                any_invol_cs = true;
            }
            // Advance prev. Unobserved → preserve prior value (so a metric
            // that drops out for one tick doesn't fabricate a delta when it
            // returns); newly observed → use current.
            self.prev_cumulative.insert(
                snap.pid,
                PrevCumulative {
                    major_page_faults: snap.major_page_faults.unwrap_or(prev.major_page_faults),
                    minor_page_faults: snap.minor_page_faults.unwrap_or(prev.minor_page_faults),
                    voluntary_ctx_switches: snap
                        .voluntary_ctx_switches
                        .unwrap_or(prev.voluntary_ctx_switches),
                    involuntary_ctx_switches: snap
                        .involuntary_ctx_switches
                        .unwrap_or(prev.involuntary_ctx_switches),
                },
            );
        }
        PerTickDeltas {
            major_page_faults: if any_major_pf { Some(major_pf) } else { None },
            minor_page_faults: if any_minor_pf { Some(minor_pf) } else { None },
            voluntary_ctx_switches: if any_vol_cs { Some(vol_cs) } else { None },
            involuntary_ctx_switches: if any_invol_cs { Some(invol_cs) } else { None },
        }
    }

    /// Materialize the running aggregate into a [`BenchmarkRecord`] given the
    /// final wall-clock running time.
    #[must_use]
    pub fn into_record(self, running_time_seconds: f64) -> BenchmarkRecord {
        if !self.data_collected {
            return BenchmarkRecord {
                running_time: running_time_seconds,
                data_collected: false,
                ..Default::default()
            };
        }
        let cum = cumulative_totals(&self.per_pid);
        let mean_load = if running_time_seconds > 0.0 {
            (cum.cpu_time / running_time_seconds) * 100.0
        } else {
            0.0
        };
        BenchmarkRecord {
            running_time: running_time_seconds,
            max_rss: Some(bytes_to_mib(self.max_rss_bytes)),
            max_vms: Some(bytes_to_mib(self.max_vms_bytes)),
            max_uss: self.max_uss_bytes.map(bytes_to_mib),
            max_pss: self.max_pss_bytes.map(bytes_to_mib),
            io_in: cum.io_in.map(bytes_to_mib),
            io_out: cum.io_out.map(bytes_to_mib),
            mean_load,
            cpu_time: cum.cpu_time,
            major_page_faults: cum.major_page_faults,
            minor_page_faults: cum.minor_page_faults,
            voluntary_ctx_switches: cum.voluntary_ctx_switches,
            involuntary_ctx_switches: cum.involuntary_ctx_switches,
            peak_n_threads: self.max_n_threads,
            peak_n_procs: self.max_n_procs,
            // Loadavg start/end are captured by run_command around the
            // sampler's lifetime, not inside the sampler itself.
            loadavg_1m_start: None,
            loadavg_1m_end: None,
            data_collected: true,
        }
    }
}

/// Options controlling sampler behavior.
#[derive(Debug, Clone)]
pub struct SamplerOptions {
    /// Wall-clock interval between samples.
    pub interval: Duration,
    /// Optional path to write a per-tick TSV trace to. When `Some`, the sampler
    /// thread opens this file, writes [`TRACE_TSV_HEADER`], and appends one row
    /// per non-empty tick. When `None`, no trace file is created.
    pub trace_path: Option<Box<Path>>,
}

impl Default for SamplerOptions {
    fn default() -> Self {
        Self { interval: DEFAULT_INTERVAL, trace_path: None }
    }
}

/// Handle to a running sampler thread.
pub struct SamplerHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<SamplerState>>,
    started_at: Instant,
}

impl SamplerHandle {
    /// Spawn a background thread that polls the process tree rooted at
    /// `root_pid` until [`Self::stop`] is called.
    ///
    /// # Panics
    /// Panics if the OS refuses to spawn a new thread (extreme resource
    /// exhaustion). Callers are expected to terminate in that case.
    #[must_use]
    pub fn spawn(root_pid: i32, options: SamplerOptions) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let started_at = Instant::now();
        let thread = thread::Builder::new()
            .name("tricord-sampler".into())
            .spawn(move || {
                run_sampler_loop(
                    root_pid,
                    options.interval,
                    options.trace_path.as_deref(),
                    started_at,
                    &stop_for_thread,
                )
            })
            .expect("failed to spawn sampler thread");
        Self { stop, thread: Some(thread), started_at }
    }

    /// Signal the sampler thread to stop and wait for it to finish.
    ///
    /// Returns the aggregated [`BenchmarkRecord`] including the final wall-clock
    /// running time computed from when [`Self::spawn`] was called.
    ///
    /// # Panics
    /// Panics if called twice on the same handle, which the type system already
    /// prevents (the method takes `self` by value).
    #[must_use]
    pub fn stop(mut self) -> BenchmarkRecord {
        self.stop.store(true, Ordering::SeqCst);
        let state =
            self.thread.take().expect("sampler thread already joined").join().unwrap_or_default();
        let elapsed = self.started_at.elapsed().as_secs_f64();
        state.into_record(elapsed)
    }
}

fn run_sampler_loop(
    root_pid: i32,
    interval: Duration,
    trace_path: Option<&Path>,
    started_at: Instant,
    stop: &AtomicBool,
) -> SamplerState {
    let mut state = SamplerState::default();
    let mut sampler = platform::new_sampler();
    let mut trace = trace_path.and_then(open_trace_writer);
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(interval);
        if stop.load(Ordering::SeqCst) {
            // Skip the sample if we were asked to stop while sleeping; the
            // child has already exited and re-reading /proc may race.
            break;
        }
        let snapshots = sampler.sample_tree(root_pid);
        state.absorb(&snapshots);
        if let Some(writer) = trace.as_mut()
            && let Some(tick) = state.tick(&snapshots, started_at.elapsed().as_secs_f64())
        {
            // Flush per row so a SIGKILL (the OOM-postmortem case the trace
            // file exists for) doesn't lose buffered ticks.
            if let Err(err) =
                writeln!(writer, "{}", tick.to_tsv_row()).and_then(|()| writer.flush())
            {
                eprintln!("tricord: failed to write trace row: {err}");
                trace = None;
            }
        }
    }
    state
}

/// Open the trace TSV at `path` and write the header. Returns `None` after
/// reporting to stderr if the file can't be opened — a failing diagnostic
/// file should never fail the benchmarked run.
fn open_trace_writer(path: &Path) -> Option<BufWriter<File>> {
    if let Err(err) = format::ensure_parent_dir(path) {
        eprintln!("tricord: cannot create trace parent for {}: {err}", path.display());
        return None;
    }
    let file = match File::create(path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("tricord: cannot open trace file {}: {err}", path.display());
            return None;
        }
    };
    let mut writer = BufWriter::new(file);
    if let Err(err) = writeln!(writer, "{TRACE_TSV_HEADER}") {
        eprintln!("tricord: cannot write trace header to {}: {err}", path.display());
        return None;
    }
    Some(writer)
}

#[cfg(test)]
mod tests {
    use crate::record::TRACE_TSV_HEADER;

    use super::*;

    #[test]
    fn empty_state_records_no_data() {
        let state = SamplerState::default();
        let record = state.into_record(1.0);
        assert!(!record.data_collected);
        assert!((record.running_time - 1.0).abs() < 1e-9);
        assert!(record.max_rss.is_none());
    }

    #[test]
    fn single_snapshot_populates_record() {
        let mut state = SamplerState::default();
        state.absorb(&[ProcessSnapshot {
            pid: 100,
            rss_bytes: 10 * 1024 * 1024,
            vms_bytes: 100 * 1024 * 1024,
            uss_bytes: Some(8 * 1024 * 1024),
            pss_bytes: Some(9 * 1024 * 1024),
            io_read_bytes: Some(1024 * 1024),
            io_write_bytes: Some(2 * 1024 * 1024),
            cpu_time_seconds: 0.5,
            ..Default::default()
        }]);
        let record = state.into_record(2.0);
        assert!(record.data_collected);
        assert_eq!(record.max_rss, Some(10.0));
        assert_eq!(record.max_vms, Some(100.0));
        assert_eq!(record.max_uss, Some(8.0));
        assert_eq!(record.max_pss, Some(9.0));
        assert_eq!(record.io_in, Some(1.0));
        assert_eq!(record.io_out, Some(2.0));
        assert!((record.cpu_time - 0.5).abs() < 1e-9);
        assert!((record.mean_load - 25.0).abs() < 1e-9); // 0.5s cpu / 2.0s wall = 25%.
        // Page faults default to None when the platform never reported them.
        assert!(record.major_page_faults.is_none());
        assert!(record.minor_page_faults.is_none());
    }

    #[test]
    fn memory_max_uses_summed_tree() {
        let mut state = SamplerState::default();
        // Tick 1: parent + child, total = 30 MiB RSS.
        state.absorb(&[
            ProcessSnapshot {
                pid: 1,
                rss_bytes: 10 * 1024 * 1024,
                vms_bytes: 0,
                cpu_time_seconds: 0.0,
                ..Default::default()
            },
            ProcessSnapshot {
                pid: 2,
                rss_bytes: 20 * 1024 * 1024,
                vms_bytes: 0,
                cpu_time_seconds: 0.0,
                ..Default::default()
            },
        ]);
        // Tick 2: parent only, total = 25 MiB RSS — peak of this tick is lower.
        state.absorb(&[ProcessSnapshot {
            pid: 1,
            rss_bytes: 25 * 1024 * 1024,
            vms_bytes: 0,
            cpu_time_seconds: 0.0,
            ..Default::default()
        }]);
        let record = state.into_record(1.0);
        assert_eq!(record.max_rss, Some(30.0)); // peak across snapshots
    }

    #[test]
    fn io_and_cpu_aggregate_across_pids_after_exit() {
        let mut state = SamplerState::default();
        // Tick 1: child A and child B both alive with I/O and CPU usage.
        state.absorb(&[
            ProcessSnapshot {
                pid: 10,
                rss_bytes: 1,
                vms_bytes: 1,
                io_read_bytes: Some(50 * 1024 * 1024),
                io_write_bytes: Some(10 * 1024 * 1024),
                cpu_time_seconds: 1.0,
                ..Default::default()
            },
            ProcessSnapshot {
                pid: 11,
                rss_bytes: 1,
                vms_bytes: 1,
                io_read_bytes: Some(20 * 1024 * 1024),
                io_write_bytes: Some(5 * 1024 * 1024),
                cpu_time_seconds: 0.5,
                ..Default::default()
            },
        ]);
        // Tick 2: child B has exited; child A continues.
        state.absorb(&[ProcessSnapshot {
            pid: 10,
            rss_bytes: 1,
            vms_bytes: 1,
            io_read_bytes: Some(60 * 1024 * 1024),
            io_write_bytes: Some(15 * 1024 * 1024),
            cpu_time_seconds: 1.5,
            ..Default::default()
        }]);
        let record = state.into_record(1.0);
        // io_in: child A latest (60) + child B latest (20) = 80 MiB
        assert_eq!(record.io_in, Some(80.0));
        assert_eq!(record.io_out, Some(20.0));
        // cpu_time: child A (1.5) + child B (0.5) = 2.0s
        assert!((record.cpu_time - 2.0).abs() < 1e-9);
    }

    #[test]
    fn tick_summarizes_live_memory_and_cumulative_io_cpu() {
        let mut state = SamplerState::default();
        let snaps = &[ProcessSnapshot {
            pid: 1,
            rss_bytes: 12 * 1024 * 1024,
            vms_bytes: 100 * 1024 * 1024,
            uss_bytes: Some(8 * 1024 * 1024),
            pss_bytes: Some(9 * 1024 * 1024),
            io_read_bytes: Some(2 * 1024 * 1024),
            io_write_bytes: Some(3 * 1024 * 1024),
            cpu_time_seconds: 0.7,
            ..Default::default()
        }];
        state.absorb(snaps);
        let tick = state.tick(snaps, 1.5).expect("tick");
        assert!((tick.elapsed - 1.5).abs() < 1e-9);
        assert!((tick.rss - 12.0).abs() < 1e-9);
        assert!((tick.vms - 100.0).abs() < 1e-9);
        assert_eq!(tick.uss, Some(8.0));
        assert_eq!(tick.pss, Some(9.0));
        assert_eq!(tick.io_in, Some(2.0));
        assert_eq!(tick.io_out, Some(3.0));
        assert!((tick.cpu_time - 0.7).abs() < 1e-9);
        assert_eq!(tick.n_procs, 1);
    }

    #[test]
    fn tick_returns_none_for_empty_snapshots() {
        let mut state = SamplerState::default();
        assert!(state.tick(&[], 1.0).is_none());
    }

    #[test]
    fn tick_io_and_cpu_include_exited_children() {
        let mut state = SamplerState::default();
        // Tick 1: parent + child both alive with I/O and CPU.
        state.absorb(&[
            ProcessSnapshot {
                pid: 99,
                rss_bytes: 1,
                vms_bytes: 1,
                io_read_bytes: Some(50 * 1024 * 1024),
                io_write_bytes: Some(10 * 1024 * 1024),
                cpu_time_seconds: 0.5,
                ..Default::default()
            },
            ProcessSnapshot {
                pid: 1,
                rss_bytes: 1,
                vms_bytes: 1,
                io_read_bytes: Some(0),
                io_write_bytes: Some(0),
                cpu_time_seconds: 0.1,
                ..Default::default()
            },
        ]);
        // Tick 2: child (pid 99) exited; only parent (pid 1) sampled.
        let snaps = &[ProcessSnapshot {
            pid: 1,
            rss_bytes: 2,
            vms_bytes: 2,
            io_read_bytes: Some(0),
            io_write_bytes: Some(0),
            cpu_time_seconds: 0.2,
            ..Default::default()
        }];
        state.absorb(snaps);
        let tick = state.tick(snaps, 1.0).expect("tick");
        // I/O cumulative: pid 99 last seen 50 + pid 1 last seen 0 = 50 MiB.
        assert_eq!(tick.io_in, Some(50.0));
        assert_eq!(tick.io_out, Some(10.0));
        // CPU cumulative: pid 99 last 0.5 + pid 1 last 0.2 = 0.7s.
        assert!((tick.cpu_time - 0.7).abs() < 1e-9);
        // Only pid 1 is alive in this tick.
        assert_eq!(tick.n_procs, 1);
    }

    #[test]
    fn sampler_thread_writes_trace_file_when_path_set() {
        let tmp = tempfile::tempdir().unwrap();
        let trace = tmp.path().join("trace.tsv");
        // Sample our own PID so the platform sampler always returns at least
        // one snapshot per tick.
        #[allow(clippy::cast_possible_wrap)]
        let pid = std::process::id() as i32;
        let handle = SamplerHandle::spawn(
            pid,
            SamplerOptions {
                interval: Duration::from_millis(50),
                trace_path: Some(trace.clone().into_boxed_path()),
            },
        );
        // Let several ticks fire.
        thread::sleep(Duration::from_millis(300));
        let _ = handle.stop();

        let text = std::fs::read_to_string(&trace).expect("trace file");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], TRACE_TSV_HEADER, "first line should be the trace header");
        assert!(lines.len() >= 3, "expected header + multiple data rows, got: {text:?}");
        let mut last_elapsed = -1.0_f64;
        let expected_cols = TRACE_TSV_HEADER.split('\t').count();
        for row in &lines[1..] {
            let cols: Vec<&str> = row.split('\t').collect();
            assert_eq!(cols.len(), expected_cols, "row has wrong column count: {row:?}");
            let elapsed: f64 = cols[0].parse().expect("elapsed parses");
            assert!(elapsed >= last_elapsed, "elapsed should be monotonic: {row:?}");
            last_elapsed = elapsed;
        }
    }

    #[test]
    fn io_read_only_leaves_io_out_none() {
        // If a process exposes io_read_bytes but never io_write_bytes (or vice
        // versa), the absent side must remain None — not silently coerced to
        // Some(0) just because the other side was observed.
        let mut state = SamplerState::default();
        state.absorb(&[ProcessSnapshot {
            pid: 1,
            rss_bytes: 1,
            vms_bytes: 1,
            io_read_bytes: Some(4 * 1024 * 1024),
            io_write_bytes: None,
            cpu_time_seconds: 0.0,
            ..Default::default()
        }]);
        let record = state.into_record(1.0);
        assert_eq!(record.io_in, Some(4.0));
        assert!(record.io_out.is_none(), "io_out must be None when never observed");
    }

    #[test]
    fn io_write_only_leaves_io_in_none() {
        let mut state = SamplerState::default();
        state.absorb(&[ProcessSnapshot {
            pid: 1,
            rss_bytes: 1,
            vms_bytes: 1,
            io_read_bytes: None,
            io_write_bytes: Some(8 * 1024 * 1024),
            cpu_time_seconds: 0.0,
            ..Default::default()
        }]);
        let record = state.into_record(1.0);
        assert!(record.io_in.is_none(), "io_in must be None when never observed");
        assert_eq!(record.io_out, Some(8.0));
    }

    #[test]
    fn page_faults_aggregate_sums_per_pid_max_across_tree() {
        let mut state = SamplerState::default();
        // Tick 1: child A (100 maj, 5000 min), child B (50 maj, 2000 min).
        state.absorb(&[
            ProcessSnapshot {
                pid: 10,
                rss_bytes: 1,
                vms_bytes: 1,
                major_page_faults: Some(100),
                minor_page_faults: Some(5000),
                ..Default::default()
            },
            ProcessSnapshot {
                pid: 11,
                rss_bytes: 1,
                vms_bytes: 1,
                major_page_faults: Some(50),
                minor_page_faults: Some(2000),
                ..Default::default()
            },
        ]);
        // Tick 2: B exited; A reached (150, 6500).
        state.absorb(&[ProcessSnapshot {
            pid: 10,
            rss_bytes: 1,
            vms_bytes: 1,
            major_page_faults: Some(150),
            minor_page_faults: Some(6500),
            ..Default::default()
        }]);
        let record = state.into_record(1.0);
        // Aggregate = sum of per-PID maxima = A(150,6500) + B(50,2000)
        assert_eq!(record.major_page_faults, Some(200));
        assert_eq!(record.minor_page_faults, Some(8500));
    }

    #[test]
    fn tick_page_faults_are_per_tick_deltas() {
        let mut state = SamplerState::default();
        // Tick 1: pid 1 at (10, 100). First observation → delta from 0.
        let t1 = [ProcessSnapshot {
            pid: 1,
            rss_bytes: 1,
            vms_bytes: 1,
            major_page_faults: Some(10),
            minor_page_faults: Some(100),
            ..Default::default()
        }];
        state.absorb(&t1);
        let tick1 = state.tick(&t1, 0.5).expect("tick1");
        assert_eq!(tick1.major_page_faults, Some(10));
        assert_eq!(tick1.minor_page_faults, Some(100));

        // Tick 2: pid 1 at (25, 400). Delta should be (15, 300).
        let t2 = [ProcessSnapshot {
            pid: 1,
            rss_bytes: 1,
            vms_bytes: 1,
            major_page_faults: Some(25),
            minor_page_faults: Some(400),
            ..Default::default()
        }];
        state.absorb(&t2);
        let tick2 = state.tick(&t2, 1.0).expect("tick2");
        assert_eq!(tick2.major_page_faults, Some(15));
        assert_eq!(tick2.minor_page_faults, Some(300));
    }

    #[test]
    fn tick_page_faults_saturate_on_pid_reuse() {
        // If a PID is reused (process exits, OS reassigns the pid), the new
        // process starts its counters at 0. saturating_sub prevents a
        // phantom "negative" delta from becoming a huge u64 wrap-around.
        let mut state = SamplerState::default();
        let t1 = [ProcessSnapshot {
            pid: 7,
            rss_bytes: 1,
            vms_bytes: 1,
            major_page_faults: Some(500),
            minor_page_faults: Some(10_000),
            ..Default::default()
        }];
        state.absorb(&t1);
        let _ = state.tick(&t1, 0.5);
        // Same pid, lower counters (new process under reused pid).
        let t2 = [ProcessSnapshot {
            pid: 7,
            rss_bytes: 1,
            vms_bytes: 1,
            major_page_faults: Some(3),
            minor_page_faults: Some(40),
            ..Default::default()
        }];
        state.absorb(&t2);
        let tick = state.tick(&t2, 1.0).expect("tick2");
        // Saturating delta is 0, not 2^64 - 497.
        assert_eq!(tick.major_page_faults, Some(0));
        assert_eq!(tick.minor_page_faults, Some(0));
    }

    #[test]
    fn tick_page_faults_none_when_no_process_exposes_them() {
        let mut state = SamplerState::default();
        let snaps = [ProcessSnapshot {
            pid: 1,
            rss_bytes: 1,
            vms_bytes: 1,
            major_page_faults: None,
            minor_page_faults: None,
            ..Default::default()
        }];
        state.absorb(&snaps);
        let tick = state.tick(&snaps, 0.5).expect("tick");
        assert!(tick.major_page_faults.is_none());
        assert!(tick.minor_page_faults.is_none());
    }

    #[test]
    fn missing_uss_and_io_remain_none_in_record() {
        let mut state = SamplerState::default();
        state.absorb(&[ProcessSnapshot {
            pid: 1,
            rss_bytes: 1024 * 1024,
            vms_bytes: 1024 * 1024,
            uss_bytes: None,
            pss_bytes: None,
            io_read_bytes: None,
            io_write_bytes: None,
            cpu_time_seconds: 0.0,
            ..Default::default()
        }]);
        let record = state.into_record(1.0);
        assert!(record.max_uss.is_none());
        assert!(record.max_pss.is_none());
        assert!(record.io_in.is_none());
        assert!(record.io_out.is_none());
    }
}

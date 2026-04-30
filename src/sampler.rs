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
}

/// Per-PID accumulator; used to keep the latest seen value of monotonically-
/// increasing counters even after the process exits.
#[derive(Debug, Clone, Copy, Default)]
struct ProcessAccum {
    io_read_bytes: Option<u64>,
    io_write_bytes: Option<u64>,
    cpu_time_seconds: f64,
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
    data_collected: bool,
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
/// and `io_out` are `None` when no process has ever exposed I/O counters.
struct CumulativeTotals {
    io_in: Option<u64>,
    io_out: Option<u64>,
    cpu_time: f64,
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

/// Sum the per-PID accumulators for I/O and CPU. Includes PIDs whose
/// processes have already exited (last-observed value persists).
fn cumulative_totals(per_pid: &HashMap<i32, ProcessAccum>) -> CumulativeTotals {
    let mut io_in: u64 = 0;
    let mut io_out: u64 = 0;
    let mut any_io_in = false;
    let mut any_io_out = false;
    let mut cpu_time = 0.0_f64;
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
    }
    CumulativeTotals {
        io_in: if any_io_in { Some(io_in) } else { None },
        io_out: if any_io_out { Some(io_out) } else { None },
        cpu_time,
    }
}

fn bytes_to_mib(bytes: u64) -> f64 {
    (bytes as f64) / (1024.0 * 1024.0)
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
        }
        self.data_collected = true;
    }

    /// Build a per-tick [`TickRecord`] from `snapshots` (the just-sampled
    /// live processes) plus this state's running cumulative I/O and CPU
    /// totals. Returns `None` when `snapshots` is empty (nothing to record).
    ///
    /// Memory totals are instantaneous (summed across `snapshots`); I/O and
    /// CPU are cumulative across every PID observed so far, including
    /// children that have already exited.
    #[must_use]
    pub fn tick(&self, snapshots: &[ProcessSnapshot], elapsed_seconds: f64) -> Option<TickRecord> {
        if snapshots.is_empty() {
            return None;
        }
        let mem = sum_memory(snapshots);
        let cum = cumulative_totals(&self.per_pid);
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
        })
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
        let state = SamplerState::default();
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
        for row in &lines[1..] {
            let cols: Vec<&str> = row.split('\t').collect();
            assert_eq!(cols.len(), 9, "row has wrong column count: {row:?}");
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
        }]);
        let record = state.into_record(1.0);
        assert!(record.max_uss.is_none());
        assert!(record.max_pss.is_none());
        assert!(record.io_in.is_none());
        assert!(record.io_out.is_none());
    }
}

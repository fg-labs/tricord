//! Exit accounting and shutdown checks using this test executable as the workload.

use std::{
    os::unix::process::ExitStatusExt,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use tricord::{BenchmarkRecord, OutputFormat, RunOptions, SchemaMode, run_command};

fn options(path: &Path, interval: Duration) -> RunOptions {
    RunOptions {
        interval,
        output_path: path.into(),
        format: OutputFormat::Json,
        force_summary: false,
        trace_path: None,
        markdown_path: None,
        schema_mode: SchemaMode::Full,
    }
}

fn worker_args(name: &str) -> Vec<String> {
    ["--ignored", "--exact", name, "--nocapture"].map(str::to_owned).into()
}

fn run_worker(name: &str, interval: Duration) -> BenchmarkRecord {
    let tmp = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let result = run_command(
        exe.to_str().unwrap(),
        &worker_args(name),
        &options(&tmp.path().join("record.json"), interval),
    )
    .unwrap();
    assert!(result.status.success(), "worker failed: {:?}", result.status);
    result.record
}

// timeval field widths differ between Linux and macOS.
#[allow(clippy::cast_precision_loss, clippy::cast_lossless)]
fn own_cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: usage points to correctly aligned, writable rusage storage.
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) }, 0);
    // SAFETY: the successful getrusage call initialized usage.
    let usage = unsafe { usage.assume_init() };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1_000_000.0;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

fn burn_cpu(seconds: f64) {
    let start = own_cpu_seconds();
    while own_cpu_seconds() - start < seconds {
        for value in 0..10_000_u64 {
            std::hint::black_box(value.wrapping_mul(value));
        }
    }
}

#[test]
#[ignore = "workload invoked by the exit-accounting tests"]
fn cpu_worker() {
    let mut memory = vec![0_u8; 16 * 1024 * 1024];
    for byte in memory.iter_mut().step_by(4096) {
        *byte = 1;
    }
    burn_cpu(0.25);
    std::hint::black_box(&memory);
}

#[test]
#[ignore = "workload invoked by the exit-accounting tests"]
fn idle_worker() {
    std::thread::sleep(Duration::from_millis(600));
}

#[test]
#[ignore = "workload invoked by the exit-accounting tests"]
fn waited_worker() {
    let status = Command::new(std::env::current_exe().unwrap())
        .args(worker_args("cpu_worker"))
        .status()
        .unwrap();
    assert!(status.success());
    burn_cpu(0.15);
    // Leave time for sampled accounting to observe the parent after its wait.
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
#[ignore = "workload invoked by the exit-accounting tests"]
fn signaled_worker() {
    burn_cpu(0.1);
    // SAFETY: send SIGTERM only to this workload process.
    unsafe { libc::raise(libc::SIGTERM) };
    panic!("SIGTERM should terminate the workload");
}

#[test]
fn exit_only_records_cpu_rss_and_stops_without_waiting_for_interval() {
    let started = Instant::now();
    let record = run_worker("cpu_worker", Duration::from_secs(30));
    let elapsed = started.elapsed().as_secs_f64();
    assert!(elapsed < 5.0, "shutdown waited for interval: {elapsed}s");
    assert!(record.running_time < 5.0);
    assert!(elapsed >= record.running_time);
    assert!(record.cpu_time >= 0.25, "lost CPU tail: {record:?}");
    assert!(record.max_rss.unwrap() >= 16.0, "lost touched RSS: {record:?}");
    assert!(record.data_collected);
    assert_eq!(record.max_vms, None);
    assert_eq!(record.io_in, None);
    assert_eq!(record.peak_n_procs, None);
    assert!((record.mean_load - 100.0 * record.cpu_time / record.running_time).abs() < 1e-9);
}

#[test]
fn wall_time_is_not_rounded_to_sampling_interval() {
    let record = run_worker("idle_worker", Duration::from_secs(30));
    assert!(record.running_time >= 0.6);
    assert!(record.running_time < 5.0, "inflated wall time: {record:?}");
}

#[test]
fn exit_accounting_includes_waited_descendants_between_ticks() {
    let record = run_worker("waited_worker", Duration::from_secs(30));
    assert!(record.cpu_time >= 0.4, "missing waited child CPU: {record:?}");
    assert!(record.cpu_time < 0.65, "unexpected CPU total: {record:?}");
}

#[test]
fn sampled_and_exit_cpu_are_not_added() {
    let record = run_worker("waited_worker", Duration::from_millis(10));
    assert!(record.peak_n_procs.is_some(), "expected samples: {record:?}");
    assert!(record.cpu_time >= 0.4, "missing CPU tail: {record:?}");
    assert!(record.cpu_time < 0.65, "double-counted CPU: {record:?}");
}

#[test]
fn concurrent_library_runs_do_not_mix_child_cpu() {
    let busy = std::thread::spawn(|| run_worker("cpu_worker", Duration::from_secs(30)));
    let idle = run_worker("idle_worker", Duration::from_secs(30));
    let busy = busy.join().unwrap();
    assert!(busy.cpu_time >= 0.25);
    assert!(idle.cpu_time < 0.15, "unrelated child CPU leaked: {idle:?}");
}

#[test]
fn signal_exit_preserves_status_and_cpu() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let outcome = run_command(
        exe.to_str().unwrap(),
        &worker_args("signaled_worker"),
        &options(&tmp.path().join("record.json"), Duration::from_secs(30)),
    )
    .unwrap();
    assert_eq!(outcome.status.signal(), Some(libc::SIGTERM));
    assert_eq!(outcome.exit_code(), 128 + libc::SIGTERM);
    assert!(outcome.record.cpu_time >= 0.1);
}

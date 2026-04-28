//! End-to-end tests: spawn the `tricorder` binary against simple shell
//! commands and verify the output file's shape, columns, and exit code.

use std::{path::PathBuf, process::Command};

fn binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // drop test binary name
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("tricorder")
}

fn run_bench(out: &std::path::Path, format: &str, command: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(binary());
    cmd.arg("--out").arg(out).arg("--format").arg(format).arg("--");
    for piece in command {
        cmd.arg(piece);
    }
    cmd.output().expect("spawn tricorder")
}

#[test]
fn tsv_output_for_short_lived_command_has_correct_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &["sh", "-c", "sleep 0.7"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).expect("read tsv");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "expected header + 1 data row, got: {text:?}");
    assert_eq!(
        lines[0],
        "s\th:m:s\tmax_rss\tmax_vms\tmax_uss\tmax_pss\tio_in\tio_out\tmean_load\tcpu_time"
    );

    let cols: Vec<&str> = lines[1].split('\t').collect();
    assert_eq!(cols.len(), 10);
    let wall: f64 = cols[0].parse().expect("wall time parses");
    assert!(wall >= 0.5, "wall time {wall} should be at least 0.5s");
    assert!(wall < 5.0, "wall time {wall} should be under 5s");
    assert!(matches!(cols[1], "0:00:00" | "0:00:01"), "unexpected h:m:s {:?}", cols[1]);
}

#[test]
fn json_output_round_trips_to_object() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.json");
    let result = run_bench(&out, "json", &["sh", "-c", "sleep 0.6"]);
    assert!(result.status.success());

    let text = std::fs::read_to_string(&out).unwrap();
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid json");
    assert!(value.is_object());
    assert!(value["running_time"].as_f64().expect("running_time number") >= 0.5);
    assert_eq!(value["data_collected"], true);
}

#[test]
fn nested_output_directory_is_created() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("does/not/exist/yet/timing.tsv");
    let result = run_bench(&out, "tsv", &["sh", "-c", "true"]);
    assert!(result.status.success());
    assert!(out.exists());
}

#[test]
fn exit_code_is_passed_through() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &["sh", "-c", "exit 42"]);
    assert_eq!(result.status.code(), Some(42));
    // The benchmark file should still exist even when the child failed.
    assert!(out.exists());
}

#[test]
fn instant_exit_yields_na_row() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &["sh", "-c", "true"]);
    assert!(result.status.success());

    let text = std::fs::read_to_string(&out).unwrap();
    let row = text.lines().nth(1).expect("data row");
    let cols: Vec<&str> = row.split('\t').collect();
    // Either we caught a sample (numbers) or we didn't (NA placeholders);
    // both are valid for an essentially-instant child. Just verify the row
    // is well-formed.
    assert_eq!(cols.len(), 10);
    let wall: f64 = cols[0].parse().expect("wall time parses");
    assert!(wall >= 0.0);
}

fn python3_available() -> bool {
    Command::new("python3").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// End-to-end check that the platform sampler observes resource usage
/// proportional to a workload of known shape:
///
///   * touches ~50 MiB of memory (drives `max_rss`)
///   * writes ~16 MiB to disk and `fsync`s (drives `io_out`)
///   * busy-loops for 1.5 s on one core (drives `cpu_time`, `mean_load`)
///
/// `python3` is pre-installed on both `ubuntu-latest` and `macos-latest`
/// GitHub Actions runners. The test skips itself if `python3` is not on
/// `PATH` so a developer without it can still run the rest of the suite.
///
/// Bounds are intentionally generous; the goal is to catch a regression
/// that drops a metric to zero, not to assert exact numbers (allocator
/// slop, Python interpreter overhead, and runner load all add noise).
#[test]
#[allow(clippy::similar_names)] // max_rss / max_pss are TSV column names
fn end_to_end_resource_usage_against_known_workload() {
    if !python3_available() {
        // In CI we want a missing python3 to be loud — the runner image
        // changing under us is a regression, not a feature.
        assert!(
            std::env::var_os("CI").is_none(),
            "python3 not on PATH in CI; the runner image changed",
        );
        eprintln!("skipping: python3 not on PATH");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("workload.tsv");
    let scratch = tmp.path().join("scratch.bin");

    let workload = format!(
        r#"
import os, time
buf = bytearray(50 * 1024 * 1024)
for i in range(0, len(buf), 4096):
    buf[i] = i & 0xff
with open({scratch:?}, "wb") as f:
    f.write(bytes(16 * 1024 * 1024))
    f.flush()
    os.fsync(f.fileno())
end = time.monotonic() + 1.5
while time.monotonic() < end:
    pass
"#,
        scratch = scratch.display().to_string(),
    );

    // Tighten the sampling interval so a ~2 s workload still yields a
    // dozen-plus samples even if the first poll lands late.
    let result = Command::new(binary())
        .arg("--out")
        .arg(&out)
        .args(["--interval", "0.1"])
        .args(["--format", "tsv"])
        .arg("--")
        .args(["python3", "-c", &workload])
        .output()
        .expect("spawn tricorder");
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).expect("read tsv");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "expected header + 1 data row, got: {text:?}");
    let cols: Vec<&str> = lines[1].split('\t').collect();
    assert_eq!(cols.len(), 10);

    let wall: f64 = cols[0].parse().expect("wall");
    let max_rss: f64 = cols[2].parse().expect("max_rss");
    let max_pss: f64 = cols[5].parse().expect("max_pss");
    let io_out: f64 = cols[7].parse().expect("io_out");
    let mean_load: f64 = cols[8].parse().expect("mean_load");
    let cpu_time: f64 = cols[9].parse().expect("cpu_time");

    assert!(wall >= 1.5, "wall {wall}s should be at least the busy-loop duration");
    assert!(wall < 30.0, "wall {wall}s unexpectedly long");

    // The 50 MiB allocation should dominate RSS, but allow ample headroom
    // for the Python interpreter (~20 MiB) and allocator slop.
    assert!(max_rss >= 35.0, "max_rss {max_rss} MiB should reflect the 50 MiB allocation");
    assert!(max_rss < 1024.0, "max_rss {max_rss} MiB unexpectedly large");

    // PSS is real on Linux and mirrors USS on macOS — both must be
    // measured for any running process.
    assert!(max_pss > 0.0, "max_pss should be non-zero");

    // One core busy-looping for 1.5 s.
    assert!(cpu_time >= 1.0, "cpu_time {cpu_time}s should reflect the busy-loop");
    assert!(mean_load >= 30.0, "mean_load {mean_load}% should reflect a single hot core");

    // Disk-write accounting differs by platform — see the README's
    // platform-notes table for the underlying syscall.
    #[cfg(target_os = "linux")]
    {
        // `/proc/<pid>/io`'s `write_bytes` counts every byte the process
        // passed to `write()`, regardless of page-cache absorption, so
        // the 16 MiB write is fully visible.
        assert!(io_out >= 12.0, "linux io_out {io_out} MiB should reflect the 16 MiB write");
    }
    #[cfg(target_os = "macos")]
    {
        // `proc_pid_rusage::ri_diskio_byteswritten` counts only physical
        // disk I/O. `fsync()` forces the flush, but the sampling interval
        // may end before the flush completes and small writes can be
        // coalesced. We assert non-trivial (rather than ≥ 12 MiB) to
        // keep the test stable against APFS / runner I/O scheduling.
        assert!(io_out >= 1.0, "macos io_out {io_out} MiB should be non-trivial");
    }
}

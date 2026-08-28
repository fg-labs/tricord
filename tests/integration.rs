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

fn run_bench(
    out: &std::path::Path,
    format: &str,
    extra_args: &[&str],
    command: &[&str],
) -> std::process::Output {
    // Tighten the sampling interval well below the default 0.5 s so a sub-
    // second workload still yields multiple samples even on a heavily-loaded
    // CI runner. Without this, `sleep 0.6` could land in a single 0.5 s
    // window that the sampler thread misses if it's late waking up — which
    // is what caused `json_output_round_trips_to_object` to flake on
    // macos-latest. Callers can override via `extra_args` (clap takes the
    // last value).
    let mut cmd = Command::new(binary());
    cmd.args(["--interval", "0.1"]);
    cmd.arg("--out").arg(out).arg("--format").arg(format);
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.arg("--");
    for piece in command {
        cmd.arg(piece);
    }
    cmd.output().expect("spawn tricorder")
}

#[test]
fn tsv_output_for_short_lived_command_has_correct_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &[], &["sh", "-c", "sleep 0.7"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).expect("read tsv");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "expected header + 1 data row, got: {text:?}");
    // Default mode (no --snakemake): full schema, 10 Snakemake columns
    // followed by every tricord-added column (page faults, ctx switches, …).
    // The expected column count is derived from the header so future
    // metric PRs only need to update the expected header string, not also
    // the column count.
    let expected_cols = lines[0].split('\t').count();
    let cols: Vec<&str> = lines[1].split('\t').collect();
    assert_eq!(cols.len(), expected_cols);
    assert!(lines[0].starts_with(
        "s\th:m:s\tmax_rss\tmax_vms\tmax_uss\tmax_pss\tio_in\tio_out\tmean_load\tcpu_time",
    ));
    assert!(lines[0].contains("\tmajor_page_faults\tminor_page_faults"));
    assert!(lines[0].contains("\tvoluntary_ctx_switches\tinvoluntary_ctx_switches"));
    let wall: f64 = cols[0].parse().expect("wall time parses");
    assert!(wall >= 0.5, "wall time {wall} should be at least 0.5s");
    assert!(wall < 5.0, "wall time {wall} should be under 5s");
    assert!(matches!(cols[1], "0:00:00" | "0:00:01"), "unexpected h:m:s {:?}", cols[1]);
}

#[test]
fn json_output_round_trips_to_object() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.json");
    let result = run_bench(&out, "json", &[], &["sh", "-c", "sleep 0.6"]);
    assert!(result.status.success());

    let text = std::fs::read_to_string(&out).unwrap();
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid json");
    assert!(value.is_object());
    assert!(value["running_time"].as_f64().expect("running_time number") >= 0.5);
    assert_eq!(value["data_collected"], true);
}

#[test]
fn json_pretty_output_round_trips_to_object() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.json");
    let result = run_bench(&out, "json_pretty", &[], &["sh", "-c", "sleep 0.6"]);
    assert!(result.status.success());

    let text = std::fs::read_to_string(&out).unwrap();
    // One line per field (21 in full mode) plus the two brace lines.
    assert_eq!(text.trim().lines().count(), 23, "pretty output: {text}");
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid json");
    assert!(value.is_object());
    assert!(value["running_time"].as_f64().expect("running_time number") >= 0.5);
    assert_eq!(value["data_collected"], true);
}

#[test]
fn nested_output_directory_is_created() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("does/not/exist/yet/timing.tsv");
    let result = run_bench(&out, "tsv", &[], &["sh", "-c", "true"]);
    assert!(result.status.success());
    assert!(out.exists());
}

#[test]
fn exit_code_is_passed_through() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &[], &["sh", "-c", "exit 42"]);
    assert_eq!(result.status.code(), Some(42));
    // The benchmark file should still exist even when the child failed.
    assert!(out.exists());
}

#[test]
fn instant_exit_yields_na_row() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &[], &["sh", "-c", "true"]);
    assert!(result.status.success());

    let text = std::fs::read_to_string(&out).unwrap();
    let row = text.lines().nth(1).expect("data row");
    let cols: Vec<&str> = row.split('\t').collect();
    // Either we caught a sample (numbers) or we didn't (NA placeholders);
    // both are valid for an essentially-instant child. Just verify the row
    // is well-formed and matches the header's column count.
    let header_cols = text.lines().next().expect("header").split('\t').count();
    assert_eq!(cols.len(), header_cols);
    let wall: f64 = cols[0].parse().expect("wall time parses");
    assert!(wall >= 0.0);
}

#[test]
fn trace_flag_writes_per_tick_tsv() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let trace = tmp.path().join("trace.tsv");
    let trace_str = trace.to_str().expect("utf8 trace path");

    let result = run_bench(&out, "tsv", &["--trace", trace_str], &["sh", "-c", "sleep 0.6"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    // Aggregate file is unaffected by --trace.
    let agg = std::fs::read_to_string(&out).expect("aggregate tsv");
    assert_eq!(agg.lines().count(), 2, "aggregate should still be header + 1 row");

    let trace_text = std::fs::read_to_string(&trace).expect("trace tsv");
    let lines: Vec<&str> = trace_text.lines().collect();
    let trace_header = lines[0];
    // Spot-check the columns by name rather than locking the exact header
    // string; future metric PRs append columns without re-recording here.
    for col in [
        "s",
        "rss",
        "n_procs",
        "major_page_faults",
        "minor_page_faults",
        "voluntary_ctx_switches",
        "involuntary_ctx_switches",
        "n_threads",
    ] {
        assert!(
            trace_header.split('\t').any(|c| c == col),
            "trace header missing {col}: {trace_header}",
        );
    }
    assert!(
        lines.len() >= 3,
        "expected header + multiple ticks, got {}: {trace_text:?}",
        lines.len()
    );

    let expected_cols = lines[0].split('\t').count();
    let mut last_elapsed = -1.0_f64;
    for row in &lines[1..] {
        let cols: Vec<&str> = row.split('\t').collect();
        assert_eq!(cols.len(), expected_cols, "row has wrong column count: {row:?}");
        let elapsed: f64 = cols[0].parse().expect("elapsed parses");
        assert!(elapsed >= last_elapsed, "elapsed should be monotonic");
        last_elapsed = elapsed;
    }
}

#[test]
fn export_markdown_writes_table_alongside_tsv() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let md = tmp.path().join("timing.md");
    let md_str = md.to_str().expect("utf8 md path");

    let result = run_bench(&out, "tsv", &["--export-markdown", md_str], &["sh", "-c", "sleep 0.4"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    // The primary TSV is unaffected by --export-markdown.
    let agg = std::fs::read_to_string(&out).expect("aggregate tsv");
    assert_eq!(agg.lines().count(), 2, "aggregate should still be header + 1 row");

    // Markdown table in full mode: header + alignment + one row per column
    // in TSV_HEADER_FULL. Derive the expected line count from the TSV so
    // metric PRs only need to update one source of truth.
    let md_text = std::fs::read_to_string(&md).expect("markdown file");
    let agg_text = std::fs::read_to_string(&out).expect("aggregate tsv");
    let expected_metric_rows = agg_text.lines().next().expect("agg header").split('\t').count();
    let lines: Vec<&str> = md_text.lines().collect();
    assert_eq!(
        lines.len(),
        2 + expected_metric_rows,
        "expected header + alignment + {expected_metric_rows} metric rows: {md_text:?}",
    );
    assert!(lines[0].starts_with("| metric"), "unexpected header: {}", lines[0]);
    assert!(lines[1].starts_with("|:") && lines[1].ends_with(":|"), "alignment row: {}", lines[1]);
    assert!(lines.iter().any(|l| l.contains("| s ")), "no `s` row: {md_text:?}");
    assert!(lines.iter().any(|l| l.contains("| cpu_time ")), "no `cpu_time` row: {md_text:?}");
    assert!(
        lines.iter().any(|l| l.contains("| major_page_faults ")),
        "no page-fault row in full-mode Markdown: {md_text:?}",
    );
}

#[test]
fn export_markdown_and_trace_can_be_combined() {
    // Lock down the "can be combined" contract — every sidecar path is
    // independent of the others and the primary output.
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let md = tmp.path().join("timing.md");
    let trace = tmp.path().join("trace.tsv");
    let result = run_bench(
        &out,
        "tsv",
        &[
            "--export-markdown",
            md.to_str().expect("utf8 md path"),
            "--trace",
            trace.to_str().expect("utf8 trace path"),
        ],
        &["sh", "-c", "sleep 0.4"],
    );
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));
    let md_text = std::fs::read_to_string(&md).expect("markdown file");
    assert!(md_text.lines().next().is_some_and(|l| l.starts_with("| metric")));
    let trace_text = std::fs::read_to_string(&trace).expect("trace file");
    assert!(trace_text.lines().next().is_some_and(|l| l.starts_with("s\trss")));
    let agg_text = std::fs::read_to_string(&out).expect("agg file");
    assert_eq!(agg_text.lines().count(), 2, "aggregate should still be header + 1 row");
}

#[test]
fn export_markdown_same_path_as_out_is_rejected() {
    // Pointing two output flags at the same path would silently clobber.
    // tricorder should refuse before running the child, surfacing both the
    // conflicting path and the involved flag names.
    let tmp = tempfile::tempdir().unwrap();
    let shared = tmp.path().join("clash.tsv");
    let shared_str = shared.to_str().expect("utf8");
    let result =
        run_bench(&shared, "tsv", &["--export-markdown", shared_str], &["sh", "-c", "true"]);
    assert!(!result.status.success(), "tricorder should refuse colliding paths");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("--out") && stderr.contains("--export-markdown"), "stderr: {stderr}");
    assert!(stderr.contains("clash.tsv"), "stderr should name the offending path: {stderr}");
}

#[test]
fn export_markdown_same_path_as_trace_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let shared = tmp.path().join("clash.tsv");
    let shared_str = shared.to_str().expect("utf8");
    let result = run_bench(
        &out,
        "tsv",
        &["--trace", shared_str, "--export-markdown", shared_str],
        &["sh", "-c", "true"],
    );
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("--trace") && stderr.contains("--export-markdown"), "stderr: {stderr}");
}

#[test]
fn trace_same_path_as_out_is_rejected() {
    // Sibling fix: --out and --trace can also silently clobber today;
    // close that with the same check.
    let tmp = tempfile::tempdir().unwrap();
    let shared = tmp.path().join("clash.tsv");
    let shared_str = shared.to_str().expect("utf8");
    let result = run_bench(&shared, "tsv", &["--trace", shared_str], &["sh", "-c", "true"]);
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("--out") && stderr.contains("--trace"), "stderr: {stderr}");
}

#[test]
fn export_markdown_is_independent_of_primary_format() {
    // The Markdown sidecar must work with any `--format` value for the
    // primary `--out` file, not just TSV. Catches accidental coupling.
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.json");
    let md = tmp.path().join("timing.md");
    let md_str = md.to_str().expect("utf8 md path");

    let result = run_bench(&out, "json", &["--export-markdown", md_str], &["sh", "-c", "true"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let json_text = std::fs::read_to_string(&out).unwrap();
    let value: serde_json::Value = serde_json::from_str(json_text.trim()).expect("valid json");
    assert!(value.is_object(), "primary output should still be JSON: {json_text}");

    let md_text = std::fs::read_to_string(&md).expect("markdown file");
    assert!(md_text.lines().next().is_some_and(|l| l.starts_with("| metric")));
}

#[test]
fn export_markdown_parent_directory_is_created() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let md = tmp.path().join("nested/dir/timing.md");
    let md_str = md.to_str().expect("utf8 md path");

    let result = run_bench(&out, "tsv", &["--export-markdown", md_str], &["sh", "-c", "true"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));
    assert!(md.exists(), "markdown file should be created in nested directory");
}

#[test]
fn trace_parent_directory_is_created() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let trace = tmp.path().join("nested/dir/trace.tsv");
    let trace_str = trace.to_str().expect("utf8 trace path");

    let result = run_bench(&out, "tsv", &["--trace", trace_str], &["sh", "-c", "sleep 0.3"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));
    assert!(trace.exists(), "trace file should be created in nested directory");
}

/// Aggregate TSV picks up two new appended columns (`major_page_faults`,
/// `minor_page_faults`). A workload that touches a large allocation should
/// drive at least one of them above zero on Linux (`/proc/<pid>/stat`); on
/// macOS only `major_page_faults` is populated (from `proc_pid_rusage`).
#[test]
fn page_faults_appear_in_aggregate_tsv() {
    if !python3_available() {
        assert!(std::env::var_os("CI").is_none(), "python3 not on PATH in CI");
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    // 30 MiB allocation + page touch generates plenty of minor faults
    // (and on a fresh process, some major faults from text/lib paging).
    // The trailing sleep guarantees at least one sampling tick fires so
    // `data_collected` is true and cells render as numbers / `-` rather
    // than the all-`NA` short-lived-process placeholder row.
    let workload = "import time\n\
                    buf = bytearray(30 * 1024 * 1024)\n\
                    for i in range(0, len(buf), 4096): buf[i] = i & 0xff\n\
                    time.sleep(0.4)";
    let result = run_bench(&out, "tsv", &[], &["python3", "-c", workload]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).expect("aggregate tsv");
    let header = text.lines().next().expect("header");
    assert!(header.contains("\tmajor_page_faults\tminor_page_faults"), "header: {header}");
    let header_cols: Vec<&str> = header.split('\t').collect();
    let cols: Vec<&str> = text.lines().nth(1).expect("data row").split('\t').collect();
    assert_eq!(cols.len(), header_cols.len(), "row col count mismatches header: {cols:?}");

    // Look up by name so column-order changes don't silently mis-target.
    let pos = |name: &str| {
        header_cols.iter().position(|c| *c == name).unwrap_or_else(|| panic!("col {name}"))
    };
    let major = cols[pos("major_page_faults")];
    let minor = cols[pos("minor_page_faults")];
    // major may legitimately be "0" on a warm-cache run; minor on Linux must
    // be > 0 after touching 30 MiB across 4 KiB pages. macOS exposes only
    // major via rusage, so minor is "-" on macOS.
    #[cfg(target_os = "linux")]
    {
        let minor_val: u64 = minor.parse().unwrap_or_else(|_| panic!("minor parses: {minor}"));
        assert!(minor_val > 100, "linux minor_page_faults {minor_val} should be high");
        let _ = major.parse::<u64>().expect("major parses on linux");
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(minor, "-", "macos minor_page_faults should be `-`, got: {minor}");
        let _ = major.parse::<u64>().expect("major parses on macos");
    }
}

#[test]
fn snakemake_strict_mode_strips_page_fault_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &["--snakemake"], &["sh", "-c", "sleep 0.4"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).unwrap();
    let header = text.lines().next().expect("header");
    assert_eq!(
        header, "s\th:m:s\tmax_rss\tmax_vms\tmax_uss\tmax_pss\tio_in\tio_out\tmean_load\tcpu_time",
        "snakemake-strict header must be the original 10 columns",
    );
    let cols: Vec<&str> = text.lines().nth(1).expect("data row").split('\t').collect();
    assert_eq!(cols.len(), 10, "strict mode row must have 10 columns: {cols:?}");
}

#[test]
fn snakemake_strict_mode_strips_page_faults_from_json() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.json");
    let result = run_bench(&out, "json", &["--snakemake"], &["sh", "-c", "sleep 0.4"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).unwrap();
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid json");
    let obj = value.as_object().expect("object");
    assert!(
        !obj.contains_key("major_page_faults"),
        "strict-mode JSON must omit major_page_faults: {value}"
    );
    assert!(
        !obj.contains_key("minor_page_faults"),
        "strict-mode JSON must omit minor_page_faults: {value}"
    );
    assert!(obj.contains_key("running_time"));
    assert!(obj.contains_key("data_collected"));
}

#[test]
fn snakemake_strict_mode_strips_page_faults_from_markdown() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let md = tmp.path().join("timing.md");
    let md_str = md.to_str().expect("utf8");
    let result = run_bench(
        &out,
        "tsv",
        &["--snakemake", "--export-markdown", md_str],
        &["sh", "-c", "sleep 0.4"],
    );
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let md_text = std::fs::read_to_string(&md).unwrap();
    assert!(
        !md_text.contains("major_page_faults"),
        "strict-mode Markdown must omit major_page_faults: {md_text}"
    );
    assert!(
        !md_text.contains("minor_page_faults"),
        "strict-mode Markdown must omit minor_page_faults: {md_text}"
    );
    // Still has the original 10 rows + header + alignment row.
    assert_eq!(md_text.lines().count(), 12, "strict Markdown should be 12 lines: {md_text:?}");
}

#[test]
fn snakemake_strict_mode_does_not_affect_trace_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let trace = tmp.path().join("trace.tsv");
    let trace_str = trace.to_str().expect("utf8");
    let result =
        run_bench(&out, "tsv", &["--snakemake", "--trace", trace_str], &["sh", "-c", "sleep 0.4"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    // Aggregate TSV is strict (10 cols).
    let agg_text = std::fs::read_to_string(&out).unwrap();
    let agg_cols: Vec<&str> = agg_text.lines().nth(1).unwrap().split('\t').collect();
    assert_eq!(agg_cols.len(), 10);

    // Trace TSV is "ours" — strict mode does not touch it. Header must
    // include the new page-fault columns.
    let trace_text = std::fs::read_to_string(&trace).expect("trace");
    let trace_header = trace_text.lines().next().expect("trace header");
    assert!(
        trace_header.contains("major_page_faults"),
        "trace header must include page faults regardless of --snakemake: {trace_header}",
    );
}

/// Voluntary + involuntary context-switch columns are appended in full mode.
/// On Linux both come from `/proc/<pid>/status`; on macOS the kernel does
/// not expose them via `proc_pid_rusage`, so both columns render as `-`.
#[test]
fn context_switches_appear_in_aggregate_tsv() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    // Several short sleeps generate many voluntary context switches on Linux
    // (each sleep yields the CPU). The wall time is long enough to guarantee
    // a tick fires.
    let result =
        run_bench(&out, "tsv", &[], &["sh", "-c", "for i in 1 2 3 4 5 6 7 8; do sleep 0.05; done"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).expect("aggregate tsv");
    let header = text.lines().next().expect("header");
    assert!(
        header.contains("\tvoluntary_ctx_switches\tinvoluntary_ctx_switches"),
        "header missing ctx-switch cols: {header}",
    );
    let header_cols: Vec<&str> = header.split('\t').collect();
    let cols: Vec<&str> = text.lines().nth(1).expect("data row").split('\t').collect();
    assert_eq!(cols.len(), header_cols.len(), "row col count mismatches header: {cols:?}");

    // Look up by name so column-order changes don't silently mis-target.
    let pos = |name: &str| {
        header_cols.iter().position(|c| *c == name).unwrap_or_else(|| panic!("col {name}"))
    };
    let voluntary = cols[pos("voluntary_ctx_switches")];
    let involuntary = cols[pos("involuntary_ctx_switches")];

    #[cfg(target_os = "linux")]
    {
        let v: u64 = voluntary.parse().unwrap_or_else(|_| panic!("vol parses: {voluntary}"));
        assert!(v > 0, "linux voluntary_ctx_switches {v} should be > 0 after 8 sleeps");
        let _ = involuntary.parse::<u64>().expect("involuntary parses on linux");
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(voluntary, "-", "macos voluntary_ctx_switches should be `-`");
        assert_eq!(involuntary, "-", "macos involuntary_ctx_switches should be `-`");
    }
}

#[test]
fn snakemake_strict_mode_strips_ctx_switch_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &["--snakemake"], &["sh", "-c", "sleep 0.4"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).unwrap();
    let header = text.lines().next().unwrap();
    assert!(!header.contains("voluntary_ctx_switches"), "strict header: {header}");
    assert!(!header.contains("involuntary_ctx_switches"));
    let cols: Vec<&str> = text.lines().nth(1).unwrap().split('\t').collect();
    assert_eq!(cols.len(), 10, "strict mode row must still be 10 cols: {cols:?}");
}

/// Peak thread + process count appear in the full-mode TSV. Both platforms
/// populate thread counts (procfs and TaskInfo); a workload with multiple
/// processes drives `peak_n_procs` above 1.
#[test]
fn peak_thread_and_proc_counts_appear_in_aggregate_tsv() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    // Two backgrounded sleeps + a foreground sleep ⇒ at least 3 procs in
    // the tree concurrently for ~0.3s, easily caught by interval=0.1.
    let result =
        run_bench(&out, "tsv", &[], &["sh", "-c", "sleep 0.3 & sleep 0.3 & sleep 0.3 ; wait"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).expect("aggregate tsv");
    let header = text.lines().next().expect("header");
    let header_cols: Vec<&str> = header.split('\t').collect();
    let cols: Vec<&str> = text.lines().nth(1).expect("data row").split('\t').collect();
    assert_eq!(cols.len(), header_cols.len(), "header/row mismatch");

    let pos = |name: &str| {
        header_cols.iter().position(|c| *c == name).unwrap_or_else(|| panic!("col {name}"))
    };
    let peak_n_procs: u64 = cols[pos("peak_n_procs")].parse().expect("peak_n_procs parses as u64");
    assert!(peak_n_procs >= 3, "peak_n_procs {peak_n_procs} should reflect 3 concurrent procs");
    let peak_n_threads: u64 =
        cols[pos("peak_n_threads")].parse().expect("peak_n_threads parses as u64");
    assert!(peak_n_threads >= peak_n_procs, "peak_n_threads must be >= peak_n_procs");
}

#[test]
fn snakemake_strict_mode_strips_peak_n_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &["--snakemake"], &["sh", "-c", "sleep 0.3"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let header = std::fs::read_to_string(&out).unwrap().lines().next().unwrap().to_string();
    assert!(!header.contains("peak_n_threads"), "strict header: {header}");
    assert!(!header.contains("peak_n_procs"), "strict header: {header}");
}

#[test]
fn trace_includes_n_threads_per_tick() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let trace = tmp.path().join("trace.tsv");
    let trace_str = trace.to_str().expect("utf8");
    let result = run_bench(&out, "tsv", &["--trace", trace_str], &["sh", "-c", "sleep 0.4"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let trace_text = std::fs::read_to_string(&trace).expect("trace tsv");
    let header = trace_text.lines().next().expect("trace header");
    assert!(header.contains("\tn_threads"), "trace header missing n_threads: {header}");
}

/// System 1-minute load average is sampled at start and end of the run
/// and added as two aggregate columns. Trace TSV is intentionally not
/// touched — loadavg is already a moving average so per-tick samples
/// would mostly be noise on the same kernel-side smoothing.
#[test]
fn loadavg_start_and_end_appear_in_aggregate_tsv() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &[], &["sh", "-c", "sleep 0.3"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).expect("aggregate tsv");
    let header = text.lines().next().expect("header");
    let header_cols: Vec<&str> = header.split('\t').collect();
    let cols: Vec<&str> = text.lines().nth(1).expect("data row").split('\t').collect();
    let pos = |name: &str| {
        header_cols.iter().position(|c| *c == name).unwrap_or_else(|| panic!("col {name}"))
    };
    let start: f64 =
        cols[pos("loadavg_1m_start")].parse().expect("loadavg_1m_start parses as float");
    let end: f64 = cols[pos("loadavg_1m_end")].parse().expect("loadavg_1m_end parses as float");
    // Loadavg is non-negative; on idle laptops it can be near zero, so >= 0 is
    // the safe lower bound. Upper bound is generous to absorb any contention.
    assert!(start >= 0.0, "loadavg_1m_start {start} must be >= 0");
    assert!(end >= 0.0, "loadavg_1m_end {end} must be >= 0");
    assert!(start < 10_000.0 && end < 10_000.0, "loadavg unexpectedly huge");
}

#[test]
fn snakemake_strict_mode_strips_loadavg_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &["--snakemake"], &["sh", "-c", "sleep 0.3"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let header = std::fs::read_to_string(&out).unwrap().lines().next().unwrap().to_string();
    assert!(!header.contains("loadavg_1m_start"), "strict header: {header}");
    assert!(!header.contains("loadavg_1m_end"), "strict header: {header}");
}

#[test]
fn loadavg_is_not_added_to_trace_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let trace = tmp.path().join("trace.tsv");
    let trace_str = trace.to_str().unwrap();
    let result = run_bench(&out, "tsv", &["--trace", trace_str], &["sh", "-c", "sleep 0.4"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let trace_header = std::fs::read_to_string(&trace).unwrap().lines().next().unwrap().to_string();
    assert!(
        !trace_header.contains("loadavg"),
        "trace TSV must not include loadavg (it's already a smoothed avg): {trace_header}",
    );
}

/// Peak swap usage shows up in the aggregate (MiB, summed across the
/// process tree). The per-tick trace adds a `swap` column with the
/// instantaneous summed value. macOS has no public per-process swap API,
/// so both columns render `-` there.
#[test]
fn swap_columns_appear_in_aggregate_and_trace() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let trace = tmp.path().join("trace.tsv");
    let trace_str = trace.to_str().unwrap();
    let result = run_bench(&out, "tsv", &["--trace", trace_str], &["sh", "-c", "sleep 0.4"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let agg_text = std::fs::read_to_string(&out).expect("aggregate tsv");
    let agg_header = agg_text.lines().next().expect("header");
    let agg_cols: Vec<&str> = agg_header.split('\t').collect();
    let data: Vec<&str> = agg_text.lines().nth(1).expect("data row").split('\t').collect();
    let pos = |name: &str| {
        agg_cols.iter().position(|c| *c == name).unwrap_or_else(|| panic!("agg col {name}"))
    };
    let max_swap = data[pos("max_swap")];
    #[cfg(target_os = "linux")]
    {
        // Linux has VmSwap in /proc/<pid>/status; for a sleep it's typically
        // 0.00 MiB but may be any non-negative float.
        let v: f64 = max_swap.parse().unwrap_or_else(|_| panic!("max_swap parses: {max_swap}"));
        assert!(v >= 0.0, "linux max_swap {v} must be >= 0");
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(max_swap, "-", "macos max_swap should be `-` (no per-process swap API)");
    }

    let trace_text = std::fs::read_to_string(&trace).expect("trace tsv");
    let trace_header = trace_text.lines().next().expect("trace header");
    assert!(
        trace_header.split('\t').any(|c| c == "swap"),
        "trace header missing swap column: {trace_header}",
    );
}

#[test]
fn snakemake_strict_mode_strips_swap_column() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &["--snakemake"], &["sh", "-c", "sleep 0.3"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let header = std::fs::read_to_string(&out).unwrap().lines().next().unwrap().to_string();
    assert!(!header.contains("max_swap"), "strict header: {header}");
}

/// System page-cache size (`Cached` in `/proc/meminfo`) is sampled at start
/// and end of the run and added as two aggregate columns, mirroring
/// `loadavg_1m_start`/`loadavg_1m_end`. macOS has no equivalent of Linux's
/// `Cached` accounting, so both columns render `-` there.
#[test]
fn page_cache_start_and_end_appear_in_aggregate_tsv() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &[], &["sh", "-c", "sleep 0.3"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).expect("aggregate tsv");
    let header = text.lines().next().expect("header");
    let header_cols: Vec<&str> = header.split('\t').collect();
    let cols: Vec<&str> = text.lines().nth(1).expect("data row").split('\t').collect();
    let pos = |name: &str| {
        header_cols.iter().position(|c| *c == name).unwrap_or_else(|| panic!("col {name}"))
    };
    let start = cols[pos("page_cache_start")];
    let end = cols[pos("page_cache_end")];
    #[cfg(target_os = "linux")]
    {
        let start_v: f64 = start.parse().unwrap_or_else(|_| panic!("page_cache_start: {start}"));
        let end_v: f64 = end.parse().unwrap_or_else(|_| panic!("page_cache_end: {end}"));
        assert!(start_v >= 0.0, "page_cache_start {start_v} must be >= 0 on any real Linux host");
        assert!(end_v >= 0.0, "page_cache_end {end_v} must be >= 0 on any real Linux host");
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(start, "-", "macos page_cache_start should be `-` (no Cached equivalent)");
        assert_eq!(end, "-", "macos page_cache_end should be `-` (no Cached equivalent)");
    }
}

#[test]
fn snakemake_strict_mode_strips_page_cache_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let result = run_bench(&out, "tsv", &["--snakemake"], &["sh", "-c", "sleep 0.3"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let header = std::fs::read_to_string(&out).unwrap().lines().next().unwrap().to_string();
    assert!(!header.contains("page_cache_start"), "strict header: {header}");
    assert!(!header.contains("page_cache_end"), "strict header: {header}");
}

#[test]
fn page_cache_is_not_added_to_trace_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("timing.tsv");
    let trace = tmp.path().join("trace.tsv");
    let trace_str = trace.to_str().unwrap();
    let result = run_bench(&out, "tsv", &["--trace", trace_str], &["sh", "-c", "sleep 0.4"]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let trace_header = std::fs::read_to_string(&trace).unwrap().lines().next().unwrap().to_string();
    assert!(
        !trace_header.contains("page_cache"),
        "trace TSV must not include page_cache (system-wide, not a per-process/per-tick \
         quantity the way rss/io/cpu are): {trace_header}",
    );
}

fn python3_available() -> bool {
    Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success())
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

    let result = run_bench(&out, "tsv", &[], &["python3", "-c", &workload]);
    assert!(result.status.success(), "stderr: {}", String::from_utf8_lossy(&result.stderr));

    let text = std::fs::read_to_string(&out).expect("read tsv");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "expected header + 1 data row, got: {text:?}");
    let cols: Vec<&str> = lines[1].split('\t').collect();
    // Default (full) mode: 10 Snakemake columns + every tricord-added
    // column. Header drives the expected count so future metric PRs need
    // not edit this assertion.
    assert_eq!(cols.len(), lines[0].split('\t').count());

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

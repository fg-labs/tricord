//! End-to-end orchestration: spawn the child, run the sampler, write output.

#[cfg(test)]
mod eintr_tests;

use std::{
    io::{self, IsTerminal},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use crate::{
    format::{self, OutputFormat},
    platform,
    record::{BenchmarkRecord, SchemaMode},
    sampler::{SamplerHandle, SamplerOptions},
    signals::SignalForwarder,
};

/// Outcome of a single `tricorder` invocation.
#[derive(Debug)]
pub struct RunOutcome {
    /// Aggregated resource record from sampling and child-exit accounting.
    pub record: BenchmarkRecord,
    /// Exit status of the spawned child.
    pub status: ExitStatus,
}

impl RunOutcome {
    /// POSIX-style exit code: child's exit code if it exited normally,
    /// otherwise `128 + signal` if it was killed by a signal.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        exit_code_for(self.status)
    }
}

/// Options controlling [`run_command`].
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Sampling interval.
    pub interval: Duration,
    /// Output file path; serialized with `format`.
    pub output_path: Box<Path>,
    /// Output file format.
    pub format: OutputFormat,
    /// If true, write a one-line summary to stderr after the child exits.
    /// If false but stderr is a terminal, still print the summary.
    pub force_summary: bool,
    /// Optional path for a per-tick TSV trace. `None` disables the trace.
    pub trace_path: Option<Box<Path>>,
    /// Optional path for an additional Markdown table of the aggregate
    /// record (`--export-markdown`). `None` disables it. Written alongside
    /// `output_path`, not in place of it.
    pub markdown_path: Option<Box<Path>>,
    /// Which schema the aggregate-record formatters (TSV, JSON, Markdown)
    /// should emit. `SnakemakeStrict` strips `tricord`-added columns;
    /// `Full` (the default) keeps them. The per-tick trace ignores this.
    pub schema_mode: SchemaMode,
}

/// Spawn `command` (with `args`), benchmark its process tree, and write the
/// aggregated record to disk in the configured format.
///
/// The command is spawned in its own process group so that signals received
/// by `tricorder` itself (`SIGINT`, `SIGTERM`, `SIGHUP`) can be forwarded
/// deliberately rather than racing the kernel's terminal-driver delivery.
///
/// # Errors
/// Returns any I/O error that prevented spawning the child or writing the
/// output file. Errors from the child itself surface as a non-zero
/// [`RunOutcome::exit_code`], not as an `Err`.
pub fn run_command(command: &str, args: &[String], options: &RunOptions) -> io::Result<RunOutcome> {
    run_command_with_post_spawn(command, args, options, || Ok(()))
}

/// Run a command and release the parent's inherited stdin/stdout pipe ends.
///
/// After a successful spawn, replace pipe- or socket-backed stdin/stdout in
/// the calling process with `/dev/null`. The child retains the original
/// streams, so downstream readers see EOF when the child closes stdout and
/// upstream writers see a broken pipe when it closes stdin. Regular files,
/// terminals, and stderr are left unchanged.
///
/// This is a **process-wide, permanent** change, affecting all threads and
/// subsequent commands. Only opt in when the caller no longer needs these
/// streams. Rust's stdout buffer is flushed before redirection; callers must
/// flush any independently owned buffers and stop concurrent access to these
/// descriptors before calling. Pipe-backed `/dev/stdin`, `/dev/stdout`, and
/// equivalent benchmark output paths are rejected before spawning. Other copies
/// of the same pipe (including stderr) can still delay EOF or broken pipes.
/// Use [`run_command`] to leave the caller's standard streams unchanged.
///
/// # Errors
/// Returns the same errors as [`run_command`], an invalid output-path error,
/// or an error flushing or releasing the parent's streams. If parent-side
/// initialization fails, the child's process group is killed and the direct
/// child reaped. Stream changes already made
/// before an error are not rolled back.
pub fn run_command_releasing_pipes(
    command: &str,
    args: &[String],
    options: &RunOptions,
) -> io::Result<RunOutcome> {
    crate::stdio::validate_output_paths(options)?;
    run_command_with_post_spawn(command, args, options, crate::stdio::release_pipes)
}

/// Shared orchestration; the hook only runs after a successful spawn.
fn run_command_with_post_spawn<F>(
    command: &str,
    args: &[String],
    options: &RunOptions,
    post_spawn: F,
) -> io::Result<RunOutcome>
where
    F: FnOnce() -> io::Result<()>,
{
    // Reject distinct flags pointing at the same path *before* spawning
    // anything — otherwise the second writer silently clobbers the first
    // and the user loses data with no warning.
    validate_output_paths(options)?;

    // Snapshot system loadavg and page-cache size before the child spawns
    // so the "start" values reflect state entering the run (before our
    // child has had time to contribute meaningfully to the loadavg EMA, or
    // to perturb the page cache with its own reads/writes) — not state the
    // child has already touched.
    let loadavg_start = platform::read_loadavg_1m();
    let page_cache_start = platform::read_page_cache_mb();

    let mut cmd = Command::new(command);
    cmd.args(args);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    cmd.process_group(0);
    let started_at = Instant::now();
    let mut child = cmd.spawn()?;

    #[allow(clippy::cast_possible_wrap)]
    let child_pid = child.id() as i32;

    // Forward signals before flushing/releasing streams: a blocked stdout
    // flush must not leave the child unprotected in its separate process group.
    let signals = match SignalForwarder::install(child_pid).and_then(|signals| {
        post_spawn()?;
        Ok(signals)
    }) {
        Ok(signals) => signals,
        Err(err) => {
            // Dropping Child does not terminate or reap it. Initialization
            // failures must not leave an unmonitored workload running.
            crate::signals::forward_to_group(child_pid, libc::SIGKILL);
            // The direct child may have moved to another process group.
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };
    let sampler = SamplerHandle::spawn(
        child_pid,
        SamplerOptions { interval: options.interval, trace_path: options.trace_path.clone() },
    );

    let waited = wait_with_usage(&mut child);
    let running_time = started_at.elapsed();
    let mut record = sampler.stop(running_time);
    let (status, usage) = waited?;
    usage.reconcile(&mut record);
    let loadavg_end = platform::read_loadavg_1m();
    let page_cache_end = platform::read_page_cache_mb();
    record.loadavg_1m_start = loadavg_start;
    record.loadavg_1m_end = loadavg_end;
    record.page_cache_start = page_cache_start;
    record.page_cache_end = page_cache_end;
    drop(signals);

    format::write_to_path(&record, &options.output_path, options.format, options.schema_mode)?;
    if let Some(path) = options.markdown_path.as_deref() {
        format::write_markdown_to_path(&record, path, options.schema_mode)?;
    }

    if options.force_summary || io::stderr().is_terminal() {
        eprintln!("tricorder: {}", record.summary_line());
    }

    Ok(RunOutcome { record, status })
}

/// Exit accounting for the direct child and descendants whose usage propagated
/// through waits. This overlaps with, but need not cover, the sampled tree.
#[derive(Debug, Clone, Copy)]
struct ExitUsage {
    cpu_time: f64,
    max_rss_mib: f64,
}

impl ExitUsage {
    /// Use the larger overlapping total, never their sum. Neither source alone
    /// necessarily includes every descendant (for example, orphaned workers).
    fn reconcile(self, record: &mut BenchmarkRecord) {
        record.cpu_time = record.cpu_time.max(self.cpu_time);
        record.max_rss = Some(record.max_rss.unwrap_or(0.0).max(self.max_rss_mib));
        record.mean_load = if record.running_time > 0.0 {
            100.0 * record.cpu_time / record.running_time
        } else {
            0.0
        };
        record.data_collected = true;
    }

    // timeval field widths differ between Linux and macOS.
    #[allow(clippy::cast_lossless)]
    fn from_rusage(usage: &libc::rusage) -> Self {
        let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1_000_000.0;
        // Linux reports KiB, while macOS reports bytes. This is a per-process
        // high-water mark, not a simultaneous sum over the process tree.
        #[cfg(target_os = "linux")]
        let rss_divisor = 1024.0;
        #[cfg(target_os = "macos")]
        let rss_divisor = 1024.0 * 1024.0;
        Self {
            cpu_time: seconds(usage.ru_utime) + seconds(usage.ru_stime),
            max_rss_mib: usage.ru_maxrss as f64 / rss_divisor,
        }
    }
}

/// Reap only this child and obtain its final kernel counters. Process-wide
/// `RUSAGE_CHILDREN` deltas would mix concurrent library callers' children.
fn wait_with_usage(child: &mut Child) -> io::Result<(ExitStatus, ExitUsage)> {
    // Match Child::wait's stdin closure so a piped child cannot wait forever
    // for input held by its parent. No other code may reap this child.
    drop(child.stdin.take());
    let pid = child.id().cast_signed();
    loop {
        let mut status = 0;
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: pid identifies our direct child; status and usage point to
        // writable, correctly aligned storage. A successful wait4 initializes
        // both outputs. Waiting for a specific PID cannot reap another child.
        let result = unsafe { libc::wait4(pid, &raw mut status, 0, usage.as_mut_ptr()) };
        if result == pid {
            // SAFETY: wait4 succeeded and initialized the resource structure.
            let usage = unsafe { usage.assume_init() };
            // Child::drop does not reap; do not call Child::wait after this.
            return Ok((ExitStatus::from_raw(status), ExitUsage::from_rusage(&usage)));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
        #[cfg(test)]
        eintr_tests::observed_interruption();
    }
}

/// Reject any pair of output flags (`--out`, `--trace`, `--export-markdown`)
/// that resolves to the same on-disk path. Distinct flags must produce
/// distinct files; otherwise the writers race each other and the user
/// silently loses one of the outputs.
///
/// Paths are compared by their `Path` value as supplied — no canonicalization,
/// so trailing-slash quirks or `./` prefixes still distinguish files that
/// would otherwise hit the same inode. The check is intentionally
/// pessimistic: false positives are user-visible and recoverable
/// (change a flag), false negatives lose data.
fn validate_output_paths(options: &RunOptions) -> io::Result<()> {
    let mut configured: Vec<(&'static str, &Path)> = Vec::with_capacity(3);
    configured.push(("--out", &options.output_path));
    if let Some(p) = options.trace_path.as_deref() {
        configured.push(("--trace", p));
    }
    if let Some(p) = options.markdown_path.as_deref() {
        configured.push(("--export-markdown", p));
    }
    for i in 0..configured.len() {
        for j in (i + 1)..configured.len() {
            let (flag_a, path_a) = configured[i];
            let (flag_b, path_b) = configured[j];
            if path_a == path_b {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{flag_a} and {flag_b} point to the same path ({}); \
                         each output flag must use a distinct path",
                        path_a.display(),
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// POSIX-style exit code derivation: child's `code()` if it exited normally,
/// `128 + signal` if it was killed, `1` otherwise.
fn exit_code_for(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        code
    } else if let Some(sig) = status.signal() {
        128 + sig
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    #[test]
    fn exit_usage_reconciles_overlapping_counters_without_adding() {
        for (sampled_cpu, exit_cpu, sampled_rss, exit_rss, cpu, rss) in
            [(8.0, 10.0, 20.0, 16.0, 10.0, 20.0), (10.0, 8.0, 16.0, 20.0, 10.0, 20.0)]
        {
            let mut record = BenchmarkRecord {
                running_time: 2.0,
                cpu_time: sampled_cpu,
                max_rss: Some(sampled_rss),
                ..Default::default()
            };
            ExitUsage { cpu_time: exit_cpu, max_rss_mib: exit_rss }.reconcile(&mut record);
            assert!((record.cpu_time - cpu).abs() < 1e-9);
            assert!((record.mean_load - 500.0).abs() < 1e-9);
            assert_eq!(record.max_rss, Some(rss));
            assert!(record.data_collected);
        }
    }

    #[test]
    fn exit_only_record_preserves_unknown_metrics_in_all_formats() {
        let mut record = BenchmarkRecord { running_time: 1.0, ..Default::default() };
        ExitUsage { cpu_time: 0.25, max_rss_mib: 16.0 }.reconcile(&mut record);
        let tsv = record.to_tsv_row(SchemaMode::Full);
        let cols: Vec<_> = tsv.split('\t').collect();
        assert_eq!(&cols[2..10], &["16.00", "-", "-", "-", "-", "-", "25.00", "0.25"]);
        assert_eq!(cols[15], "-");
        let json: serde_json::Value =
            serde_json::from_str(&record.to_json(SchemaMode::Full).unwrap()).unwrap();
        assert_eq!(json["data_collected"], true);
        assert_eq!(json["cpu_time"], 0.25);
        for key in ["max_vms", "max_uss", "io_in", "peak_n_procs"] {
            assert_eq!(
                json.get(key),
                Some(&serde_json::Value::Null),
                "{key} must be present and unknown"
            );
        }
        let markdown = record.to_markdown_document(SchemaMode::Full);
        for metric in ["max_vms", "peak_n_procs"] {
            let value = markdown
                .lines()
                .find_map(|line| {
                    let mut cells = line.split('|').map(str::trim);
                    cells.next()?;
                    (cells.next()? == metric).then(|| cells.next()).flatten()
                })
                .unwrap_or_else(|| panic!("missing Markdown row: {metric}"));
            assert_eq!(value, "-", "{metric} must be unknown");
        }
    }

    #[test]
    fn zero_wall_time_does_not_produce_infinite_load() {
        let mut record = BenchmarkRecord::default();
        ExitUsage { cpu_time: 0.25, max_rss_mib: 16.0 }.reconcile(&mut record);
        assert!(record.mean_load.abs() < f64::EPSILON);
    }

    #[test]
    fn rusage_conversion_normalizes_units() {
        // SAFETY: rusage contains only integer/timeval fields; all-zero is valid.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        usage.ru_utime.tv_sec = 1;
        usage.ru_utime.tv_usec = 250_000;
        usage.ru_stime.tv_usec = 125_000;
        #[cfg(target_os = "linux")]
        {
            usage.ru_maxrss = 16 * 1024;
        }
        #[cfg(target_os = "macos")]
        {
            usage.ru_maxrss = 16 * 1024 * 1024;
        }
        let converted = ExitUsage::from_rusage(&usage);
        assert!((converted.cpu_time - 1.375).abs() < 1e-9);
        assert!((converted.max_rss_mib - 16.0).abs() < 1e-9);
    }

    #[test]
    fn exit_code_normal_exit_passes_through() {
        let status = ExitStatus::from_raw(7 << 8); // exit code 7, no signal
        assert_eq!(exit_code_for(status), 7);
    }

    #[test]
    fn exit_code_signal_uses_128_plus_signum() {
        let status = ExitStatus::from_raw(15); // SIGTERM with no core dump
        assert_eq!(exit_code_for(status), 128 + 15);
    }

    fn options_with(out: &str, trace: Option<&str>, markdown: Option<&str>) -> RunOptions {
        RunOptions {
            interval: Duration::from_millis(100),
            output_path: Path::new(out).into(),
            format: OutputFormat::Tsv,
            force_summary: false,
            trace_path: trace.map(|p| Path::new(p).into()),
            markdown_path: markdown.map(|p| Path::new(p).into()),
            schema_mode: SchemaMode::Full,
        }
    }

    #[test]
    fn validate_output_paths_accepts_all_distinct() {
        let opts = options_with("/tmp/a.tsv", Some("/tmp/b.tsv"), Some("/tmp/c.md"));
        validate_output_paths(&opts).expect("distinct paths are fine");
    }

    #[test]
    fn validate_output_paths_accepts_when_sidecars_absent() {
        let opts = options_with("/tmp/a.tsv", None, None);
        validate_output_paths(&opts).expect("single path can't collide with itself");
    }

    #[test]
    fn validate_output_paths_rejects_out_equals_markdown() {
        let opts = options_with("/tmp/shared.tsv", None, Some("/tmp/shared.tsv"));
        let err = validate_output_paths(&opts).expect_err("must reject collision");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let msg = err.to_string();
        assert!(msg.contains("--out") && msg.contains("--export-markdown"), "msg: {msg}");
        assert!(msg.contains("/tmp/shared.tsv"), "msg: {msg}");
    }

    #[test]
    fn validate_output_paths_rejects_trace_equals_markdown() {
        let opts = options_with("/tmp/agg.tsv", Some("/tmp/shared.tsv"), Some("/tmp/shared.tsv"));
        let err = validate_output_paths(&opts).expect_err("must reject collision");
        let msg = err.to_string();
        assert!(msg.contains("--trace") && msg.contains("--export-markdown"), "msg: {msg}");
    }

    #[test]
    fn validate_output_paths_rejects_out_equals_trace() {
        let opts = options_with("/tmp/shared.tsv", Some("/tmp/shared.tsv"), None);
        let err = validate_output_paths(&opts).expect_err("must reject collision");
        let msg = err.to_string();
        assert!(msg.contains("--out") && msg.contains("--trace"), "msg: {msg}");
    }

    #[test]
    fn spawn_failure_does_not_invoke_post_spawn_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_command = tmp.path().join("missing-command");
        let out = tmp.path().join("timing.tsv");
        let opts = options_with(out.to_str().unwrap(), None, None);
        let mut invoked = false;
        let err =
            run_command_with_post_spawn(missing_command.to_str().unwrap(), &[], &opts, || {
                invoked = true;
                Ok(())
            })
            .expect_err("missing executable must fail");

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!invoked, "post-spawn hook must not run when spawning fails");
        assert!(!out.exists());
    }

    /// Recover published PIDs even when a readiness assertion panics inside the hook.
    struct ChildCleanup(std::path::PathBuf);

    impl Drop for ChildCleanup {
        fn drop(&mut self) {
            let pids = published_pids(&self.0);
            if let Some(&child) = pids.first() {
                // SAFETY: only the original group and positive PIDs published by our fixture.
                unsafe {
                    libc::kill(-child, libc::SIGKILL);
                    for &pid in &pids {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < deadline {
                    // SAFETY: the direct child is ours; never block during unwinding.
                    if unsafe { libc::waitpid(child, std::ptr::null_mut(), libc::WNOHANG) } != 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    fn published_pids(path: &Path) -> Vec<i32> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|pid| pid.parse().ok().filter(|pid| *pid > 0))
            .collect()
    }

    fn wait_for_pids(path: &Path, count: usize) -> Vec<i32> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let pids = published_pids(path);
            if pids.len() == count {
                return pids;
            }
            assert!(std::time::Instant::now() < deadline, "fixture did not publish {count} PIDs");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Orphan zombies can persist under Linux container init and are not running workloads.
    fn assert_not_running(pid: i32) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let output = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .expect("inspect fixture process state");
            assert!(output.status.success() || output.status.code() == Some(1), "ps failed");
            let state = std::str::from_utf8(&output.stdout).unwrap().trim();
            if state.is_empty() || state.starts_with('Z') {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "PID {pid} is still running: {state}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_child_reaped(pid: i32) {
        // SAFETY: signal 0 checks existence without sending a signal.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "child must no longer exist");
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        // SAFETY: this is the direct child; WNOHANG makes the reap check nonblocking.
        assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
    }

    #[test]
    fn post_spawn_failure_preserves_error_and_kills_and_reaps_child() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("children.pid");
        let out = tmp.path().join("timing.tsv");
        let opts = options_with(out.to_str().unwrap(), None, None);
        let _cleanup = ChildCleanup(pid_path.clone());
        let mut pids = Vec::new();
        let mut failed_at = None;
        let err = run_command_with_post_spawn(
            "sh",
            &[
                "-c".into(),
                "printf '%s\\n' \"$$\" > \"$1\"; sleep 30 & \
                 printf '%s\\n' \"$!\" >> \"$1\"; wait"
                    .into(),
                "sh".into(),
                pid_path.to_str().unwrap().into(),
            ],
            &opts,
            || {
                pids = wait_for_pids(&pid_path, 2);
                // SAFETY: inspect only the fixture's published descendant.
                assert_eq!(unsafe { libc::getpgid(pids[1]) }, pids[0]);
                failed_at = Some(std::time::Instant::now());
                Err(io::Error::from_raw_os_error(libc::EACCES))
            },
        )
        .expect_err("hook error must propagate");

        assert_eq!(err.raw_os_error(), Some(libc::EACCES));
        assert!(
            failed_at.unwrap().elapsed() < Duration::from_secs(5),
            "child must be killed promptly"
        );
        assert_child_reaped(pids[0]);
        for pid in pids {
            assert_not_running(pid);
        }
        assert!(!out.exists(), "initialization failure must not write a benchmark");
        // Disarm the cleanup guard before a successfully reaped PID can be reused.
        std::fs::remove_file(&pid_path).unwrap();
    }

    #[test]
    #[ignore = "subprocess fixture; invoked explicitly by the process-group fallback test"]
    fn post_spawn_moved_group_helper() {
        let pid_path = std::env::var_os("TRICORD_TEST_PID_PATH").expect("fixture PID path");
        // SAFETY: this isolated subprocess joins its parent's group, not the test harness itself.
        unsafe {
            let parent_group = libc::getpgid(libc::getppid());
            assert!(parent_group > 0);
            assert_eq!(libc::setpgid(0, parent_group), 0);
        }
        std::fs::write(pid_path, std::process::id().to_string()).unwrap();
        // Bound even a mutant that omits the direct-child kill and waits for natural exit.
        std::thread::sleep(Duration::from_secs(10));
    }

    #[test]
    fn post_spawn_failure_kills_and_reaps_child_that_moved_process_group() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("child.pid");
        let out = tmp.path().join("timing.tsv");
        let opts = options_with(out.to_str().unwrap(), None, None);
        let _cleanup = ChildCleanup(pid_path.clone());
        let executable = std::env::current_exe().unwrap();
        let mut pid = None;
        let mut failed_at = None;
        let err = run_command_with_post_spawn(
            "sh",
            &[
                "-c".into(),
                "TRICORD_TEST_PID_PATH=\"$2\" exec \"$1\" --exact \
                 run::tests::post_spawn_moved_group_helper --ignored --nocapture"
                    .into(),
                "sh".into(),
                executable.to_str().unwrap().into(),
                pid_path.to_str().unwrap().into(),
            ],
            &opts,
            || {
                let child = wait_for_pids(&pid_path, 1)[0];
                pid = Some(child);
                // SAFETY: getpgrp takes no arguments; getpgid inspects only our child.
                assert_eq!(unsafe { libc::getpgid(child) }, unsafe { libc::getpgrp() });
                failed_at = Some(std::time::Instant::now());
                Err(io::Error::from_raw_os_error(libc::EACCES))
            },
        )
        .expect_err("hook error must propagate after the child changes groups");

        assert_eq!(err.raw_os_error(), Some(libc::EACCES));
        assert!(
            failed_at.unwrap().elapsed() < Duration::from_secs(5),
            "child must be killed promptly"
        );
        assert_child_reaped(pid.unwrap());
        assert!(!out.exists(), "initialization failure must not write a benchmark");
        // Disarm the cleanup guard before a successfully reaped PID can be reused.
        std::fs::remove_file(&pid_path).unwrap();
    }
}

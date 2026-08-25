//! End-to-end orchestration: spawn the child, run the sampler, write output.

use std::{
    io::{self, IsTerminal},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::Path,
    process::{Command, ExitStatus, Stdio},
    time::Duration,
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
    /// Aggregated resource record produced by the sampler.
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
    let mut child = cmd.spawn()?;

    #[allow(clippy::cast_possible_wrap)]
    let child_pid = child.id() as i32;

    // Install signal forwarding before the sampler so a fast Ctrl-C still
    // reaches the child even if the sampler thread hasn't started yet.
    let signals = SignalForwarder::install(child_pid)?;
    let sampler = SamplerHandle::spawn(
        child_pid,
        SamplerOptions { interval: options.interval, trace_path: options.trace_path.clone() },
    );

    let status = child.wait()?;
    let mut record = sampler.stop();
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
}

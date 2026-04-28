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
    record::BenchmarkRecord,
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
    let sampler = SamplerHandle::spawn(child_pid, SamplerOptions { interval: options.interval });

    let status = child.wait()?;
    let record = sampler.stop();
    drop(signals);

    format::write_to_path(&record, &options.output_path, options.format)?;

    if options.force_summary || io::stderr().is_terminal() {
        eprintln!("tricorder: {}", record.summary_line());
    }

    Ok(RunOutcome { record, status })
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
}

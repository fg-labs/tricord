//! Command-line argument schema.

use std::{path::PathBuf, time::Duration};

use clap::{Parser, ValueEnum};

use crate::format::OutputFormat;

/// Top-level CLI arguments.
///
/// The command to benchmark is everything after `--`. `clap` collects it into
/// [`Args::command`] verbatim — no shell interpolation, no quoting heuristics.
#[derive(Debug, Parser)]
#[command(
    name = "tricorder",
    version,
    about = "Run a command and report its process tree's CPU, memory, and I/O usage.",
    long_about = None,
    after_help = "Example:\n  tricorder --out timing.tsv -- bash -c 'samtools sort big.bam'",
)]
pub struct Args {
    /// Path to write the benchmark record. Parent directories are created if
    /// missing; existing files are overwritten.
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Tsv)]
    pub format: Format,

    /// Sampling interval in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 0.5)]
    pub interval: f64,

    /// Print a one-line resource summary to stderr after the child exits.
    #[arg(long)]
    pub summary: bool,

    /// Verbosity (each `-v` raises the log level: warn → info → debug → trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// The command to run, separated from this binary's flags by `--`.
    #[arg(trailing_var_arg = true, num_args = 1.., required = true, value_name = "CMD")]
    pub command: Vec<String>,
}

/// Output-format flag variants. Mirrors [`OutputFormat`] but exists separately
/// so `clap`'s `ValueEnum` derive doesn't impose itself on the library type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Snakemake-format TSV (default).
    Tsv,
    /// Single JSON object on one line.
    Json,
}

impl From<Format> for OutputFormat {
    fn from(value: Format) -> Self {
        match value {
            Format::Tsv => Self::Tsv,
            Format::Json => Self::Json,
        }
    }
}

impl Args {
    /// Convert the user-supplied `--interval` (seconds) to a [`Duration`].
    /// Clamps to a minimum of 10 ms to keep the sampler responsive but avoid
    /// pathological busy-loops when somebody passes `0`.
    #[must_use]
    pub fn interval_duration(&self) -> Duration {
        const MIN_MS: u64 = 10;
        let ms = (self.interval * 1000.0).round();
        let ms = if ms.is_finite() && ms >= MIN_MS as f64 { ms as u64 } else { MIN_MS };
        Duration::from_millis(ms)
    }

    /// Map `-v` count to a log level for `env_logger`.
    #[must_use]
    pub fn log_level(&self) -> log::LevelFilter {
        match self.verbose {
            0 => log::LevelFilter::Warn,
            1 => log::LevelFilter::Info,
            2 => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_basic_args() {
        let args =
            Args::parse_from(["tricorder", "--out", "/tmp/timing.tsv", "--", "echo", "hello"]);
        assert_eq!(args.out, PathBuf::from("/tmp/timing.tsv"));
        assert_eq!(args.format, Format::Tsv);
        assert_eq!(args.command, vec!["echo".to_string(), "hello".to_string()]);
    }

    #[test]
    fn parses_json_format_and_summary() {
        let args = Args::parse_from([
            "tricorder",
            "--out",
            "out.json",
            "--format",
            "json",
            "--summary",
            "--",
            "true",
        ]);
        assert_eq!(args.format, Format::Json);
        assert!(args.summary);
    }

    #[test]
    fn interval_clamps_to_minimum() {
        let args = Args::parse_from(["tricorder", "--out", "x", "--interval", "0", "--", "true"]);
        assert_eq!(args.interval_duration(), Duration::from_millis(10));
    }

    #[test]
    fn interval_respects_user_value() {
        let args =
            Args::parse_from(["tricorder", "--out", "x", "--interval", "1.25", "--", "true"]);
        assert_eq!(args.interval_duration(), Duration::from_millis(1250));
    }

    #[test]
    fn missing_command_fails() {
        let result = Args::try_parse_from(["tricorder", "--out", "x"]);
        assert!(result.is_err());
    }

    #[test]
    fn log_level_scales_with_verbosity() {
        let args = Args::parse_from(["tricorder", "--out", "x", "-vv", "--", "true"]);
        assert_eq!(args.log_level(), log::LevelFilter::Debug);
    }
}

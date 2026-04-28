//! `tricord` — scan a process tree's wall time, CPU time, peak memory, and
//! disk I/O, then write the result in
//! [Snakemake's `benchmark:` directive][snakemake-benchmark] TSV format
//! (or, optionally, JSON). The companion binary is named `tricorder`.
//!
//! The library exposes the same components the binary uses, so you can embed
//! the sampler in another Rust tool when the CLI shape doesn't fit (for
//! example: pipeline coordinators that already manage their own subprocesses
//! and just need the resource numbers).
//!
//! [snakemake-benchmark]: https://snakemake.readthedocs.io/en/stable/snakefiles/rules.html#benchmark-rules
//!
//! # Quick start
//! ```no_run
//! use std::time::Duration;
//! use tricord::{
//!     run::{run_command, RunOptions},
//!     format::OutputFormat,
//! };
//!
//! let options = RunOptions {
//!     interval: Duration::from_millis(500),
//!     output_path: std::path::Path::new("/tmp/timing.tsv").into(),
//!     format: OutputFormat::Tsv,
//!     force_summary: false,
//! };
//! let outcome = run_command("echo", &["hello".to_string()], &options).unwrap();
//! assert_eq!(outcome.exit_code(), 0);
//! ```

#![warn(clippy::pedantic)]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions
)]

// `cli` is re-exported as `pub` because the `tricorder` binary's `main.rs`
// (a separate compilation target) uses `tricord::cli::Args` to parse its
// flags. Library consumers won't normally need it.
pub mod cli;
pub mod format;
pub mod record;
pub mod run;

// Internal modules — implementation detail of `run_command` / the sampler.
pub(crate) mod platform;
pub(crate) mod sampler;
pub(crate) mod signals;

pub use format::OutputFormat;
pub use record::{BenchmarkRecord, TSV_HEADER};
pub use run::{RunOptions, RunOutcome, run_command};

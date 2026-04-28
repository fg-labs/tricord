//! Entry point for the `tricorder` binary.

use std::process::ExitCode;

use clap::Parser;
use tricord::{
    cli::Args,
    run::{RunOptions, run_command},
};

fn main() -> ExitCode {
    let args = Args::parse();

    env_logger::Builder::new().filter_level(args.log_level()).format_timestamp_secs().init();

    let Some((command, command_args)) = args.command.split_first() else {
        eprintln!("tricorder: error: no command given (separate with `--`)");
        return ExitCode::from(2);
    };
    let command = command.clone();
    let command_args = command_args.to_vec();

    let options = RunOptions {
        interval: args.interval_duration(),
        output_path: args.out.clone().into_boxed_path(),
        format: args.format.into(),
        force_summary: args.summary,
    };

    match run_command(&command, &command_args, &options) {
        Ok(outcome) => clamp_exit_code(outcome.exit_code()),
        Err(err) => {
            eprintln!("tricorder: error: {err}");
            ExitCode::from(127)
        }
    }
}

/// `ExitCode` only carries values 0..=255; clamp negative or out-of-range
/// values to a sensible byte. `ExitCode::from` accepts `u8` directly so the
/// rare case of a child exit code outside that range doesn't panic.
fn clamp_exit_code(code: i32) -> ExitCode {
    match u8::try_from(code) {
        Ok(byte) => ExitCode::from(byte),
        Err(_) => ExitCode::from(1),
    }
}

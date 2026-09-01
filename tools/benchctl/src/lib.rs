//! Benchctl owns operational benchmark controls, never benchmark scenarios.

mod build;
mod cli;
mod cpuidle;
mod error;
mod journal;
mod receipt;
mod runtime;
mod supervision;

use std::ffi::OsString;
use std::process::ExitCode;

pub fn run_from(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    match cli::run(arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("benchctl: {error}");
            eprintln!("retry: {}", error.retry_advice());
            error.exit_code()
        }
    }
}

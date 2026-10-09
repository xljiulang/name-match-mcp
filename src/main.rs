//! Entry point of the `name-match` CLI.
//!
//! Exit codes: `0` on success, `1` on any failure. JSON always goes to stdout;
//! diagnostics go to stderr.

use std::process::ExitCode;

use name_match::cli::{self, Command};

fn main() -> ExitCode {
    match cli::parse(std::env::args().skip(1)) {
        Ok(Command::Help) => {
            print!("{}", cli::HELP);
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("name-match {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Match(args)) => match cli::run(&args) {
            Ok(summary) => match cli::render_summary(&summary) {
                Ok(json) => {
                    println!("{json}");
                    ExitCode::SUCCESS
                }
                Err(error) => fail(&error),
            },
            Err(error) => fail(&error),
        },
        Err(error) => fail(&error),
    }
}

/// Print the JSON error document on stdout and the reason on stderr.
fn fail(error: &cli::CliError) -> ExitCode {
    println!("{}", cli::render_error(error));
    eprintln!("name-match: {}", error.message);
    ExitCode::FAILURE
}

mod check;
mod cli;
mod ui;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}

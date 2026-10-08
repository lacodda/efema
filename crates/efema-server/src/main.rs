mod cli;
mod commands;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();

    let result = match cli.command {
        cli::Command::Serve(args) => commands::serve::run(args),
        cli::Command::Streams(args) => commands::streams::run(args),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // The chain matters: the top line says what failed, the causes say
            // why. Printing only the outer error loses the actual reason.
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

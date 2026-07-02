//! Conductor — fleet cycle runner: scan → triage → plan → dispatch → verify → report.

mod bd;
mod cli;
mod config;
mod deck;
mod dispatch;
mod fields;
mod ledger;
mod plan;
mod ratchet;
mod scan;
mod state;
mod triage;
mod verify;

const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");
const USAGE: &str = "usage: conductor [--version]";

fn print_usage() {
    eprintln!("{USAGE}");
}

fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => {
            print_usage();
            std::process::ExitCode::from(2)
        }
        Some("--version") => {
            println!("conductor {CRATE_VERSION}");
            std::process::ExitCode::SUCCESS
        }
        Some(flag) => {
            eprintln!("unknown argument: {flag}");
            print_usage();
            std::process::ExitCode::from(2)
        }
    }
}

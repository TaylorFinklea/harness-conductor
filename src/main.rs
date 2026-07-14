//! Conductor — fleet cycle runner: scan → triage → plan → dispatch → verify → report.

mod arena;
mod bd;
mod bursar;
mod cli;
mod config;
mod cycle;
mod deck;
mod dispatch;
mod dispatch_cycle;
mod fields;
mod ledger;
mod plan;
mod ratchet;
mod roster_drift;
mod route;
mod scan;
mod state;
mod triage;
mod verify;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    cli::run(args)
}

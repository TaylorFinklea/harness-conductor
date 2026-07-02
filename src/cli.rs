//! subcommand parsing, exit codes (0 ok; 1 cycle had flags/failures; 2 config/env error)

use std::path::PathBuf;
use std::process::ExitCode;

use crate::config;

const USAGE: &str = "usage: conductor [--version] [config check [--config <path>]]";

pub(crate) fn run(args: Vec<String>) -> ExitCode {
    let mut it = args.into_iter();
    match it.next().as_deref() {
        None => {
            print_usage();
            ExitCode::from(2)
        }
        Some("--version") => {
            println!("conductor {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("config") => run_config(&mut it),
        Some(cmd) => {
            eprintln!("unknown subcommand: {cmd}");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn run_config(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    match it.next().as_deref() {
        None => {
            eprintln!("usage: conductor config check [--config <path>]");
            ExitCode::from(2)
        }
        Some("check") => run_config_check(it),
        Some(sub) => {
            eprintln!("unknown config subcommand: {sub}");
            ExitCode::from(2)
        }
    }
}

fn run_config_check(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let mut config_path = PathBuf::from("conductor.toml");
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                let Some(p) = it.next() else {
                    eprintln!("--config requires a path argument");
                    return ExitCode::from(2);
                };
                config_path = PathBuf::from(p);
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let cfg = match config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: invalid — {e}");
            return ExitCode::from(2);
        }
    };
    println!("config: valid ({} roster entries)", cfg.roster.len());

    let path_var = std::env::var("PATH").unwrap_or_default();
    let state_dir = home_state_dir();
    let checks = config::preflight_checks(&path_var, state_dir.as_deref());
    let mut all_ok = true;
    for check in &checks {
        let status = if check.ok { "ok" } else { "FAIL" };
        println!("{}: {status} — {}", check.name, check.message);
        if !check.ok {
            all_ok = false;
        }
    }
    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

fn home_state_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    if home.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("conductor"),
    )
}

fn print_usage() {
    eprintln!("{USAGE}");
}

//! subcommand parsing, exit codes (0 ok; 1 cycle had flags/failures; 2 config/env error)

use std::path::PathBuf;
use std::process::ExitCode;

use crate::config;

const USAGE: &str = "usage: conductor [--version] [config check [--config <path>]] [roster drift [--config <path>]] [route explain --repo <path> --tier-floor <lead|senior|junior> --complexity <S|M|L|XL> [--intent <cheap-work|outside-perspective>] [--json] [--config <path>]] [scan [--json] [--config <path>]] [status] [cycle --dry-run [--repo <name|path>]... [--only <repo>:<issue-id>]... [--config <path>]] [dispatch <cycle-id> [--config <path>]] [arena run --repo <repo|path> --bead <id> [--profiles a,b|all] [--parallel N] [--no-apply] [--config <path>]]";

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
        Some("arena") => run_arena(&mut it),
        Some("config") => run_config(&mut it),
        Some("cycle") => run_cycle(&mut it),
        Some("dispatch") => run_dispatch(&mut it),
        Some("roster") => run_roster(&mut it),
        Some("route") => run_route(&mut it),
        Some("scan") => run_scan(&mut it),
        Some("status") => run_status(&mut it),
        Some(cmd) => {
            eprintln!("unknown subcommand: {cmd}");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn run_route(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    match it.next().as_deref() {
        Some("explain") => run_route_explain(it),
        None => {
            eprintln!(
                "usage: conductor route explain --repo <path> --tier-floor <lead|senior|junior> --complexity <S|M|L|XL> [--intent <cheap-work|outside-perspective>] [--json] [--config <path>]"
            );
            ExitCode::from(2)
        }
        Some(sub) => {
            eprintln!("unknown route subcommand: {sub}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteExplainOptions {
    repo: PathBuf,
    tier_floor: crate::config::Tier,
    complexity: crate::config::Ceiling,
    intent: Option<crate::route::RouteIntent>,
    json: bool,
    config: PathBuf,
}

fn parse_route_explain_options(args: &[String]) -> Result<RouteExplainOptions, String> {
    let mut repo = None;
    let mut tier_floor = None;
    let mut complexity = None;
    let mut intent = None;
    let mut json = false;
    let mut config_path = PathBuf::from("conductor.toml");
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--repo" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--repo requires a path".to_string())?;
                repo = Some(PathBuf::from(value));
            }
            "--tier-floor" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--tier-floor requires lead, senior, or junior".to_string())?;
                tier_floor = Some(
                    value
                        .parse()
                        .map_err(|error: crate::config::ConfigError| error.to_string())?,
                );
            }
            "--complexity" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--complexity requires S, M, L, or XL".to_string())?;
                complexity = Some(
                    value
                        .parse()
                        .map_err(|error: crate::config::ConfigError| error.to_string())?,
                );
            }
            "--intent" => {
                let value = it.next().ok_or_else(|| {
                    "--intent requires cheap-work or outside-perspective".to_string()
                })?;
                intent = Some(
                    value
                        .parse()
                        .map_err(|error: crate::route::RouteError| error.to_string())?,
                );
            }
            "--json" => json = true,
            "--config" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_string())?;
                config_path = PathBuf::from(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(RouteExplainOptions {
        repo: repo.ok_or_else(|| "route explain requires --repo <path>".to_string())?,
        tier_floor: tier_floor
            .ok_or_else(|| "route explain requires --tier-floor <value>".to_string())?,
        complexity: complexity
            .ok_or_else(|| "route explain requires --complexity <value>".to_string())?,
        intent,
        json,
        config: config_path,
    })
}

fn run_route_explain(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let args: Vec<String> = it.collect();
    let options = match parse_route_explain_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("route explain: {error}");
            return ExitCode::from(2);
        }
    };
    let config = match config::load(&options.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    let bursar = crate::bursar::CommandBursarClient::new();
    let output = route_explain_output(&config, &options, &bursar);
    println!("{output}");
    ExitCode::SUCCESS
}

fn route_explain_output(
    config: &crate::config::Config,
    options: &RouteExplainOptions,
    bursar: &dyn crate::bursar::BursarClient,
) -> String {
    let routing = crate::fields::RoutingFields {
        tier_floor: options.tier_floor,
        complexity: options.complexity,
        verify_cmd: None,
        trains_ok: false,
    };
    let advice = crate::route::explain(config, &options.repo, &routing, options.intent, bursar);
    if options.json {
        serde_json::to_string_pretty(&advice.to_json()).expect("route advice JSON is serializable")
    } else {
        advice.human()
    }
}

fn run_arena(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    match it.next().as_deref() {
        Some("run") => run_arena_run(it),
        None => {
            eprintln!(
                "usage: conductor arena run --repo <repo|path> --bead <id> [--profiles a,b|all] [--parallel N] [--no-apply] [--config <path>]"
            );
            ExitCode::from(2)
        }
        Some(sub) => {
            eprintln!("unknown arena subcommand: {sub}");
            ExitCode::from(2)
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "manual CLI parser stays local to the subcommand"
)]
fn run_arena_run(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let mut config_path = PathBuf::from("conductor.toml");
    let mut repo: Option<String> = None;
    let mut bead: Option<String> = None;
    let mut profiles = crate::arena::ProfileSelection::All;
    let mut parallel = None;
    let mut auto_apply = true;

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                let Some(p) = it.next() else {
                    eprintln!("--config requires a path argument");
                    return ExitCode::from(2);
                };
                config_path = PathBuf::from(p);
            }
            "--repo" => {
                let Some(value) = it.next() else {
                    eprintln!("--repo requires an argument");
                    return ExitCode::from(2);
                };
                repo = Some(value);
            }
            "--bead" => {
                let Some(value) = it.next() else {
                    eprintln!("--bead requires an argument");
                    return ExitCode::from(2);
                };
                bead = Some(value);
            }
            "--profiles" => {
                let Some(value) = it.next() else {
                    eprintln!("--profiles requires an argument");
                    return ExitCode::from(2);
                };
                profiles = if value == "all" {
                    crate::arena::ProfileSelection::All
                } else {
                    let names: Vec<String> = value
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    if names.is_empty() {
                        eprintln!("--profiles requires at least one profile name or all");
                        return ExitCode::from(2);
                    }
                    crate::arena::ProfileSelection::Named(names)
                };
            }
            "--parallel" => {
                let Some(value) = it.next() else {
                    eprintln!("--parallel requires an integer argument");
                    return ExitCode::from(2);
                };
                let Ok(parsed) = value.parse::<u32>() else {
                    eprintln!("--parallel must be an integer");
                    return ExitCode::from(2);
                };
                if parsed == 0 {
                    eprintln!("--parallel must be at least 1");
                    return ExitCode::from(2);
                }
                parallel = Some(parsed);
            }
            "--no-apply" => auto_apply = false,
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let Some(repo) = repo else {
        eprintln!("arena run requires --repo <repo|path>");
        return ExitCode::from(2);
    };
    let Some(bead) = bead else {
        eprintln!("arena run requires --bead <id>");
        return ExitCode::from(2);
    };

    let cfg = match config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: invalid — {e}");
            return ExitCode::from(2);
        }
    };
    let bd = crate::bd::CommandBdClient::new();
    let options = crate::arena::ArenaRunOptions {
        repo,
        bead,
        profiles,
        parallel,
        auto_apply,
    };
    match crate::arena::run(
        &cfg,
        &bd,
        &reports_home(),
        &state_dir(),
        &ledger_path(),
        &options,
    ) {
        Ok(result) => {
            println!("arena {}: complete", result.run_id);
            println!("report: {}", result.report_path.display());
            match result.winner_profile {
                Some(winner) if result.applied => println!("winner applied: {winner}"),
                Some(winner) => println!("winner not applied: {winner}"),
                None => println!("winner: none"),
            }
            if result.applied {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("arena: {e}");
            ExitCode::from(1)
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

fn reports_home() -> PathBuf {
    std::env::var("CONDUCTOR_REPORTS_HOME").map_or_else(
        |_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home)
        },
        PathBuf::from,
    )
}

fn state_dir() -> PathBuf {
    std::env::var("CONDUCTOR_STATE_DIR").map_or_else(
        |_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("conductor")
        },
        PathBuf::from,
    )
}

fn ledger_path() -> PathBuf {
    std::env::var("CONDUCTOR_LEDGER_PATH").map_or_else(
        |_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home)
                .join(".claude")
                .join("model-bench.jsonl")
        },
        PathBuf::from,
    )
}

fn run_roster(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    match it.next().as_deref() {
        None => {
            eprintln!("usage: conductor roster drift [--config <path>]");
            ExitCode::from(2)
        }
        Some("drift") => run_roster_drift(it),
        Some(sub) => {
            eprintln!("unknown roster subcommand: {sub}");
            ExitCode::from(2)
        }
    }
}

fn run_roster_drift(it: &mut std::vec::IntoIter<String>) -> ExitCode {
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

    let home = match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => h,
        _ => {
            eprintln!("roster drift: HOME not set; cannot locate ~/.claude/model-scorecard.md");
            return ExitCode::from(1);
        }
    };
    let scorecard_path = PathBuf::from(home)
        .join(".claude")
        .join("model-scorecard.md");

    let content = match std::fs::read_to_string(&scorecard_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("roster drift: cannot read scorecard: {e}");
            return ExitCode::from(1);
        }
    };

    let scorecard_entries = match crate::roster_drift::parse_scorecard(&content) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("roster drift: cannot parse scorecard: {e}");
            return ExitCode::from(1);
        }
    };

    let report = crate::roster_drift::diff(&scorecard_entries, &cfg.roster);
    crate::roster_drift::print_report(&report);
    ExitCode::SUCCESS
}

fn run_scan(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let mut json_output = false;
    let mut config_path = PathBuf::from("conductor.toml");
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_output = true,
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

    let client = crate::bd::CommandBdClient::new();
    let snapshots = match crate::scan::scan(&cfg.scan, &client) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scan: {e}");
            return ExitCode::from(2);
        }
    };

    if json_output {
        match serde_json::to_string_pretty(&snapshots) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                eprintln!("json: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        print_scan_table(&snapshots);
    }

    scan_exit_code(&snapshots)
}

fn scan_exit_code(snapshots: &[crate::scan::RepoSnapshot]) -> ExitCode {
    use crate::scan::SkipReason;

    // Ordinary skips (not-beads, excluded, in-progress, not-git) are expected
    // fleet composition, not failures. Only a real ScanGap is reportable.
    let has_scan_gap = snapshots
        .iter()
        .any(|s| matches!(s.skip_reason, Some(SkipReason::ScanGap { .. })));
    if has_scan_gap {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn print_scan_table(snapshots: &[crate::scan::RepoSnapshot]) {
    use crate::scan::{Freshness, SkipReason, ZeroState};

    let headers = ["REPO", "READY", "ZERO-STATE", "FRESH", "FLAGS"];

    let rows: Vec<[String; 5]> = snapshots
        .iter()
        .map(|s| {
            let ready = if s.is_beads_repo && s.skip_reason.is_none() {
                s.ready.len().to_string()
            } else {
                "-".to_string()
            };

            let zero_state = match s.zero_state {
                ZeroState::Drained => "drained".to_string(),
                ZeroState::Blocked => "blocked".to_string(),
                ZeroState::NotApplicable => "-".to_string(),
            };

            let freshness = if s.is_beads_repo {
                match s.freshness {
                    Freshness::Fresh => "fresh".to_string(),
                    Freshness::Recent => "recent".to_string(),
                    Freshness::Stale => "stale".to_string(),
                    Freshness::Unknown => "unknown".to_string(),
                }
            } else {
                "-".to_string()
            };

            let flags = match &s.skip_reason {
                Some(SkipReason::InProgress) => "in-progress".to_string(),
                Some(SkipReason::Excluded) => "excluded".to_string(),
                Some(SkipReason::NotBeadsRepo) => "not-beads".to_string(),
                Some(SkipReason::NotGitRepo) => "not-git".to_string(),
                Some(SkipReason::ScanGap { .. }) => "scan-gap".to_string(),
                None => "-".to_string(),
            };

            [s.name.clone(), ready, zero_state, freshness, flags]
        })
        .collect();

    let mut widths = [0usize; 5];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let header_line: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:<width$}", h, width = widths[i]))
        .collect();
    println!("{}", header_line.join("  "));

    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{:<width$}", cell, width = widths[i]))
            .collect();
        println!("{}", line.join("  "));
    }
}

fn run_status(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    // Reject unknown arguments
    if let Some(arg) = it.next() {
        eprintln!("unknown argument: {arg}");
        return ExitCode::from(2);
    }

    let Some(state_dir) = home_state_dir() else {
        eprintln!("status: HOME not set; cannot locate state directory");
        return ExitCode::from(2);
    };

    let journal_path = state_dir.join("journal.json");
    if !journal_path.is_file() {
        println!("no cycles recorded yet");
        println!();
        println!("state directory: {}", state_dir.display());
        return ExitCode::SUCCESS;
    }

    let content = match std::fs::read_to_string(&journal_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("status: cannot read journal: {e}");
            return ExitCode::from(2);
        }
    };

    let journal: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("status: invalid journal: {e}");
            return ExitCode::from(2);
        }
    };

    if let Some(last_cycle) = journal.get("last_cycle") {
        if let Some(id) = last_cycle.get("id").and_then(|v| v.as_str()) {
            println!("last cycle: {id}");
        }
        if let Some(ts) = last_cycle.get("completed_at").and_then(|v| v.as_str()) {
            println!("completed:  {ts}");
        }
        if let Some(summary) = last_cycle.get("summary").and_then(|v| v.as_object()) {
            let scanned = summary
                .get("scanned")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let ready = summary
                .get("ready")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let dispatched = summary
                .get("dispatched")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let verified = summary
                .get("verified")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let flagged = summary
                .get("flagged")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            println!(
                "summary:    scanned={scanned} ready={ready} dispatched={dispatched} verified={verified} flagged={flagged}"
            );
        }
    } else {
        println!("no cycles recorded yet");
    }

    println!();
    println!("state directory: {}", state_dir.display());
    ExitCode::SUCCESS
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CycleOptions {
    dry_run: bool,
    config: PathBuf,
    scope: crate::cycle::CycleScopeRequest,
}

fn parse_cycle_options(args: &[String]) -> Result<CycleOptions, String> {
    let mut dry_run = false;
    let mut config_path = PathBuf::from("conductor.toml");
    let mut repos = Vec::new();
    let mut only = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--repo" => repos.push(
                it.next()
                    .ok_or_else(|| "--repo requires a name or path".to_string())?
                    .clone(),
            ),
            "--only" => only.push(
                it.next()
                    .ok_or_else(|| "--only requires <repo>:<issue-id>".to_string())?
                    .clone(),
            ),
            "--config" => {
                config_path = PathBuf::from(
                    it.next()
                        .ok_or_else(|| "--config requires a path argument".to_string())?,
                );
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if !dry_run {
        return Err("only --dry-run is supported in this version".to_string());
    }
    Ok(CycleOptions {
        dry_run,
        config: config_path,
        scope: crate::cycle::CycleScopeRequest { repos, only },
    })
}

fn run_cycle(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let args: Vec<String> = it.collect();
    let options = match parse_cycle_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("cycle: {error}");
            return ExitCode::from(2);
        }
    };
    debug_assert!(options.dry_run);

    let cfg = match config::load(&options.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: invalid — {e}");
            return ExitCode::from(2);
        }
    };

    let reports_home = reports_home();
    let state_dir = state_dir();

    let client = crate::bd::CommandBdClient::new();
    let bursar = crate::bursar::CommandBursarClient::new();
    match crate::cycle::run_dry_run_scoped(
        &cfg,
        &client,
        &bursar,
        &reports_home,
        &state_dir,
        &options.scope,
    ) {
        Ok(result) => {
            println!("cycle {}: dry-run complete", result.cycle_id);
            println!("report: {}", result.report_path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cycle: {e}");
            if e.is_scope_error() {
                ExitCode::from(2)
            } else {
                ExitCode::from(1)
            }
        }
    }
}

fn run_dispatch(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let Some(cycle_id) = it.next() else {
        eprintln!("usage: conductor dispatch <cycle-id> [--config <path>]");
        return ExitCode::from(2);
    };
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

    let bd = crate::bd::CommandBdClient::new();
    let bursar = crate::bursar::CommandBursarClient::new();
    let exec = crate::dispatch::CommandExec;
    let commits = crate::dispatch::GitCommitProbe;
    let live = crate::dispatch_cycle::DeckLiveSink;
    let options = crate::dispatch_cycle::DispatchCycleOptions::from_config(&cfg);
    match crate::dispatch_cycle::run_dispatch_cycle(
        &cfg,
        &bd,
        &exec,
        &commits,
        &reports_home(),
        &state_dir(),
        &ledger_path(),
        &cycle_id,
        &options,
        &live,
        &bursar,
    ) {
        Ok(result) => match result.gate {
            crate::dispatch_cycle::ApprovalGate::Approved => {
                println!(
                    "dispatch {cycle_id}: ran {} item(s), verified {}, failed {}",
                    result.dispatched, result.verified, result.failed
                );
                if result.failed == 0 {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                }
            }
            crate::dispatch_cycle::ApprovalGate::ChangesRequested => {
                println!("dispatch {cycle_id}: changes requested; cycle closed");
                ExitCode::SUCCESS
            }
        },
        Err(e) if e.is_not_answered() => {
            eprintln!("dispatch {cycle_id}: {e}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("dispatch {cycle_id}: {e}");
            ExitCode::from(1)
        }
    }
}

fn print_usage() {
    eprintln!("{USAGE}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Freshness, RepoSnapshot, SkipReason, ZeroState};
    use std::path::PathBuf;

    #[test]
    fn route_explain_accepts_read_only_provider_advice_arguments() {
        let options = parse_route_explain_options(&[
            "--repo".to_string(),
            "/tmp/chezmoi-personal".to_string(),
            "--tier-floor".to_string(),
            "senior".to_string(),
            "--complexity".to_string(),
            "M".to_string(),
            "--intent".to_string(),
            "outside-perspective".to_string(),
            "--json".to_string(),
            "--config".to_string(),
            "fixture.toml".to_string(),
        ])
        .expect("valid route explain arguments");

        assert_eq!(options.repo, PathBuf::from("/tmp/chezmoi-personal"));
        assert_eq!(options.tier_floor, crate::config::Tier::Senior);
        assert_eq!(options.complexity, crate::config::Ceiling::M);
        assert_eq!(
            options.intent,
            Some(crate::route::RouteIntent::OutsidePerspective)
        );
        assert!(options.json);
        assert_eq!(options.config, PathBuf::from("fixture.toml"));
    }

    #[test]
    fn route_explain_render_path_has_no_scan_bd_or_mutation_seam() {
        let source = include_str!("cli.rs");
        let route_body = source
            .split("fn run_route_explain")
            .nth(1)
            .expect("route command exists")
            .split("\nfn ")
            .next()
            .expect("route command body exists");
        assert!(!route_body.contains("scan::scan"));
        assert!(!route_body.contains("CommandBdClient"));
        assert!(!route_body.contains("claim"));
        assert!(!route_body.contains("dispatch"));
        assert!(!route_body.contains("write"));
    }

    #[test]
    fn route_explain_renders_human_and_json_from_the_shared_advice() {
        let config = crate::config::parse_str(
            r#"
[budgets]
use_bursar = false

[[roster]]
name = "fixture-model"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "fixture-dispatch"
provider = "fixture-provider"
"#,
        )
        .expect("fixture config parses");
        let human = parse_route_explain_options(&[
            "--repo".to_string(),
            "/tmp/advice-repo".to_string(),
            "--tier-floor".to_string(),
            "senior".to_string(),
            "--complexity".to_string(),
        ])
        .expect_err("incomplete options are rejected");
        assert!(human.contains("--complexity"));

        let options = parse_route_explain_options(&[
            "--repo".to_string(),
            "/tmp/advice-repo".to_string(),
            "--tier-floor".to_string(),
            "senior".to_string(),
            "--complexity".to_string(),
            "M".to_string(),
        ])
        .expect("complete options parse");
        let bursar = crate::bursar::test_support::FakeBursarClient::unavailable();
        let human = route_explain_output(&config, &options, &bursar);
        assert!(human.contains("selected: fixture-model"));
        assert!(human.contains("backend=pi"));
        assert!(human.contains("dispatch_id=fixture-dispatch"));
        assert!(human.contains("provider=fixture-provider"));
        assert!(human.contains("action=static-caps"));
        assert!(human.contains("CANDIDATE AUDIT"));

        let json_options = RouteExplainOptions {
            json: true,
            ..options
        };
        let json = route_explain_output(&config, &json_options, &bursar);
        assert!(json.contains("\"selected\""));
        assert!(json.contains("\"audit\""));
    }

    fn make_snapshot(name: &str, ready_count: usize, skip: Option<SkipReason>) -> RepoSnapshot {
        let is_beads_repo =
            skip != Some(SkipReason::NotBeadsRepo) && skip != Some(SkipReason::Excluded);
        let zero_state = if ready_count == 0 && skip.is_none() {
            ZeroState::Drained
        } else {
            ZeroState::NotApplicable
        };
        let freshness = if skip.is_some() {
            Freshness::Unknown
        } else {
            Freshness::Fresh
        };

        let mut ready = Vec::new();
        for i in 0..ready_count {
            ready.push(crate::bd::Issue {
                id: format!("{name}-{i}"),
                title: format!("Issue {i}"),
                description: String::new(),
                acceptance_criteria: String::new(),
                notes: String::new(),
                status: "open".to_string(),
                priority: 1,
                issue_type: "task".to_string(),
                assignee: None,
                owner: "test".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                created_by: "test".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                started_at: None,
                labels: None,
                estimated_minutes: None,
                metadata: None,
                parent: None,
                dependencies: None,
                dependency_count: None,
                dependent_count: None,
                comment_count: None,
            });
        }

        RepoSnapshot {
            path: PathBuf::from(format!("/test/{name}")),
            name: name.to_string(),
            is_beads_repo,
            skip_reason: skip,
            ready,
            count: ready_count as u64,
            blocked: Vec::new(),
            zero_state,
            freshness,
        }
    }

    #[test]
    fn scan_subcommand_json_outputs_snapshots() {
        let snapshots = vec![
            make_snapshot("repo-a", 3, None),
            make_snapshot("repo-b", 0, Some(SkipReason::Excluded)),
        ];

        let json = serde_json::to_string(&snapshots).expect("serialize");
        assert!(json.contains("repo-a"));
        assert!(json.contains("repo-b"));
        assert!(json.contains("Excluded"));
    }

    #[test]
    fn scan_exit_code_is_success_for_ordinary_skips() {
        let snapshots = vec![
            make_snapshot("a", 3, None),
            make_snapshot("b", 0, Some(SkipReason::NotBeadsRepo)),
            make_snapshot("c", 0, Some(SkipReason::Excluded)),
            make_snapshot("d", 0, Some(SkipReason::InProgress)),
            make_snapshot("e", 0, Some(SkipReason::NotGitRepo)),
        ];

        assert_eq!(scan_exit_code(&snapshots), ExitCode::SUCCESS);
    }

    #[test]
    fn cycle_scope_parser_collects_repeatable_repo_and_only_selectors() {
        let args = [
            "--dry-run",
            "--repo",
            "alpha",
            "--repo",
            "/repos/bravo",
            "--only",
            "alpha:a-1",
            "--only",
            "/repos/bravo:b-2",
            "--config",
            "/tmp/conductor.toml",
        ]
        .map(str::to_string);
        let options = parse_cycle_options(&args).expect("cycle options");
        assert!(options.dry_run);
        assert_eq!(options.scope.repos, ["alpha", "/repos/bravo"]);
        assert_eq!(options.scope.only, ["alpha:a-1", "/repos/bravo:b-2"]);
        assert_eq!(options.config, PathBuf::from("/tmp/conductor.toml"));
    }

    #[test]
    fn cycle_scope_parser_rejects_missing_values_and_unknown_arguments() {
        assert!(parse_cycle_options(&["--repo".to_string()]).is_err());
        assert!(parse_cycle_options(&["--only".to_string()]).is_err());
        assert!(parse_cycle_options(&["--dry-run".to_string(), "--wide".to_string()]).is_err());
        assert!(parse_cycle_options(&[]).is_err());
    }

    #[test]
    fn scan_exit_code_fails_only_on_scan_gap() {
        let snapshots = vec![
            make_snapshot("a", 3, None),
            make_snapshot(
                "b",
                0,
                Some(SkipReason::ScanGap {
                    command: "bd ready --json".to_string(),
                    message: "boom".to_string(),
                }),
            ),
        ];

        assert_eq!(scan_exit_code(&snapshots), ExitCode::from(1));
    }

    #[test]
    fn scan_table_formats_columns() {
        let snapshots = vec![
            make_snapshot("alpha", 5, None),
            make_snapshot("beta-long-name", 12, None),
            make_snapshot("gamma", 0, Some(SkipReason::InProgress)),
        ];

        // Capture output by calling the function and checking it doesn't panic
        print_scan_table(&snapshots);
    }

    #[test]
    fn scan_table_handles_empty_list() {
        let snapshots: Vec<RepoSnapshot> = vec![];
        print_scan_table(&snapshots);
    }

    #[test]
    fn scan_table_shows_zero_states() {
        let mut snap = make_snapshot("drained", 0, None);
        snap.zero_state = ZeroState::Drained;
        snap.freshness = Freshness::Stale;

        let snapshots = vec![snap];
        print_scan_table(&snapshots);
    }

    #[test]
    fn scan_table_shows_blocked_zero_state() {
        let mut snap = make_snapshot("blocked", 0, None);
        snap.zero_state = ZeroState::Blocked;
        snap.freshness = Freshness::Recent;

        let snapshots = vec![snap];
        print_scan_table(&snapshots);
    }

    #[test]
    fn scan_table_shows_all_skip_reasons() {
        let snapshots = vec![
            make_snapshot("a", 0, Some(SkipReason::InProgress)),
            make_snapshot("b", 0, Some(SkipReason::Excluded)),
            make_snapshot("c", 0, Some(SkipReason::NotBeadsRepo)),
            make_snapshot("d", 0, Some(SkipReason::NotGitRepo)),
            make_snapshot(
                "e",
                0,
                Some(SkipReason::ScanGap {
                    command: "bd ready --json".to_string(),
                    message: "failed to parse JSON from `bd ready`: fixture".to_string(),
                }),
            ),
        ];

        print_scan_table(&snapshots);
    }

    #[test]
    fn scan_table_shows_all_freshness_levels() {
        let mut s1 = make_snapshot("fresh", 1, None);
        s1.freshness = Freshness::Fresh;

        let mut s2 = make_snapshot("recent", 1, None);
        s2.freshness = Freshness::Recent;

        let mut s3 = make_snapshot("stale", 1, None);
        s3.freshness = Freshness::Stale;

        let mut s4 = make_snapshot("unknown", 1, None);
        s4.freshness = Freshness::Unknown;

        let snapshots = vec![s1, s2, s3, s4];
        print_scan_table(&snapshots);
    }
}

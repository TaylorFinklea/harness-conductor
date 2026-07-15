//! subcommand parsing, exit codes (0 ok; 1 cycle had flags/failures; 2 config/env error)

use std::path::PathBuf;
use std::process::ExitCode;

use crate::config;

const USAGE: &str = "usage: conductor [--version] [adversarial-review plan --artifact <path> --reviewers <N> [--question <text>] [--models <a,b,...>] [--config <path>]] [adversarial-review dispatch <review-id> [--config <path>]] [config check [--config <path>]] [roster drift [--config <path>]] [route explain --repo <path> --tier-floor <lead|senior|junior> --complexity <S|M|L|XL> [--intent <cheap-work|outside-perspective>] [--json] [--config <path>]] [scan [--json] [--config <path>]] [status] [cycle --dry-run [--repo <name|path>]... [--only <repo>:<issue-id>]... [--config <path>]] [dispatch <cycle-id> [--config <path>]] [arena run --repo <repo|path> --bead <id> [--profiles a,b|all] [--parallel N] [--no-apply] [--config <path>]]";

const DEFAULT_ADVERSARIAL_QUESTION: &str =
    "What are the highest-risk flaws in this artifact, and what must change before proceeding?";

pub(crate) fn run(args: Vec<String>) -> ExitCode {
    let mut it = args.into_iter();
    match it.next().as_deref() {
        None => {
            print_usage();
            ExitCode::from(2)
        }
        Some("--help" | "-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("--version") => {
            println!("conductor {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("adversarial-review") => run_adversarial(&mut it),
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdversarialPlanOptions {
    artifact: PathBuf,
    reviewers: usize,
    question: String,
    models: Option<Vec<String>>,
    config: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdversarialDispatchOptions {
    review_id: String,
    config: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdversarialPaths {
    state_root: PathBuf,
    reports_home: PathBuf,
    ledger_path: PathBuf,
}

impl AdversarialPaths {
    fn from_environment() -> Self {
        Self {
            state_root: state_dir().join("adversarial-reviews"),
            reports_home: reports_home(),
            ledger_path: ledger_path(),
        }
    }
}

fn run_adversarial(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    match it.next().as_deref() {
        Some("plan") => run_adversarial_plan(it),
        Some("dispatch") => run_adversarial_dispatch(it),
        None => {
            eprintln!(
                "usage: conductor adversarial-review <plan --artifact <path> --reviewers <N>|dispatch <review-id>> [options]"
            );
            ExitCode::from(2)
        }
        Some(subcommand) => {
            eprintln!("unknown adversarial-review subcommand: {subcommand}");
            ExitCode::from(2)
        }
    }
}

fn parse_adversarial_plan_options(args: &[String]) -> Result<AdversarialPlanOptions, String> {
    let mut artifact = None;
    let mut reviewers = None;
    let mut question = None;
    let mut models = None;
    let mut config_path = PathBuf::from("conductor.toml");
    let mut config_seen = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--artifact" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--artifact requires a path".to_string())?;
                if artifact.replace(PathBuf::from(value)).is_some() {
                    return Err("--artifact may only be supplied once".to_string());
                }
            }
            "--reviewers" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--reviewers requires an integer".to_string())?;
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| "--reviewers must be a positive integer".to_string())?;
                if parsed == 0 {
                    return Err("--reviewers must be at least 1".to_string());
                }
                if reviewers.replace(parsed).is_some() {
                    return Err("--reviewers may only be supplied once".to_string());
                }
            }
            "--question" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--question requires text".to_string())?;
                if value.trim().is_empty() {
                    return Err("--question must not be empty".to_string());
                }
                if question.replace(value.clone()).is_some() {
                    return Err("--question may only be supplied once".to_string());
                }
            }
            "--models" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--models requires comma-separated roster names".to_string())?;
                let parsed = value
                    .split(',')
                    .map(str::trim)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                if parsed.is_empty() || parsed.iter().any(String::is_empty) {
                    return Err(
                        "--models requires non-empty comma-separated roster names".to_string()
                    );
                }
                if models.replace(parsed).is_some() {
                    return Err("--models may only be supplied once".to_string());
                }
            }
            "--config" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_string())?;
                if config_seen {
                    return Err("--config may only be supplied once".to_string());
                }
                config_seen = true;
                config_path = PathBuf::from(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let reviewers =
        reviewers.ok_or_else(|| "adversarial-review plan requires --reviewers <N>".to_string())?;
    if let Some(explicit) = &models
        && explicit.len() != reviewers
    {
        return Err(format!(
            "--models contains {} entries; expected {reviewers}",
            explicit.len()
        ));
    }
    Ok(AdversarialPlanOptions {
        artifact: artifact
            .ok_or_else(|| "adversarial-review plan requires --artifact <path>".to_string())?,
        reviewers,
        question: question.unwrap_or_else(|| DEFAULT_ADVERSARIAL_QUESTION.to_string()),
        models,
        config: config_path,
    })
}

fn parse_adversarial_dispatch_options(
    args: &[String],
) -> Result<AdversarialDispatchOptions, String> {
    let Some(review_id) = args.first() else {
        return Err("adversarial-review dispatch requires <review-id>".to_string());
    };
    if !valid_cli_review_id(review_id) {
        return Err("review id must contain only alphanumeric, '_' or '-' bytes".to_string());
    }
    let mut config_path = PathBuf::from("conductor.toml");
    let mut config_seen = false;
    let mut it = args[1..].iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                let value = it
                    .next()
                    .ok_or_else(|| "--config requires a path argument".to_string())?;
                if config_seen {
                    return Err("--config may only be supplied once".to_string());
                }
                config_seen = true;
                config_path = PathBuf::from(value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(AdversarialDispatchOptions {
        review_id: review_id.clone(),
        config: config_path,
    })
}

fn valid_cli_review_id(review_id: &str) -> bool {
    let mut bytes = review_id.bytes();
    !review_id.is_empty()
        && review_id.len() <= 128
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn run_adversarial_plan(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let args = it.collect::<Vec<_>>();
    let options = match parse_adversarial_plan_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("adversarial-review plan: {error}");
            return ExitCode::from(2);
        }
    };
    let cfg = match config::load(&options.config) {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    if options.reviewers > cfg.adversarial_review.max_reviewers as usize {
        eprintln!(
            "adversarial-review plan: --reviewers must be between 1 and {}",
            cfg.adversarial_review.max_reviewers
        );
        return ExitCode::from(2);
    }
    let paths = AdversarialPaths::from_environment();
    let bursar = crate::bursar::CommandBursarClient::new();
    let validator = crate::deck::CommandDeckValidator::new();
    let review_id = new_adversarial_review_id();
    let created_at = chrono::Utc::now().to_rfc3339();
    match execute_adversarial_plan(
        &cfg,
        &options,
        &paths,
        &bursar,
        &validator,
        &review_id,
        &created_at,
    ) {
        Ok(published) => {
            println!(
                "adversarial-review plan {}: awaiting approval",
                published.plan.review_id
            );
            println!(
                "calls: nominal {}, worst-case {}",
                published.plan.limits.nominal_calls, published.plan.limits.worst_case_calls
            );
            println!(
                "state: {}",
                paths.state_root.join(&published.plan.review_id).display()
            );
            println!("report: {}", published.report_path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("adversarial-review plan: {error}");
            ExitCode::from(1)
        }
    }
}

fn execute_adversarial_plan<C, V>(
    cfg: &crate::config::Config,
    options: &AdversarialPlanOptions,
    paths: &AdversarialPaths,
    bursar: &C,
    validator: &V,
    review_id: &str,
    created_at: &str,
) -> Result<crate::adversarial::PublishedApproval, String>
where
    C: crate::bursar::BursarClient + ?Sized,
    V: crate::deck::DeckValidator,
{
    let provider_snapshot = adversarial_provider_snapshot(cfg, bursar);
    let panel = crate::adversarial::plan_panel(
        &cfg.roster,
        &cfg.adversarial_review,
        &provider_snapshot,
        options.reviewers,
        options.models.as_deref(),
    )
    .map_err(|error| error.to_string())?;
    let snapshot =
        crate::adversarial::snapshot_artifact(&options.artifact, &paths.state_root, review_id)
            .map_err(|error| error.to_string())?;
    crate::adversarial::publish_approval_plan(
        crate::adversarial::ApprovalPlanRequest {
            snapshot: &snapshot,
            roster: &cfg.roster,
            config: &cfg.adversarial_review,
            provider_snapshot: &provider_snapshot,
            panel,
            question: &options.question,
            created_at,
            deck_home: &paths.reports_home,
        },
        validator,
    )
    .map_err(|error| error.to_string())
}

fn run_adversarial_dispatch(it: &mut std::vec::IntoIter<String>) -> ExitCode {
    let args = it.collect::<Vec<_>>();
    let options = match parse_adversarial_dispatch_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("adversarial-review dispatch: {error}");
            return ExitCode::from(2);
        }
    };
    let cfg = match config::load(&options.config) {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!("config: invalid — {error}");
            return ExitCode::from(2);
        }
    };
    let paths = AdversarialPaths::from_environment();
    let bursar = crate::bursar::CommandBursarClient::new();
    let exec = crate::dispatch::CommandExec;
    let result = execute_adversarial_dispatch(&cfg, &options, &paths, &bursar, &exec);
    match &result {
        Ok(run) => {
            let outcome = match run.outcome {
                crate::adversarial::ReviewLifecycleOutcome::Complete => "complete",
                crate::adversarial::ReviewLifecycleOutcome::Partial => "partial",
            };
            println!(
                "adversarial-review dispatch {}: {outcome}",
                options.review_id
            );
            println!("report: {}", run.report_path.display());
            for failure in &run.failures {
                eprintln!("failure: {failure}");
            }
        }
        Err(error) => eprintln!("adversarial-review dispatch {}: {error}", options.review_id),
    }
    adversarial_dispatch_result_exit_code(&result)
}

fn execute_adversarial_dispatch<C, E>(
    cfg: &crate::config::Config,
    options: &AdversarialDispatchOptions,
    paths: &AdversarialPaths,
    bursar: &C,
    exec: &E,
) -> Result<crate::adversarial::AdversarialRun, String>
where
    C: crate::bursar::BursarClient + ?Sized,
    E: crate::dispatch::Exec + Sync,
{
    let review_dir = paths.state_root.join(&options.review_id);
    let plan =
        crate::adversarial::load_review_plan(&review_dir).map_err(|error| error.to_string())?;
    let artifact_path = PathBuf::from(plan.artifact_source_path());
    let provider_snapshot = adversarial_provider_snapshot(cfg, bursar);
    let authorized = crate::adversarial::authorize_approved_execution(
        &review_dir,
        &paths.reports_home,
        &artifact_path,
        &cfg.roster,
        &cfg.adversarial_review,
        &provider_snapshot,
    )
    .map_err(|error| error.to_string())?;
    let calls =
        crate::adversarial::ReviewerCallBudget::new(authorized.plan.limits.worst_case_calls);
    let timeout = std::time::Duration::from_secs(
        u64::from(cfg.budgets.item_wall_clock_mins).saturating_mul(60),
    );
    let reviewer_run =
        crate::adversarial::run_reviewers(&authorized, &cfg.roster, exec, timeout, &calls)
            .map_err(|error| error.to_string())?;

    let judge_provider_snapshot = adversarial_provider_snapshot(cfg, bursar);
    crate::adversarial::finalize_review(crate::adversarial::SynthesisRequest {
        authorized: &authorized,
        reviewer_run,
        roster: &cfg.roster,
        judge_provider_snapshot: &judge_provider_snapshot,
        exec,
        timeout,
        calls: &calls,
        ledger_path: &paths.ledger_path,
        deck_home: &paths.reports_home,
    })
    .map_err(|error| error.to_string())
}

fn adversarial_provider_snapshot<C: crate::bursar::BursarClient + ?Sized>(
    cfg: &crate::config::Config,
    bursar: &C,
) -> std::collections::BTreeMap<String, crate::bursar::BudgetDecision> {
    crate::bursar::evaluate_provider_snapshot(
        bursar,
        cfg.roster.iter().map(|entry| entry.provider.as_str()),
        cfg.budgets.use_bursar,
    )
}

fn adversarial_dispatch_result_exit_code(
    result: &Result<crate::adversarial::AdversarialRun, String>,
) -> ExitCode {
    match result {
        Ok(run)
            if run.outcome == crate::adversarial::ReviewLifecycleOutcome::Complete
                && run.synthesis.is_some() =>
        {
            ExitCode::SUCCESS
        }
        Ok(_) | Err(_) => ExitCode::from(1),
    }
}

fn new_adversarial_review_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("adversarial-{nanos}-{}", std::process::id())
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

fn print_help() {
    println!("{USAGE}");
    println!();
    println!("Commands:");
    println!("  adversarial-review  Plan or dispatch an approval-gated read-only design review");
    println!("  config check   Validate conductor.toml and run preflight checks");
    println!("  roster drift   Diff conductor.toml's roster against ~/.claude/model-scorecard.md");
    println!("  scan           Enumerate fleet repos and snapshot ready work");
    println!("  status         Show the most recently recorded cycle");
    println!("  cycle          Dry-run scan -> triage -> plan and publish a report");
    println!("  dispatch       Dispatch an approved cycle's ready items");
    println!("  arena run      Head-to-head harness/model arena on one bead");
    println!();
    println!("Notes:");
    println!("  adversarial-review dispatch exits 0 only for complete validated synthesis.");
    println!("  arena run applies the winning patch by default; pass --no-apply to opt out.");
    println!("  cycle --dry-run still writes a report file even though it makes no bd writes.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bursar::Availability;
    use crate::scan::{Freshness, RepoSnapshot, SkipReason, ZeroState};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn adversarial_plan_and_dispatch_parsers_enforce_exact_grammar() {
        let plan = parse_adversarial_plan_options(&[
            "--artifact".to_string(),
            "/tmp/design.md".to_string(),
            "--reviewers".to_string(),
            "2".to_string(),
            "--question".to_string(),
            "Should this ship?".to_string(),
            "--models".to_string(),
            "reviewer-one,reviewer-two".to_string(),
            "--config".to_string(),
            "/tmp/conductor.toml".to_string(),
        ])
        .expect("exact plan grammar");
        assert_eq!(plan.artifact, PathBuf::from("/tmp/design.md"));
        assert_eq!(plan.reviewers, 2);
        assert_eq!(plan.question, "Should this ship?");
        assert_eq!(
            plan.models,
            Some(vec!["reviewer-one".to_string(), "reviewer-two".to_string()])
        );
        assert_eq!(plan.config, PathBuf::from("/tmp/conductor.toml"));

        let dispatch = parse_adversarial_dispatch_options(&[
            "review-123".to_string(),
            "--config".to_string(),
            "/tmp/conductor.toml".to_string(),
        ])
        .expect("exact dispatch grammar");
        assert_eq!(dispatch.review_id, "review-123");
        assert_eq!(dispatch.config, PathBuf::from("/tmp/conductor.toml"));

        for invalid in [
            vec![],
            vec!["--artifact".to_string(), "/tmp/design.md".to_string()],
            vec![
                "--artifact".to_string(),
                "/tmp/design.md".to_string(),
                "--reviewers".to_string(),
                "0".to_string(),
            ],
            vec![
                "--artifact".to_string(),
                "/tmp/design.md".to_string(),
                "--reviewers".to_string(),
                "2".to_string(),
                "--models".to_string(),
                ",".to_string(),
            ],
            vec![
                "--artifact".to_string(),
                "/tmp/design.md".to_string(),
                "--reviewers".to_string(),
                "2".to_string(),
                "--wide".to_string(),
            ],
        ] {
            assert!(
                parse_adversarial_plan_options(&invalid).is_err(),
                "invalid plan parsed: {invalid:?}"
            );
        }
        assert!(parse_adversarial_dispatch_options(&[]).is_err());
        assert!(
            parse_adversarial_dispatch_options(&[
                "review-123".to_string(),
                "--artifact".to_string(),
                "/tmp/design.md".to_string(),
            ])
            .is_err()
        );
    }

    #[test]
    fn adversarial_usage_and_config_errors_exit_two() {
        assert_eq!(
            run(vec!["adversarial-review".to_string()]),
            ExitCode::from(2)
        );
        assert_eq!(
            run(vec![
                "adversarial-review".to_string(),
                "plan".to_string(),
                "--artifact".to_string(),
                "/tmp/design.md".to_string(),
            ]),
            ExitCode::from(2)
        );
        assert_eq!(
            run(vec![
                "adversarial-review".to_string(),
                "dispatch".to_string(),
                "review-123".to_string(),
                "--config".to_string(),
                "/definitely/missing/conductor.toml".to_string(),
            ]),
            ExitCode::from(2)
        );
    }

    #[test]
    fn adversarial_explicit_models_must_match_reviewer_count_before_state_write() {
        let fixture = AdversarialCliFixture::new("cli-explicit-count");
        let mut options = fixture.plan_options();
        options.models = Some(vec!["reviewer-one".to_string()]);

        let error = execute_adversarial_plan(
            &fixture.config,
            &options,
            &fixture.paths,
            &fixture.bursar,
            &NoopDeckValidator,
            "review-explicit-count",
            "2026-07-15T12:00:00Z",
        )
        .expect_err("one explicit model cannot fill two slots");

        assert!(error.contains("expected 2"));
        assert!(!fixture.paths.state_root.exists());
    }

    #[test]
    fn adversarial_reviewer_upper_bound_exits_two_before_state_write() {
        let fixture = AdversarialCliFixture::new("cli-reviewer-bound");
        let mut options = fixture.plan_options();
        options.reviewers = 4;
        options.models = None;

        let error = execute_adversarial_plan(
            &fixture.config,
            &options,
            &fixture.paths,
            &fixture.bursar,
            &NoopDeckValidator,
            "review-upper-bound",
            "2026-07-15T12:00:00Z",
        )
        .expect_err("configured reviewer maximum is enforced");

        assert!(error.contains("between 1 and 3"));
        assert!(!fixture.paths.state_root.exists());
        assert_eq!(
            run(vec![
                "adversarial-review".to_string(),
                "plan".to_string(),
                "--artifact".to_string(),
                fixture.artifact.display().to_string(),
                "--reviewers".to_string(),
                "4".to_string(),
                "--config".to_string(),
                fixture.config_path.display().to_string(),
            ]),
            ExitCode::from(2)
        );
    }

    #[test]
    fn adversarial_missing_approval_exits_one() {
        let fixture = AdversarialCliFixture::new("cli-approval-failure");
        let published = fixture.plan("review-approval-failure");
        let exec = CliReviewExec::default();
        let result = execute_adversarial_dispatch(
            &fixture.config,
            &AdversarialDispatchOptions {
                review_id: published.plan.review_id.clone(),
                config: fixture.config_path.clone(),
            },
            &fixture.paths,
            &fixture.bursar,
            &exec,
        );

        assert_eq!(
            adversarial_dispatch_result_exit_code(&result),
            ExitCode::from(1)
        );
        assert!(result.unwrap_err().contains("awaiting approval"));
        assert!(exec.spawns().is_empty());
    }

    #[test]
    fn adversarial_successful_dispatch_keeps_all_mutation_sentinels_untouched() {
        let fixture = AdversarialCliFixture::new("cli-no-mutation");
        let sentinels = fixture.seed_mutation_sentinels();
        let published = fixture.plan("review-no-mutation");
        fixture.approve(&published.plan);
        let exec = CliReviewExec::default();

        let result = execute_adversarial_dispatch(
            &fixture.config,
            &AdversarialDispatchOptions {
                review_id: published.plan.review_id.clone(),
                config: fixture.config_path.clone(),
            },
            &fixture.paths,
            &fixture.bursar,
            &exec,
        );

        assert_eq!(
            adversarial_dispatch_result_exit_code(&result),
            ExitCode::SUCCESS
        );
        let run = result.expect("approved fake dispatch completes");
        assert_eq!(
            run.outcome,
            crate::adversarial::ReviewLifecycleOutcome::Complete
        );
        assert!(run.synthesis.is_some());
        assert_eq!(run.report_path, published.report_path);
        assert_eq!(exec.spawns().len(), 3);
        assert!(fixture.paths.ledger_path.is_file());
        assert_eq!(
            std::fs::read_to_string(&fixture.artifact).unwrap(),
            "immutable design"
        );
        for (path, expected) in sentinels {
            assert_eq!(
                std::fs::read(&path).unwrap(),
                expected,
                "mutation sentinel changed: {}",
                path.display()
            );
        }
        for spawn in exec.spawns() {
            assert!(spawn.cwd.starts_with(&fixture.paths.state_root));
            assert!(!spawn.cwd.starts_with(&fixture.target_repo));
        }
    }

    #[test]
    fn adversarial_partial_dispatch_exits_one_without_spawning_judge() {
        let fixture = AdversarialCliFixture::new("cli-partial-exit");
        let published = fixture.plan("review-partial-exit");
        fixture.approve(&published.plan);
        let exec = CliReviewExec::malformed_reviewers();

        let result = execute_adversarial_dispatch(
            &fixture.config,
            &AdversarialDispatchOptions {
                review_id: published.plan.review_id.clone(),
                config: fixture.config_path.clone(),
            },
            &fixture.paths,
            &fixture.bursar,
            &exec,
        );

        assert_eq!(
            adversarial_dispatch_result_exit_code(&result),
            ExitCode::from(1)
        );
        let run = result.expect("reviewer schema failures produce a partial result");
        assert_eq!(
            run.outcome,
            crate::adversarial::ReviewLifecycleOutcome::Partial
        );
        assert!(run.synthesis.is_none());
        assert!(run.judge_attempt.is_none());
        assert_eq!(exec.spawns().len(), 4);
    }

    #[test]
    fn dispatch_rejects_scope_selectors_that_could_widen_an_approved_plan() {
        assert_eq!(
            run(vec![
                "dispatch".to_string(),
                "cycle-1".to_string(),
                "--repo".to_string(),
                "alpha".to_string(),
            ]),
            ExitCode::from(2)
        );
        assert_eq!(
            run(vec![
                "dispatch".to_string(),
                "cycle-1".to_string(),
                "--only".to_string(),
                "alpha:a-1".to_string(),
            ]),
            ExitCode::from(2)
        );
    }

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

    struct AdversarialCliFixture {
        _temp: CliTempDir,
        target_repo: PathBuf,
        artifact: PathBuf,
        config_path: PathBuf,
        config: crate::config::Config,
        paths: AdversarialPaths,
        bursar: crate::bursar::test_support::FakeBursarClient,
    }

    impl AdversarialCliFixture {
        fn new(label: &str) -> Self {
            let temp = CliTempDir::new(label);
            let target_repo = temp.path().join("target-repo");
            std::fs::create_dir_all(&target_repo).unwrap();
            let artifact = target_repo.join("design.md");
            std::fs::write(&artifact, b"immutable design").unwrap();
            let config_path = temp.path().join("conductor.toml");
            let config = crate::config::parse_str(
                r#"
[budgets]
use_bursar = true
item_wall_clock_mins = 1

[adversarial_review]
max_reviewers = 3
parallel = 2
judge = "judge"

[[roster]]
name = "reviewer-one"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "reviewer-one"
provider = "opencode-go"

[[roster]]
name = "reviewer-two"
tier = "lead"
ceiling = "L"
efficiency = "std"
backend = "pi"
dispatch_id = "reviewer-two"
provider = "agy"

[[roster]]
name = "judge"
tier = "lead"
ceiling = "XL"
efficiency = "heavy"
backend = "pi"
dispatch_id = "judge"
provider = "codex"
"#,
            )
            .unwrap();
            std::fs::write(
                &config_path,
                r#"
[budgets]
use_bursar = true
item_wall_clock_mins = 1

[adversarial_review]
max_reviewers = 3
parallel = 2
judge = "judge"

[[roster]]
name = "reviewer-one"
tier = "senior"
ceiling = "M"
efficiency = "lean"
backend = "pi"
dispatch_id = "reviewer-one"
provider = "opencode-go"

[[roster]]
name = "reviewer-two"
tier = "lead"
ceiling = "L"
efficiency = "std"
backend = "pi"
dispatch_id = "reviewer-two"
provider = "agy"

[[roster]]
name = "judge"
tier = "lead"
ceiling = "XL"
efficiency = "heavy"
backend = "pi"
dispatch_id = "judge"
provider = "codex"
"#,
            )
            .unwrap();
            let paths = AdversarialPaths {
                state_root: temp.path().join("state").join("adversarial-reviews"),
                reports_home: temp.path().join("reports-home"),
                ledger_path: temp.path().join("ledger").join("model-bench.jsonl"),
            };
            let bursar =
                crate::bursar::test_support::FakeBursarClient::with_provider_availabilities(&[
                    ("opencode-go", Availability::Healthy),
                    ("agy", Availability::Healthy),
                    ("codex", Availability::Healthy),
                ]);
            Self {
                _temp: temp,
                target_repo,
                artifact,
                config_path,
                config,
                paths,
                bursar,
            }
        }

        fn plan_options(&self) -> AdversarialPlanOptions {
            AdversarialPlanOptions {
                artifact: self.artifact.clone(),
                reviewers: 2,
                question: "Should this architecture proceed?".to_string(),
                models: Some(vec!["reviewer-one".to_string(), "reviewer-two".to_string()]),
                config: self.config_path.clone(),
            }
        }

        fn plan(&self, review_id: &str) -> crate::adversarial::PublishedApproval {
            execute_adversarial_plan(
                &self.config,
                &self.plan_options(),
                &self.paths,
                &self.bursar,
                &NoopDeckValidator,
                review_id,
                "2026-07-15T12:00:00Z",
            )
            .unwrap()
        }

        fn approve(&self, plan: &crate::adversarial::AdversarialReviewPlan) {
            let run_dir =
                crate::deck::report_run_dir(&self.paths.reports_home, &plan.review_id).unwrap();
            std::fs::write(
                run_dir.join("responses.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "version": 1,
                    "responses": {
                        crate::adversarial::approval_block_id(plan): {
                            "value": "approved",
                            "at": "2026-07-15T12:01:00Z"
                        }
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }

        fn seed_mutation_sentinels(&self) -> Vec<(PathBuf, Vec<u8>)> {
            [
                "beads.sentinel",
                "git.sentinel",
                "worktree.sentinel",
                "cycle.sentinel",
                "repository.sentinel",
                "chezmoi-apply.sentinel",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let path = self.target_repo.join(name);
                let bytes = format!("sentinel-{index}").into_bytes();
                std::fs::write(&path, &bytes).unwrap();
                (path, bytes)
            })
            .collect()
        }
    }

    struct CliTempDir(PathBuf);

    impl CliTempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "conductor-cli-{label}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for CliTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct NoopDeckValidator;

    impl crate::deck::DeckValidator for NoopDeckValidator {
        fn validate(&self, report_path: &Path) -> crate::deck::Result<()> {
            assert!(report_path.is_file());
            Ok(())
        }
    }

    #[derive(Default)]
    struct CliReviewExec {
        spawns: Mutex<Vec<crate::dispatch::SpawnRequest>>,
        malformed_reviewers: bool,
    }

    impl CliReviewExec {
        fn malformed_reviewers() -> Self {
            Self {
                spawns: Mutex::new(Vec::new()),
                malformed_reviewers: true,
            }
        }

        fn spawns(&self) -> Vec<crate::dispatch::SpawnRequest> {
            self.spawns.lock().unwrap().clone()
        }
    }

    impl crate::dispatch::Exec for CliReviewExec {
        fn spawn(
            &self,
            request: &crate::dispatch::SpawnRequest,
        ) -> crate::dispatch::Result<Box<dyn crate::dispatch::ChildProcess>> {
            let prompt_index = request
                .argv
                .iter()
                .position(|arg| arg == "-p")
                .expect("read-only prompt flag");
            let prompt = &request.argv[prompt_index + 1];
            let output = if prompt.contains("adversarial synthesis") {
                serde_json::json!({
                    "verdict": "conditional-go",
                    "consensus": ["preserve the boundary"],
                    "disagreements": [{
                        "topic": "timing",
                        "positions": [
                            {"reviewers": ["R1"], "position": "ship"},
                            {"reviewers": ["R2"], "position": "wait"}
                        ]
                    }],
                    "unique_risks": [{"reviewer": "R2", "risk": "timing"}],
                    "required_changes": ["document the boundary"],
                    "deferred_questions": ["none"],
                    "confidence": "high",
                    "coverage": ["R1", "R2"]
                })
                .to_string()
            } else if self.malformed_reviewers {
                "not-json".to_string()
            } else {
                serde_json::json!({
                    "verdict": "conditional-go",
                    "findings": [{
                        "id": "boundary",
                        "severity": "high",
                        "claim": "boundary required",
                        "evidence": "artifact",
                        "consequence": "drift",
                        "recommendation": "document it"
                    }],
                    "assumptions": ["artifact is authoritative"],
                    "scope_to_cut": ["migration"],
                    "recommended_sequencing": ["boundary first"]
                })
                .to_string()
            };
            std::fs::create_dir_all(request.stdout_path.parent().unwrap()).unwrap();
            std::fs::write(&request.stdout_path, output).unwrap();
            std::fs::write(&request.stderr_path, b"").unwrap();
            self.spawns.lock().unwrap().push(request.clone());
            Ok(Box::new(CliReviewChild))
        }
    }

    struct CliReviewChild;

    impl crate::dispatch::ChildProcess for CliReviewChild {
        fn wait_for(
            &mut self,
            _timeout: std::time::Duration,
        ) -> crate::dispatch::Result<Option<crate::dispatch::ProcessStatus>> {
            Ok(Some(crate::dispatch::ProcessStatus::code(0)))
        }

        fn terminate(&mut self) -> crate::dispatch::Result<()> {
            Ok(())
        }

        fn kill(&mut self) -> crate::dispatch::Result<()> {
            Ok(())
        }

        fn wait(&mut self) -> crate::dispatch::Result<crate::dispatch::ProcessStatus> {
            Ok(crate::dispatch::ProcessStatus::code(0))
        }
    }
}

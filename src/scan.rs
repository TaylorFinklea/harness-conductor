//! fleet enumeration (walk ~/git, .beads/metadata.json detection, exclusions, unborn-HEAD safe)
//!
//! Scans the configured root directory (depth 1, tilde-expanded) for beads repos,
//! applying exclusions (config list + hardcoded chezmoi-config deny). For each
//! beads repo, queries bd via the injected `&dyn BdClient` to gather ready list,
//! count, blocked items, and distinguishes the two zero-states (drained vs blocked).
//! Computes freshness from `.beads/last-touched` mtime (fresh/recent/stale buckets).
//! Marks repos with ANY `in_progress` issue as `SkippedInProgress` (invariant 4).
//!
//! Pure logic is separated from filesystem/bd IO so table-driven tests use fixtures/fakes.

#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::bd::{BdClient, Issue};
use crate::config::ScanConfig;

/// Freshness buckets for `.beads/last-touched` mtime.
///
/// - `Fresh`: touched within the last 24 hours
/// - `Recent`: touched within the last 7 days
/// - `Stale`: touched more than 7 days ago
/// - `Unknown`: no `.beads/last-touched` file or mtime unavailable
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) enum Freshness {
    Fresh,
    Recent,
    Stale,
    Unknown,
}

const FRESH_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours
const RECENT_THRESHOLD: Duration = Duration::from_secs(7 * 24 * 60 * 60); // 7 days

/// Why a repo was skipped from enumeration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) enum SkipReason {
    /// Repo has at least one `in_progress` issue (a human/agent may be mid-work).
    InProgress,
    /// Repo is in the config exclude list or is hardcoded-excluded.
    Excluded,
    /// Directory is not a beads repo (no `.beads/metadata.json`).
    NotBeadsRepo,
    /// Directory is not a git repo or has an unborn HEAD.
    NotGitRepo,
    /// `bd ready --json` parsed as invalid or schema-drifted JSON for this repo.
    ScanGap { command: String, message: String },
}

/// Zero-state distinction for repos with no ready work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) enum ZeroState {
    /// No open issues at all (drained).
    Drained,
    /// Open issues exist but all have blocking dependencies.
    Blocked,
    /// Not applicable (repo has ready work, was skipped, or is not a beads repo).
    NotApplicable,
}

/// Snapshot of a single repo's state during fleet enumeration.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct RepoSnapshot {
    /// Absolute path to the repo.
    pub(crate) path: PathBuf,
    /// Repo name (directory basename).
    pub(crate) name: String,
    /// Whether this is a beads repo (`.beads/metadata.json` exists).
    pub(crate) is_beads_repo: bool,
    /// Why the repo was skipped, if any.
    pub(crate) skip_reason: Option<SkipReason>,
    /// Ready issues from `bd ready --json`.
    pub(crate) ready: Vec<Issue>,
    /// Total open issue count from `bd count --json`.
    pub(crate) count: u64,
    /// Blocked issues from `bd blocked --json`.
    pub(crate) blocked: Vec<Issue>,
    /// Zero-state distinction (drained vs blocked).
    pub(crate) zero_state: ZeroState,
    /// Freshness from `.beads/last-touched` mtime.
    pub(crate) freshness: Freshness,
}

/// Scans the fleet under `config.root`, returning a snapshot for each directory.
///
/// The scan is depth-1: only immediate children of `root` are considered.
/// Tilde (`~`) in `root` is expanded to `$HOME`.
///
/// # Errors
///
/// Returns an error if `root` cannot be expanded or read.
pub(crate) fn scan(
    config: &ScanConfig,
    client: &dyn BdClient,
) -> Result<Vec<RepoSnapshot>, ScanError> {
    let root = expand_tilde(&config.root)?;

    if !root.is_dir() {
        return Err(ScanError::new(format!(
            "scan root {} is not a directory",
            root.display()
        )));
    }

    let exclude_set: HashSet<String> = config
        .exclude
        .iter()
        .cloned()
        .chain(
            crate::config::HARDCODED_EXCLUDE
                .iter()
                .map(|s| (*s).to_string()),
        )
        .collect();

    let mut snapshots = Vec::new();
    let entries = std::fs::read_dir(&root)
        .map_err(|e| ScanError::new(format!("cannot read {}: {e}", root.display())))?;

    for entry in entries {
        let entry = entry.map_err(|e| ScanError::new(format!("read_dir entry: {e}")))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .unwrap_or_default();

        if exclude_set.contains(&name) {
            snapshots.push(RepoSnapshot {
                path,
                name,
                is_beads_repo: false,
                skip_reason: Some(SkipReason::Excluded),
                ready: Vec::new(),
                count: 0,
                blocked: Vec::new(),
                zero_state: ZeroState::NotApplicable,
                freshness: Freshness::Unknown,
            });
            continue;
        }

        let snapshot = scan_repo(&path, &name, client);
        snapshots.push(snapshot);
    }

    snapshots.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(snapshots)
}

fn scan_repo(path: &Path, name: &str, client: &dyn BdClient) -> RepoSnapshot {
    let metadata_json = path.join(".beads").join("metadata.json");
    if !metadata_json.is_file() {
        return RepoSnapshot {
            path: path.to_path_buf(),
            name: name.to_string(),
            is_beads_repo: false,
            skip_reason: Some(SkipReason::NotBeadsRepo),
            ready: Vec::new(),
            count: 0,
            blocked: Vec::new(),
            zero_state: ZeroState::NotApplicable,
            freshness: Freshness::Unknown,
        };
    }

    // Check if it's a git repo (and handle unborn HEAD gracefully)
    if let Some(skip) = check_git_repo(path) {
        return RepoSnapshot {
            path: path.to_path_buf(),
            name: name.to_string(),
            is_beads_repo: true,
            skip_reason: Some(skip),
            ready: Vec::new(),
            count: 0,
            blocked: Vec::new(),
            zero_state: ZeroState::NotApplicable,
            freshness: read_freshness(path),
        };
    }

    // Query bd for ready, count, blocked
    let ready = match client.ready(path) {
        Ok(ready) => ready,
        Err(err) if err.is_json_parse() => {
            let message = err.to_string();
            eprintln!(
                "scan gap: {}: bd ready --json parse failed: {message}",
                path.display()
            );
            return RepoSnapshot {
                path: path.to_path_buf(),
                name: name.to_string(),
                is_beads_repo: true,
                skip_reason: Some(SkipReason::ScanGap {
                    command: "bd ready --json".to_string(),
                    message,
                }),
                ready: Vec::new(),
                count: 0,
                blocked: Vec::new(),
                zero_state: ZeroState::NotApplicable,
                freshness: read_freshness(path),
            };
        }
        Err(_) => Vec::new(),
    };
    let count = client.count(path).unwrap_or_default();
    let blocked = client.blocked(path).unwrap_or_default();

    // Check for in_progress issues (invariant 4)
    let has_in_progress = ready
        .iter()
        .chain(blocked.iter())
        .any(|issue| issue.status == "in_progress");

    if has_in_progress {
        return RepoSnapshot {
            path: path.to_path_buf(),
            name: name.to_string(),
            is_beads_repo: true,
            skip_reason: Some(SkipReason::InProgress),
            ready,
            count,
            blocked,
            zero_state: ZeroState::NotApplicable,
            freshness: read_freshness(path),
        };
    }

    // Determine zero-state
    let zero_state = if ready.is_empty() {
        if count == 0 {
            ZeroState::Drained
        } else if !blocked.is_empty() {
            ZeroState::Blocked
        } else {
            // count > 0 but no ready and no blocked: ambiguous, treat as drained
            ZeroState::Drained
        }
    } else {
        ZeroState::NotApplicable
    };

    RepoSnapshot {
        path: path.to_path_buf(),
        name: name.to_string(),
        is_beads_repo: true,
        skip_reason: None,
        ready,
        count,
        blocked,
        zero_state,
        freshness: read_freshness(path),
    }
}

/// Checks if a path is a valid git repo with a born HEAD.
/// Returns `Some(SkipReason::NotGitRepo)` if the repo should be skipped.
fn check_git_repo(path: &Path) -> Option<SkipReason> {
    let git_dir = path.join(".git");
    if !git_dir.exists() {
        return Some(SkipReason::NotGitRepo);
    }

    // Try to read HEAD to detect unborn repos
    let head_file = git_dir.join("HEAD");
    if head_file.is_file() {
        if let Ok(head_content) = std::fs::read_to_string(&head_file) {
            let head_content = head_content.trim();
            if head_content.starts_with("ref: ") {
                let ref_path = head_content.strip_prefix("ref: ").unwrap();
                let full_ref = git_dir.join(ref_path);
                if !full_ref.exists() {
                    // Unborn HEAD: ref doesn't exist yet
                    return Some(SkipReason::NotGitRepo);
                }
            }
        }
    }

    None
}

fn read_freshness(repo: &Path) -> Freshness {
    let last_touched = repo.join(".beads").join("last-touched");
    if !last_touched.exists() {
        return Freshness::Unknown;
    }

    let Ok(metadata) = std::fs::metadata(&last_touched) else {
        return Freshness::Unknown;
    };

    let Ok(modified) = metadata.modified() else {
        return Freshness::Unknown;
    };

    let now = SystemTime::now();
    let Ok(age) = now.duration_since(modified) else {
        return Freshness::Unknown;
    };

    if age < FRESH_THRESHOLD {
        Freshness::Fresh
    } else if age < RECENT_THRESHOLD {
        Freshness::Recent
    } else {
        Freshness::Stale
    }
}

fn expand_tilde(path: &str) -> Result<PathBuf, ScanError> {
    if !path.starts_with('~') {
        return Ok(PathBuf::from(path));
    }

    let home =
        std::env::var("HOME").map_err(|_| ScanError::new("HOME not set; cannot expand ~"))?;
    if home.is_empty() {
        return Err(ScanError::new("HOME is empty; cannot expand ~"));
    }

    let rest = path.strip_prefix("~/").unwrap_or(&path[1..]);
    Ok(PathBuf::from(home).join(rest))
}

#[derive(Debug)]
pub(crate) struct ScanError {
    message: String,
}

impl ScanError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ScanError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bd::{BdError, Comment, Issue};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    // --- Fake BdClient for testing ---

    struct FakeBdClient {
        ready: RefCell<HashMap<PathBuf, Vec<Issue>>>,
        ready_errors: RefCell<HashMap<PathBuf, BdError>>,
        count: RefCell<HashMap<PathBuf, u64>>,
        blocked: RefCell<HashMap<PathBuf, Vec<Issue>>>,
    }

    impl FakeBdClient {
        fn new() -> Self {
            Self {
                ready: RefCell::new(HashMap::new()),
                ready_errors: RefCell::new(HashMap::new()),
                count: RefCell::new(HashMap::new()),
                blocked: RefCell::new(HashMap::new()),
            }
        }

        fn set_ready(&self, repo: &Path, issues: Vec<Issue>) {
            self.ready.borrow_mut().insert(repo.to_path_buf(), issues);
        }

        fn set_ready_error(&self, repo: &Path, error: BdError) {
            self.ready_errors
                .borrow_mut()
                .insert(repo.to_path_buf(), error);
        }

        fn set_count(&self, repo: &Path, count: u64) {
            self.count.borrow_mut().insert(repo.to_path_buf(), count);
        }

        fn set_blocked(&self, repo: &Path, issues: Vec<Issue>) {
            self.blocked.borrow_mut().insert(repo.to_path_buf(), issues);
        }
    }

    impl BdClient for FakeBdClient {
        fn ready(&self, repo: &Path) -> crate::bd::Result<Vec<Issue>> {
            if let Some(error) = self.ready_errors.borrow().get(repo).cloned() {
                return Err(error);
            }
            self.ready
                .borrow()
                .get(repo)
                .cloned()
                .ok_or_else(|| BdError::new(format!("no ready data for {}", repo.display())))
        }

        fn show(&self, _repo: &Path, _id: &str) -> crate::bd::Result<Issue> {
            Err(BdError::new("show not implemented in fake"))
        }

        fn count(&self, repo: &Path) -> crate::bd::Result<u64> {
            self.count
                .borrow()
                .get(repo)
                .copied()
                .ok_or_else(|| BdError::new(format!("no count data for {}", repo.display())))
        }

        fn blocked(&self, repo: &Path) -> crate::bd::Result<Vec<Issue>> {
            self.blocked
                .borrow()
                .get(repo)
                .cloned()
                .ok_or_else(|| BdError::new(format!("no blocked data for {}", repo.display())))
        }

        fn claim(&self, _repo: &Path, _id: &str, _actor: &str) -> crate::bd::Result<Issue> {
            Err(BdError::new("claim not implemented in fake"))
        }

        fn release(&self, _repo: &Path, _id: &str) -> crate::bd::Result<Issue> {
            Err(BdError::new("release not implemented in fake"))
        }

        fn close(&self, _repo: &Path, _id: &str, _reason: &str) -> crate::bd::Result<Issue> {
            Err(BdError::new("close not implemented in fake"))
        }

        fn comment(&self, _repo: &Path, _id: &str, _text: &str) -> crate::bd::Result<Comment> {
            Err(BdError::new("comment not implemented in fake"))
        }

        fn set_metadata(
            &self,
            _repo: &Path,
            _id: &str,
            _key: &str,
            _value: &str,
        ) -> crate::bd::Result<Issue> {
            Err(BdError::new("set_metadata not implemented in fake"))
        }
    }

    // --- Test helpers ---

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("conductor-scan-{label}-{nanos}"));
            std::fs::create_dir_all(&path).expect("mkdir temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_issue(id: &str, status: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: format!("Issue {id}"),
            description: String::new(),
            acceptance_criteria: String::new(),
            notes: String::new(),
            status: status.to_string(),
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
        }
    }

    fn ready_json_error(output: &str) -> BdError {
        let err = serde_json::from_str::<Vec<Issue>>(output)
            .expect_err("fixture must fail as bd ready issue JSON");
        BdError::json("bd ready", &err)
    }

    fn assert_scan_gap(snapshot: &RepoSnapshot, expected_message: &str) {
        match &snapshot.skip_reason {
            Some(SkipReason::ScanGap { command, message }) => {
                assert_eq!(command, "bd ready --json");
                assert!(
                    message.contains(expected_message),
                    "scan gap message {message:?} did not contain {expected_message:?}"
                );
            }
            other => panic!("expected scan gap, got {other:?}"),
        }
    }

    fn init_git_repo(path: &Path) {
        let git_dir = path.join(".git");
        std::fs::create_dir_all(&git_dir).expect("mkdir .git");
        let head = git_dir.join("HEAD");
        std::fs::write(&head, "ref: refs/heads/main\n").expect("write HEAD");
        let refs_dir = git_dir.join("refs").join("heads");
        std::fs::create_dir_all(&refs_dir).expect("mkdir refs/heads");
        let main_ref = refs_dir.join("main");
        std::fs::write(&main_ref, "abc123\n").expect("write main ref");
    }

    fn init_beads_repo(path: &Path) {
        init_git_repo(path);
        let beads_dir = path.join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("mkdir .beads");
        let metadata = beads_dir.join("metadata.json");
        std::fs::write(&metadata, r#"{"backend":"dolt"}"#).expect("write metadata.json");
    }

    fn touch_last_touched(path: &Path, age: Duration) {
        let beads_dir = path.join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("mkdir .beads");
        let last_touched = beads_dir.join("last-touched");
        std::fs::write(&last_touched, b"").expect("write last-touched");

        // Set mtime to `age` ago
        let mtime = SystemTime::now() - age;
        let file = std::fs::File::open(&last_touched).expect("open last-touched");
        file.set_modified(mtime).expect("set modified time");
    }

    // --- Tests ---

    #[test]
    fn scan_detects_beads_repos_via_metadata_json() {
        let temp = TempDir::new("beads-detection");
        let root = temp.path();

        // Create a beads repo
        let repo1 = root.join("repo1");
        std::fs::create_dir_all(&repo1).expect("mkdir repo1");
        init_beads_repo(&repo1);

        // Create a non-beads dir
        let repo2 = root.join("repo2");
        std::fs::create_dir_all(&repo2).expect("mkdir repo2");

        // Create a non-beads git repo
        let repo3 = root.join("repo3");
        std::fs::create_dir_all(&repo3).expect("mkdir repo3");
        init_git_repo(&repo3);

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        client.set_ready(&repo1, vec![make_issue("r1-1", "open")]);
        client.set_count(&repo1, 1);
        client.set_blocked(&repo1, vec![]);

        let snapshots = scan(&config, &client).expect("scan succeeds");

        assert_eq!(snapshots.len(), 3);

        let r1 = snapshots.iter().find(|s| s.name == "repo1").unwrap();
        assert!(r1.is_beads_repo);
        assert_eq!(r1.skip_reason, None);
        assert_eq!(r1.ready.len(), 1);

        let r2 = snapshots.iter().find(|s| s.name == "repo2").unwrap();
        assert!(!r2.is_beads_repo);
        assert_eq!(r2.skip_reason, Some(SkipReason::NotBeadsRepo));

        let r3 = snapshots.iter().find(|s| s.name == "repo3").unwrap();
        assert!(!r3.is_beads_repo);
        assert_eq!(r3.skip_reason, Some(SkipReason::NotBeadsRepo));
    }

    #[test]
    fn scan_applies_config_exclusions() {
        let temp = TempDir::new("config-exclude");
        let root = temp.path();

        let repo1 = root.join("repo1");
        std::fs::create_dir_all(&repo1).expect("mkdir repo1");
        init_beads_repo(&repo1);

        let repo2 = root.join("repo2");
        std::fs::create_dir_all(&repo2).expect("mkdir repo2");
        init_beads_repo(&repo2);

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: vec!["repo2".to_string()],
        };

        let client = FakeBdClient::new();
        client.set_ready(&repo1, vec![]);
        client.set_count(&repo1, 0);
        client.set_blocked(&repo1, vec![]);

        let snapshots = scan(&config, &client).expect("scan succeeds");

        let r1 = snapshots.iter().find(|s| s.name == "repo1").unwrap();
        assert!(r1.is_beads_repo);
        assert_eq!(r1.skip_reason, None);

        let r2 = snapshots.iter().find(|s| s.name == "repo2").unwrap();
        assert_eq!(r2.skip_reason, Some(SkipReason::Excluded));
    }

    #[test]
    fn scan_applies_hardcoded_chezmoi_config_exclusion() {
        let temp = TempDir::new("hardcoded-exclude");
        let root = temp.path();

        let chezmoi = root.join("chezmoi-config");
        std::fs::create_dir_all(&chezmoi).expect("mkdir chezmoi-config");
        init_beads_repo(&chezmoi);

        let repo1 = root.join("repo1");
        std::fs::create_dir_all(&repo1).expect("mkdir repo1");
        init_beads_repo(&repo1);

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        client.set_ready(&repo1, vec![]);
        client.set_count(&repo1, 0);
        client.set_blocked(&repo1, vec![]);

        let snapshots = scan(&config, &client).expect("scan succeeds");

        let chez = snapshots
            .iter()
            .find(|s| s.name == "chezmoi-config")
            .unwrap();
        assert_eq!(chez.skip_reason, Some(SkipReason::Excluded));

        let r1 = snapshots.iter().find(|s| s.name == "repo1").unwrap();
        assert!(r1.is_beads_repo);
        assert_eq!(r1.skip_reason, None);
    }

    #[test]
    fn scan_handles_unborn_head() {
        let temp = TempDir::new("unborn-head");
        let root = temp.path();

        let repo = root.join("unborn");
        std::fs::create_dir_all(&repo).expect("mkdir unborn");
        init_git_repo(&repo);
        // Remove the ref file to simulate unborn HEAD
        let refs_dir = repo.join(".git").join("refs").join("heads");
        std::fs::remove_dir_all(&refs_dir).expect("remove refs/heads");

        // Add .beads/metadata.json
        let beads_dir = repo.join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("mkdir .beads");
        let metadata = beads_dir.join("metadata.json");
        std::fs::write(&metadata, r#"{"backend":"dolt"}"#).expect("write metadata.json");

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        let snapshots = scan(&config, &client).expect("scan succeeds");

        let r = snapshots.iter().find(|s| s.name == "unborn").unwrap();
        assert!(r.is_beads_repo);
        assert_eq!(r.skip_reason, Some(SkipReason::NotGitRepo));
    }

    #[test]
    fn scan_distinguishes_drained_vs_blocked_zero_states() {
        let temp = TempDir::new("zero-states");
        let root = temp.path();

        // Drained: no open issues
        let drained = root.join("drained");
        std::fs::create_dir_all(&drained).expect("mkdir drained");
        init_beads_repo(&drained);

        // Blocked: open issues but all have blocking deps
        let blocked = root.join("blocked");
        std::fs::create_dir_all(&blocked).expect("mkdir blocked");
        init_beads_repo(&blocked);

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        client.set_ready(&drained, vec![]);
        client.set_count(&drained, 0);
        client.set_blocked(&drained, vec![]);

        client.set_ready(&blocked, vec![]);
        client.set_count(&blocked, 2);
        client.set_blocked(&blocked, vec![make_issue("b1", "open")]);

        let snapshots = scan(&config, &client).expect("scan succeeds");

        let d = snapshots.iter().find(|s| s.name == "drained").unwrap();
        assert_eq!(d.zero_state, ZeroState::Drained);
        assert_eq!(d.count, 0);
        assert!(d.ready.is_empty());

        let b = snapshots.iter().find(|s| s.name == "blocked").unwrap();
        assert_eq!(b.zero_state, ZeroState::Blocked);
        assert_eq!(b.count, 2);
        assert!(b.ready.is_empty());
        assert_eq!(b.blocked.len(), 1);
    }

    #[test]
    fn scan_skips_repos_with_in_progress_issues() {
        let temp = TempDir::new("in-progress");
        let root = temp.path();

        let repo = root.join("inprogress");
        std::fs::create_dir_all(&repo).expect("mkdir inprogress");
        init_beads_repo(&repo);

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        client.set_ready(
            &repo,
            vec![make_issue("ip1", "in_progress"), make_issue("ip2", "open")],
        );
        client.set_count(&repo, 2);
        client.set_blocked(&repo, vec![]);

        let snapshots = scan(&config, &client).expect("scan succeeds");

        let r = snapshots.iter().find(|s| s.name == "inprogress").unwrap();
        assert_eq!(r.skip_reason, Some(SkipReason::InProgress));
        assert_eq!(r.ready.len(), 2);
        assert_eq!(r.count, 2);
    }

    #[test]
    fn scan_handles_missing_last_touched() {
        let temp = TempDir::new("missing-last-touched");
        let root = temp.path();

        let repo = root.join("no-touch");
        std::fs::create_dir_all(&repo).expect("mkdir no-touch");
        init_beads_repo(&repo);
        // Don't create .beads/last-touched

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        client.set_ready(&repo, vec![]);
        client.set_count(&repo, 0);
        client.set_blocked(&repo, vec![]);

        let snapshots = scan(&config, &client).expect("scan succeeds");

        let r = snapshots.iter().find(|s| s.name == "no-touch").unwrap();
        assert_eq!(r.freshness, Freshness::Unknown);
    }

    #[test]
    fn scan_computes_freshness_buckets() {
        let temp = TempDir::new("freshness");
        let root = temp.path();

        // Fresh: < 24 hours
        let fresh = root.join("fresh");
        std::fs::create_dir_all(&fresh).expect("mkdir fresh");
        init_beads_repo(&fresh);
        touch_last_touched(&fresh, Duration::from_secs(60 * 60)); // 1 hour ago

        // Recent: < 7 days
        let recent = root.join("recent");
        std::fs::create_dir_all(&recent).expect("mkdir recent");
        init_beads_repo(&recent);
        touch_last_touched(&recent, Duration::from_secs(2 * 24 * 60 * 60)); // 2 days ago

        // Stale: > 7 days
        let stale = root.join("stale");
        std::fs::create_dir_all(&stale).expect("mkdir stale");
        init_beads_repo(&stale);
        touch_last_touched(&stale, Duration::from_secs(10 * 24 * 60 * 60)); // 10 days ago

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        for repo in [&fresh, &recent, &stale] {
            client.set_ready(repo, vec![]);
            client.set_count(repo, 0);
            client.set_blocked(repo, vec![]);
        }

        let snapshots = scan(&config, &client).expect("scan succeeds");

        let f = snapshots.iter().find(|s| s.name == "fresh").unwrap();
        assert_eq!(f.freshness, Freshness::Fresh);

        let r = snapshots.iter().find(|s| s.name == "recent").unwrap();
        assert_eq!(r.freshness, Freshness::Recent);

        let s = snapshots.iter().find(|s| s.name == "stale").unwrap();
        assert_eq!(s.freshness, Freshness::Stale);
    }

    #[test]
    fn scan_expands_tilde_in_root() {
        let home = std::env::var("HOME").expect("HOME set");
        let temp = TempDir::new("tilde");
        let root = temp.path();

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_beads_repo(&repo);

        // Create a symlink from $HOME/test-scan-root to our temp dir
        let link = PathBuf::from(&home).join("test-scan-root");
        let _ = std::fs::remove_file(&link); // clean up any prior test
        std::os::unix::fs::symlink(root, &link).expect("symlink");

        let config = ScanConfig {
            root: "~/test-scan-root".to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        client.set_ready(&repo, vec![]);
        client.set_count(&repo, 0);
        client.set_blocked(&repo, vec![]);

        let result = scan(&config, &client);
        let _ = std::fs::remove_file(&link); // clean up

        let snapshots = result.expect("scan succeeds");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].name, "repo");
    }

    #[test]
    fn scan_rejects_nonexistent_root() {
        let config = ScanConfig {
            root: "/nonexistent/path/that/does/not/exist".to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        let result = scan(&config, &client);
        assert!(result.is_err());
    }

    #[test]
    fn scan_sorts_results_by_name() {
        let temp = TempDir::new("sort");
        let root = temp.path();

        for name in ["zebra", "alpha", "middle"] {
            let repo = root.join(name);
            std::fs::create_dir_all(&repo).expect("mkdir");
            init_beads_repo(&repo);
        }

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        for name in ["zebra", "alpha", "middle"] {
            let repo = root.join(name);
            client.set_ready(&repo, vec![]);
            client.set_count(&repo, 0);
            client.set_blocked(&repo, vec![]);
        }

        let snapshots = scan(&config, &client).expect("scan succeeds");
        let names: Vec<&str> = snapshots.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn scan_handles_non_directory_entries() {
        let temp = TempDir::new("non-dir");
        let root = temp.path();

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_beads_repo(&repo);

        // Create a file (not a directory)
        let file = root.join("not-a-dir.txt");
        std::fs::write(&file, b"hello").expect("write file");

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        client.set_ready(&repo, vec![]);
        client.set_count(&repo, 0);
        client.set_blocked(&repo, vec![]);

        let snapshots = scan(&config, &client).expect("scan succeeds");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].name, "repo");
    }

    #[test]
    fn scan_flags_malformed_ready_json_as_gap_and_keeps_other_repos() {
        let temp = TempDir::new("malformed-ready-json");
        let root = temp.path();

        let bad = root.join("bad");
        std::fs::create_dir_all(&bad).expect("mkdir bad");
        init_beads_repo(&bad);

        let good = root.join("good");
        std::fs::create_dir_all(&good).expect("mkdir good");
        init_beads_repo(&good);

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        client.set_ready_error(&bad, ready_json_error("{"));
        client.set_ready(&good, vec![make_issue("g1", "open")]);
        client.set_count(&good, 1);
        client.set_blocked(&good, vec![]);

        let snapshots = scan(&config, &client).expect("scan succeeds despite one parse gap");

        let bad_snapshot = snapshots.iter().find(|s| s.name == "bad").unwrap();
        assert!(bad_snapshot.is_beads_repo);
        assert_scan_gap(bad_snapshot, "failed to parse JSON from `bd ready`");
        assert!(bad_snapshot.ready.is_empty());
        assert_eq!(bad_snapshot.zero_state, ZeroState::NotApplicable);

        let good_snapshot = snapshots.iter().find(|s| s.name == "good").unwrap();
        assert_eq!(good_snapshot.skip_reason, None);
        assert_eq!(good_snapshot.ready.len(), 1);
    }

    #[test]
    fn scan_flags_valid_ready_schema_drift_as_gap_not_drained() {
        let temp = TempDir::new("schema-drift-ready-json");
        let root = temp.path();

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_beads_repo(&repo);

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        client.set_ready_error(
            &repo,
            ready_json_error(
                r#"[{"title":"missing id","status":"open","priority":1,"issue_type":"task","owner":"test","created_at":"2026-01-01T00:00:00Z","created_by":"test","updated_at":"2026-01-01T00:00:00Z"}]"#,
            ),
        );

        let snapshots = scan(&config, &client).expect("scan succeeds despite schema gap");

        let snapshot = snapshots.iter().find(|s| s.name == "repo").unwrap();
        assert_scan_gap(snapshot, "missing field `id`");
        assert!(snapshot.ready.is_empty());
        assert_eq!(snapshot.zero_state, ZeroState::NotApplicable);

        let json = serde_json::to_value(snapshot).expect("snapshot serializes");
        assert_eq!(json["skip_reason"]["ScanGap"]["command"], "bd ready --json");
    }

    #[test]
    fn scan_survives_bd_errors() {
        let temp = TempDir::new("bd-errors");
        let root = temp.path();

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_beads_repo(&repo);

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        // Don't set any data in the fake client - all bd calls will error
        let client = FakeBdClient::new();

        let snapshots = scan(&config, &client).expect("scan succeeds despite bd errors");

        let r = snapshots.iter().find(|s| s.name == "repo").unwrap();
        assert!(r.is_beads_repo);
        assert_eq!(r.skip_reason, None);
        assert_eq!(r.ready.len(), 0);
        assert_eq!(r.count, 0);
        assert_eq!(r.blocked.len(), 0);
    }

    #[test]
    fn scan_handles_repos_with_ready_work() {
        let temp = TempDir::new("ready-work");
        let root = temp.path();

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_beads_repo(&repo);

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        client.set_ready(
            &repo,
            vec![
                make_issue("r1", "open"),
                make_issue("r2", "open"),
                make_issue("r3", "open"),
            ],
        );
        client.set_count(&repo, 5);
        client.set_blocked(
            &repo,
            vec![make_issue("b1", "open"), make_issue("b2", "open")],
        );

        let snapshots = scan(&config, &client).expect("scan succeeds");

        let r = snapshots.iter().find(|s| s.name == "repo").unwrap();
        assert!(r.is_beads_repo);
        assert_eq!(r.skip_reason, None);
        assert_eq!(r.ready.len(), 3);
        assert_eq!(r.count, 5);
        assert_eq!(r.blocked.len(), 2);
        assert_eq!(r.zero_state, ZeroState::NotApplicable);
    }

    #[test]
    fn scan_handles_in_progress_in_blocked_list() {
        let temp = TempDir::new("in-progress-blocked");
        let root = temp.path();

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_beads_repo(&repo);

        let config = ScanConfig {
            root: root.display().to_string(),
            exclude: Vec::new(),
        };

        let client = FakeBdClient::new();
        client.set_ready(&repo, vec![make_issue("r1", "open")]);
        client.set_count(&repo, 2);
        client.set_blocked(&repo, vec![make_issue("b1", "in_progress")]);

        let snapshots = scan(&config, &client).expect("scan succeeds");

        let r = snapshots.iter().find(|s| s.name == "repo").unwrap();
        assert_eq!(r.skip_reason, Some(SkipReason::InProgress));
    }
}

//! Dirty-tree quarantine for failed or timed-out worker attempts.
//!
//! A worker can modify tracked files and create untracked files before
//! exiting nonzero, timing out, or losing provider capacity — all without
//! producing an accepted commit. Left alone, that leaves the real
//! repository dirty and strands both the partial work and every later
//! Conductor cycle targeting the same repo. This module captures that
//! uncommitted state as a hashed, immutable run artifact and restores the
//! repository to its exact pre-attempt state, but only when ownership can
//! be authenticated: HEAD must still match the value recorded before the
//! attempt started, and the post-restore state must itself be proven
//! clean. Any doubt fails closed and leaves the tree untouched.

#![allow(dead_code)]

use std::fmt;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use fs2::FileExt as _;
use sha2::{Digest, Sha256};

use crate::dispatch::CommitProbe;
use crate::run::{ArtifactRef, RunHandle, RunJob, RunLifecycle, RunManifest};

pub(crate) type Result<T> = std::result::Result<T, QuarantineError>;

/// Bounds how many changed paths are recorded as run evidence. The
/// repository itself is never bounded — only the metadata durably recorded
/// about it, so a run with thousands of stray files cannot blow up the
/// event log.
const MAX_RECORDED_PATHS: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuarantineError {
    /// HEAD moved since the attempt started (a real commit may be present).
    /// It is never safe to reset or clean in this state.
    HeadMoved {
        expected: Option<String>,
        found: Option<String>,
    },
    /// Repository state could not be read, or the dirty patch could not be
    /// captured. No destructive operation is attempted in this case.
    CaptureFailed(String),
    /// The patch was captured, but the restore step could not be proven to
    /// have returned the repository to a clean state at `before_head`. The
    /// captured evidence is not lost even when this happens.
    CleanupUnproven(String),
}

impl fmt::Display for QuarantineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeadMoved { expected, found } => write!(
                f,
                "repository HEAD moved since the attempt started (expected {}, found {}); refusing to touch the working tree",
                expected.as_deref().unwrap_or("<none>"),
                found.as_deref().unwrap_or("<none>"),
            ),
            Self::CaptureFailed(message) => {
                write!(f, "failed to capture dirty repository state: {message}")
            }
            Self::CleanupUnproven(message) => {
                write!(f, "cleanup could not be proven complete: {message}")
            }
        }
    }
}

impl std::error::Error for QuarantineError {}

/// Outcome of one quarantine attempt: either the tree was already clean
/// (a no-op, so replaying quarantine on a clean tree is safe), or dirty
/// state was captured and the tree was restored.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct QuarantineCapture {
    pub(crate) artifact: Option<ArtifactRef>,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) truncated: bool,
}

impl QuarantineCapture {
    pub(crate) fn is_noop(&self) -> bool {
        self.artifact.is_none()
    }

    /// Bounded, human-readable summary safe to place in run event outcomes
    /// — counts and paths only, never patch content.
    pub(crate) fn summary(&self) -> String {
        let Some(artifact) = self.artifact.as_ref() else {
            return "clean".to_string();
        };
        format!(
            "quarantined {} path(s){} into {}#{}",
            self.changed_paths.len(),
            if self.truncated { " (truncated)" } else { "" },
            artifact.path,
            &artifact.sha256[..12],
        )
    }
}

/// Destructive repository mutation seam used only by quarantine recovery.
/// Kept separate from [`CommitProbe`] (a read-only seam many existing fakes
/// already implement) so these methods stay narrowly scoped to the one
/// place that is allowed to discard working-tree state.
pub(crate) trait RepoRecovery {
    /// Tracked and untracked (non-ignored) changed paths, repo-relative.
    fn changed_paths(&self, repo: &Path) -> std::result::Result<Vec<String>, String>;
    /// A single patch covering every tracked and untracked change.
    fn capture_patch(&self, repo: &Path) -> std::result::Result<Vec<u8>, String>;
    /// Discards tracked modifications and removes untracked (non-ignored)
    /// files, leaving the tree exactly at HEAD.
    fn restore_clean(&self, repo: &Path) -> std::result::Result<(), String>;
    /// Re-applies a patch previously produced by `capture_patch` onto the
    /// working tree (without touching the index). Used only to recover the
    /// pre-clean dirty state when `restore_clean` itself fails partway.
    /// `excluding` lists paths to leave untouched — a partial `restore_clean`
    /// failure can leave some paths already reflecting non-HEAD state (an
    /// untracked file `clean` never reached, or a tracked file `reset` never
    /// reached), and reapplying their hunks would either conflict (`apply`
    /// refuses to recreate a file that already exists) or corrupt content
    /// that is already correct.
    fn apply_patch(
        &self,
        repo: &Path,
        patch: &[u8],
        excluding: &[String],
    ) -> std::result::Result<(), String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GitRepoRecovery;

impl RepoRecovery for GitRepoRecovery {
    fn changed_paths(&self, repo: &Path) -> std::result::Result<Vec<String>, String> {
        let raw = git_bytes(
            repo,
            &["status", "--porcelain", "-z", "--untracked-files=normal"],
        )?;
        parse_porcelain_z(&raw)
    }

    fn capture_patch(&self, repo: &Path) -> std::result::Result<Vec<u8>, String> {
        let mut patch = git_bytes(repo, &["diff", "HEAD", "--binary", "--"])?;
        let untracked_raw = git_bytes(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
        for path in split_nul_paths(&untracked_raw)? {
            let file_patch = git_bytes_diff_no_index(
                repo,
                &[
                    "diff",
                    "--no-index",
                    "--binary",
                    "--",
                    "/dev/null",
                    path.as_str(),
                ],
            )?;
            // An untracked file always produces at least a "new file mode"
            // header, even when the file is empty — so an empty result here
            // means the path could not actually be diffed (e.g. it no longer
            // exists, or git treated it as a different literal path than the
            // one we asked for). Silently continuing would let `restore_clean`
            // delete a file whose content was never captured.
            if file_patch.is_empty() {
                return Err(format!(
                    "captured an empty diff for untracked path {path:?}; refusing to discard it"
                ));
            }
            patch.extend_from_slice(&file_patch);
        }
        Ok(patch)
    }

    fn restore_clean(&self, repo: &Path) -> std::result::Result<(), String> {
        git_text(repo, &["reset", "--hard", "HEAD"])?;
        git_text(repo, &["clean", "-fd"])?;
        Ok(())
    }

    fn apply_patch(
        &self,
        repo: &Path,
        patch: &[u8],
        excluding: &[String],
    ) -> std::result::Result<(), String> {
        git_apply_stdin(repo, patch, excluding)
    }
}

/// Parses `git status --porcelain -z` output into a flat list of
/// repo-relative paths. `-z` disables C-style quoting entirely, so a path
/// containing spaces, quotes, or non-ASCII bytes round-trips exactly instead
/// of arriving as an escaped string that no longer matches any real file on
/// disk. Rename/copy entries (`R`/`C` status) carry a second NUL-delimited
/// token for their origin path, which is consumed and discarded here since
/// only the current path is a "changed path" going forward.
fn parse_porcelain_z(raw: &[u8]) -> std::result::Result<Vec<String>, String> {
    let mut paths = Vec::new();
    let mut tokens = raw
        .split(|&byte| byte == 0)
        .filter(|token| !token.is_empty());
    while let Some(token) = tokens.next() {
        let text = std::str::from_utf8(token).map_err(|error| {
            format!(
                "git status reported a non-UTF-8 path ({error}); refusing to guess at its \
                 identity and leaving the repository completely untouched — this path requires \
                 manual `git status`/`git diff` inspection and recovery outside Conductor"
            )
        })?;
        if text.len() < 3 {
            return Err(format!(
                "git status produced a malformed porcelain entry: {text:?}"
            ));
        }
        let (status, rest) = text.split_at(2);
        let path = rest.strip_prefix(' ').unwrap_or(rest);
        if path.is_empty() {
            return Err("git status produced an empty path".to_string());
        }
        paths.push(path.to_string());
        if (status.contains('R') || status.contains('C')) && tokens.next().is_none() {
            return Err(format!(
                "git status rename/copy entry for {path:?} is missing its origin path"
            ));
        }
    }
    Ok(paths)
}

/// Splits `-z`-delimited raw output (e.g. `git ls-files -z`) into owned,
/// exact-byte-for-byte repo-relative path strings — no C-quoting to undo.
fn split_nul_paths(raw: &[u8]) -> std::result::Result<Vec<String>, String> {
    raw.split(|&byte| byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| {
            std::str::from_utf8(chunk).map(str::to_string).map_err(|error| {
                format!(
                    "git reported a non-UTF-8 path ({error}); refusing to guess at its identity \
                     and leaving the repository completely untouched — this path requires \
                     manual `git status`/`git diff` inspection and recovery outside Conductor"
                )
            })
        })
        .collect()
}

fn run_git(repo: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

fn git_text(repo: &Path, args: &[&str]) -> std::result::Result<String, String> {
    git_bytes(repo, args).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn git_bytes(repo: &Path, args: &[&str]) -> std::result::Result<Vec<u8>, String> {
    let output = run_git(repo, args).map_err(|error| {
        format!(
            "failed to run git {} in {}: {error}",
            args.join(" "),
            repo.display()
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

/// `git diff --no-index` exits 1 when the compared paths differ — the
/// expected outcome for every untracked file — and only above 1 on a real
/// error, so exit code 1 is accepted here unlike every other git call.
fn git_bytes_diff_no_index(repo: &Path, args: &[&str]) -> std::result::Result<Vec<u8>, String> {
    let output = run_git(repo, args).map_err(|error| {
        format!(
            "failed to run git {} in {}: {error}",
            args.join(" "),
            repo.display()
        )
    })?;
    match output.status.code() {
        Some(0 | 1) => Ok(output.stdout),
        _ => Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Re-applies a previously captured patch (tracked + untracked, `--binary`)
/// to the working tree only — never the index — so untracked files come
/// back untracked and tracked edits come back unstaged, mirroring how
/// `capture_patch` originally observed them.
fn git_apply_stdin(
    repo: &Path,
    patch: &[u8],
    excluding: &[String],
) -> std::result::Result<(), String> {
    let mut args = vec![
        "apply".to_string(),
        "--binary".to_string(),
        "--whitespace=nowarn".to_string(),
    ];
    args.extend(excluding.iter().map(|path| format!("--exclude={path}")));
    args.push("-".to_string());
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to spawn git apply in {}: {error}", repo.display()))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "git apply stdin was not piped".to_string())?
        .write_all(patch)
        .map_err(|error| format!("failed to write patch to git apply stdin: {error}"))?;
    let output = child.wait_with_output().map_err(|error| {
        format!(
            "failed to wait for git apply in {}: {error}",
            repo.display()
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "git apply failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Captures a worker's uncommitted changes (if any) as a hashed run
/// artifact and restores the repository to `before_head`.
///
/// Returns `Ok(QuarantineCapture::default())` — a no-op — when the tree is
/// already clean, so calling this repeatedly on an already-restored tree is
/// safe. Never mutates the tree unless HEAD still matches `before_head`;
/// never reports success unless the post-restore tree is proven clean and
/// still at `before_head`.
pub(crate) fn quarantine_dirty_attempt<C, R>(
    repo: &Path,
    commits: &C,
    recovery: &R,
    run_artifacts: &RunHandle,
    before_head: Option<&str>,
    artifact_label: &str,
) -> Result<QuarantineCapture>
where
    C: CommitProbe + ?Sized,
    R: RepoRecovery + ?Sized,
{
    let current_head = commits
        .head(repo)
        .map_err(|error| QuarantineError::CaptureFailed(format!("git head: {error}")))?;
    if current_head.as_deref() != before_head {
        return Err(QuarantineError::HeadMoved {
            expected: before_head.map(str::to_string),
            found: current_head,
        });
    }
    let is_clean = commits
        .is_clean(repo)
        .map_err(|error| QuarantineError::CaptureFailed(format!("git status: {error}")))?;
    if is_clean {
        return Ok(QuarantineCapture::default());
    }

    let full_changed_paths = recovery
        .changed_paths(repo)
        .map_err(QuarantineError::CaptureFailed)?;
    let truncated = full_changed_paths.len() > MAX_RECORDED_PATHS;
    let mut changed_paths = full_changed_paths.clone();
    changed_paths.truncate(MAX_RECORDED_PATHS);

    let patch = recovery
        .capture_patch(repo)
        .map_err(QuarantineError::CaptureFailed)?;
    if patch.is_empty() {
        return Err(QuarantineError::CaptureFailed(
            "repository is dirty but no patch content could be captured".to_string(),
        ));
    }
    let artifact = write_patch_artifact(run_artifacts, artifact_label, &patch)
        .map_err(QuarantineError::CaptureFailed)?;

    if let Err(error) = recovery.restore_clean(repo) {
        return Err(QuarantineError::CleanupUnproven(transactional_diagnostic(
            &error,
            recovery,
            repo,
            &patch,
            &full_changed_paths,
        )));
    }

    let post_head = commits.head(repo).map_err(|error| {
        QuarantineError::CleanupUnproven(format!("git head after restore: {error}"))
    })?;
    let post_clean = commits.is_clean(repo).map_err(|error| {
        QuarantineError::CleanupUnproven(format!("git status after restore: {error}"))
    })?;
    if post_head.as_deref() != before_head || !post_clean {
        let symptom = if post_head.as_deref() == before_head {
            "repository is still dirty after restore".to_string()
        } else {
            format!(
                "HEAD changed during restore: expected {}, found {}",
                before_head.unwrap_or("<none>"),
                post_head.as_deref().unwrap_or("<none>"),
            )
        };
        return Err(QuarantineError::CleanupUnproven(transactional_diagnostic(
            &symptom,
            recovery,
            repo,
            &patch,
            &full_changed_paths,
        )));
    }

    Ok(QuarantineCapture {
        artifact: Some(artifact),
        changed_paths,
        truncated,
    })
}

/// Holds an exclusive, repo-scoped advisory lease for the duration of a
/// capture/restore. A stable guard file in the repository's common Git
/// directory (or under `<state_dir>/leases/` for synthetic identities) carries
/// a kernel whole-file lock, so exactly one process can inspect or replace the
/// separately committed owner generation across state directories and linked
/// worktrees. Keeping the guard inode stable is what makes dead-holder reclaim
/// single-winner: no contender ever unlinks the path another holder has locked.
///
/// The owner record is written to a private staging file and atomically
/// renamed into place only after it is complete. A process crash releases the
/// kernel lock automatically; the next process either sees the complete dead
/// generation or safely discards an uncommitted staging file. Clean `Drop`
/// removes only the exact generation this value installed while the guard is
/// still locked.
#[derive(Debug)]
pub(crate) struct RepoLease {
    guard: std::fs::File,
    owner_path: PathBuf,
    generation: String,
}

impl RepoLease {
    /// Acquires the lease, reclaiming it exactly once if the recorded holder
    /// process is provably dead. A lease file surviving its holder's crash
    /// (killed before reaching the `Drop` release) would otherwise wedge
    /// every future recovery attempt against this repo forever — an
    /// unrecoverable state a purely advisory, `O_EXCL`-only lock cannot
    /// escape on its own. Reclaim only fires when the recorded `pid` can be
    /// read and is confirmed not running; an unparseable, unauthenticated, or
    /// unreadable holder record is treated as still-held (ambiguous, not
    /// provably dead), so it never auto-reclaims. The stable guard's
    /// nonblocking kernel lock is the atomic single-winner primitive shared by
    /// ordinary acquisition and dead-holder reclaim.
    #[expect(
        clippy::too_many_lines,
        reason = "lease reclaim keeps lock, liveness, and crash-atomic owner transitions together"
    )]
    pub(crate) fn acquire(
        state_dir: &Path,
        canonical_repo: &str,
        holder_run_id: &str,
    ) -> Result<Self> {
        let location = lease_location(state_dir, canonical_repo)?;
        let leases_dir = &location.leases_dir;
        let lease_identity = &location.identity;
        std::fs::create_dir_all(leases_dir).map_err(|error| {
            QuarantineError::CaptureFailed(format!(
                "failed to create leases dir {}: {error}",
                leases_dir.display()
            ))
        })?;
        let key = lease_key(lease_identity);
        let path = leases_dir.join(format!("{key}.lock"));
        let owner_path = leases_dir.join(format!("{key}.owner"));
        let pending_path = leases_dir.join(format!("{key}.owner.pending"));
        let mut guard = open_private_lock(&path).map_err(|error| {
            QuarantineError::CaptureFailed(format!(
                "failed to open repository lease guard at {}: {error}",
                path.display()
            ))
        })?;

        #[cfg(test)]
        wait_at_test_reclaim_gate();
        match guard.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if error.kind() == fs2::lock_contended_error().kind() => {
                return Err(QuarantineError::CaptureFailed(format!(
                    "repository lease for {canonical_repo} is already held; refusing to touch a \
                     repo another Conductor operation may currently be using"
                )));
            }
            Err(error) => {
                return Err(QuarantineError::CaptureFailed(format!(
                    "failed to lock repository lease guard at {}: {error}",
                    path.display()
                )));
            }
        }

        #[cfg(test)]
        abort_at_test_lease_phase("after_guard_lock");

        let (holder, owner_exists) = match std::fs::read_to_string(&owner_path) {
            Ok(holder) => (holder, true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                guard.seek(SeekFrom::Start(0)).map_err(|error| {
                    QuarantineError::CaptureFailed(format!(
                        "failed to seek legacy repository lease at {}: {error}",
                        path.display()
                    ))
                })?;
                let mut holder = String::new();
                guard.read_to_string(&mut holder).map_err(|error| {
                    QuarantineError::CaptureFailed(format!(
                        "failed to read legacy repository lease at {}: {error}",
                        path.display()
                    ))
                })?;
                (holder, false)
            }
            Err(error) => {
                return Err(QuarantineError::CaptureFailed(format!(
                    "failed to read repository lease owner at {}: {error}",
                    owner_path.display()
                )));
            }
        };

        let holder_pid = if owner_exists {
            Some(
                parse_lease_owner(&holder, lease_identity)
                    .ok_or_else(|| lease_held_error(canonical_repo, &holder))?
                    .pid,
            )
        } else if holder.trim().is_empty() {
            None
        } else {
            Some(
                parse_lease_pid(&holder)
                    .ok_or_else(|| lease_held_error(canonical_repo, &holder))?,
            )
        };
        if holder_pid.is_some_and(process_alive) {
            return Err(lease_held_error(canonical_repo, &holder));
        }

        remove_stale_pending_owner(&pending_path)?;
        let generation = lease_generation(lease_identity, holder_run_id);
        let contents = format!(
            "lease_version=2\ngeneration={generation}\nrun_id={holder_run_id}\npid={}\nrepo={lease_identity}\n",
            std::process::id()
        );
        if let Err(error) = write_pending_owner(&pending_path, &contents) {
            let _ = std::fs::remove_file(&pending_path);
            return Err(QuarantineError::CaptureFailed(format!(
                "failed to stage repository lease owner at {}: {error}",
                pending_path.display()
            )));
        }

        #[cfg(test)]
        abort_at_test_lease_phase("after_owner_stage");

        if !owner_exists && !holder.is_empty() {
            let clear_result = guard
                .set_len(0)
                .and_then(|()| guard.seek(SeekFrom::Start(0)).map(|_| ()))
                .and_then(|()| guard.sync_data());
            if let Err(error) = clear_result {
                let _ = std::fs::remove_file(&pending_path);
                return Err(QuarantineError::CaptureFailed(format!(
                    "failed to clear reclaimed legacy lease at {}: {error}",
                    path.display()
                )));
            }
        }
        match std::fs::remove_file(&owner_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let _ = std::fs::remove_file(&pending_path);
                return Err(QuarantineError::CaptureFailed(format!(
                    "failed to retire stale repository lease owner at {}: {error}",
                    owner_path.display()
                )));
            }
        }

        #[cfg(test)]
        abort_at_test_lease_phase("after_owner_remove");

        if let Err(error) = std::fs::rename(&pending_path, &owner_path) {
            let _ = std::fs::remove_file(&pending_path);
            return Err(QuarantineError::CaptureFailed(format!(
                "failed to commit repository lease owner at {}: {error}",
                owner_path.display()
            )));
        }
        #[cfg(test)]
        abort_at_test_lease_phase("after_owner_commit");
        Ok(Self {
            guard,
            owner_path,
            generation,
        })
    }
}

struct LeaseLocation {
    leases_dir: PathBuf,
    identity: String,
}

fn lease_location(state_dir: &Path, canonical_repo: &str) -> Result<LeaseLocation> {
    let repo = Path::new(canonical_repo);
    if !repo.is_dir() {
        return Ok(LeaseLocation {
            leases_dir: state_dir.join("leases"),
            identity: canonical_repo.to_string(),
        });
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-common-dir"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            QuarantineError::CaptureFailed(format!(
                "failed to resolve repository-scoped lease directory for {canonical_repo}: {error}"
            ))
        })?;
    if !output.status.success() {
        return Err(QuarantineError::CaptureFailed(format!(
            "failed to resolve repository-scoped lease directory for {canonical_repo}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let git_common_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if git_common_dir.is_empty() {
        return Err(QuarantineError::CaptureFailed(format!(
            "git returned an empty repository-scoped lease directory for {canonical_repo}"
        )));
    }
    let git_common_dir = PathBuf::from(git_common_dir);
    let git_common_dir = if git_common_dir.is_absolute() {
        git_common_dir
    } else {
        repo.join(git_common_dir)
    };
    let git_common_dir = std::fs::canonicalize(&git_common_dir).map_err(|error| {
        QuarantineError::CaptureFailed(format!(
            "failed to canonicalize repository-scoped lease directory {}: {error}",
            git_common_dir.display()
        ))
    })?;
    let identity_path = if git_common_dir.file_name().and_then(|name| name.to_str()) == Some(".git")
    {
        git_common_dir.parent().unwrap_or(&git_common_dir)
    } else {
        &git_common_dir
    };
    let identity = identity_path.to_str().ok_or_else(|| {
        QuarantineError::CaptureFailed(format!(
            "repository-scoped lease identity is not UTF-8: {}",
            identity_path.display()
        ))
    })?;
    Ok(LeaseLocation {
        leases_dir: git_common_dir.join("conductor").join("leases"),
        identity: identity.to_string(),
    })
}

impl Drop for RepoLease {
    fn drop(&mut self) {
        let owned_generation = std::fs::read_to_string(&self.owner_path)
            .ok()
            .and_then(|contents| parse_lease_generation(&contents));
        if owned_generation.as_deref() == Some(self.generation.as_str()) {
            let _ = std::fs::remove_file(&self.owner_path);
        }
        let _ = fs2::FileExt::unlock(&self.guard);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeaseOwner {
    pid: u32,
    generation: String,
}

fn parse_lease_owner(contents: &str, canonical_repo: &str) -> Option<LeaseOwner> {
    let version = lease_field(contents, "lease_version")?;
    let generation = lease_field(contents, "generation")?;
    let run_id = lease_field(contents, "run_id")?;
    let pid = lease_field(contents, "pid")?.parse().ok()?;
    let repo = lease_field(contents, "repo")?;
    if version != "2"
        || generation.len() != 64
        || !generation.bytes().all(|byte| byte.is_ascii_hexdigit())
        || run_id.is_empty()
        || repo != canonical_repo
    {
        return None;
    }
    Some(LeaseOwner {
        pid,
        generation: generation.to_string(),
    })
}

fn parse_lease_generation(contents: &str) -> Option<String> {
    let generation = lease_field(contents, "generation")?;
    (generation.len() == 64 && generation.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| generation.to_string())
}

fn lease_held_error(canonical_repo: &str, holder: &str) -> QuarantineError {
    QuarantineError::CaptureFailed(format!(
        "repository lease for {canonical_repo} is already held ({}); refusing to touch a repo \
         another Conductor operation may currently be using",
        holder.trim().replace('\n', ", ")
    ))
}

fn lease_field<'a>(contents: &'a str, name: &str) -> Option<&'a str> {
    contents.lines().find_map(|line| {
        line.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
    })
}

fn lease_generation(canonical_repo: &str, holder_run_id: &str) -> String {
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!(
        "{:x}",
        Sha256::digest(format!(
            "{}\0{sequence}\0{nanos}\0{holder_run_id}\0{canonical_repo}",
            std::process::id()
        ))
    )
}

fn remove_stale_pending_owner(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(QuarantineError::CaptureFailed(format!(
            "failed to remove interrupted repository lease staging file {}: {error}",
            path.display()
        ))),
    }
}

fn write_pending_owner(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut file = open_private_new(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

/// State-directory-wide process lease for top-level dispatch. It reuses the
/// same stable guard, crash-atomic owner generation, and provably-dead-holder
/// reclaim boundary as [`RepoLease`], but has a dedicated synthetic resource
/// identity so it never collides with a canonical repository path.
#[derive(Debug)]
pub(crate) struct DispatchLease {
    _lease: RepoLease,
}

impl DispatchLease {
    pub(crate) fn acquire(state_dir: &Path, cycle_id: &str) -> Result<Self> {
        RepoLease::acquire(state_dir, "conductor://dispatch", cycle_id)
            .map(|lease| Self { _lease: lease })
    }
}

fn lease_key(canonical_repo: &str) -> String {
    format!("{:x}", Sha256::digest(canonical_repo.as_bytes()))
}

/// Extracts the `pid=<n>` value from a lease file's contents, as written by
/// [`RepoLease::acquire`]. `None` for anything that doesn't parse cleanly —
/// a corrupt or foreign-format lease file must never be treated as provably
/// stale.
fn parse_lease_pid(contents: &str) -> Option<u32> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .and_then(|value| value.trim().parse().ok())
}

#[cfg(test)]
fn wait_at_test_reclaim_gate() {
    let Ok(gate_dir) = std::env::var("CONDUCTOR_TEST_LEASE_RECLAIM_GATE_DIR") else {
        return;
    };
    let contender = std::env::var("CONDUCTOR_TEST_LEASE_CONTENDER")
        .expect("lease contender id must accompany the reclaim gate");
    let gate_dir = PathBuf::from(gate_dir);
    std::fs::write(gate_dir.join(format!("{contender}.ready")), b"ready\n")
        .expect("signal reclaim readiness");
    wait_for_test_path(&gate_dir.join(format!("{contender}.go")));
}

#[cfg(test)]
fn abort_at_test_lease_phase(phase: &str) {
    if std::env::var("CONDUCTOR_TEST_LEASE_ABORT_AT").as_deref() == Ok(phase) {
        std::process::exit(86);
    }
}

#[cfg(test)]
fn wait_for_test_path(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Probes whether `pid` still refers to a live process via `kill -0`, the
/// standard POSIX existence check (mirrors the shell-out style already used
/// by [`send_signal_to_group`](crate::dispatch) for the same reason: this
/// crate forbids `unsafe`, so a direct `kill(2)` syscall is not an option).
/// Fails closed on *every* ambiguous signal — the `kill` binary missing, an
/// unreadable result, or, critically, `EPERM` (the process exists but is
/// owned by another user, e.g. after a pid landed on someone else's daemon):
/// only a positively confirmed `ESRCH` ("No such process") reports death.
/// Reclaim therefore only ever fires on a *proven*-dead holder, never on an
/// inconclusive probe. Shared with `dispatch_cycle`'s stale-claim reclaim so
/// both recovery paths authenticate a dead owner the same, single way rather
/// than each growing its own weaker liveness check.
#[cfg(unix)]
pub(crate) fn process_alive(pid: u32) -> bool {
    !kill_probe_confirmed_absent(&pid.to_string())
}

#[cfg(not(unix))]
pub(crate) fn process_alive(_pid: u32) -> bool {
    true
}

/// Probes whether any process in the group led by `pgid` is still alive.
/// POSIX `kill(2)` treats a negative pid as a process-group target, so
/// `kill -0 -<pgid>` succeeds while the group has members, reports `EPERM`
/// when it has members we cannot signal, and only reports `ESRCH` once the
/// group is empty. This proves an orphaned worker *and every descendant still
/// in its group* is gone — a dead `conductor` owner is not proof its
/// separately grouped worker died with it. Fails closed exactly like
/// [`process_alive`]: anything short of a confirmed-empty group reads as
/// still-alive.
#[cfg(unix)]
pub(crate) fn process_group_alive(pgid: u32) -> bool {
    // Match `send_signal_to_group`'s `-<pid>` convention: the negative operand
    // addresses the whole group.
    !kill_probe_confirmed_absent(&format!("-{pgid}"))
}

#[cfg(not(unix))]
pub(crate) fn process_group_alive(_pgid: u32) -> bool {
    true
}

/// Runs `kill -0 <target>` under a forced `C` locale (so the errno text is
/// the canonical `strerror` form regardless of the operator's `LANG`) and
/// reports whether the target is *positively confirmed absent*. Only an
/// `ESRCH` ("No such process") result qualifies; success (alive), `EPERM`
/// (exists but not ours), a missing `kill` binary, or any unrecognized
/// failure all report `false` (cannot prove absence) so callers fail closed.
#[cfg(unix)]
fn kill_probe_confirmed_absent(target: &str) -> bool {
    let output = Command::new("kill")
        .env("LC_ALL", "C")
        .arg("-0")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(output) => classify_kill_probe(output.status.success(), &output.stderr),
        Err(_) => false,
    }
}

/// Pure classifier for a `kill -0` result: `true` only when the probe
/// positively confirms the target is absent (`ESRCH`). Split out from the
/// shell-out so the `EPERM`-versus-`ESRCH` boundary can be unit-tested without
/// spawning a real cross-user process.
#[cfg(unix)]
fn classify_kill_probe(success: bool, stderr: &[u8]) -> bool {
    if success {
        return false;
    }
    String::from_utf8_lossy(stderr)
        .to_ascii_lowercase()
        .contains("no such process")
}

/// Scans every run recorded under `state_dir` for another run — one whose
/// `run_id` is not `exclude_run_id` — that still targets `repo` and has not
/// reached `Finished`. A `Running` run may still be writing to the working
/// tree, so a destructive capture/restore must never proceed alongside one.
/// Unlike [`most_recent_failed_run`] this checks every job kind and ignores
/// bead, since any live run against this repository — not just a Work run
/// for the same bead — is a conflict. Fails closed (an `Err`, not a silent
/// `Ok(None)`) on any manifest that might be relevant but cannot be read or
/// parsed, for the same reason `most_recent_failed_run` does.
fn running_run_conflict(
    state_dir: &Path,
    repo: &str,
    exclude_run_id: &str,
) -> Result<Option<String>> {
    let root = crate::run::runs_dir(state_dir);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(QuarantineError::CaptureFailed(format!(
                "failed to read runs dir {}: {error}",
                root.display()
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            QuarantineError::CaptureFailed(format!("failed to read run directory entry: {error}"))
        })?;
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        match manifest_relevance(&manifest_path, repo, None) {
            ManifestRelevance::NotRelevant => continue,
            ManifestRelevance::Ambiguous => {
                return Err(QuarantineError::CaptureFailed(format!(
                    "run evidence at {} could not be read or parsed and cannot be ruled out as \
                     targeting {repo}; refusing to proceed while a run might still be Running \
                     against it",
                    manifest_path.display()
                )));
            }
            ManifestRelevance::Relevant => {}
        }
        let manifest = crate::run::read_manifest(&manifest_path).map_err(|error| {
            QuarantineError::CaptureFailed(format!(
                "run evidence at {} failed validation while checking for a concurrent run: {}",
                manifest_path.display(),
                error.into_message()
            ))
        })?;
        if manifest.run_id == exclude_run_id {
            continue;
        }
        if manifest.lifecycle == RunLifecycle::Running {
            return Ok(Some(manifest.run_id));
        }
    }
    Ok(None)
}

/// The only entry point production callers should use to capture/restore a
/// dirty attempt: holds a repo-scoped [`RepoLease`] for the duration (so two
/// concurrent attempts against the same repo can never race each other's
/// destructive git operations) and refuses while any other run is still
/// `Running` against this repo, before delegating to
/// [`quarantine_dirty_attempt`]. The lease is released — success or error —
/// the moment this function returns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn quarantine_dirty_attempt_with_lease<C, R>(
    repo: &Path,
    canonical_repo: &str,
    state_dir: &Path,
    commits: &C,
    recovery: &R,
    run_artifacts: &RunHandle,
    before_head: Option<&str>,
    artifact_label: &str,
) -> Result<QuarantineCapture>
where
    C: CommitProbe + ?Sized,
    R: RepoRecovery + ?Sized,
{
    let lease = RepoLease::acquire(state_dir, canonical_repo, run_artifacts.run_id())?;
    quarantine_dirty_attempt_under_lease(
        &lease,
        repo,
        canonical_repo,
        state_dir,
        commits,
        recovery,
        run_artifacts,
        before_head,
        artifact_label,
    )
}

/// Captures/restores while the caller keeps the canonical checkout lease held
/// across a larger worker/verify/checkpoint transaction. Taking the guard by
/// reference makes the no-reacquire boundary explicit and prevents the
/// dispatch path from self-deadlocking on its own lease.
#[allow(clippy::too_many_arguments)]
pub(crate) fn quarantine_dirty_attempt_under_lease<C, R>(
    _lease: &RepoLease,
    repo: &Path,
    canonical_repo: &str,
    state_dir: &Path,
    commits: &C,
    recovery: &R,
    run_artifacts: &RunHandle,
    before_head: Option<&str>,
    artifact_label: &str,
) -> Result<QuarantineCapture>
where
    C: CommitProbe + ?Sized,
    R: RepoRecovery + ?Sized,
{
    if let Some(conflicting_run_id) =
        running_run_conflict(state_dir, canonical_repo, run_artifacts.run_id())?
    {
        return Err(QuarantineError::CaptureFailed(format!(
            "refusing to capture/restore {canonical_repo}: run {conflicting_run_id} is still \
             Running against this repository"
        )));
    }
    quarantine_dirty_attempt(
        repo,
        commits,
        recovery,
        run_artifacts,
        before_head,
        artifact_label,
    )
}

/// Builds the diagnostic for a failed/unproven restore, first attempting to
/// bring the working tree back to the exact pre-clean dirty state so a
/// partial `restore_clean` never leaves the repository in a state that is
/// neither the original dirty attempt nor a clean checkout. Recovery is
/// skipped (and trivially reported as satisfied) when the tree already
/// matches the pre-clean state — reapplying the patch on top of unchanged
/// content would only produce spurious conflicts.
fn transactional_diagnostic<R>(
    symptom: &str,
    recovery: &R,
    repo: &Path,
    patch: &[u8],
    expected_paths: &[String],
) -> String
where
    R: RepoRecovery + ?Sized,
{
    match recover_pre_clean_state(recovery, repo, patch, expected_paths) {
        Ok(()) => format!(
            "{symptom}; the pre-clean dirty state was restored and verified intact — no data \
             lost, the captured artifact remains authoritative and safe to discard once resolved"
        ),
        Err(recovery_error) => format!(
            "{symptom}; additionally failed to restore the pre-clean dirty state \
             ({recovery_error}) — the repository may be left in an inconsistent state and \
             requires manual inspection before any further dispatch"
        ),
    }
}

fn recover_pre_clean_state<R>(
    recovery: &R,
    repo: &Path,
    patch: &[u8],
    expected_paths: &[String],
) -> std::result::Result<(), String>
where
    R: RepoRecovery + ?Sized,
{
    let expected_hash = patch_hash(patch);

    // Content, not just which paths are reported as changed, is what proves
    // the pre-clean dirty state survived intact: a partial `reset --hard`
    // can leave a tracked file's path in the same "changed" set while its
    // *content* has already reverted to HEAD. Recapturing the tree as a
    // patch and hashing it catches that a path-set comparison alone cannot.
    let current_patch = recovery.capture_patch(repo)?;
    if patch_hash(&current_patch) == expected_hash {
        // Nothing was actually destroyed (e.g. `restore_clean` failed before
        // mutating anything, or every path already survived intact) — the
        // working tree's content already matches the pre-clean dirty state
        // byte-for-byte.
        return Ok(());
    }

    // Any path git still reports as changed already reflects some non-HEAD
    // state: either the untouched original dirty edit (a survivor `clean`
    // or `reset` never reached) or, for an untracked file, its untouched
    // original content. Reapplying either would conflict (`apply` refuses
    // to recreate a file that already exists) or corrupt content that is
    // already correct, so those paths are excluded from reapplication —
    // the hash check below, not this exclusion set, is what proves the
    // final outcome.
    let present = recovery.changed_paths(repo)?;
    let excluding: Vec<String> = expected_paths
        .iter()
        .filter(|path| present.contains(path))
        .cloned()
        .collect();
    if excluding.len() < expected_paths.len() {
        recovery.apply_patch(repo, patch, &excluding)?;
    }

    let after_patch = recovery.capture_patch(repo)?;
    let after_hash = patch_hash(&after_patch);
    if after_hash == expected_hash {
        return Ok(());
    }
    Err(format!(
        "the working tree content still does not match the pre-clean dirty state byte-for-byte \
         after recovery (expected patch sha256 {}, found {})",
        &expected_hash[..12],
        &after_hash[..12],
    ))
}

fn patch_hash(patch: &[u8]) -> String {
    format!("{:x}", Sha256::digest(patch))
}

fn write_patch_artifact(
    run_artifacts: &RunHandle,
    artifact_label: &str,
    patch: &[u8],
) -> std::result::Result<ArtifactRef, String> {
    let tmp = stage_private_patch_file(artifact_label, patch)
        .map_err(|error| format!("failed to stage quarantine patch: {error}"))?;
    let destination =
        PathBuf::from("artifacts").join(format!("{}.patch", sanitize(artifact_label)));
    let result = run_artifacts
        .capture_artifact(&tmp, &destination)
        .map_err(|error| {
            format!(
                "failed to capture quarantine artifact: {}",
                error.into_message()
            )
        });
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Stages `patch` in a private, exclusively-created temporary file — never a
/// world-readable file at a name an attacker could pre-place a symlink at.
/// `create_new` refuses to follow an existing symlink or overwrite an
/// existing file (the anti-race property), and the file is created `0600` so
/// only this process's owner can read the uncommitted patch content while it
/// is staged. A handful of retries with a fresh timestamp absorb the
/// vanishingly unlikely case of a genuine name collision.
fn stage_private_patch_file(artifact_label: &str, patch: &[u8]) -> std::io::Result<PathBuf> {
    let label = sanitize(artifact_label);
    let mut last_error = None;
    for _ in 0..4 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let candidate = std::env::temp_dir().join(format!(
            "conductor-quarantine-{}-{label}-{nanos}.patch",
            std::process::id(),
        ));
        match open_private_new(&candidate) {
            Ok(mut file) => {
                file.write_all(patch)?;
                file.sync_all()?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::other("failed to create a private temporary file after retries")
    }))
}

#[cfg(unix)]
fn open_private_lock(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_lock(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[cfg(unix)]
fn open_private_new(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_new(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// A prior Finished work run for the same repo/bead whose outcome was not a
/// verified pass — the only kind of run whose dirty leftovers are eligible
/// for one-time legacy adoption ahead of a fresh dispatch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdoptableRun {
    pub(crate) run_id: String,
    pub(crate) before_head: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    /// Whether this run's own `events.jsonl` was readable and contained at
    /// least one `AttemptStarted` event — durable proof that a worker
    /// actually spawned against this repo, rather than a run that was
    /// created and then abandoned or corrupted before any worker ran.
    /// `false` for both "no such event" and "the event log could not be
    /// read/parsed" — either way there is no proof, so automatic adoption
    /// must not proceed without explicit operator authorization.
    pub(crate) attempt_started: bool,
}

/// Scans every run recorded under `state_dir` and returns the single
/// most-recently-created Finished work run targeting `repo`/`bead`, if that
/// run's outcome was not a verified pass. Returns `Err` — never a silent
/// `Ok(None)` — when a run manifest that targets this repo/bead cannot be
/// read cleanly, since a tampered or corrupt manifest for our own target is
/// exactly the ambiguous-provenance case recovery must refuse. Manifests
/// for other targets are ignored regardless of their own validity.
pub(crate) fn most_recent_failed_run(
    state_dir: &Path,
    repo: &str,
    bead: &str,
) -> Result<Option<AdoptableRun>> {
    let root = crate::run::runs_dir(state_dir);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(QuarantineError::CaptureFailed(format!(
                "failed to read runs dir {}: {error}",
                root.display()
            )));
        }
    };
    let mut latest: Option<(DateTime<Utc>, RunManifest)> = None;
    for entry in entries {
        let entry = entry.map_err(|error| {
            QuarantineError::CaptureFailed(format!("failed to read run directory entry: {error}"))
        })?;
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        match manifest_relevance(&manifest_path, repo, Some(bead)) {
            ManifestRelevance::NotRelevant => continue,
            ManifestRelevance::Ambiguous => {
                return Err(QuarantineError::CaptureFailed(format!(
                    "run evidence at {} could not be read or parsed and cannot be ruled out as \
                     relevant to {repo}/{bead}; refusing to scan further",
                    manifest_path.display()
                )));
            }
            ManifestRelevance::Relevant => {}
        }
        // This manifest claims our exact target: it must now validate
        // strictly, or we cannot trust any run evidence for this target.
        let manifest = crate::run::read_manifest(&manifest_path).map_err(|error| {
            QuarantineError::CaptureFailed(format!(
                "run evidence for {repo}/{bead} at {} failed validation: {}",
                manifest_path.display(),
                error.into_message()
            ))
        })?;
        if manifest.job != RunJob::Work || manifest.lifecycle != RunLifecycle::Finished {
            continue;
        }
        let Ok(created_at) = DateTime::parse_from_rfc3339(&manifest.created_at) else {
            return Err(QuarantineError::CaptureFailed(format!(
                "run evidence for {repo}/{bead} has an unparseable created_at timestamp"
            )));
        };
        let created_at = created_at.with_timezone(&Utc);
        let is_more_recent = match latest.as_ref() {
            Some((current, _)) => created_at > *current,
            None => true,
        };
        if is_more_recent {
            latest = Some((created_at, manifest));
        }
    }
    let Some((created_at, manifest)) = latest else {
        return Ok(None);
    };
    if manifest.outcome.as_deref() == Some("verified") {
        return Ok(None);
    }
    let before_head = manifest.work.and_then(|work| work.before_head);
    let attempt_started = run_has_readable_attempt_started_event(&root, &manifest.run_id);
    Ok(Some(AdoptableRun {
        run_id: manifest.run_id,
        before_head,
        created_at,
        attempt_started,
    }))
}

/// Best-effort proof that `run_id` actually spawned a worker: its own
/// `events.jsonl` must be readable and contain at least one `AttemptStarted`
/// event. Any read/parse failure is treated the same as "no such event" —
/// there is no proof either way, so the caller must require explicit
/// operator authorization rather than adopt automatically.
fn run_has_readable_attempt_started_event(runs_root: &Path, run_id: &str) -> bool {
    let events_path = runs_root.join(run_id).join("events.jsonl");
    let Ok(events) = crate::run::read_events(&events_path) else {
        return false;
    };
    events
        .iter()
        .any(|event| event.kind == crate::run::EventKind::AttemptStarted)
}

/// Result of a best-effort, pre-validation relevance check on a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestRelevance {
    /// Readable, parses as JSON, and definitely targets some other run.
    /// Safe to ignore regardless of that other manifest's own validity.
    NotRelevant,
    /// Readable, parses as JSON, and targets `repo`/`bead` exactly.
    Relevant,
    /// Could not be read or parsed, and cannot be ruled out as relevant —
    /// treated as provenance for our own target being unreadable, which is
    /// fatal to the whole scan rather than something to silently skip.
    Ambiguous,
}

/// Cheap, non-strict peek at whether a manifest targets `repo` (and, when
/// given, an exact `bead`), without requiring it to pass full schema/hash
/// validation first. A manifest that cannot even be read or parsed as JSON
/// is not simply skipped: a raw substring search for the identifiers in the
/// raw bytes decides whether it might still be our own (unreadable) run
/// evidence (`Ambiguous`) or is almost certainly unrelated (`NotRelevant`).
fn manifest_relevance(manifest_path: &Path, repo: &str, bead: Option<&str>) -> ManifestRelevance {
    let Ok(bytes) = std::fs::read(manifest_path) else {
        return ManifestRelevance::Ambiguous;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        let text = String::from_utf8_lossy(&bytes);
        let mentions_bead = bead.is_none_or(|bead| text.contains(bead));
        return if text.contains(repo) && mentions_bead {
            ManifestRelevance::Ambiguous
        } else {
            ManifestRelevance::NotRelevant
        };
    };
    let repo_matches = value
        .get("target")
        .and_then(|target| target.get("repo"))
        .and_then(serde_json::Value::as_str)
        == Some(repo);
    let bead_matches = bead.is_none_or(|bead| {
        value
            .get("target")
            .and_then(|target| target.get("bead"))
            .and_then(serde_json::Value::as_str)
            == Some(bead)
    });
    if repo_matches && bead_matches {
        ManifestRelevance::Relevant
    } else {
        ManifestRelevance::NotRelevant
    }
}

/// Authenticates that no commit has landed on `repo` since `run` failed, so
/// its dirty leftovers can be safely attributed to that run alone. Requires
/// an exact HEAD match against `run.before_head` — the durable record that a
/// real worker attempt started from this exact commit.
///
/// A manifest written before `before_head` capture existed has no such
/// record, so there is no automatic, safe way to prove which commit it
/// started from — timestamp comparisons are not evidence of authorship.
/// Likewise, a run whose own event log cannot prove an `AttemptStarted`
/// event ever fired has no proof a worker even ran — it may have been
/// created and then abandoned or corrupted before any dispatch happened.
/// The only path for either case is `operator_authorized_run_id` naming this
/// exact `run.run_id`: a deliberate, per-run acknowledgment (not a blanket
/// policy toggle) that an operator has manually reviewed this specific
/// stranded run and accepts the residual risk of adopting its dirty tree
/// without that proof. Even then, this never mutates the repository —
/// it only reports the current HEAD for the caller to record as the
/// authenticated `before_head` going forward.
pub(crate) fn authenticate_legacy_adoption<C>(
    commits: &C,
    repo: &Path,
    run: &AdoptableRun,
    operator_authorized_run_id: Option<&str>,
) -> Result<String>
where
    C: CommitProbe + ?Sized,
{
    let operator_authorized = operator_authorized_run_id == Some(run.run_id.as_str());
    if !run.attempt_started && !operator_authorized {
        return Err(QuarantineError::CaptureFailed(format!(
            "run {} has no readable AttemptStarted event proving a worker ever actually ran \
             against this repository; automatic legacy adoption refuses to guess and requires \
             explicit operator authorization naming this exact run_id before this repository \
             can be recovered",
            run.run_id
        )));
    }
    let Some(expected) = run.before_head.as_deref() else {
        if !operator_authorized {
            return Err(QuarantineError::CaptureFailed(format!(
                "run {} has no recorded before_head (predates worker-attempt provenance \
                 capture); automatic legacy adoption refuses to guess and requires explicit \
                 operator authorization naming this exact run_id before this repository can be \
                 recovered",
                run.run_id
            )));
        }
        let current_head = commits
            .head(repo)
            .map_err(|error| QuarantineError::CaptureFailed(format!("git head: {error}")))?;
        return current_head.ok_or_else(|| {
            QuarantineError::CaptureFailed(
                "repository has no HEAD to authenticate against".to_string(),
            )
        });
    };
    let current_head = commits
        .head(repo)
        .map_err(|error| QuarantineError::CaptureFailed(format!("git head: {error}")))?;
    if current_head.as_deref() != Some(expected) {
        return Err(QuarantineError::HeadMoved {
            expected: Some(expected.to_string()),
            found: current_head,
        });
    }
    Ok(expected.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{EventInput, EventKind};
    use std::cell::RefCell;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("conductor-quarantine-{label}-{nanos}"));
            std::fs::create_dir_all(&path).expect("mkdir temp");
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

    fn wait_for_path(path: &Path) {
        wait_for_test_path(path);
    }

    fn spawn_dead_pid() -> u32 {
        let mut dead = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived process");
        let pid = dead.id();
        dead.wait().expect("reap short-lived process");
        pid
    }

    #[derive(Clone, Copy)]
    enum TestLeaseKind {
        Repo,
        Dispatch,
    }

    impl TestLeaseKind {
        const fn label(self) -> &'static str {
            match self {
                Self::Repo => "repo",
                Self::Dispatch => "dispatch",
            }
        }
    }

    #[derive(Clone, Copy)]
    enum TestLeaseOwnerFormat {
        Legacy,
        V2,
    }

    impl TestLeaseOwnerFormat {
        const fn label(self) -> &'static str {
            match self {
                Self::Legacy => "legacy",
                Self::V2 => "v2",
            }
        }
    }

    #[derive(Clone, Copy)]
    enum TestLeaseRace {
        Ordered,
        Simultaneous,
    }

    impl TestLeaseRace {
        const fn label(self) -> &'static str {
            match self {
                Self::Ordered => "ordered",
                Self::Simultaneous => "simultaneous",
            }
        }
    }

    enum TestHeldLease {
        Repo(RepoLease),
        Dispatch(DispatchLease),
    }

    fn write_lease_process_result(path: &Path, contents: impl AsRef<[u8]>) {
        let pending = path.with_extension("result.pending");
        std::fs::write(&pending, contents).expect("stage lease process result");
        std::fs::rename(&pending, path).expect("commit lease process result");
    }

    fn acquire_test_lease(
        kind: TestLeaseKind,
        state_dir: &Path,
        identity: &str,
        run_id: &str,
    ) -> Result<TestHeldLease> {
        match kind {
            TestLeaseKind::Repo => {
                RepoLease::acquire(state_dir, identity, run_id).map(TestHeldLease::Repo)
            }
            TestLeaseKind::Dispatch => {
                DispatchLease::acquire(state_dir, run_id).map(TestHeldLease::Dispatch)
            }
        }
    }

    fn spawn_lease_process(
        state_dir: &Path,
        gate_dir: &Path,
        contender: &str,
        kind: TestLeaseKind,
        identity: &str,
        run_id: &str,
        abort_at: Option<&str>,
    ) -> std::process::Child {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg("--exact")
            .arg("--ignored")
            .arg("quarantine::tests::repo_lease_process_helper")
            .env("CONDUCTOR_TEST_LEASE_STATE_DIR", state_dir)
            .env("CONDUCTOR_TEST_LEASE_RECLAIM_GATE_DIR", gate_dir)
            .env("CONDUCTOR_TEST_LEASE_CONTENDER", contender)
            .env("CONDUCTOR_TEST_LEASE_KIND", kind.label())
            .env("CONDUCTOR_TEST_LEASE_IDENTITY", identity)
            .env("CONDUCTOR_TEST_LEASE_RUN_ID", run_id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(phase) = abort_at {
            command.env("CONDUCTOR_TEST_LEASE_ABORT_AT", phase);
        }
        command.spawn().expect("spawn independent lease contender")
    }

    #[test]
    #[ignore = "independent-process helper invoked by lease contention tests"]
    fn repo_lease_process_helper() {
        let state_dir = PathBuf::from(
            std::env::var("CONDUCTOR_TEST_LEASE_STATE_DIR").expect("lease state dir"),
        );
        let gate_dir = PathBuf::from(
            std::env::var("CONDUCTOR_TEST_LEASE_RECLAIM_GATE_DIR").expect("lease gate dir"),
        );
        let contender =
            std::env::var("CONDUCTOR_TEST_LEASE_CONTENDER").expect("lease contender id");
        let kind = match std::env::var("CONDUCTOR_TEST_LEASE_KIND")
            .expect("lease kind")
            .as_str()
        {
            "repo" => TestLeaseKind::Repo,
            "dispatch" => TestLeaseKind::Dispatch,
            other => panic!("unknown lease kind {other:?}"),
        };
        let identity = std::env::var("CONDUCTOR_TEST_LEASE_IDENTITY").expect("lease identity");
        let run_id = std::env::var("CONDUCTOR_TEST_LEASE_RUN_ID").expect("lease run id");
        let result_path = gate_dir.join(format!("{contender}.result"));
        let release_path = gate_dir.join(format!("{contender}.release"));

        match acquire_test_lease(kind, &state_dir, &identity, &run_id) {
            Ok(lease) => {
                write_lease_process_result(&result_path, b"acquired");
                wait_for_path(&release_path);
                drop(lease);
            }
            Err(error) => {
                write_lease_process_result(&result_path, format!("refused:{error}"));
            }
        }
    }

    /// Sequential fakes: each call pops the next programmed result, mirroring
    /// `dispatch::tests::FakeCommits`. `heads`/`cleans` are consumed in the
    /// exact order `quarantine_dirty_attempt` calls them: HEAD (preflight),
    /// `is_clean` (preflight), then — only if dirty and restore is attempted
    /// — HEAD (post-restore), `is_clean` (post-restore).
    struct FakeCommits {
        heads: RefCell<Vec<Option<String>>>,
        cleans: RefCell<Vec<bool>>,
    }

    impl FakeCommits {
        fn new<const N: usize, const M: usize>(
            heads: [Option<&str>; N],
            cleans: [bool; M],
        ) -> Self {
            Self {
                heads: RefCell::new(heads.into_iter().map(|h| h.map(str::to_string)).collect()),
                cleans: RefCell::new(cleans.to_vec()),
            }
        }
    }

    impl CommitProbe for FakeCommits {
        fn head(&self, _repo: &Path) -> crate::dispatch::Result<Option<String>> {
            Ok(self.heads.borrow_mut().remove(0))
        }

        fn is_clean(&self, _repo: &Path) -> crate::dispatch::Result<bool> {
            Ok(self.cleans.borrow_mut().remove(0))
        }

        fn is_direct_child(
            &self,
            _repo: &Path,
            _before: Option<&str>,
            _commit: &str,
        ) -> crate::dispatch::Result<bool> {
            Ok(true)
        }
    }

    /// `changed_paths_sequence`, when non-empty, overrides `changed_paths` on
    /// a per-call basis (popped in order) so tests can simulate the tree's
    /// state actually changing across the capture → restore-fails →
    /// transactional-recovery sequence. Tests that don't care about that
    /// distinction just set the fixed `changed_paths` field, which every
    /// call falls back to once the sequence is drained. `capture_patch`
    /// works the same way, except its *first* call always returns the fixed
    /// `patch` field (mirroring the one real initial capture inside
    /// `quarantine_dirty_attempt`) — only calls after that pop
    /// `capture_patch_sequence`, so a test can simulate the tree's actual
    /// content diverging only during the transactional-recovery phase.
    #[derive(Default)]
    struct FakeRecovery {
        changed_paths: Vec<String>,
        changed_paths_sequence: RefCell<Vec<Vec<String>>>,
        changed_paths_error: RefCell<Option<String>>,
        patch: Vec<u8>,
        capture_patch_calls: RefCell<u32>,
        capture_patch_sequence: RefCell<Vec<Vec<u8>>>,
        restore_error: RefCell<Option<String>>,
        restore_calls: RefCell<u32>,
        apply_error: RefCell<Option<String>>,
        apply_excluding_calls: RefCell<Vec<Vec<String>>>,
    }

    impl RepoRecovery for FakeRecovery {
        fn changed_paths(&self, _repo: &Path) -> std::result::Result<Vec<String>, String> {
            if let Some(message) = self.changed_paths_error.borrow().clone() {
                return Err(message);
            }
            let mut sequence = self.changed_paths_sequence.borrow_mut();
            if !sequence.is_empty() {
                return Ok(sequence.remove(0));
            }
            Ok(self.changed_paths.clone())
        }

        fn capture_patch(&self, _repo: &Path) -> std::result::Result<Vec<u8>, String> {
            let mut calls = self.capture_patch_calls.borrow_mut();
            *calls += 1;
            if *calls == 1 {
                return Ok(self.patch.clone());
            }
            let mut sequence = self.capture_patch_sequence.borrow_mut();
            if !sequence.is_empty() {
                return Ok(sequence.remove(0));
            }
            Ok(self.patch.clone())
        }

        fn restore_clean(&self, _repo: &Path) -> std::result::Result<(), String> {
            *self.restore_calls.borrow_mut() += 1;
            match self.restore_error.borrow().clone() {
                Some(message) => Err(message),
                None => Ok(()),
            }
        }

        fn apply_patch(
            &self,
            _repo: &Path,
            _patch: &[u8],
            excluding: &[String],
        ) -> std::result::Result<(), String> {
            self.apply_excluding_calls
                .borrow_mut()
                .push(excluding.to_vec());
            match self.apply_error.borrow().clone() {
                Some(message) => Err(message),
                None => Ok(()),
            }
        }
    }

    fn fixed_now() -> DateTime<Utc> {
        "2026-07-17T14:45:26Z".parse().expect("fixed timestamp")
    }

    fn run_handle(temp: &TempDir, label: &str) -> RunHandle {
        RunHandle::create(
            temp.path(),
            RunJob::Work,
            crate::run::NewRun {
                target: crate::run::RunTarget {
                    repo: "/repo/conductor".to_string(),
                    bead: Some(format!("bead-{label}")),
                },
                approved_profiles: vec!["claude-sonnet-5".to_string()],
                bursar_roster_artifact: None,
                limits: crate::run::RunLimits::default(),
                verifier: crate::run::RunVerifier::default(),
                work: Some(crate::run::WorkState {
                    cycle_id: "cycle-1".to_string(),
                    authorization_sha256: "a".repeat(64),
                    before_head: Some("b".repeat(40)),
                    owner_pid: None,
                    worker_pgid: None,
                    worker_profile: None,
                    worker_commit: None,
                    mechanical: None,
                    stage: crate::run::WorkStage::Implementing,
                }),
                approval: None,
            },
        )
        .expect("create run")
    }

    #[test]
    fn quarantine_worker_failure_captures_tracked_edits_and_restores_repo() {
        let temp = TempDir::new("tracked");
        let handle = run_handle(&temp, "tracked");
        let commits = FakeCommits::new([Some("head1"), Some("head1")], [false, true]);
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: b"diff --git a/src/lib.rs b/src/lib.rs\n+dirty\n".to_vec(),
            ..FakeRecovery::default()
        };

        let capture = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "001-attempt",
        )
        .expect("capture succeeds");

        assert!(!capture.is_noop());
        assert_eq!(capture.changed_paths, vec!["src/lib.rs".to_string()]);
        assert!(!capture.truncated);
        let artifact = capture.artifact.expect("artifact captured");
        assert_eq!(artifact.path, "artifacts/001-attempt.patch");
        assert_eq!(*recovery.restore_calls.borrow(), 1);
        assert_eq!(
            std::fs::read(handle.dir().join(&artifact.path)).expect("read captured patch"),
            recovery.patch,
        );
    }

    #[test]
    fn quarantine_worker_failure_captures_untracked_files() {
        let temp = TempDir::new("untracked");
        let handle = run_handle(&temp, "untracked");
        let commits = FakeCommits::new([Some("head1"), Some("head1")], [false, true]);
        let recovery = FakeRecovery {
            changed_paths: vec!["fixtures/new_case.json".to_string()],
            patch: b"diff --git a/fixtures/new_case.json b/fixtures/new_case.json\nnew file\n"
                .to_vec(),
            ..FakeRecovery::default()
        };

        let capture = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "002-attempt",
        )
        .expect("capture succeeds");

        assert_eq!(
            capture.changed_paths,
            vec!["fixtures/new_case.json".to_string()]
        );
        assert!(capture.artifact.is_some());
    }

    #[test]
    fn quarantine_replay_on_already_clean_tree_is_idempotent_noop() {
        let temp = TempDir::new("idempotent");
        let handle = run_handle(&temp, "idempotent");
        let commits = FakeCommits::new([Some("head1"), Some("head1")], [true, true]);
        let recovery = FakeRecovery::default();

        let first = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "003-attempt",
        )
        .expect("first call succeeds");
        let second = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "003-attempt",
        )
        .expect("second call succeeds");

        assert!(first.is_noop());
        assert!(second.is_noop());
        assert_eq!(*recovery.restore_calls.borrow(), 0);
    }

    #[test]
    fn quarantine_head_moved_since_attempt_start_fails_closed_untouched() {
        let temp = TempDir::new("head-moved");
        let handle = run_handle(&temp, "head-moved");
        let commits = FakeCommits::new([Some("head2")], []);
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: b"diff\n".to_vec(),
            ..FakeRecovery::default()
        };

        let error = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "004-attempt",
        )
        .expect_err("head moved must fail closed");

        assert!(matches!(error, QuarantineError::HeadMoved { .. }));
        assert_eq!(*recovery.restore_calls.borrow(), 0, "must not mutate tree");
    }

    #[test]
    fn quarantine_committed_then_failed_is_treated_as_head_moved() {
        // A worker that commits and then fails later (e.g. a post-commit
        // crash) must never be treated as an uncommitted dirty attempt.
        let temp = TempDir::new("committed-then-failed");
        let handle = run_handle(&temp, "committed-then-failed");
        let commits = FakeCommits::new([Some("committed-sha")], []);
        let recovery = FakeRecovery::default();

        let error = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("pre-attempt-sha"),
            "005-attempt",
        )
        .expect_err("committed-then-failed must fail closed");

        assert!(matches!(error, QuarantineError::HeadMoved { .. }));
        assert_eq!(*recovery.restore_calls.borrow(), 0);
    }

    #[test]
    fn quarantine_cleanup_failure_reports_diagnostic_without_losing_capture() {
        let temp = TempDir::new("cleanup-failure");
        let handle = run_handle(&temp, "cleanup-failure");
        let commits = FakeCommits::new([Some("head1")], [false]);
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: b"diff --git a/src/lib.rs b/src/lib.rs\n+dirty\n".to_vec(),
            restore_error: RefCell::new(Some("permission denied removing stray.tmp".to_string())),
            ..FakeRecovery::default()
        };

        let error = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "006-attempt",
        )
        .expect_err("cleanup failure must fail closed");

        assert!(matches!(error, QuarantineError::CleanupUnproven(_)));
        // The patch is still durably captured on disk even though the
        // restore step itself failed — no data loss on partial failure.
        assert!(handle.dir().join("artifacts/006-attempt.patch").is_file());
    }

    #[test]
    fn quarantine_cleanup_that_leaves_tree_dirty_is_not_proven_and_fails_closed() {
        let temp = TempDir::new("cleanup-unproven");
        let handle = run_handle(&temp, "cleanup-unproven");
        let commits = FakeCommits::new([Some("head1"), Some("head1")], [false, false]);
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: b"diff\n".to_vec(),
            ..FakeRecovery::default()
        };

        let error = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "007-attempt",
        )
        .expect_err("unproven cleanup must fail closed");

        assert!(matches!(error, QuarantineError::CleanupUnproven(_)));
    }

    #[test]
    fn quarantine_transactional_recovery_detects_content_mismatch_even_when_path_set_matches() {
        // A partial `reset --hard` can revert a tracked file's *content* to
        // HEAD while git still reports the same path as changed (now for a
        // different reason). A path-set-only comparison would wrongly call
        // that "intact"; content hashing must catch it.
        let temp = TempDir::new("transactional-content-mismatch");
        let handle = run_handle(&temp, "transactional-content-mismatch");
        let commits = FakeCommits::new([Some("head1")], [false]);
        let original_patch = b"diff --git a/src/lib.rs b/src/lib.rs\n+dirty\n".to_vec();
        let corrupted_patch = b"diff --git a/src/lib.rs b/src/lib.rs\n+DIFFERENT\n".to_vec();
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: original_patch,
            capture_patch_sequence: RefCell::new(vec![corrupted_patch.clone(), corrupted_patch]),
            restore_error: RefCell::new(Some("disk full mid-reset".to_string())),
            ..FakeRecovery::default()
        };

        let error = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "010-attempt",
        )
        .expect_err("content mismatch must fail closed even though the path set matches");

        let QuarantineError::CleanupUnproven(message) = error else {
            panic!("expected CleanupUnproven, got a different variant");
        };
        assert!(
            message.contains("still does not match"),
            "diagnostic must report the content-hash mismatch, got: {message}"
        );
        assert!(
            recovery.apply_excluding_calls.borrow().is_empty(),
            "the only path is a survivor by path-set, so reapply must never be attempted"
        );
    }

    #[test]
    fn quarantine_transactional_recovery_tolerates_survivors_and_reapplies_only_missing_paths() {
        // "src/lib.rs" survived the partial `restore_clean` failure intact
        // (its dirty content was never touched) while "fixtures/new.json"
        // was already removed by `git clean` before the failure. Recovery
        // must leave the survivor alone (excluded from `apply_patch`, never
        // conflicting with an "already exists" error) and only reapply the
        // missing path, then prove success by content hash.
        let temp = TempDir::new("transactional-tolerates-survivors");
        let handle = run_handle(&temp, "transactional-tolerates-survivors");
        let commits = FakeCommits::new([Some("head1")], [false]);
        let full_patch =
            b"diff --git a/src/lib.rs b/src/lib.rs\n+dirty\ndiff --git a/fixtures/new.json\n+new\n"
                .to_vec();
        let partial_patch = b"diff --git a/src/lib.rs b/src/lib.rs\n+dirty\n".to_vec();
        let recovery = FakeRecovery {
            changed_paths_sequence: RefCell::new(vec![
                vec!["src/lib.rs".to_string(), "fixtures/new.json".to_string()],
                vec!["src/lib.rs".to_string()],
            ]),
            patch: full_patch.clone(),
            capture_patch_sequence: RefCell::new(vec![partial_patch, full_patch]),
            restore_error: RefCell::new(Some("permission denied removing new.json".to_string())),
            ..FakeRecovery::default()
        };

        let error = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "011-attempt",
        )
        .expect_err("restore_clean itself still failed, even though recovery succeeded");

        let QuarantineError::CleanupUnproven(message) = error else {
            panic!("expected CleanupUnproven, got a different variant");
        };
        assert!(
            message.contains("restored and verified intact"),
            "diagnostic must report successful transactional recovery, got: {message}"
        );
        assert_eq!(
            recovery.apply_excluding_calls.borrow().as_slice(),
            [vec!["src/lib.rs".to_string()]],
            "only the missing path is reapplied; the survivor is excluded"
        );
    }

    #[test]
    fn quarantine_captured_artifact_tamper_is_detected_by_hash_validation() {
        let temp = TempDir::new("tamper");
        let handle = run_handle(&temp, "tamper");
        let commits = FakeCommits::new([Some("head1"), Some("head1")], [false, true]);
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: b"diff --git a/src/lib.rs b/src/lib.rs\n+dirty\n".to_vec(),
            ..FakeRecovery::default()
        };
        let run_id = handle.run_id().to_string();
        let capture = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "008-attempt",
        )
        .expect("capture succeeds");
        drop(handle);
        let artifact = capture.artifact.expect("artifact captured");

        std::fs::write(
            temp.path().join("runs").join(&run_id).join(&artifact.path),
            b"tampered",
        )
        .expect("tamper artifact bytes");

        // The tampered bytes no longer match the hash recorded at capture
        // time — the same tamper-evidence property `run.rs` already proves
        // for every other artifact captured through `RunHandle`.
        assert_ne!(
            format!("{:x}", Sha256::digest(b"tampered")),
            artifact.sha256
        );
        assert!(
            RunHandle::open(temp.path(), &run_id).is_ok(),
            "manifest itself is untouched by tampering an artifact it never referenced"
        );
    }

    #[test]
    fn quarantine_no_raw_patch_bytes_ever_reach_the_manifest_or_event_log() {
        let temp = TempDir::new("no-raw-patch");
        let handle = run_handle(&temp, "no-raw-patch");
        let commits = FakeCommits::new([Some("head1"), Some("head1")], [false, true]);
        let secret_marker = "TOTALLY_UNIQUE_DIRTY_PATCH_CONTENT_MARKER";
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: format!("diff --git a/src/lib.rs b/src/lib.rs\n+{secret_marker}\n").into_bytes(),
            ..FakeRecovery::default()
        };
        let run_id = handle.run_id().to_string();

        quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "009-attempt",
        )
        .expect("capture succeeds");
        drop(handle);

        let manifest_text =
            std::fs::read_to_string(temp.path().join("runs").join(&run_id).join("manifest.json"))
                .expect("read manifest");
        let events_text =
            std::fs::read_to_string(temp.path().join("runs").join(&run_id).join("events.jsonl"))
                .expect("read events");
        assert!(!manifest_text.contains(secret_marker));
        assert!(!events_text.contains(secret_marker));
    }

    fn manifest_for(
        temp: &TempDir,
        repo: &str,
        bead: &str,
        outcome: &str,
        before_head: Option<&str>,
        created_at: DateTime<Utc>,
    ) -> String {
        let mut handle = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            crate::run::NewRun {
                target: crate::run::RunTarget {
                    repo: repo.to_string(),
                    bead: Some(bead.to_string()),
                },
                approved_profiles: vec!["claude-sonnet-5".to_string()],
                bursar_roster_artifact: None,
                limits: crate::run::RunLimits::default(),
                verifier: crate::run::RunVerifier::default(),
                work: Some(crate::run::WorkState {
                    cycle_id: "cycle-legacy".to_string(),
                    authorization_sha256: "a".repeat(64),
                    before_head: before_head.map(str::to_string),
                    owner_pid: None,
                    worker_pgid: None,
                    worker_profile: None,
                    worker_commit: None,
                    mechanical: None,
                    stage: crate::run::WorkStage::Implementing,
                }),
                approval: None,
            },
            created_at,
        )
        .expect("create legacy run");
        handle.finish(outcome).expect("finish legacy run");
        handle.run_id().to_string()
    }

    /// Same as `manifest_for`, but appends a real `AttemptStarted` event
    /// before finishing — the durable proof a worker actually spawned that
    /// `authenticate_legacy_adoption` now requires for automatic adoption.
    fn finished_run_with_attempt_started_for(
        temp: &TempDir,
        repo: &str,
        bead: &str,
        outcome: &str,
        created_at: DateTime<Utc>,
    ) -> String {
        let mut handle = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            crate::run::NewRun {
                target: crate::run::RunTarget {
                    repo: repo.to_string(),
                    bead: Some(bead.to_string()),
                },
                approved_profiles: vec!["claude-sonnet-5".to_string()],
                bursar_roster_artifact: None,
                limits: crate::run::RunLimits::default(),
                verifier: crate::run::RunVerifier::default(),
                work: Some(crate::run::WorkState {
                    cycle_id: "cycle-legacy".to_string(),
                    authorization_sha256: "a".repeat(64),
                    before_head: Some("d".repeat(40)),
                    owner_pid: None,
                    worker_pgid: None,
                    worker_profile: None,
                    worker_commit: None,
                    mechanical: None,
                    stage: crate::run::WorkStage::Implementing,
                }),
                approval: None,
            },
            created_at,
        )
        .expect("create legacy run");
        handle
            .append_event(
                EventKind::AttemptStarted,
                EventInput {
                    profile_id: Some("fake-worker".to_string()),
                    outcome: Some("running".to_string()),
                    ..EventInput::default()
                },
            )
            .expect("append attempt-started event");
        handle.finish(outcome).expect("finish legacy run");
        handle.run_id().to_string()
    }

    /// Creates a run manifest whose lifecycle is `Running` (Started ->
    /// Running happens on the first non-terminal event) and leaves it that
    /// way — used to exercise `running_run_conflict`.
    fn running_manifest_for(
        temp: &TempDir,
        repo: &str,
        bead: &str,
        created_at: DateTime<Utc>,
    ) -> String {
        let mut handle = RunHandle::create_at(
            temp.path(),
            RunJob::Work,
            crate::run::NewRun {
                target: crate::run::RunTarget {
                    repo: repo.to_string(),
                    bead: Some(bead.to_string()),
                },
                approved_profiles: vec!["claude-sonnet-5".to_string()],
                bursar_roster_artifact: None,
                limits: crate::run::RunLimits::default(),
                verifier: crate::run::RunVerifier::default(),
                work: Some(crate::run::WorkState {
                    cycle_id: "cycle-running".to_string(),
                    authorization_sha256: "a".repeat(64),
                    before_head: Some("c".repeat(40)),
                    owner_pid: None,
                    worker_pgid: None,
                    worker_profile: None,
                    worker_commit: None,
                    mechanical: None,
                    stage: crate::run::WorkStage::Implementing,
                }),
                approval: None,
            },
            created_at,
        )
        .expect("create running run");
        handle
            .append_event(
                EventKind::AttemptStarted,
                EventInput {
                    profile_id: Some("fake-worker".to_string()),
                    outcome: Some("running".to_string()),
                    ..EventInput::default()
                },
            )
            .expect("append attempt-started event");
        handle.run_id().to_string()
    }

    #[test]
    fn repo_lease_acquire_twice_for_the_same_repo_refuses_the_second() {
        let temp = TempDir::new("lease-conflict");
        let _first =
            RepoLease::acquire(temp.path(), "/repo/bursar", "run-a").expect("first lease acquires");

        let error = RepoLease::acquire(temp.path(), "/repo/bursar", "run-b")
            .expect_err("second concurrent lease must refuse");

        assert!(matches!(error, QuarantineError::CaptureFailed(_)));
    }

    #[test]
    fn repo_lease_release_on_drop_lets_a_later_attempt_acquire_it() {
        let temp = TempDir::new("lease-release");
        {
            let _lease = RepoLease::acquire(temp.path(), "/repo/bursar", "run-a")
                .expect("first lease acquires");
        }

        let _second = RepoLease::acquire(temp.path(), "/repo/bursar", "run-b")
            .expect("lease is available again once the holder drops it");
    }

    #[test]
    fn repo_leases_for_different_repos_do_not_conflict() {
        let temp = TempDir::new("lease-different-repos");
        let _a = RepoLease::acquire(temp.path(), "/repo/bursar", "run-a").expect("lease a");
        let _b = RepoLease::acquire(temp.path(), "/repo/other", "run-b").expect("lease b");
    }

    #[test]
    fn independent_processes_with_distinct_state_dirs_share_checkout_and_linked_worktree_lease() {
        let temp = TempDir::new("lease-real-checkout-cross-state");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        git_ok(&repo, &["init", "-b", "main"]);
        git_ok(&repo, &["config", "user.name", "Conductor Test"]);
        git_ok(
            &repo,
            &["config", "user.email", "conductor@example.invalid"],
        );
        std::fs::write(repo.join("README.md"), b"base\n").expect("write base");
        git_ok(&repo, &["add", "README.md"]);
        git_ok(&repo, &["commit", "-m", "initial"]);
        let linked = temp.path().join("linked");
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                linked.to_str().expect("utf8 linked path"),
            ],
        );

        for (label, contender_repo) in [("same", repo.as_path()), ("linked", linked.as_path())] {
            let gate = temp.path().join(format!("gate-{label}"));
            let first_state = temp.path().join(format!("state-a-{label}"));
            let second_state = temp.path().join(format!("state-b-{label}"));
            std::fs::create_dir_all(&gate).expect("mkdir gate");
            let repo_text = repo.to_str().expect("utf8 repo");
            let contender_text = contender_repo.to_str().expect("utf8 contender repo");

            let mut first = spawn_lease_process(
                &first_state,
                &gate,
                "a",
                TestLeaseKind::Repo,
                repo_text,
                "run-a",
                None,
            );
            std::fs::write(gate.join("a.go"), b"go\n").expect("release first contender");
            wait_for_path(&gate.join("a.result"));
            let first_result =
                std::fs::read_to_string(gate.join("a.result")).expect("read first result");
            assert_eq!(first_result, "acquired");

            let mut second = spawn_lease_process(
                &second_state,
                &gate,
                "b",
                TestLeaseKind::Repo,
                contender_text,
                "run-b",
                None,
            );
            std::fs::write(gate.join("b.go"), b"go\n").expect("release second contender");
            wait_for_path(&gate.join("b.result"));
            let second_result =
                std::fs::read_to_string(gate.join("b.result")).expect("read second result");

            std::fs::write(gate.join("a.release"), b"release\n").expect("drop first lease");
            std::fs::write(gate.join("b.release"), b"release\n").expect("drop second lease");
            assert!(first.wait().expect("wait first contender").success());
            assert!(second.wait().expect("wait second contender").success());
            assert!(
                second_result.starts_with("refused:"),
                "{label} checkout used a second lease domain: {second_result}"
            );
        }
    }

    fn git_ok(repo: &Path, args: &[&str]) {
        let output = run_git(repo, args).expect("spawn git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn repo_lease_reclaims_a_stale_holder_whose_process_is_confirmed_dead() {
        // Simulates the crash case: a holder that never reached its `Drop`
        // release because its process was killed. The lease file survives
        // on disk, but the recorded pid is provably no longer running.
        let temp = TempDir::new("lease-stale-reclaim");
        let mut dead = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived process");
        let dead_pid = dead.id();
        dead.wait().expect("reap short-lived process");

        let leases_dir = temp.path().join("leases");
        std::fs::create_dir_all(&leases_dir).expect("mkdir leases dir");
        let stale_path = leases_dir.join(format!("{}.lock", lease_key("/repo/bursar")));
        std::fs::write(
            &stale_path,
            format!("run_id=stale-run\npid={dead_pid}\nrepo=/repo/bursar\n"),
        )
        .expect("write stale lease file");

        let lease = RepoLease::acquire(temp.path(), "/repo/bursar", "run-new")
            .expect("a lease held by a provably dead process must be reclaimed");

        drop(lease);
        assert!(
            stale_path.is_file(),
            "the stable guard inode must survive clean release"
        );
        assert_eq!(
            std::fs::read_to_string(&stale_path).expect("read cleared legacy guard"),
            "",
            "reclaimed legacy holder metadata must be cleared"
        );
        assert!(
            !stale_path.with_extension("owner").exists(),
            "clean drop must remove the released owner generation"
        );
    }

    fn lease_artifact_paths(state_dir: &Path, identity: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = state_dir.join("leases").join(lease_key(identity));
        (
            base.with_extension("lock"),
            base.with_extension("owner"),
            base.with_extension("owner.pending"),
        )
    }

    fn seed_dead_holder(
        state_dir: &Path,
        identity: &str,
        owner_format: TestLeaseOwnerFormat,
    ) -> PathBuf {
        let (guard_path, owner_path, _) = lease_artifact_paths(state_dir, identity);
        std::fs::create_dir_all(guard_path.parent().expect("lease parent"))
            .expect("mkdir leases dir");
        let dead_pid = spawn_dead_pid();
        match owner_format {
            TestLeaseOwnerFormat::Legacy => {
                std::fs::write(
                    &guard_path,
                    format!("run_id=stale-run\npid={dead_pid}\nrepo={identity}\n"),
                )
                .expect("seed legacy stale lease");
            }
            TestLeaseOwnerFormat::V2 => {
                std::fs::write(&guard_path, b"").expect("seed stable lease guard");
                std::fs::write(
                    &owner_path,
                    format!(
                        "lease_version=2\ngeneration={}\nrun_id=stale-run\npid={dead_pid}\nrepo={identity}\n",
                        "a".repeat(64)
                    ),
                )
                .expect("seed v2 stale lease owner");
            }
        }
        guard_path
    }

    fn seed_legacy_dead_holder(state_dir: &Path, identity: &str) -> PathBuf {
        seed_dead_holder(state_dir, identity, TestLeaseOwnerFormat::Legacy)
    }

    fn assert_single_winner_contention_round(
        root: &Path,
        kind: TestLeaseKind,
        identity: &str,
        owner_format: TestLeaseOwnerFormat,
        race: TestLeaseRace,
        round: usize,
    ) {
        let state_dir = root.join(format!(
            "{}-{}-{}-{round}",
            kind.label(),
            owner_format.label(),
            race.label()
        ));
        let gate_dir = state_dir.join("gate");
        std::fs::create_dir_all(&gate_dir).expect("mkdir reclaim gate");
        let guard_path = seed_dead_holder(&state_dir, identity, owner_format);
        let (_, owner_path, pending_path) = lease_artifact_paths(&state_dir, identity);
        let run_a = format!("{}-{round}-a", kind.label());
        let run_b = format!("{}-{round}-b", kind.label());

        let mut contender_a =
            spawn_lease_process(&state_dir, &gate_dir, "a", kind, identity, &run_a, None);
        let mut contender_b =
            spawn_lease_process(&state_dir, &gate_dir, "b", kind, identity, &run_b, None);
        wait_for_path(&gate_dir.join("a.ready"));
        wait_for_path(&gate_dir.join("b.ready"));

        match race {
            TestLeaseRace::Ordered => {
                std::fs::write(gate_dir.join("a.go"), b"go\n").expect("release contender a");
                wait_for_path(&gate_dir.join("a.result"));
                std::fs::write(gate_dir.join("b.go"), b"go\n").expect("release contender b");
                wait_for_path(&gate_dir.join("b.result"));
            }
            TestLeaseRace::Simultaneous => {
                std::fs::write(gate_dir.join("a.go"), b"go\n").expect("release contender a");
                std::fs::write(gate_dir.join("b.go"), b"go\n").expect("release contender b");
                wait_for_path(&gate_dir.join("a.result"));
                wait_for_path(&gate_dir.join("b.result"));
            }
        }

        // Capture both outcomes and the committed owner before either winner
        // can drop, so the loser cannot hide an overwrite behind clean release.
        let result_a =
            std::fs::read_to_string(gate_dir.join("a.result")).expect("read contender a result");
        let result_b =
            std::fs::read_to_string(gate_dir.join("b.result")).expect("read contender b result");
        let authoritative_holder = if owner_path.is_file() {
            &owner_path
        } else {
            &guard_path
        };
        let holder_while_winner_is_live = std::fs::read_to_string(authoritative_holder);

        std::fs::write(gate_dir.join("a.release"), b"release\n")
            .expect("release contender a lease");
        std::fs::write(gate_dir.join("b.release"), b"release\n")
            .expect("release contender b lease");
        assert!(contender_a.wait().expect("wait contender a").success());
        assert!(contender_b.wait().expect("wait contender b").success());

        let (winner_run, loser_run, loser_result) = match (
            result_a.as_str() == "acquired",
            result_b.as_str() == "acquired",
        ) {
            (true, false) => (run_a.as_str(), run_b.as_str(), result_b.as_str()),
            (false, true) => (run_b.as_str(), run_a.as_str(), result_a.as_str()),
            _ => panic!("exactly one contender must acquire; got a={result_a:?}, b={result_b:?}"),
        };
        if matches!(race, TestLeaseRace::Ordered) {
            assert_eq!(winner_run, run_a, "first released contender must win");
        }
        assert!(
            loser_result.starts_with("refused:"),
            "the losing contender must fail closed, got {loser_result:?}"
        );
        let holder_while_winner_is_live =
            holder_while_winner_is_live.expect("read lease while the winning process holds it");
        assert!(
            holder_while_winner_is_live.contains(&format!("run_id={winner_run}")),
            "the loser removed or overwrote the winner's lease: {holder_while_winner_is_live:?}"
        );
        assert!(
            !holder_while_winner_is_live.contains(&format!("run_id={loser_run}")),
            "the loser replaced the winner's identity: {holder_while_winner_is_live:?}"
        );

        {
            let _next = acquire_test_lease(kind, &state_dir, identity, "run-after-drop")
                .expect("winner drop makes the lease available to the next generation");
        }
        assert!(guard_path.is_file(), "the stable guard must persist");
        assert_eq!(
            std::fs::read_to_string(&guard_path).expect("read stable guard"),
            "",
            "legacy owner bytes must not survive reclaim"
        );
        assert!(
            !owner_path.exists(),
            "clean drop must remove its generation"
        );
        assert!(
            !pending_path.exists(),
            "successful reclaim must not strand staging state"
        );
    }

    #[test]
    fn concurrent_dead_holder_reclaim_has_one_real_process_winner_for_repo_and_dispatch_leases() {
        let temp = TempDir::new("lease-stale-contention");
        for round in 0..5 {
            assert_single_winner_contention_round(
                temp.path(),
                TestLeaseKind::Repo,
                "/repo/bursar",
                TestLeaseOwnerFormat::Legacy,
                TestLeaseRace::Ordered,
                round,
            );
            assert_single_winner_contention_round(
                temp.path(),
                TestLeaseKind::Dispatch,
                "conductor://dispatch",
                TestLeaseOwnerFormat::Legacy,
                TestLeaseRace::Ordered,
                round,
            );
            for owner_format in [TestLeaseOwnerFormat::Legacy, TestLeaseOwnerFormat::V2] {
                assert_single_winner_contention_round(
                    temp.path(),
                    TestLeaseKind::Repo,
                    "/repo/bursar",
                    owner_format,
                    TestLeaseRace::Simultaneous,
                    round,
                );
                assert_single_winner_contention_round(
                    temp.path(),
                    TestLeaseKind::Dispatch,
                    "conductor://dispatch",
                    owner_format,
                    TestLeaseRace::Simultaneous,
                    round,
                );
            }
        }
    }

    #[test]
    fn interrupted_reclaim_releases_guard_and_cleans_stale_staging_for_both_identities() {
        let temp = TempDir::new("lease-interrupted-reclaim");
        for kind in [TestLeaseKind::Repo, TestLeaseKind::Dispatch] {
            let identity = match kind {
                TestLeaseKind::Repo => "/repo/bursar",
                TestLeaseKind::Dispatch => "conductor://dispatch",
            };
            for phase in [
                "after_guard_lock",
                "after_owner_stage",
                "after_owner_remove",
                "after_owner_commit",
            ] {
                let state_dir = temp.path().join(format!("{}-{phase}", kind.label()));
                let gate_dir = state_dir.join("gate");
                std::fs::create_dir_all(&gate_dir).expect("mkdir crash gate");
                std::fs::write(gate_dir.join("crash.go"), b"go\n")
                    .expect("pre-release crashing contender");
                let guard_path = seed_legacy_dead_holder(&state_dir, identity);
                let (_, owner_path, pending_path) = lease_artifact_paths(&state_dir, identity);
                let mut interrupted = spawn_lease_process(
                    &state_dir,
                    &gate_dir,
                    "crash",
                    kind,
                    identity,
                    "interrupted-run",
                    Some(phase),
                );

                let status = interrupted.wait().expect("wait interrupted reclaimer");
                assert_eq!(
                    status.code(),
                    Some(86),
                    "helper must exit at the requested {phase} crash point"
                );
                {
                    let _recovered =
                        acquire_test_lease(kind, &state_dir, identity, "post-interruption")
                            .expect("next process reclaims after interrupted metadata transition");
                }
                assert!(guard_path.is_file(), "stable guard survives {phase}");
                assert_eq!(
                    std::fs::read_to_string(&guard_path).expect("read guard after recovery"),
                    "",
                    "legacy metadata is cleared after recovering {phase}"
                );
                assert!(
                    !owner_path.exists(),
                    "clean recovery drops owner after {phase}"
                );
                assert!(
                    !pending_path.exists(),
                    "stale staging file must be cleaned after {phase}"
                );
            }
        }
    }

    #[test]
    fn repo_lease_leaves_live_or_unauthenticated_unlocked_owner_records_untouched() {
        let temp = TempDir::new("lease-owner-fail-closed");
        let identity = "/repo/bursar";
        for (label, contents) in [
            ("empty", String::new()),
            (
                "eperm-or-live",
                format!(
                    "lease_version=2\ngeneration={}\nrun_id=other-user-run\npid=1\nrepo={identity}\n",
                    "c".repeat(64)
                ),
            ),
            (
                "pid-reuse",
                format!(
                    "lease_version=2\ngeneration={}\nrun_id=live-run\npid={}\nrepo={identity}\n",
                    "a".repeat(64),
                    std::process::id()
                ),
            ),
            (
                "unauthenticated",
                format!(
                    "lease_version=2\ngeneration=not-a-generation\nrun_id=corrupt-run\npid={}\nrepo={identity}\n",
                    spawn_dead_pid()
                ),
            ),
        ] {
            let state_dir = temp.path().join(label);
            let (guard_path, owner_path, _) = lease_artifact_paths(&state_dir, identity);
            std::fs::create_dir_all(guard_path.parent().expect("lease parent"))
                .expect("mkdir leases");
            std::fs::write(&guard_path, b"").expect("write stable guard");
            std::fs::write(&owner_path, &contents).expect("write owner record");

            let error = RepoLease::acquire(&state_dir, identity, "new-run")
                .expect_err("live or unauthenticated owner must fail closed");

            assert!(matches!(error, QuarantineError::CaptureFailed(_)));
            assert_eq!(
                std::fs::read_to_string(&owner_path).expect("read untouched owner"),
                contents,
                "refusal must not mutate the {label} owner record"
            );
        }
    }

    #[test]
    fn repo_lease_drop_removes_only_its_authenticated_generation() {
        let temp = TempDir::new("lease-drop-generation");
        let identity = "/repo/bursar";
        let lease = RepoLease::acquire(temp.path(), identity, "owned-run")
            .expect("acquire original generation");
        let (_, owner_path, _) = lease_artifact_paths(temp.path(), identity);
        let owner = std::fs::read_to_string(&owner_path).expect("read original generation");
        let original_generation = parse_lease_generation(&owner).expect("original generation");
        let replacement_generation = "b".repeat(64);
        assert_ne!(original_generation, replacement_generation);
        let replacement = owner.replace(&original_generation, &replacement_generation);
        std::fs::write(&owner_path, &replacement).expect("replace owner generation");

        drop(lease);

        assert_eq!(
            std::fs::read_to_string(&owner_path).expect("replacement generation survives"),
            replacement,
            "a stale Drop must never remove a different owner generation"
        );
        RepoLease::acquire(temp.path(), identity, "later-run")
            .expect_err("the live replacement identity must remain fail-closed");
    }

    #[test]
    fn classify_kill_probe_only_confirms_absence_on_esrch() {
        // Success => the target exists and we could signal it => alive.
        assert!(!classify_kill_probe(true, b""));
        // ESRCH => positively confirmed absent.
        assert!(classify_kill_probe(
            false,
            b"kill: (12345) - No such process\n"
        ));
        assert!(classify_kill_probe(false, b"kill: 12345: no such process"));
        // EPERM => the process exists but is owned by another user; it must
        // read as alive (fail closed), never as dead.
        assert!(!classify_kill_probe(
            false,
            b"kill: (1) - Operation not permitted\n"
        ));
        // Any other unrecognized failure is inconclusive => alive.
        assert!(!classify_kill_probe(false, b"kill: something unexpected\n"));
    }

    #[test]
    fn process_alive_reports_live_and_reaped_processes_and_survives_eperm() {
        // pid 1 (init / launchd) is always running but is owned by root, so a
        // non-root `kill -0 1` returns EPERM rather than success — the exact
        // ambiguity that must never be misread as death. Run as root it simply
        // succeeds; either way pid 1 is alive.
        assert!(process_alive(1), "pid 1 must never read as dead");

        // The current test process is trivially alive.
        assert!(process_alive(std::process::id()));

        // A spawned-then-reaped process is provably gone (ESRCH).
        let mut dead = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived process");
        let dead_pid = dead.id();
        dead.wait().expect("reap short-lived process");
        assert!(
            !process_alive(dead_pid),
            "a reaped process must read as dead"
        );
    }

    #[test]
    fn process_group_alive_tracks_an_orphaned_worker_group_across_its_death() {
        use std::os::unix::process::CommandExt;

        // Mirror how `CommandExec` launches a worker: as the leader of its own
        // process group, so the group id equals the child pid and a dead
        // `conductor` parent would leave this group orphaned but alive.
        let mut worker = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn worker in its own process group");
        let pgid = worker.id();

        assert!(
            process_group_alive(pgid),
            "a live orphaned worker group must never read as gone"
        );

        // The parent (this process, standing in for conductor) tears the whole
        // group down and reaps it — only now is the worker provably gone.
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{pgid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        worker.wait().expect("reap worker");

        assert!(
            !process_group_alive(pgid),
            "an emptied worker group must read as gone"
        );
    }

    #[test]
    fn repo_lease_does_not_reclaim_when_the_holder_pid_cannot_be_parsed() {
        // A corrupt or foreign-format lease file has no provable-dead pid —
        // it must be treated as still held, never auto-reclaimed.
        let temp = TempDir::new("lease-unparseable-holder");
        let leases_dir = temp.path().join("leases");
        std::fs::create_dir_all(&leases_dir).expect("mkdir leases dir");
        let path = leases_dir.join(format!("{}.lock", lease_key("/repo/bursar")));
        std::fs::write(&path, "not a lease file at all\n").expect("write corrupt lease file");

        let error = RepoLease::acquire(temp.path(), "/repo/bursar", "run-new")
            .expect_err("a lease whose holder cannot be proven dead must not be reclaimed");

        assert!(matches!(error, QuarantineError::CaptureFailed(_)));
        assert!(
            path.exists(),
            "the unparseable lease file must be left in place"
        );
    }

    #[test]
    fn running_run_conflict_finds_another_running_run_for_the_same_repo() {
        let temp = TempDir::new("running-conflict");
        let running_run_id = running_manifest_for(&temp, "/repo/bursar", "bursar-467", fixed_now());

        let found = running_run_conflict(temp.path(), "/repo/bursar", "some-other-run-id")
            .expect("scan succeeds")
            .expect("conflict found");

        assert_eq!(found, running_run_id);
    }

    #[test]
    fn running_run_conflict_excludes_the_calling_runs_own_id() {
        let temp = TempDir::new("running-self-exclude");
        let running_run_id = running_manifest_for(&temp, "/repo/bursar", "bursar-467", fixed_now());

        let found = running_run_conflict(temp.path(), "/repo/bursar", &running_run_id)
            .expect("scan succeeds");

        assert_eq!(found, None, "a run must never conflict with itself");
    }

    #[test]
    fn running_run_conflict_ignores_finished_runs_and_other_repos() {
        let temp = TempDir::new("running-ignore");
        manifest_for(
            &temp,
            "/repo/bursar",
            "bursar-467",
            "failed",
            None,
            fixed_now(),
        );
        running_manifest_for(&temp, "/repo/other", "other-1", fixed_now());

        let found =
            running_run_conflict(temp.path(), "/repo/bursar", "excluded").expect("scan succeeds");

        assert_eq!(found, None);
    }

    #[test]
    fn quarantine_dirty_attempt_with_lease_refuses_while_another_run_is_running_against_the_repo() {
        let temp = TempDir::new("leased-running-conflict");
        let handle = run_handle(&temp, "leased-running-conflict");
        running_manifest_for(&temp, "/repo/conductor", "bead-other", fixed_now());
        let commits = FakeCommits::new([Some("head1"), Some("head1")], [false, true]);
        let recovery = FakeRecovery {
            changed_paths: vec!["src/lib.rs".to_string()],
            patch: b"diff --git a/src/lib.rs b/src/lib.rs\n+dirty\n".to_vec(),
            ..FakeRecovery::default()
        };

        let error = quarantine_dirty_attempt_with_lease(
            Path::new("/repo/conductor"),
            "/repo/conductor",
            temp.path(),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "010-attempt",
        )
        .expect_err("must refuse while another run is Running against this repo");

        assert!(matches!(error, QuarantineError::CaptureFailed(_)));
        assert_eq!(
            *recovery.restore_calls.borrow(),
            0,
            "must not mutate the tree"
        );
    }

    #[test]
    fn most_recent_failed_run_finds_unique_recent_failure_for_target() {
        let temp = TempDir::new("legacy-lookup");
        let created_at = fixed_now();
        let run_id = manifest_for(
            &temp,
            "/repo/bursar",
            "bursar-467",
            "failed",
            None,
            created_at,
        );

        let found = most_recent_failed_run(temp.path(), "/repo/bursar", "bursar-467")
            .expect("lookup succeeds")
            .expect("run found");

        assert_eq!(found.run_id, run_id);
        assert_eq!(found.before_head, None);
        assert_eq!(found.created_at, created_at);
        assert!(
            !found.attempt_started,
            "manifest_for never appends an AttemptStarted event, so this must be false"
        );
    }

    #[test]
    fn most_recent_failed_run_reports_attempt_started_true_when_the_event_is_present() {
        let temp = TempDir::new("legacy-attempt-started");
        let created_at = fixed_now();
        let run_id = finished_run_with_attempt_started_for(
            &temp,
            "/repo/bursar",
            "bursar-467",
            "failed",
            created_at,
        );

        let found = most_recent_failed_run(temp.path(), "/repo/bursar", "bursar-467")
            .expect("lookup succeeds")
            .expect("run found");

        assert_eq!(found.run_id, run_id);
        assert!(found.attempt_started);
    }

    #[test]
    fn most_recent_failed_run_ignores_runs_for_other_targets() {
        let temp = TempDir::new("legacy-other-target");
        manifest_for(&temp, "/repo/other", "other-1", "failed", None, fixed_now());

        let found = most_recent_failed_run(temp.path(), "/repo/bursar", "bursar-467")
            .expect("lookup succeeds");

        assert_eq!(found, None);
    }

    #[test]
    fn most_recent_failed_run_refuses_when_a_later_run_supersedes_the_failure() {
        let temp = TempDir::new("legacy-superseded");
        manifest_for(
            &temp,
            "/repo/bursar",
            "bursar-467",
            "failed",
            None,
            fixed_now(),
        );
        manifest_for(
            &temp,
            "/repo/bursar",
            "bursar-467",
            "verified",
            None,
            fixed_now() + chrono::Duration::minutes(5),
        );

        let found = most_recent_failed_run(temp.path(), "/repo/bursar", "bursar-467")
            .expect("lookup succeeds");

        assert_eq!(found, None, "a later verified run means nothing to adopt");
    }

    #[test]
    fn most_recent_failed_run_fails_closed_on_tampered_evidence_for_our_target() {
        let temp = TempDir::new("legacy-tampered");
        let run_id = manifest_for(
            &temp,
            "/repo/bursar",
            "bursar-467",
            "failed",
            None,
            fixed_now(),
        );
        let manifest_path = temp.path().join("runs").join(&run_id).join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["work"]["authorization_sha256"] = serde_json::json!("not-a-sha256");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = most_recent_failed_run(temp.path(), "/repo/bursar", "bursar-467")
            .expect_err("tampered evidence for our own target must fail closed");

        assert!(matches!(error, QuarantineError::CaptureFailed(_)));
    }

    #[test]
    fn most_recent_failed_run_fails_closed_on_unparseable_manifest_that_may_be_relevant() {
        // A manifest so badly corrupted it doesn't even parse as JSON can't
        // be checked via the normal `target.repo`/`target.bead` fields, but
        // the raw bytes still mention both identifiers — good enough reason
        // to refuse rather than silently skip it as unrelated.
        let temp = TempDir::new("legacy-unparseable");
        let run_id = manifest_for(
            &temp,
            "/repo/bursar",
            "bursar-467",
            "failed",
            None,
            fixed_now(),
        );
        let manifest_path = temp.path().join("runs").join(&run_id).join("manifest.json");
        std::fs::write(
            &manifest_path,
            b"{ not valid json but still mentions /repo/bursar and bursar-467 \xff\xfe",
        )
        .unwrap();

        let error = most_recent_failed_run(temp.path(), "/repo/bursar", "bursar-467")
            .expect_err("unparseable evidence that might be ours must fail closed");

        assert!(matches!(error, QuarantineError::CaptureFailed(_)));
    }

    #[test]
    fn most_recent_failed_run_ignores_unparseable_manifest_for_an_unrelated_target() {
        // The flip side: a manifest that is definitely unrelated (mentions
        // neither our repo nor our bead anywhere in its bytes) stays safely
        // ignorable even when it's corrupt, matching the existing tolerance
        // for broken manifests belonging to other targets.
        let temp = TempDir::new("legacy-unparseable-unrelated");
        let run_id = manifest_for(&temp, "/repo/other", "other-1", "failed", None, fixed_now());
        let manifest_path = temp.path().join("runs").join(&run_id).join("manifest.json");
        std::fs::write(
            &manifest_path,
            b"{ not valid json, and unrelated to our target",
        )
        .unwrap();

        let found = most_recent_failed_run(temp.path(), "/repo/bursar", "bursar-467")
            .expect("unrelated corrupt manifest must not block the scan");

        assert_eq!(found, None);
    }

    #[test]
    fn authenticate_legacy_adoption_matches_recorded_before_head_exactly() {
        let commits = FakeCommits::new([Some("sha-a")], []);
        let run = AdoptableRun {
            run_id: "run-work-1".to_string(),
            before_head: Some("sha-a".to_string()),
            created_at: fixed_now(),
            attempt_started: true,
        };

        let head = authenticate_legacy_adoption(&commits, Path::new("/repo/bursar"), &run, None)
            .expect("authentication succeeds");

        assert_eq!(head, "sha-a");
    }

    #[test]
    fn authenticate_legacy_adoption_refuses_when_recorded_head_does_not_match() {
        let commits = FakeCommits::new([Some("sha-b")], []);
        let run = AdoptableRun {
            run_id: "run-work-1".to_string(),
            before_head: Some("sha-a".to_string()),
            created_at: fixed_now(),
            attempt_started: true,
        };

        let error = authenticate_legacy_adoption(&commits, Path::new("/repo/bursar"), &run, None)
            .expect_err("head mismatch must refuse");

        assert!(matches!(error, QuarantineError::HeadMoved { .. }));
    }

    #[test]
    fn authenticate_legacy_adoption_refuses_manifests_without_a_recorded_before_head() {
        // A manifest predating `before_head` capture has no durable proof of
        // which commit the failed worker attempt actually started from —
        // there is no safe automatic heuristic for this, only deliberate
        // operator recovery, so authentication must refuse outright rather
        // than infer provenance from commit timestamps. An empty `heads`
        // queue also proves `commits.head` is never even called in this
        // path — calling it would panic on the empty queue.
        let commits = FakeCommits::new([], []);
        let run = AdoptableRun {
            run_id: "run-work-legacy".to_string(),
            before_head: None,
            created_at: fixed_now(),
            attempt_started: true,
        };

        let error = authenticate_legacy_adoption(&commits, Path::new("/repo/bursar"), &run, None)
            .expect_err("before_head-less legacy adoption must fail closed");

        assert!(matches!(error, QuarantineError::CaptureFailed(_)));
    }

    #[test]
    fn authenticate_legacy_adoption_refuses_when_operator_authorization_names_a_different_run() {
        let commits = FakeCommits::new([], []);
        let run = AdoptableRun {
            run_id: "run-work-legacy".to_string(),
            before_head: None,
            created_at: fixed_now(),
            attempt_started: true,
        };

        let error = authenticate_legacy_adoption(
            &commits,
            Path::new("/repo/bursar"),
            &run,
            Some("run-work-some-other-run"),
        )
        .expect_err("authorization for a different run_id must not authorize this one");

        assert!(matches!(error, QuarantineError::CaptureFailed(_)));
    }

    #[test]
    fn authenticate_legacy_adoption_accepts_explicit_operator_authorization_by_exact_run_id() {
        // The bursar-467-shaped incident: no before_head was ever recorded,
        // but an operator has explicitly named this exact run_id (not a
        // blanket policy) as reviewed and safe to adopt. Authentication
        // trusts the current HEAD as-is since there is still no before_head
        // to compare against.
        let commits = FakeCommits::new([Some("current-head-sha")], []);
        let run = AdoptableRun {
            run_id: "run-work-legacy".to_string(),
            before_head: None,
            created_at: fixed_now(),
            attempt_started: true,
        };

        let head = authenticate_legacy_adoption(
            &commits,
            Path::new("/repo/bursar"),
            &run,
            Some("run-work-legacy"),
        )
        .expect("explicit per-run operator authorization is sufficient");

        assert_eq!(head, "current-head-sha");
    }

    #[test]
    fn authenticate_legacy_adoption_refuses_when_attempt_started_is_missing_even_with_recorded_before_head()
     {
        // A recorded before_head alone is not proof a worker ever actually
        // ran — a run manifest could in principle be created and then
        // abandoned before any dispatch. An empty `heads` queue also proves
        // `commits.head` is never called: refusal happens before any HEAD
        // comparison is attempted.
        let commits = FakeCommits::new([], []);
        let run = AdoptableRun {
            run_id: "run-work-no-attempt".to_string(),
            before_head: Some("sha-a".to_string()),
            created_at: fixed_now(),
            attempt_started: false,
        };

        let error = authenticate_legacy_adoption(&commits, Path::new("/repo/bursar"), &run, None)
            .expect_err("missing AttemptStarted proof must fail closed even with a before_head");

        assert!(matches!(error, QuarantineError::CaptureFailed(_)));
    }

    #[test]
    fn authenticate_legacy_adoption_accepts_missing_attempt_started_with_explicit_operator_authorization()
     {
        // The explicit per-run override covers both missing-evidence cases
        // at once: no AttemptStarted proof, but the operator has manually
        // reviewed and authorized this exact run_id. The recorded
        // before_head is still checked against the current HEAD since it is
        // present.
        let commits = FakeCommits::new([Some("sha-a")], []);
        let run = AdoptableRun {
            run_id: "run-work-no-attempt".to_string(),
            before_head: Some("sha-a".to_string()),
            created_at: fixed_now(),
            attempt_started: false,
        };

        let head = authenticate_legacy_adoption(
            &commits,
            Path::new("/repo/bursar"),
            &run,
            Some("run-work-no-attempt"),
        )
        .expect("operator authorization overrides missing AttemptStarted proof");

        assert_eq!(head, "sha-a");
    }

    #[test]
    fn parse_porcelain_z_rejects_non_utf8_paths_with_a_manual_recovery_diagnostic() {
        // `-z` mode disables C-quoting entirely, so an invalid byte in a raw
        // path appears literally rather than escaped — this is the exact
        // shape `git status --porcelain -z` produces for a non-UTF-8 name.
        let mut raw = b"?? stray-".to_vec();
        raw.push(0xFF);
        raw.extend_from_slice(b"name.tmp");
        raw.push(0);

        let error = parse_porcelain_z(&raw).expect_err("non-UTF-8 path must be rejected");

        assert!(
            error.to_lowercase().contains("utf-8"),
            "diagnostic must explain the non-UTF-8 path, got: {error}"
        );
        assert!(
            error.contains("manual"),
            "diagnostic must point at manual recovery, got: {error}"
        );
    }

    #[test]
    fn split_nul_paths_rejects_non_utf8_paths_with_a_manual_recovery_diagnostic() {
        let mut raw = b"stray-".to_vec();
        raw.push(0xFF);
        raw.extend_from_slice(b"name.tmp");
        raw.push(0);

        let error = split_nul_paths(&raw).expect_err("non-UTF-8 path must be rejected");

        assert!(
            error.to_lowercase().contains("utf-8"),
            "diagnostic must explain the non-UTF-8 path, got: {error}"
        );
        assert!(
            error.contains("manual"),
            "diagnostic must point at manual recovery, got: {error}"
        );
    }

    #[test]
    fn quarantine_dirty_attempt_never_mutates_when_changed_paths_cannot_be_read() {
        // Mirrors what a real non-UTF-8 path produces: `changed_paths` fails
        // before any destructive git command has a chance to run. This
        // proves the fail-closed property at the `quarantine_dirty_attempt`
        // level, not just inside the parser.
        let temp = TempDir::new("changed-paths-unreadable");
        let handle = run_handle(&temp, "changed-paths-unreadable");
        let commits = FakeCommits::new([Some("head1")], [false]);
        let recovery = FakeRecovery {
            changed_paths_error: RefCell::new(Some(
                "git status reported a non-UTF-8 path".to_string(),
            )),
            ..FakeRecovery::default()
        };

        let error = quarantine_dirty_attempt(
            Path::new("/repo/conductor"),
            &commits,
            &recovery,
            &handle,
            Some("head1"),
            "013-attempt",
        )
        .expect_err("unreadable changed paths must fail closed");

        assert!(matches!(error, QuarantineError::CaptureFailed(_)));
        assert_eq!(*recovery.restore_calls.borrow(), 0, "must not mutate tree");
        assert!(
            recovery.apply_excluding_calls.borrow().is_empty(),
            "must not attempt any reapply either"
        );
    }
}

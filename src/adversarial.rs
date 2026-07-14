//! Approval-gated, read-only adversarial design review state.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;
const MAX_REVIEW_ID_BYTES: usize = 128;
const ARTIFACT_FILE: &str = "artifact.bin";

#[derive(Debug)]
pub(crate) struct AdversarialError(String);

impl AdversarialError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn io(action: &str, path: &Path, error: &std::io::Error) -> Self {
        Self(format!("{action} {}: {error}", path.display()))
    }
}

impl fmt::Display for AdversarialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AdversarialError {}

type Result<T> = std::result::Result<T, AdversarialError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArtifactSnapshot {
    pub(crate) source_path: PathBuf,
    pub(crate) snapshot_path: PathBuf,
    pub(crate) review_dir: PathBuf,
    pub(crate) sha256: String,
    pub(crate) size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ArtifactRecord {
    source_path: String,
    snapshot_file: String,
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ArtifactPlan {
    schema: String,
    review_id: String,
    artifact: ArtifactRecord,
}

#[derive(Serialize)]
struct EmptyProviderSnapshot<'a> {
    schema: &'a str,
    providers: [String; 0],
}

#[derive(Serialize)]
struct LifecycleState<'a> {
    schema: &'a str,
    status: &'a str,
}

pub(crate) fn snapshot_artifact(
    artifact_path: &Path,
    state_root: &Path,
    review_id: &str,
) -> Result<ArtifactSnapshot> {
    validate_review_id(review_id)?;
    let (source_path, bytes) = read_artifact(artifact_path)?;
    let size_bytes = u64::try_from(bytes.len())
        .map_err(|_| AdversarialError::new("artifact byte length does not fit u64"))?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let review_dir = state_root.join(review_id);
    if review_dir.exists() {
        return Err(AdversarialError::new(format!(
            "review state already exists: {}",
            review_dir.display()
        )));
    }
    fs::create_dir_all(state_root)
        .map_err(|error| AdversarialError::io("failed to create state root", state_root, &error))?;
    let temp_dir = create_temp_review_dir(state_root, review_id)?;
    let plan = ArtifactPlan {
        schema: "conductor-adversarial-plan-v1".to_string(),
        review_id: review_id.to_string(),
        artifact: ArtifactRecord {
            source_path: source_path
                .to_str()
                .ok_or_else(|| AdversarialError::new("canonical artifact path is not UTF-8"))?
                .to_string(),
            snapshot_file: ARTIFACT_FILE.to_string(),
            sha256: sha256.clone(),
            size_bytes,
        },
    };
    let write_result = (|| {
        write_new_file(&temp_dir.join(ARTIFACT_FILE), &bytes)?;
        write_new_file(
            &temp_dir.join("artifact.sha256"),
            format!("{sha256}\n").as_bytes(),
        )?;
        write_json(&temp_dir.join("plan.json"), &plan)?;
        write_json(
            &temp_dir.join("provider-snapshot.json"),
            &EmptyProviderSnapshot {
                schema: "conductor-adversarial-provider-snapshot-v1",
                providers: [],
            },
        )?;
        write_json(
            &temp_dir.join("lifecycle.json"),
            &LifecycleState {
                schema: "conductor-adversarial-lifecycle-v1",
                status: "artifact-snapshotted",
            },
        )?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(error);
    }
    if review_dir.exists() {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(AdversarialError::new(format!(
            "review state already exists: {}",
            review_dir.display()
        )));
    }
    if let Err(error) = fs::rename(&temp_dir, &review_dir) {
        let _ = fs::remove_dir_all(&temp_dir);
        return Err(AdversarialError::io(
            "failed to publish review state",
            &review_dir,
            &error,
        ));
    }

    Ok(ArtifactSnapshot {
        source_path,
        snapshot_path: review_dir.join(ARTIFACT_FILE),
        review_dir,
        sha256,
        size_bytes,
    })
}

fn read_artifact(path: &Path) -> Result<(PathBuf, Vec<u8>)> {
    if contains_ai_scratch(path) {
        return Err(AdversarialError::new(
            "artifact path contains a forbidden ai-scratch component",
        ));
    }
    let initial = fs::symlink_metadata(path)
        .map_err(|error| AdversarialError::io("failed to inspect artifact", path, &error))?;
    if initial.file_type().is_symlink() {
        return Err(AdversarialError::new("artifact must not be a symlink"));
    }
    require_regular_readable_file(path, &initial)?;
    let canonical = fs::canonicalize(path)
        .map_err(|error| AdversarialError::io("failed to canonicalize artifact", path, &error))?;
    if contains_ai_scratch(&canonical) {
        return Err(AdversarialError::new(
            "canonical artifact path contains a forbidden ai-scratch component",
        ));
    }

    let mut file = File::open(&canonical)
        .map_err(|error| AdversarialError::io("failed to open artifact", &canonical, &error))?;
    let opened = file.metadata().map_err(|error| {
        AdversarialError::io("failed to inspect opened artifact", &canonical, &error)
    })?;
    let current = fs::symlink_metadata(&canonical).map_err(|error| {
        AdversarialError::io(
            "failed to re-inspect canonical artifact",
            &canonical,
            &error,
        )
    })?;
    if current.file_type().is_symlink() {
        return Err(AdversarialError::new(
            "artifact became a symlink while being opened",
        ));
    }
    require_regular_readable_file(&canonical, &opened)?;
    if !same_file(&opened, &current) {
        return Err(AdversarialError::new(
            "artifact identity changed while being opened",
        ));
    }

    let mut bytes = Vec::with_capacity(usize::try_from(opened.len()).unwrap_or(0));
    (&mut file)
        .take(u64::try_from(MAX_ARTIFACT_BYTES + 1).expect("artifact limit fits u64"))
        .read_to_end(&mut bytes)
        .map_err(|error| AdversarialError::io("failed to read artifact", &canonical, &error))?;
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(AdversarialError::new(format!(
            "artifact exceeds {MAX_ARTIFACT_BYTES} bytes"
        )));
    }
    let after = file.metadata().map_err(|error| {
        AdversarialError::io("failed to re-inspect artifact", &canonical, &error)
    })?;
    if !same_file(&opened, &after)
        || after.len() != opened.len()
        || after.len() != bytes.len() as u64
    {
        return Err(AdversarialError::new(
            "artifact changed while its immutable snapshot was being read",
        ));
    }
    Ok((canonical, bytes))
}

fn require_regular_readable_file(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if !metadata.is_file() {
        return Err(AdversarialError::new("artifact must be a regular file"));
    }
    if metadata.len() > MAX_ARTIFACT_BYTES as u64 {
        return Err(AdversarialError::new(format!(
            "artifact exceeds {MAX_ARTIFACT_BYTES} bytes"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o444 == 0 {
            return Err(AdversarialError::new(format!(
                "artifact is not readable: {}",
                path.display()
            )));
        }
        if metadata.nlink() > 1 {
            return Err(AdversarialError::new(format!(
                "artifact has multiple hard links and cannot be proven outside ai-scratch: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    true
}

fn contains_ai_scratch(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::Normal(name)
                if name == OsStr::new("ai-scratch")
                    || name.to_str().is_some_and(|name| name.eq_ignore_ascii_case("ai-scratch"))
        )
    })
}

fn validate_review_id(review_id: &str) -> Result<()> {
    let mut bytes = review_id.bytes();
    let valid = !review_id.is_empty()
        && review_id.len() <= MAX_REVIEW_ID_BYTES
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(AdversarialError::new(format!(
            "invalid review id {review_id:?}; expected an alphanumeric prefix followed by alphanumeric, '_' or '-' bytes"
        )))
    }
}

fn create_temp_review_dir(state_root: &Path, review_id: &str) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    for attempt in 0_u8..100 {
        let path = state_root.join(format!(
            ".{review_id}.{}-{nanos}-{attempt}.tmp",
            std::process::id()
        ));
        let created = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700).create(&path)
            }
            #[cfg(not(unix))]
            {
                fs::create_dir(&path)
            }
        };
        match created {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(AdversarialError::io(
                    "failed to create temporary review state",
                    &path,
                    &error,
                ));
            }
        }
    }
    Err(AdversarialError::new(
        "failed to allocate a unique temporary review state directory",
    ))
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| AdversarialError::io("failed to create state file", path, &error))?;
    file.write_all(bytes)
        .map_err(|error| AdversarialError::io("failed to write state file", path, &error))?;
    file.sync_all()
        .map_err(|error| AdversarialError::io("failed to sync state file", path, &error))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| AdversarialError::new(format!("failed to serialize state: {error}")))?;
    bytes.push(b'\n');
    write_new_file(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn artifact_snapshot_preserves_exact_bytes_hash_and_atomic_state() {
        let temp = TempDir::new("artifact-exact");
        let artifact = temp.path().join("decision.bin");
        let bytes = b"line one\r\n\0\xffline two\n";
        std::fs::write(&artifact, bytes).expect("write artifact");

        let snapshot = snapshot_artifact(&artifact, &temp.path().join("state"), "review-exact")
            .expect("snapshot accepted artifact");

        assert_eq!(snapshot.size_bytes, bytes.len() as u64);
        assert_eq!(snapshot.sha256, format!("{:x}", Sha256::digest(bytes)));
        assert_eq!(std::fs::read(&snapshot.snapshot_path).unwrap(), bytes);
        assert_eq!(
            snapshot.source_path,
            std::fs::canonicalize(&artifact).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(snapshot.review_dir.join("artifact.sha256")).unwrap(),
            format!("{}\n", snapshot.sha256)
        );
        for file in ["plan.json", "provider-snapshot.json", "lifecycle.json"] {
            assert!(snapshot.review_dir.join(file).is_file(), "missing {file}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&snapshot.review_dir)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&snapshot.snapshot_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let plan: serde_json::Value =
            serde_json::from_slice(&std::fs::read(snapshot.review_dir.join("plan.json")).unwrap())
                .unwrap();
        assert_eq!(plan["artifact"]["sha256"], snapshot.sha256);
        assert_eq!(plan["artifact"]["size_bytes"], bytes.len() as u64);
    }

    #[test]
    fn artifact_rejects_directory_oversize_and_ai_scratch_component() {
        let temp = TempDir::new("artifact-boundaries");
        let state = temp.path().join("state");
        assert!(snapshot_artifact(temp.path(), &state, "review-directory").is_err());

        let oversized = temp.path().join("oversized.bin");
        std::fs::write(&oversized, vec![0_u8; MAX_ARTIFACT_BYTES + 1]).unwrap();
        assert!(snapshot_artifact(&oversized, &state, "review-oversized").is_err());
        assert!(!state.join("review-oversized").exists());

        let scratch = temp.path().join("AI-SCRATCH");
        std::fs::create_dir(&scratch).unwrap();
        let scratch_artifact = scratch.join("decision.md");
        std::fs::write(&scratch_artifact, b"secret").unwrap();
        assert!(snapshot_artifact(&scratch_artifact, &state, "review-scratch").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn artifact_rejects_symlinks_unreadable_files_and_canonical_ai_scratch() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = TempDir::new("artifact-unix-boundaries");
        let state = temp.path().join("state");
        let target = temp.path().join("target.md");
        std::fs::write(&target, b"target").unwrap();
        let link = temp.path().join("link.md");
        symlink(&target, &link).unwrap();
        assert!(snapshot_artifact(&link, &state, "review-symlink").is_err());

        let unreadable = temp.path().join("unreadable.md");
        std::fs::write(&unreadable, b"closed").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = snapshot_artifact(&unreadable, &state, "review-unreadable");
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(result.is_err());

        let scratch = temp.path().join("ai-scratch");
        std::fs::create_dir(&scratch).unwrap();
        std::fs::write(scratch.join("decision.md"), b"secret").unwrap();
        let alias = temp.path().join("alias");
        symlink(&scratch, &alias).unwrap();
        assert!(
            snapshot_artifact(
                &alias.join("decision.md"),
                &state,
                "review-canonical-scratch"
            )
            .is_err()
        );

        let hard_link = temp.path().join("hard-link-alias.md");
        std::fs::hard_link(scratch.join("decision.md"), &hard_link).unwrap();
        assert!(snapshot_artifact(&hard_link, &state, "review-hard-link").is_err());
    }

    #[test]
    fn artifact_rejects_invalid_or_reused_review_id() {
        let temp = TempDir::new("artifact-review-id");
        let artifact = temp.path().join("decision.md");
        let state = temp.path().join("state");
        std::fs::write(&artifact, b"decision").unwrap();
        assert!(snapshot_artifact(&artifact, &state, "../escape").is_err());
        snapshot_artifact(&artifact, &state, "review-once").expect("first snapshot");
        assert!(snapshot_artifact(&artifact, &state, "review-once").is_err());
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("conductor-{label}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
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
}

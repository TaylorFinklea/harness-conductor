//! journal (~/.local/state/conductor/)
//!
//! Writes/overwrites `journal.json` in the state dir with the latest cycle entry.
//! The `conductor status` command reads `last_cycle` from this file.

#![allow(dead_code)]

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Top-level journal shape — `last_cycle` is the most recent entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Journal {
    pub(crate) last_cycle: JournalEntry,
}

/// One cycle's journal entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JournalEntry {
    pub(crate) id: String,
    pub(crate) completed_at: String,
    pub(crate) dry_run: bool,
    pub(crate) summary: JournalSummary,
}

/// Numeric summary of one cycle's outcomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JournalSummary {
    pub(crate) scanned: u64,
    pub(crate) ready: u64,
    pub(crate) dispatched: u64,
    pub(crate) proposed: u64,
    pub(crate) verified: u64,
    pub(crate) flagged: u64,
    pub(crate) skipped: u64,
}

/// Writes (overwrites) `journal.json` with the given entry as `last_cycle`.
pub(crate) fn write_journal(state_dir: &Path, entry: &JournalEntry) -> io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let journal = Journal {
        last_cycle: entry.clone(),
    };
    let json = serde_json::to_vec_pretty(&journal)
        .map_err(io::Error::other)?;
    let path = state_dir.join("journal.json");
    std::fs::write(path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn write_journal_creates_valid_json() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("conductor-state-test-{nanos}"));
        let _ = std::fs::remove_dir_all(&tmp);

        let entry = JournalEntry {
            id: "cycle-20260702-120000".to_string(),
            completed_at: "2026-07-02T12:00:00Z".to_string(),
            dry_run: true,
            summary: JournalSummary {
                scanned: 5,
                ready: 10,
                dispatched: 0,
                proposed: 3,
                verified: 0,
                flagged: 2,
                skipped: 1,
            },
        };

        write_journal(&tmp, &entry).unwrap();

        let path = tmp.join("journal.json");
        assert!(path.is_file());

        let content = std::fs::read_to_string(&path).unwrap();
        let journal: Journal = serde_json::from_str(&content).unwrap();
        assert_eq!(journal.last_cycle.id, "cycle-20260702-120000");
        assert!(journal.last_cycle.dry_run);
        assert_eq!(journal.last_cycle.summary.scanned, 5);
        assert_eq!(journal.last_cycle.summary.ready, 10);
        assert_eq!(journal.last_cycle.summary.proposed, 3);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

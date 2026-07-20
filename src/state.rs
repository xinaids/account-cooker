//! Single-checkpoint crash recovery.
//!
//! An agent that gets SIGKILLed mid-sleep and restarted has no memory of
//! when it last acted — without a checkpoint it would either wait a full
//! fresh interval (harmless but wasteful) or, in a restart-loop scenario,
//! could fire actions far more often than its configured cadence, which is
//! itself a clustering tell (a burst of tightly-spaced actions after a
//! crash-restart loop looks nothing like human behavior).
//!
//! This is deliberately ONE checkpoint per agent (last action time + last
//! signature), not a multi-checkpoint journal. It is written atomically
//! (tmp file + rename) so a kill mid-write can never leave a torn/partial
//! file — the reader either sees the old checkpoint or the new one, never
//! a corrupt mix. See `scripts/recovery_test.sh` for the executed proof.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Checkpoint {
    /// Unix timestamp (seconds) of the last completed action.
    pub last_action_unix: i64,
    /// Signature (or opaque id) of the last completed action, if any.
    pub last_sig: Option<String>,
    /// Unix timestamp (seconds) at which the next action is due. Computed
    /// once right after an action completes, so a restart resumes the
    /// remaining wait instead of restarting the full interval (or worse,
    /// acting immediately).
    pub next_action_due_unix: i64,
    /// Monotonically increasing count of completed actions. Used by the
    /// recovery test to detect duplication (an action recorded twice) or
    /// state loss (the count resetting after a restart).
    pub action_count: u64,
}

fn checkpoint_path(state_dir: &Path, label: &str) -> PathBuf {
    state_dir.join(format!("{label}.json"))
}

impl Checkpoint {
    pub fn load(state_dir: &Path, label: &str) -> Option<Self> {
        let path = checkpoint_path(state_dir, label);
        let raw = fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Atomic write: write to a sibling tmp file, then rename over the
    /// real path. `rename` on the same filesystem is atomic, so a SIGKILL
    /// at any point during this call leaves either the previous checkpoint
    /// (untouched) or the new one — never a truncated/corrupt file.
    pub fn save(&self, state_dir: &Path, label: &str) -> anyhow::Result<()> {
        fs::create_dir_all(state_dir)?;
        let final_path = checkpoint_path(state_dir, label);
        let tmp_path = state_dir.join(format!("{label}.json.tmp"));
        let json = serde_json::to_string(self)?;
        {
            let mut f = fs::File::create(&tmp_path)?;
            f.write_all(json.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trips_through_disk() {
        let dir = tempdir().unwrap();
        let cp = Checkpoint {
            last_action_unix: 1000,
            last_sig: Some("abc123".into()),
            next_action_due_unix: 1500,
            action_count: 7,
        };
        cp.save(dir.path(), "agent-01").unwrap();
        let loaded = Checkpoint::load(dir.path(), "agent-01").unwrap();
        assert_eq!(cp, loaded);
    }

    #[test]
    fn missing_checkpoint_is_none_not_a_crash() {
        let dir = tempdir().unwrap();
        assert!(Checkpoint::load(dir.path(), "never-ran").is_none());
    }

    #[test]
    fn save_overwrites_atomically_leaving_no_tmp_file() {
        let dir = tempdir().unwrap();
        let cp1 = Checkpoint {
            last_action_unix: 1,
            last_sig: None,
            next_action_due_unix: 2,
            action_count: 1,
        };
        let cp2 = Checkpoint {
            last_action_unix: 3,
            last_sig: Some("x".into()),
            next_action_due_unix: 4,
            action_count: 2,
        };
        cp1.save(dir.path(), "a").unwrap();
        cp2.save(dir.path(), "a").unwrap();
        let loaded = Checkpoint::load(dir.path(), "a").unwrap();
        assert_eq!(loaded, cp2);
        assert!(!dir.path().join("a.json.tmp").exists());
    }
}

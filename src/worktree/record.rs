//! What meta records about a worktree set it created.
//!
//! There is no global list. Each linked worktree meta creates gets a record
//! in git's own admin directory for that worktree
//! (`<repo git dir>/worktrees/<id>/meta-worktree.json`). Git ignores the
//! file and deletes it with the worktree (`git worktree remove` or `prune`).
//! Only something that can write the repository's git directory can make
//! one, so a record is meta's proof that it made the set the record names.

use anyhow::{Context, Result};
use meta_core::display::escape_path;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The record's file name inside a worktree's admin directory.
pub const RECORD_FILE: &str = "meta-worktree.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetRecord {
    /// The set directory, resolved when it was created.
    pub set: PathBuf,
    /// The meta directory the set was created from.
    pub project: PathBuf,
    pub created_at: String,
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub custom: HashMap<String, String>,
}

impl SetRecord {
    /// The set's name: its directory's name.
    pub fn name(&self) -> String {
        self.set
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Seconds until the TTL expires: `None` without a TTL, negative once
    /// expired. A malformed `created_at` warns and never expires.
    pub fn ttl_remaining(&self, now_epoch: i64) -> Option<i64> {
        self.ttl_seconds.map(|ttl| {
            let created = match chrono::DateTime::parse_from_rfc3339(&self.created_at) {
                Ok(dt) => dt.timestamp(),
                Err(e) => {
                    log::warn!(
                        "Malformed created_at '{}' in the record of '{}': {}",
                        meta_core::display::escape(&self.created_at),
                        escape_path(&self.set),
                        e
                    );
                    return i64::MAX;
                }
            };
            let ttl = i64::try_from(ttl).unwrap_or(i64::MAX);
            created.saturating_add(ttl).saturating_sub(now_epoch)
        })
    }
}

/// The record in a worktree's admin directory, if it has one.
pub fn read(admin: &Path) -> Result<Option<SetRecord>> {
    let path = admin.join(RECORD_FILE);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Cannot read {}", escape_path(&path))),
    };
    serde_json::from_str(&content)
        .map(Some)
        .with_context(|| format!("Cannot parse {}", escape_path(&path)))
}

/// Record `record` in a worktree's admin directory.
pub fn write(admin: &Path, record: &SetRecord) -> Result<()> {
    meta_core::store::write_atomic(&admin.join(RECORD_FILE), record)
}

/// Delete the record in a worktree's admin directory, if any.
pub fn delete(admin: &Path) -> Result<()> {
    let path = admin.join(RECORD_FILE);
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("Cannot delete {}", escape_path(&path)))
        }
        _ => Ok(()),
    }
}

/// Meta's record of creating the set at `set`, from the first of its
/// worktrees `checkouts` whose record names it. Worktrees whose record
/// can't be read are passed over: this finds metadata, it doesn't prove
/// anything (the removal check does).
pub fn find<P: AsRef<Path>>(
    set: &Path,
    checkouts: impl IntoIterator<Item = P>,
) -> Option<SetRecord> {
    let set = set.canonicalize().ok()?;
    checkouts.into_iter().find_map(|checkout| {
        let admin = super::safety::linked_worktree_admin_dir(checkout.as_ref()).ok()?;
        read(&admin)
            .ok()
            .flatten()
            .filter(|record| record.set == set)
    })
}

/// Every record in the worktree admin directories of the repository whose
/// git directory is `git_dir`, with the admin directory it is in. Admin
/// directories that can't be read are skipped: their worktrees are found
/// again, and checked, through the set they are in.
pub fn in_repo(git_dir: &Path) -> Vec<(PathBuf, SetRecord)> {
    let Ok(entries) = std::fs::read_dir(git_dir.join("worktrees")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let admin = entry.path();
            match read(&admin) {
                Ok(record) => record.map(|record| (admin, record)),
                Err(e) => {
                    log::debug!("Skipping {}: {e:#}", escape_path(&admin));
                    None
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(created_at: &str, ttl_seconds: Option<u64>) -> SetRecord {
        SetRecord {
            set: PathBuf::from("/p/.worktrees/test-wt"),
            project: PathBuf::from("/p"),
            created_at: created_at.to_string(),
            ephemeral: false,
            ttl_seconds,
            custom: HashMap::new(),
        }
    }

    #[test]
    fn name_is_the_set_directory_name() {
        assert_eq!(record("", None).name(), "test-wt");
    }

    #[test]
    fn ttl_remaining() {
        // 2025-01-01T00:00:00Z is epoch 1735689600
        let created = 1_735_689_600i64;
        let r = record("2025-01-01T00:00:00Z", Some(3600));
        assert_eq!(r.ttl_remaining(created + 1800), Some(1800));
        assert_eq!(r.ttl_remaining(created + 3600), Some(0));
        assert_eq!(r.ttl_remaining(created + 7200), Some(-3600));
        assert_eq!(record("2025-01-01T00:00:00Z", None).ttl_remaining(0), None);
        let huge = record("2025-01-01T00:00:00Z", Some(u64::MAX));
        assert_eq!(
            huge.ttl_remaining(1_800_000_000),
            Some(i64::MAX - 1_800_000_000)
        );
        let malformed = record("not-a-date", Some(3600));
        assert_eq!(malformed.ttl_remaining(1_700_000_000), Some(i64::MAX));
    }

    #[test]
    fn write_read_delete_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let admin = temp.path().join("worktrees/wt");
        std::fs::create_dir_all(&admin).unwrap();
        assert_eq!(read(&admin).unwrap(), None);

        let mut r = record("2025-01-01T00:00:00Z", Some(60));
        r.custom.insert("k".into(), "v".into());
        write(&admin, &r).unwrap();
        assert_eq!(read(&admin).unwrap(), Some(r.clone()));
        assert_eq!(in_repo(temp.path()), vec![(admin.clone(), r)]);

        delete(&admin).unwrap();
        delete(&admin).unwrap();
        assert_eq!(read(&admin).unwrap(), None);
        assert!(in_repo(temp.path()).is_empty());
    }

    #[test]
    fn a_malformed_record_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(RECORD_FILE), "not json").unwrap();
        assert!(read(temp.path()).is_err());
    }
}

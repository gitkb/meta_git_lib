//! Workspace snapshot functionality for meta repositories.
//!
//! Captures and restores the git state of all repos in a meta workspace,
//! enabling safe batch operations with rollback capability.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use meta_cli::git_utils;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

const SNAPSHOTS_DIR: &str = ".meta-snapshots";

/// State of a single repository at snapshot time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoState {
    /// The commit SHA at snapshot time
    pub sha: String,
    /// The branch name (None if detached HEAD)
    pub branch: Option<String>,
    /// Whether the repo had uncommitted changes
    pub dirty: bool,
    /// Whether a stash was created during restore (only set after restore)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stash_created: bool,
}

/// A complete workspace snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Name of the snapshot
    pub name: String,
    /// When the snapshot was created
    pub created: DateTime<Utc>,
    /// State of each repository (key is relative path from meta root)
    pub repos: HashMap<String, RepoState>,
}

/// Summary info about a snapshot (for listing)
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotInfo {
    pub name: String,
    pub created: DateTime<Utc>,
    pub repo_count: usize,
    pub dirty_count: usize,
}

/// Result of a restore operation for a single repo
#[derive(Debug, Clone, Serialize)]
pub struct RestoreResult {
    pub repo: String,
    pub success: bool,
    pub stashed: bool,
    pub message: String,
}

/// Capture the current git state of a repository
pub fn capture_repo_state(repo_path: &Path) -> Result<RepoState> {
    // Get current SHA
    let sha_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to run git rev-parse HEAD")?;

    if !sha_output.status.success() {
        anyhow::bail!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&sha_output.stderr)
        );
    }

    let sha = String::from_utf8_lossy(&sha_output.stdout)
        .trim()
        .to_string();

    let branch = git_utils::current_branch(repo_path);

    let dirty = git_utils::is_dirty(repo_path).unwrap_or(false);

    Ok(RepoState {
        sha,
        branch,
        dirty,
        stash_created: false,
    })
}

/// The first 8 characters of a SHA, for display.
pub fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

/// Reject snapshot names that could escape the snapshots directory or that
/// can't be part of a backup branch name (see [`backup_branch_name`]).
pub fn validate_snapshot_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid {
        anyhow::bail!(
            "Invalid snapshot name '{name}': use ASCII letters, digits, '-', '_' and single dots \
             inside the name (not at the start or end, and not ending in '.lock')"
        );
    }
    Ok(())
}

/// The branch that keeps a branch's newer commits when a restore moves it back.
pub fn backup_branch_name(snapshot: &str, branch: &str) -> String {
    format!("meta-snapshot/{snapshot}/{branch}")
}

fn git_output(repo_path: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))
}

fn rev_parse(repo_path: &Path, rev: &str) -> Option<String> {
    let output = git_output(repo_path, &["rev-parse", "-q", "--verify", rev]).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Commits a restore would leave on no branch, and the branch that keeps them.
#[derive(Debug, Clone)]
pub struct Backup {
    /// The backup branch to create.
    pub name: String,
    /// The commit it points at.
    pub tip: String,
    pub commits: usize,
    /// Where the commits are now: the recorded branch, or "detached HEAD".
    pub from: String,
}

fn count_commits(repo_path: &Path, args: &[&str]) -> Result<usize> {
    let output = git_output(repo_path, &[&["rev-list", "--count"], args].concat())?;
    if !output.status.success() {
        anyhow::bail!(
            "git rev-list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("Unexpected git rev-list output")
}

/// The backups a restore needs so no commit is left on no branch: the
/// recorded branch's commits newer than the snapshot, and commits that only
/// the current detached HEAD holds.
pub fn backups_needed(repo_path: &Path, state: &RepoState, snapshot: &str) -> Result<Vec<Backup>> {
    let mut backups = Vec::new();
    if let Some(branch) = &state.branch {
        let branch_ref = format!("refs/heads/{branch}");
        if let Some(tip) = rev_parse(repo_path, &branch_ref) {
            let commits = count_commits(repo_path, &[&format!("{}..{branch_ref}", state.sha)])?;
            if commits > 0 {
                backups.push(Backup {
                    name: backup_branch_name(snapshot, branch),
                    tip,
                    commits,
                    from: branch.clone(),
                });
            }
        }
    }
    if let Some(head) = rev_parse(repo_path, "HEAD") {
        // Commits the snapshot itself holds stay reachable from the restore.
        let commits = count_commits(
            repo_path,
            &[
                "HEAD",
                "--not",
                "--branches",
                "--tags",
                "--remotes",
                &state.sha,
            ],
        )?;
        if commits > 0 {
            backups.push(Backup {
                // No branch can be named HEAD, so this never collides with one.
                name: backup_branch_name(snapshot, "HEAD"),
                tip: head,
                commits,
                from: "detached HEAD".to_string(),
            });
        }
    }
    Ok(backups)
}

/// Restore a repository to a snapshot state.
///
/// Commits the restore would leave on no branch (see [`backups_needed`]) are
/// kept on backup branches first, then uncommitted changes are stashed, then
/// the branch moves.
pub fn restore_repo_state(
    repo_path: &Path,
    state: &RepoState,
    snapshot_name: &str,
) -> Result<RestoreResult> {
    let repo_name = repo_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    let failed = |stashed: bool, message: String| {
        Ok(RestoreResult {
            repo: repo_name.clone(),
            success: false,
            stashed,
            message,
        })
    };

    // A SHA that's gone (garbage collected) can't be restored; stop before
    // changing anything.
    if rev_parse(repo_path, &format!("{}^{{commit}}", state.sha)).is_none() {
        return failed(
            false,
            format!(
                "SHA {} no longer exists. Check git reflog for recovery options.",
                short_sha(&state.sha)
            ),
        );
    }

    // What this restore saves and replaces. Every move below is conditional
    // on these, so a commit made meanwhile is never dropped.
    let head_before = rev_parse(repo_path, "HEAD");
    let branch_before = state
        .branch
        .as_ref()
        .and_then(|b| rev_parse(repo_path, &format!("refs/heads/{b}")));
    // Where to switch back to if git won't let this worktree take the branch.
    let switch_back = git_output(repo_path, &["symbolic-ref", "-q", "--short", "HEAD"])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .or_else(|| head_before.clone());

    // Keep every commit the restore would leave on no branch.
    let backups = match backups_needed(repo_path, state, snapshot_name) {
        Ok(backups) => backups,
        Err(e) => return failed(false, format!("Cannot check for newer commits: {e:#}")),
    };
    for backup in &backups {
        let created = git_output(repo_path, &["branch", &backup.name, &backup.tip])?;
        // A rerun after a failed restore finds the backup already in place.
        let saved = created.status.success()
            || rev_parse(repo_path, &format!("refs/heads/{}", backup.name)).as_ref()
                == Some(&backup.tip);
        if !saved {
            return failed(
                false,
                format!(
                    "Keeping {} commit(s) from {} on '{}' failed: {} \
                     (if that branch is an older backup, rename or delete it and restore again)",
                    backup.commits,
                    backup.from,
                    backup.name,
                    String::from_utf8_lossy(&created.stderr).trim()
                ),
            );
        }
    }
    // Stash uncommitted changes. Untracked files stay in place (checkout
    // refuses to overwrite them), so the stash may record nothing.
    let mut stashed = false;
    if git_utils::is_dirty(repo_path).unwrap_or(false) {
        let before = rev_parse(repo_path, "refs/stash");
        let stash_output = git_output(
            repo_path,
            &["stash", "push", "-m", "meta-snapshot-auto-stash"],
        )?;
        // git stores the stash before cleaning the checkout, which can fail;
        // so whether a stash exists is read either way.
        stashed = rev_parse(repo_path, "refs/stash") != before;
        if !stash_output.status.success() {
            return failed(
                stashed,
                format!(
                    "Failed to stash: {}",
                    String::from_utf8_lossy(&stash_output.stderr)
                ),
            );
        }
    }

    let kept_note: String = backups
        .iter()
        .map(|b| {
            format!(
                " ({} commit(s) from {} kept on {})",
                b.commits, b.from, b.name
            )
        })
        .collect();

    // Detach to the snapshot SHA (the branch isn't touched yet), then move the
    // branch only if it still points where it did when this restore began.
    if rev_parse(repo_path, "HEAD") != head_before {
        return failed(
            stashed,
            format!("HEAD moved during the restore; nothing was changed{kept_note}"),
        );
    }
    let checkout_output = git_output(repo_path, &["checkout", "--detach", &state.sha])?;
    if !checkout_output.status.success() {
        let stderr = String::from_utf8_lossy(&checkout_output.stderr);
        return failed(
            stashed,
            format!("Failed to checkout: {}{kept_note}", stderr.trim()),
        );
    }
    // A post-checkout hook may have committed or switched; then HEAD is the
    // only thing holding its work, so stop here and leave it.
    let detached_at_sha = git_output(repo_path, &["symbolic-ref", "-q", "HEAD"])?
        .status
        .code()
        == Some(1)
        && rev_parse(repo_path, "HEAD").as_deref() == Some(state.sha.as_str());
    if !detached_at_sha {
        return failed(
            stashed,
            format!(
                "a checkout hook moved HEAD away from {}; left it where the hook put it{kept_note}",
                short_sha(&state.sha)
            ),
        );
    }
    if let Some(branch) = &state.branch {
        // Now that this worktree no longer holds the branch: refused if another
        // worktree has it checked out (an unborn one too, so even an absent
        // branch is checked) or is rebasing or bisecting it. Then nothing moves.
        if let Err(e) = crate::worktree::git_ops::git_branch_is_free(repo_path, branch) {
            let back = switch_back.as_deref().is_some_and(|to| {
                git_output(repo_path, &["checkout", "-q", to]).is_ok_and(|o| o.status.success())
            });
            let state_note = if back {
                "switched back; no branch moved".to_string()
            } else {
                format!(
                    "left detached at {}; no branch moved",
                    short_sha(&state.sha)
                )
            };
            return failed(
                stashed,
                format!("Failed to checkout: {e} ({state_note}){kept_note}"),
            );
        }
        if let Err(e) = move_branch(repo_path, branch, &state.sha, branch_before.as_deref()) {
            return failed(
                stashed,
                format!("{e}; left detached at {}{kept_note}", short_sha(&state.sha)),
            );
        }
    }

    let message = match &state.branch {
        Some(branch) => format!("{} -> {branch}{kept_note}", short_sha(&state.sha)),
        None => format!("{} (detached){kept_note}", short_sha(&state.sha)),
    };
    Ok(RestoreResult {
        repo: repo_name,
        success: true,
        stashed,
        message,
    })
}

/// Point `branch` at `sha` and check it out, but only if the branch still
/// points at `expected` (`None`: doesn't exist), through git's ref
/// transaction. HEAD must already be detached at `sha`.
fn move_branch(repo_path: &Path, branch: &str, sha: &str, expected: Option<&str>) -> Result<()> {
    let branch_ref = format!("refs/heads/{branch}");
    let moved = git_output(
        repo_path,
        &[
            "update-ref",
            "--no-deref",
            "-m",
            "meta snapshot restore",
            &branch_ref,
            sha,
            expected.unwrap_or(""),
        ],
    )?;
    if !moved.status.success() {
        anyhow::bail!(
            "'{branch}' changed during the restore and was left alone: {}",
            String::from_utf8_lossy(&moved.stderr).trim()
        );
    }
    let attached = git_output(repo_path, &["symbolic-ref", "HEAD", &branch_ref])?;
    if !attached.status.success() {
        anyhow::bail!(
            "Cannot check out '{branch}': {}",
            String::from_utf8_lossy(&attached.stderr).trim()
        );
    }
    Ok(())
}

/// Save a snapshot to disk
pub fn save_snapshot(meta_root: &Path, snapshot: &Snapshot) -> Result<()> {
    validate_snapshot_name(&snapshot.name)?;
    let snapshots_dir = meta_root.join(SNAPSHOTS_DIR);
    fs::create_dir_all(&snapshots_dir).context("Failed to create snapshots directory")?;

    let snapshot_path = snapshots_dir.join(format!("{}.json", snapshot.name));
    let json = serde_json::to_string_pretty(snapshot).context("Failed to serialize snapshot")?;

    fs::write(&snapshot_path, json).context("Failed to write snapshot file")?;

    Ok(())
}

/// Load a snapshot from disk
pub fn load_snapshot(meta_root: &Path, name: &str) -> Result<Snapshot> {
    validate_snapshot_name(name)?;
    let snapshot_path = meta_root.join(SNAPSHOTS_DIR).join(format!("{name}.json"));

    if !snapshot_path.exists() {
        anyhow::bail!("Snapshot '{name}' not found");
    }

    let json = fs::read_to_string(&snapshot_path).context("Failed to read snapshot file")?;
    let snapshot: Snapshot = serde_json::from_str(&json).context("Failed to parse snapshot")?;
    for (repo, state) in &snapshot.repos {
        let hex = state.sha.chars().all(|c| c.is_ascii_hexdigit());
        if !hex || !matches!(state.sha.len(), 40 | 64) {
            anyhow::bail!(
                "Snapshot '{name}' is corrupt: '{}' is not a full commit SHA (repo {repo})",
                state.sha
            );
        }
    }

    Ok(snapshot)
}

/// List all snapshots in a meta root
pub fn list_snapshots(meta_root: &Path) -> Result<Vec<SnapshotInfo>> {
    let snapshots_dir = meta_root.join(SNAPSHOTS_DIR);

    if !snapshots_dir.exists() {
        return Ok(Vec::new());
    }

    let mut snapshots = Vec::new();

    for entry in fs::read_dir(&snapshots_dir).context("Failed to read snapshots directory")? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().map(|e| e == "json").unwrap_or(false) {
            if let Ok(json) = fs::read_to_string(&path) {
                if let Ok(snapshot) = serde_json::from_str::<Snapshot>(&json) {
                    let dirty_count = snapshot.repos.values().filter(|r| r.dirty).count();
                    snapshots.push(SnapshotInfo {
                        name: snapshot.name,
                        created: snapshot.created,
                        repo_count: snapshot.repos.len(),
                        dirty_count,
                    });
                }
            }
        }
    }

    // Sort by creation time, newest first
    snapshots.sort_by_key(|s| std::cmp::Reverse(s.created));

    Ok(snapshots)
}

/// Delete a snapshot
pub fn delete_snapshot(meta_root: &Path, name: &str) -> Result<()> {
    validate_snapshot_name(name)?;
    let snapshot_path = meta_root.join(SNAPSHOTS_DIR).join(format!("{name}.json"));

    if !snapshot_path.exists() {
        anyhow::bail!("Snapshot '{name}' not found");
    }

    fs::remove_file(&snapshot_path).context("Failed to delete snapshot file")?;

    Ok(())
}

/// Check if a path is a git repository
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists() || path.join(".git").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_repo(dir: &Path) -> Result<()> {
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()?;
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()?;
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()?;
        fs::write(dir.join("README.md"), "# Test")?;
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()?;
        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(dir)
            .output()?;
        Ok(())
    }

    #[test]
    fn test_capture_clean_repo() {
        let temp = TempDir::new().unwrap();
        create_test_repo(temp.path()).unwrap();

        let state = capture_repo_state(temp.path()).unwrap();
        assert!(!state.sha.is_empty());
        assert!(!state.dirty);
        // Default branch could be main or master
        assert!(state.branch.is_some());
    }

    #[test]
    fn test_capture_dirty_repo() {
        let temp = TempDir::new().unwrap();
        create_test_repo(temp.path()).unwrap();

        // Make it dirty
        fs::write(temp.path().join("new_file.txt"), "dirty").unwrap();

        let state = capture_repo_state(temp.path()).unwrap();
        assert!(state.dirty);
    }

    #[test]
    fn test_save_and_load_snapshot() {
        let temp = TempDir::new().unwrap();

        let snapshot = Snapshot {
            name: "test-snapshot".to_string(),
            created: Utc::now(),
            repos: HashMap::from([(
                ".".to_string(),
                RepoState {
                    sha: "a".repeat(40),
                    branch: Some("main".to_string()),
                    dirty: false,
                    stash_created: false,
                },
            )]),
        };

        save_snapshot(temp.path(), &snapshot).unwrap();
        let loaded = load_snapshot(temp.path(), "test-snapshot").unwrap();

        assert_eq!(loaded.name, "test-snapshot");
        assert_eq!(loaded.repos.len(), 1);
        assert_eq!(loaded.repos["."].sha, "a".repeat(40));
    }

    #[test]
    fn test_list_snapshots() {
        let temp = TempDir::new().unwrap();

        // Create two snapshots
        for name in ["snap1", "snap2"] {
            let snapshot = Snapshot {
                name: name.to_string(),
                created: Utc::now(),
                repos: HashMap::new(),
            };
            save_snapshot(temp.path(), &snapshot).unwrap();
        }

        let list = list_snapshots(temp.path()).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_delete_snapshot() {
        let temp = TempDir::new().unwrap();

        let snapshot = Snapshot {
            name: "to-delete".to_string(),
            created: Utc::now(),
            repos: HashMap::new(),
        };
        save_snapshot(temp.path(), &snapshot).unwrap();

        assert!(load_snapshot(temp.path(), "to-delete").is_ok());

        delete_snapshot(temp.path(), "to-delete").unwrap();

        assert!(load_snapshot(temp.path(), "to-delete").is_err());
    }

    #[test]
    fn snapshot_names_cannot_escape_the_snapshots_dir() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(SNAPSHOTS_DIR)).unwrap();
        fs::write(temp.path().join("package.json"), "{}").unwrap();

        assert!(delete_snapshot(temp.path(), "../package").is_err());
        assert!(temp.path().join("package.json").exists());
        assert!(load_snapshot(temp.path(), "../package").is_err());
        let escaping = Snapshot {
            name: "../../escaped".to_string(),
            created: Utc::now(),
            repos: HashMap::new(),
        };
        assert!(save_snapshot(temp.path(), &escaping).is_err());
        for name in ["", ".hidden", "a/b", "a\\b", "..", "a..b", "a.lock", "a."] {
            assert!(validate_snapshot_name(name).is_err(), "{name:?}");
        }
        assert!(validate_snapshot_name("before-v1.2_x").is_ok());
    }

    #[test]
    fn load_rejects_a_short_sha_instead_of_panicking_later() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(SNAPSHOTS_DIR)).unwrap();
        fs::write(
            temp.path().join(SNAPSHOTS_DIR).join("bad.json"),
            r#"{"name":"bad","created":"2026-01-01T00:00:00Z","repos":{"a":{"sha":"abc","branch":"main","dirty":false}}}"#,
        )
        .unwrap();

        let err = load_snapshot(temp.path(), "bad").unwrap_err();
        assert!(err.to_string().contains("not a full commit SHA"), "{err}");
        assert_eq!(short_sha("abc"), "abc");
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn restore_keeps_newer_commits_on_a_backup_branch() {
        let temp = TempDir::new().unwrap();
        create_test_repo(temp.path()).unwrap();
        let state = capture_repo_state(temp.path()).unwrap();
        let branch = state.branch.clone().unwrap();
        git(
            temp.path(),
            &["commit", "-q", "--allow-empty", "-m", "after snapshot"],
        );
        let newer = git(temp.path(), &["rev-parse", "HEAD"]);
        assert_eq!(
            backups_needed(temp.path(), &state, "before").unwrap().len(),
            1
        );

        let result = restore_repo_state(temp.path(), &state, "before").unwrap();
        assert!(result.success, "{}", result.message);
        assert!(
            result.message.contains("kept on meta-snapshot/before/"),
            "{}",
            result.message
        );
        assert_eq!(git(temp.path(), &["rev-parse", &branch]), state.sha);
        let backup = backup_branch_name("before", &branch);
        assert_eq!(git(temp.path(), &["rev-parse", &backup]), newer);
    }

    #[test]
    fn restore_keeps_commits_only_a_detached_head_holds() {
        let temp = TempDir::new().unwrap();
        create_test_repo(temp.path()).unwrap();
        let state = capture_repo_state(temp.path()).unwrap();
        git(temp.path(), &["checkout", "-q", "--detach"]);
        git(
            temp.path(),
            &["commit", "-q", "--allow-empty", "-m", "detached work"],
        );
        let detached = git(temp.path(), &["rev-parse", "HEAD"]);

        let result = restore_repo_state(temp.path(), &state, "before").unwrap();
        assert!(result.success, "{}", result.message);
        assert!(
            result
                .message
                .contains("from detached HEAD kept on meta-snapshot/before/HEAD"),
            "{}",
            result.message
        );
        assert_eq!(
            git(temp.path(), &["rev-parse", "meta-snapshot/before/HEAD"]),
            detached
        );
    }

    #[test]
    fn restoring_a_detached_snapshot_in_place_needs_no_backup() {
        let temp = TempDir::new().unwrap();
        create_test_repo(temp.path()).unwrap();
        git(temp.path(), &["checkout", "-q", "--detach"]);
        git(
            temp.path(),
            &["commit", "-q", "--allow-empty", "-m", "detached"],
        );
        let state = capture_repo_state(temp.path()).unwrap();
        assert!(state.branch.is_none());
        assert!(backups_needed(temp.path(), &state, "s").unwrap().is_empty());
    }

    #[test]
    fn restore_reports_stashed_only_when_a_stash_was_made() {
        let temp = TempDir::new().unwrap();
        create_test_repo(temp.path()).unwrap();
        let state = capture_repo_state(temp.path()).unwrap();

        fs::write(temp.path().join("untracked.txt"), "x").unwrap();
        let result = restore_repo_state(temp.path(), &state, "s").unwrap();
        assert!(result.success && !result.stashed, "{result:?}");

        fs::write(temp.path().join("README.md"), "changed").unwrap();
        let result = restore_repo_state(temp.path(), &state, "s").unwrap();
        assert!(result.success && result.stashed, "{result:?}");
    }

    #[test]
    fn restore_fails_when_the_branch_is_checked_out_elsewhere() {
        let temp = TempDir::new().unwrap();
        create_test_repo(temp.path()).unwrap();
        let state = capture_repo_state(temp.path()).unwrap();
        let branch = state.branch.clone().unwrap();
        let other = temp.path().join("other");
        git(temp.path(), &["checkout", "-q", "-b", "elsewhere"]);
        git(
            temp.path(),
            &["worktree", "add", "-q", other.to_str().unwrap(), &branch],
        );

        let result = restore_repo_state(temp.path(), &state, "s").unwrap();
        assert!(!result.success, "{result:?}");
        assert!(
            result.message.contains("Failed to checkout"),
            "{}",
            result.message
        );
        assert_eq!(git(temp.path(), &["branch", "--show-current"]), "elsewhere");
    }

    #[test]
    fn moving_the_branch_back_refuses_if_it_moved_since() {
        let temp = TempDir::new().unwrap();
        create_test_repo(temp.path()).unwrap();
        let start = git(temp.path(), &["rev-parse", "HEAD"]);
        let branch = git(temp.path(), &["branch", "--show-current"]);
        git(temp.path(), &["commit", "-q", "--allow-empty", "-m", "a"]);
        let seen = git(temp.path(), &["rev-parse", "HEAD"]);
        // Another commit lands after the restore read the tip.
        git(temp.path(), &["commit", "-q", "--allow-empty", "-m", "b"]);
        let newest = git(temp.path(), &["rev-parse", "HEAD"]);
        git(temp.path(), &["checkout", "-q", "--detach", &start]);

        assert!(move_branch(temp.path(), &branch, &start, Some(&seen)).is_err());
        assert_eq!(git(temp.path(), &["rev-parse", &branch]), newest);
        move_branch(temp.path(), &branch, &start, Some(&newest)).unwrap();
        assert_eq!(git(temp.path(), &["branch", "--show-current"]), branch);
    }

    #[cfg(unix)]
    #[test]
    fn restore_stops_when_a_checkout_hook_commits() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        create_test_repo(temp.path()).unwrap();
        let state = capture_repo_state(temp.path()).unwrap();
        git(
            temp.path(),
            &["commit", "-q", "--allow-empty", "-m", "after snapshot"],
        );
        let hooks = temp.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        let hook = hooks.join("post-checkout");
        fs::write(&hook, "#!/bin/sh\ngit commit -q --allow-empty -m hook\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        git(
            temp.path(),
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );

        let result = restore_repo_state(temp.path(), &state, "s").unwrap();
        assert!(!result.success, "{result:?}");
        assert!(
            result.message.contains("checkout hook moved HEAD"),
            "{}",
            result.message
        );
        assert_eq!(
            git(temp.path(), &["log", "-1", "--format=%s", "HEAD"]),
            "hook"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_leaves_a_branch_another_worktree_is_rebasing() {
        let temp = TempDir::new().unwrap();
        create_test_repo(temp.path()).unwrap();
        let state = capture_repo_state(temp.path()).unwrap();
        let branch = state.branch.clone().unwrap();
        git(
            temp.path(),
            &["commit", "-q", "--allow-empty", "-m", "newer"],
        );
        let newer = git(temp.path(), &["rev-parse", "HEAD"]);
        git(temp.path(), &["checkout", "-q", "-b", "elsewhere"]);
        // Another worktree rebases the branch: its HEAD is detached, but git
        // still reserves the branch for it.
        let other = temp.path().join("other");
        git(
            temp.path(),
            &["worktree", "add", "-q", other.to_str().unwrap(), &branch],
        );
        let rebase = Command::new("git")
            .args(["rebase", "-q", "-i", "HEAD~1"])
            .env("GIT_SEQUENCE_EDITOR", "sed -i -e 1s/^pick/edit/")
            .current_dir(&other)
            .output()
            .unwrap();
        assert!(
            rebase.status.success(),
            "{}",
            String::from_utf8_lossy(&rebase.stderr)
        );

        let result = restore_repo_state(temp.path(), &state, "s").unwrap();
        assert!(!result.success, "{result:?}");
        assert!(
            result.message.contains("switched back"),
            "{}",
            result.message
        );
        assert_eq!(
            git(temp.path(), &["rev-parse", &format!("refs/heads/{branch}")]),
            newer
        );
        assert_eq!(git(temp.path(), &["branch", "--show-current"]), "elsewhere");
    }

    #[test]
    fn restore_leaves_a_missing_branch_another_worktree_holds_unborn() {
        let temp = TempDir::new().unwrap();
        create_test_repo(temp.path()).unwrap();
        git(temp.path(), &["checkout", "-q", "-b", "topic"]);
        let state = capture_repo_state(temp.path()).unwrap();
        git(temp.path(), &["checkout", "-q", "-b", "elsewhere"]);
        git(temp.path(), &["branch", "-q", "-D", "topic"]);
        let other = temp.path().join("other");
        git(
            temp.path(),
            &["worktree", "add", "-q", "--detach", other.to_str().unwrap()],
        );
        git(&other, &["checkout", "-q", "--orphan", "topic"]);

        let result = restore_repo_state(temp.path(), &state, "s").unwrap();
        assert!(!result.success, "{result:?}");
        let topic = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/topic"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(!topic.status.success(), "topic was not created");
        assert_eq!(git(temp.path(), &["branch", "--show-current"]), "elsewhere");
    }

    #[test]
    fn test_is_git_repo() {
        let temp = TempDir::new().unwrap();
        assert!(!is_git_repo(temp.path()));

        create_test_repo(temp.path()).unwrap();
        assert!(is_git_repo(temp.path()));
    }
}

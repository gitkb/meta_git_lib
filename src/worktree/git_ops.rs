//! Git operations for worktree management.

use anyhow::Result;
use std::path::Path;
use std::process::{Command, Stdio};

use super::types::GitStatusSummary;

const GIT_LOCAL_ENV_VARS: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_INTERNAL_SUPER_PREFIX",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

fn clear_git_local_env(command: &mut Command) {
    for key in GIT_LOCAL_ENV_VARS {
        command.env_remove(key);
    }
}

fn git_command(repo_path: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(repo_path);
    clear_git_local_env(&mut command);
    command
}

fn shell_quote_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn ensure_checkout_config_is_not_bare(repo_path: &Path) -> Result<()> {
    if !repo_path.join(".git").exists() {
        return Ok(());
    }

    let output = git_command(repo_path)
        .args(["config", "--local", "--bool", "--get", "core.bare"])
        .output()?;

    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout);
        if value.trim() == "true" {
            anyhow::bail!(
                "Repository '{}' is a checked-out work tree but its shared Git config has \
                 core.bare=true.\nRepair it with:\n  git -C {} config --local core.bare false",
                repo_path.display(),
                shell_quote_path(repo_path)
            );
        }
    } else if output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to inspect core.bare for '{}': {}",
            repo_path.display(),
            stderr.trim()
        );
    }

    Ok(())
}

pub fn git_worktree_add(
    repo_path: &Path,
    worktree_dest: &Path,
    branch: &str,
    from_ref: Option<&str>,
) -> Result<bool> {
    ensure_checkout_config_is_not_bare(repo_path)?;

    // If from_ref is specified, verify it exists in this repo
    if let Some(ref_name) = from_ref {
        let ref_exists = git_command(repo_path)
            .args(["rev-parse", "--verify", ref_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success();

        if !ref_exists {
            anyhow::bail!(
                "Ref '{}' not found in repo '{}'",
                ref_name,
                repo_path.display()
            );
        }

        // Create branch from the specified ref
        let output = git_command(repo_path)
            .args([
                "worktree",
                "add",
                "-b",
                branch,
                &worktree_dest.to_string_lossy(),
                ref_name,
            ])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "git worktree add failed for '{}' (branch: {}, ref: {}): {}",
                repo_path.display(),
                branch,
                ref_name,
                stderr.trim()
            );
        }
        return Ok(true); // Always creates a new branch from ref
    }

    // Check if branch exists locally
    let branch_exists = git_command(repo_path)
        .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success();

    // Also check if branch exists on remote
    let remote_branch_exists = if !branch_exists {
        git_command(repo_path)
            .args([
                "rev-parse",
                "--verify",
                &format!("refs/remotes/origin/{branch}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success()
    } else {
        false
    };

    let dest_str = worktree_dest.to_string_lossy();
    let remote_ref = format!("origin/{branch}");

    let wt_args: Vec<&str> = if branch_exists {
        // Use existing local branch
        vec!["worktree", "add", &dest_str, branch]
    } else if remote_branch_exists {
        // Create local tracking branch from remote
        vec![
            "worktree",
            "add",
            "--track",
            "-b",
            branch,
            &dest_str,
            &remote_ref,
        ]
    } else {
        // Create new branch from HEAD
        vec!["worktree", "add", "-b", branch, &dest_str]
    };

    let output = git_command(repo_path).args(&wt_args).output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git worktree add failed for '{}' (branch: {}): {}",
            repo_path.display(),
            branch,
            stderr.trim()
        );
    }

    // Return whether we created a new branch
    let created_branch = !branch_exists && !remote_branch_exists;
    Ok(created_branch)
}

pub fn git_worktree_remove(repo_path: &Path, worktree_path: &Path, force: bool) -> Result<()> {
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    let wt_str = worktree_path.to_string_lossy();
    args.push(&wt_str);

    let output = git_command(repo_path).args(&args).output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree remove failed: {}", stderr.trim());
    }
    Ok(())
}

pub fn git_status_summary(repo_path: &Path) -> Result<GitStatusSummary> {
    ensure_checkout_config_is_not_bare(repo_path)?;

    let output = git_command(repo_path)
        .args(["status", "--porcelain"])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git status failed for '{}': {}",
            repo_path.display(),
            stderr.trim()
        );
    }

    let mut modified_files = Vec::new();
    let mut untracked_count = 0;

    // git status --porcelain format: "XY filename"
    // Positions 0-1: index (X) and work-tree (Y) status codes
    // Position 2: space separator
    // Position 3+: filename
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.len() < 3 {
            continue;
        }
        let status = &line[..2];
        let file = &line[3..];

        if status == "??" {
            untracked_count += 1;
        } else if !file.is_empty() {
            // Tracked file with modifications (staged, unstaged, or both).
            // For renames ("R  old -> new"), extract the new name.
            let name = file.split(" -> ").last().unwrap_or(file);
            modified_files.push(name.to_string());
        }
    }

    let dirty = !modified_files.is_empty() || untracked_count > 0;
    Ok(GitStatusSummary {
        dirty,
        modified_files,
        untracked_count,
    })
}

pub fn git_ahead_behind(repo_path: &Path) -> Result<(u32, u32)> {
    let output = git_command(repo_path)
        .args(["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
        .stderr(Stdio::null())
        .output()?;

    if !output.status.success() {
        // No upstream configured
        return Ok((0, 0));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = text.trim().split('\t').collect();
    if parts.len() == 2 {
        let ahead = parts[0].parse::<u32>().unwrap_or(0);
        let behind = parts[1].parse::<u32>().unwrap_or(0);
        Ok((ahead, behind))
    } else {
        Ok((0, 0))
    }
}

pub fn git_diff_stat(
    worktree_path: &Path,
    base_ref: &str,
) -> Result<(usize, usize, usize, Vec<String>)> {
    // Try three-dot diff first (changes since divergence)
    let numstat_output = git_command(worktree_path)
        .args(["diff", "--numstat", &format!("{base_ref}...HEAD")])
        .stderr(Stdio::null())
        .output()?;

    let numstat_text = if numstat_output.status.success() {
        String::from_utf8_lossy(&numstat_output.stdout).to_string()
    } else {
        // Fallback to two-dot diff
        let fallback = git_command(worktree_path)
            .args(["diff", "--numstat", &format!("{base_ref}..HEAD")])
            .stderr(Stdio::null())
            .output()?;
        if fallback.status.success() {
            String::from_utf8_lossy(&fallback.stdout).to_string()
        } else {
            String::new()
        }
    };

    let mut files_changed = 0;
    let mut insertions = 0;
    let mut deletions = 0;
    let mut files = Vec::new();

    for line in numstat_text.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            files_changed += 1;
            insertions += parts[0].parse::<usize>().unwrap_or(0);
            deletions += parts[1].parse::<usize>().unwrap_or(0);
            files.push(parts[2].to_string());
        }
    }

    Ok((files_changed, insertions, deletions, files))
}

/// Remove all worktree repos in correct order (children first, "." last).
/// In force mode, continues past failures and prints warnings.
/// Returns the number of repos that failed to remove (always 0 in non-force mode,
/// since non-force mode returns on first error).
pub fn remove_worktree_repos(
    repos: &[meta_cli::worktree::WorktreeRepoInfo],
    force: bool,
    verbose: bool,
) -> Result<usize> {
    let mut failures = 0;

    // Remove child repos first
    for r in repos.iter().filter(|r| r.alias != ".") {
        if verbose {
            eprintln!(
                "Removing worktree for '{}' at {}",
                r.alias,
                r.path.display()
            );
        }
        if let Err(e) = git_worktree_remove(&r.source_path, &r.path, force) {
            if force {
                failures += 1;
                log::warn!("Failed to remove worktree for '{}': {e}", r.alias);
            } else {
                return Err(e);
            }
        }
    }

    // Remove "." last (children live inside it)
    if let Some(r) = repos.iter().find(|r| r.alias == ".") {
        if verbose {
            eprintln!("Removing meta repo worktree at {}", r.path.display());
        }
        if let Err(e) = git_worktree_remove(&r.source_path, &r.path, force) {
            if force {
                failures += 1;
                log::warn!("Failed to remove meta repo worktree: {e}");
            } else {
                return Err(e);
            }
        }
    }

    Ok(failures)
}

/// Fetch a branch from origin if not locally available.
pub fn git_fetch_branch(repo_path: &Path, branch: &str) -> Result<()> {
    let output = git_command(repo_path)
        .args(["fetch", "origin", branch])
        .stderr(Stdio::piped())
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to fetch branch '{}': {}", branch, stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Create a minimal git repo in a tempdir and return the tempdir handle.
    fn init_git_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git_command(tmp.path())
            .args(["init"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        git_command(tmp.path())
            .args(["config", "user.email", "test@test.com"])
            .status()
            .unwrap();
        git_command(tmp.path())
            .args(["config", "user.name", "Test"])
            .status()
            .unwrap();
        tmp
    }

    /// Create an initial commit so HEAD exists.
    fn make_initial_commit(repo: &std::path::Path) {
        std::fs::write(repo.join("README.md"), "init\n").unwrap();
        git_command(repo)
            .args(["add", "README.md"])
            .status()
            .unwrap();
        git_command(repo)
            .args(["commit", "-m", "initial"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
    }

    // ── git_status_summary ──────────────────────────────────

    #[test]
    fn git_local_env_vars_cover_git_reported_variables() {
        let output = Command::new("git")
            .args(["rev-parse", "--local-env-vars"])
            .output()
            .expect("git rev-parse should run");
        assert!(
            output.status.success(),
            "git rev-parse --local-env-vars failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let reported: BTreeSet<_> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        let configured: BTreeSet<_> = GIT_LOCAL_ENV_VARS
            .iter()
            .map(|value| (*value).to_string())
            .collect();

        for name in &reported {
            assert!(
                configured.contains(name),
                "GIT_LOCAL_ENV_VARS is missing git-reported local env var {name}"
            );
        }

        let allowed_extra: BTreeSet<_> = ["GIT_INTERNAL_SUPER_PREFIX".to_string()]
            .into_iter()
            .collect();
        for name in configured.difference(&reported) {
            assert!(
                allowed_extra.contains(name),
                "GIT_LOCAL_ENV_VARS contains unexpected non-local env var {name}"
            );
        }
    }

    #[test]
    fn clear_git_local_env_removes_configured_variables() {
        let mut command = Command::new("sh");
        command.args(["-c", "env"]);
        for key in GIT_LOCAL_ENV_VARS {
            command.env(key, "hostile");
        }

        clear_git_local_env(&mut command);
        let output = command.output().expect("environment probe should run");
        assert!(output.status.success());

        let environment = String::from_utf8(output.stdout).expect("environment should be UTF-8");
        for key in GIT_LOCAL_ENV_VARS {
            assert!(
                !environment
                    .lines()
                    .any(|line| line.starts_with(&format!("{key}="))),
                "{key} leaked into the subprocess environment"
            );
        }
    }

    #[test]
    fn checkout_config_reports_core_bare_corruption_with_scoped_repair() {
        let tmp = init_git_repo();
        let status = git_command(tmp.path())
            .args(["config", "--local", "core.bare", "true"])
            .status()
            .unwrap();
        assert!(status.success());

        let error = match git_status_summary(tmp.path()) {
            Ok(_) => panic!("corrupt checked-out repository should be rejected"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("core.bare=true"));
        assert!(error.contains(&format!(
            "git -C '{}' config --local core.bare false",
            tmp.path().display()
        )));
    }

    #[test]
    fn repair_command_shell_quotes_repository_path() {
        assert_eq!(
            shell_quote_path(Path::new("/tmp/operator's repo")),
            "'/tmp/operator'\"'\"'s repo'"
        );
    }

    #[test]
    fn checkout_config_guard_allows_legitimate_bare_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let status = git_command(tmp.path())
            .args(["init", "--bare", "--quiet"])
            .status()
            .unwrap();
        assert!(status.success());

        ensure_checkout_config_is_not_bare(tmp.path()).unwrap();
    }

    #[test]
    fn status_summary_clean_repo() {
        let tmp = init_git_repo();
        make_initial_commit(tmp.path());

        let summary = git_status_summary(tmp.path()).unwrap();
        assert!(!summary.dirty);
        assert!(summary.modified_files.is_empty());
        assert_eq!(summary.untracked_count, 0);
    }

    #[test]
    fn status_summary_untracked_file() {
        let tmp = init_git_repo();
        make_initial_commit(tmp.path());

        std::fs::write(tmp.path().join("new.txt"), "hello").unwrap();

        let summary = git_status_summary(tmp.path()).unwrap();
        assert!(summary.dirty);
        assert_eq!(summary.untracked_count, 1);
        assert!(summary.modified_files.is_empty());
    }

    #[test]
    fn status_summary_modified_tracked_file() {
        let tmp = init_git_repo();
        make_initial_commit(tmp.path());

        // Modify an existing tracked file
        std::fs::write(tmp.path().join("README.md"), "changed\n").unwrap();

        let summary = git_status_summary(tmp.path()).unwrap();
        assert!(summary.dirty);
        assert_eq!(summary.untracked_count, 0);
        assert!(summary.modified_files.contains(&"README.md".to_string()));
    }

    #[test]
    fn status_summary_staged_file() {
        let tmp = init_git_repo();
        make_initial_commit(tmp.path());

        std::fs::write(tmp.path().join("README.md"), "staged\n").unwrap();
        git_command(tmp.path())
            .args(["add", "README.md"])
            .status()
            .unwrap();

        let summary = git_status_summary(tmp.path()).unwrap();
        assert!(summary.dirty);
        assert!(summary.modified_files.contains(&"README.md".to_string()));
    }

    #[test]
    fn status_summary_mixed_state() {
        let tmp = init_git_repo();
        make_initial_commit(tmp.path());

        // Untracked
        std::fs::write(tmp.path().join("untracked.txt"), "u").unwrap();
        // Modified tracked
        std::fs::write(tmp.path().join("README.md"), "mod\n").unwrap();

        let summary = git_status_summary(tmp.path()).unwrap();
        assert!(summary.dirty);
        assert_eq!(summary.untracked_count, 1);
        assert!(summary.modified_files.contains(&"README.md".to_string()));
    }

    // ── git_ahead_behind ────────────────────────────────────

    #[test]
    fn ahead_behind_no_upstream_returns_zeros() {
        let tmp = init_git_repo();
        make_initial_commit(tmp.path());

        let (ahead, behind) = git_ahead_behind(tmp.path()).unwrap();
        assert_eq!(ahead, 0);
        assert_eq!(behind, 0);
    }

    // ── remove_worktree_repos ordering ──────────────────────
    // These tests verify the ordering logic without actual git operations.
    // We construct WorktreeRepoInfo values and check that "." is processed last.

    #[test]
    fn removal_ordering_children_before_root() {
        // Verify via the filter logic that children come before "."
        let repos = [
            meta_cli::worktree::WorktreeRepoInfo {
                alias: ".".to_string(),
                branch: "main".to_string(),
                path: std::path::PathBuf::from("/tmp/wt"),
                source_path: std::path::PathBuf::from("/tmp/src"),
                created_branch: None,
            },
            meta_cli::worktree::WorktreeRepoInfo {
                alias: "lib".to_string(),
                branch: "main".to_string(),
                path: std::path::PathBuf::from("/tmp/wt/lib"),
                source_path: std::path::PathBuf::from("/tmp/src/lib"),
                created_branch: None,
            },
            meta_cli::worktree::WorktreeRepoInfo {
                alias: "cli".to_string(),
                branch: "main".to_string(),
                path: std::path::PathBuf::from("/tmp/wt/cli"),
                source_path: std::path::PathBuf::from("/tmp/src/cli"),
                created_branch: None,
            },
        ];

        // Verify ordering: children (non-".") are iterated first
        let children: Vec<&str> = repos
            .iter()
            .filter(|r| r.alias != ".")
            .map(|r| r.alias.as_str())
            .collect();
        assert_eq!(children, vec!["lib", "cli"]);

        // Root comes last
        let root = repos.iter().find(|r| r.alias == ".");
        assert!(root.is_some());
    }

    #[test]
    fn removal_ordering_no_root_repo() {
        // When there's no "." repo, all repos are children
        let repos = [meta_cli::worktree::WorktreeRepoInfo {
            alias: "lib".to_string(),
            branch: "main".to_string(),
            path: std::path::PathBuf::from("/tmp/wt/lib"),
            source_path: std::path::PathBuf::from("/tmp/src/lib"),
            created_branch: None,
        }];

        let root = repos.iter().find(|r| r.alias == ".");
        assert!(root.is_none());

        let children: Vec<&str> = repos
            .iter()
            .filter(|r| r.alias != ".")
            .map(|r| r.alias.as_str())
            .collect();
        assert_eq!(children, vec!["lib"]);
    }
}

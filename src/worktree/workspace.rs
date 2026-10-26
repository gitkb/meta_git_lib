//! The workspace a command runs in, and the repositories that make it up.
//!
//! Commands that delete worktree sets keep to this workspace's sets: those
//! whose every worktree belongs to one of its repositories. Where the sets
//! are kept (`META_WORKTREES` may be shared, or point into another
//! workspace) doesn't change that.

use anyhow::{Context, Result};
use meta_core::display::escape_path;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::safety::{relative_label, RemovalCheck};

/// A workspace's root meta directory and its repositories.
pub struct Workspace {
    pub root: PathBuf,
    /// Each repository (the root's own, then every one in its `.meta`
    /// tree, nested ones included) with its git directory: where the
    /// records of the worktrees made from it are.
    pub repos: Vec<(PathBuf, PathBuf)>,
    /// What couldn't be read (the project tree, a repository), with why:
    /// worktrees from there count as another workspace's.
    pub unreadable: Vec<(PathBuf, anyhow::Error)>,
    git_dirs: HashSet<PathBuf>,
}

impl Workspace {
    /// The workspace `meta_dir` belongs to (see [`workspace_root`]).
    pub fn of(meta_dir: &Path) -> Workspace {
        let root = workspace_root(meta_dir);
        let mut paths = vec![root.clone()];
        let mut unreadable = Vec::new();
        match meta_core::config::walk_meta_tree(&root, None) {
            Ok(tree) => paths.extend(
                meta_core::config::build_project_map(&tree, &root, "")
                    .into_values()
                    .map(|(path, _)| path),
            ),
            Err(e) => unreadable.push((root.clone(), e)),
        }
        let mut repos = Vec::new();
        for path in paths {
            match repo_git_dir(&path) {
                Ok(Some(git_dir)) => repos.push((path, git_dir)),
                Ok(None) => {}
                Err(e) => unreadable.push((path, e)),
            }
        }
        let git_dirs = repos
            .iter()
            .filter_map(|(_, git_dir)| git_dir.canonicalize().ok())
            .collect();
        Workspace {
            root,
            repos,
            unreadable,
            git_dirs,
        }
    }

    /// Whether `git_dir` is the git directory of one of this workspace's
    /// repositories.
    pub fn holds(&self, git_dir: &Path) -> bool {
        git_dir
            .canonicalize()
            .is_ok_and(|git_dir| self.git_dirs.contains(&git_dir))
    }

    /// The worktrees of the set at `wt_dir` (checked as `check`) that come
    /// from repositories outside this workspace, each with the workspace
    /// its record names. A set with any is another workspace's.
    pub fn foreign_members(&self, wt_dir: &Path, check: &RemovalCheck) -> Vec<String> {
        check
            .members
            .iter()
            .filter(|m| !self.holds(&m.repo_git_dir))
            .map(|m| {
                let label = relative_label(wt_dir, &m.path);
                let repo = m.repo_git_dir.canonicalize();
                let repo = escape_path(repo.as_deref().unwrap_or(&m.repo_git_dir));
                match &m.record {
                    Some(record) => format!(
                        "{label}: its repository {repo} is not one of this workspace's; its \
                         record names the workspace at {}",
                        escape_path(&record.project)
                    ),
                    None => {
                        format!("{label}: its repository {repo} is not one of this workspace's")
                    }
                }
            })
            .collect()
    }
}

/// The root of the workspace `meta_dir` belongs to: `meta_dir`, or the
/// nearest meta directory above it whose project tree lists it, and so on
/// upward. A meta directory its parent doesn't list (or whose parent's tree
/// can't be read) is a workspace of its own.
pub fn workspace_root(meta_dir: &Path) -> PathBuf {
    let mut root = meta_dir.to_path_buf();
    for parent in meta_dir.ancestors().skip(1) {
        match meta_core::config::find_meta_config_in(parent) {
            Ok(None) => continue,
            Ok(Some(_)) if lists(parent, &root) => root = parent.to_path_buf(),
            _ => break,
        }
    }
    root
}

/// Whether the project tree of the meta directory `parent` lists `dir`.
fn lists(parent: &Path, dir: &Path) -> bool {
    let (Ok(tree), Ok(dir)) = (
        meta_core::config::walk_meta_tree(parent, None),
        dir.canonicalize(),
    ) else {
        return false;
    };
    meta_core::config::build_project_map(&tree, parent, "")
        .into_values()
        .any(|(path, _)| path.canonicalize().is_ok_and(|path| path == dir))
}

/// The git directory holding the worktrees of the repository at `repo`:
/// `None` if there is no repository there (not cloned).
pub fn repo_git_dir(repo: &Path) -> Result<Option<PathBuf>> {
    let dot_git = repo.join(".git");
    // Links are followed: a `.git` linked to the real git directory is one
    let meta = match std::fs::metadata(&dot_git) {
        Ok(meta) => meta,
        // A bare repository is its own git directory
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let bare = repo.join("HEAD").is_file()
                && repo.join("objects").is_dir()
                && repo.join("refs").is_dir();
            return Ok(bare.then(|| repo.to_path_buf()));
        }
        Err(e) => {
            return Err(
                anyhow::Error::from(e).context(format!("Cannot inspect {}", escape_path(&dot_git)))
            )
        }
    };
    if meta.is_dir() {
        return Ok(Some(dot_git));
    }
    // A `.git` file: a linked worktree's (its admin directory names the
    // repository's git directory in `commondir`) or a `--separate-git-dir`
    // checkout's (it names the git directory itself, with no `commondir`)
    let content = std::fs::read_to_string(&dot_git)
        .with_context(|| format!("Cannot read {}", escape_path(&dot_git)))?;
    let target = content
        .trim()
        .strip_prefix("gitdir:")
        .map(|target| repo.join(target.trim()))
        .with_context(|| format!("{} has no gitdir line", escape_path(&dot_git)))?;
    let commondir = target.join("commondir");
    match std::fs::read_to_string(&commondir) {
        Ok(common) => Ok(Some(target.join(common.trim()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(target)),
        Err(e) => {
            Err(anyhow::Error::from(e).context(format!("Cannot read {}", escape_path(&commondir))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path, projects: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(".meta.json"),
            format!(r#"{{"projects": {{{projects}}}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn the_root_climbs_only_through_parents_that_list_the_child() {
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().canonicalize().unwrap();
        config(&top, r#""inner": {"repo": "x", "meta": true}"#);
        config(&top.join("inner"), r#""deep": {"repo": "x", "meta": true}"#);
        config(&top.join("inner/deep"), "");
        assert_eq!(workspace_root(&top.join("inner/deep")), top);
        assert_eq!(workspace_root(&top.join("inner")), top);

        // A workspace inside another that doesn't list it is its own
        let unlisted = top.join("inner/elsewhere");
        config(&unlisted, "");
        assert_eq!(workspace_root(&unlisted), unlisted);
        // and so is one under a directory without a config
        let below = top.join("plain/sub");
        config(&below, "");
        assert_eq!(workspace_root(&below), below);
    }
}

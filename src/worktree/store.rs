//! Centralized worktree store operations.
//!
//! Manages `~/.meta/worktree.json` — the persistent record of all worktrees.

use anyhow::Result;
use std::path::{Path, PathBuf};

use super::types::{StoreRepoEntry, WorktreeStoreData, WorktreeStoreEntry};

/// Derive the store key from a worktree path.
///
/// Attempts to canonicalize the path to resolve symlinks and normalize
/// path components. This prevents collisions when the same physical
/// directory is referenced via different paths (symlinks, relative paths, etc.).
///
/// If canonicalization fails (e.g., path doesn't exist yet), falls back
/// to using the path as-is to maintain backward compatibility.
fn store_key(worktree_path: &Path) -> String {
    match worktree_path.canonicalize() {
        Ok(canonical) => canonical.to_string_lossy().into_owned(),
        Err(e) => {
            log::debug!(
                "Failed to canonicalize path '{}': {}. Using original path.",
                worktree_path.display(),
                e
            );
            worktree_path.to_string_lossy().into_owned()
        }
    }
}

fn store_path() -> PathBuf {
    meta_core::data_dir::data_file("worktree")
}

fn store_lock_path(data_path: &Path) -> PathBuf {
    data_path.with_extension("lock")
}

/// Return (data_path, lock_path) for the worktree store.
fn store_paths() -> (PathBuf, PathBuf) {
    let data_path = store_path();
    let lock_path = store_lock_path(&data_path);
    (data_path, lock_path)
}

/// Add a worktree entry to the centralized store.
pub fn store_add(worktree_path: &Path, entry: WorktreeStoreEntry) -> Result<()> {
    meta_core::data_dir::ensure_meta_dir()?;
    let (data_path, lock_path) = store_paths();
    let key = store_key(worktree_path);

    meta_core::store::update::<WorktreeStoreData, _>(&data_path, &lock_path, |store| {
        store.worktrees.insert(key, entry);
    })
}

/// Get all entries from the store.
pub fn store_list() -> Result<WorktreeStoreData> {
    meta_core::store::read(&store_path())
}

/// Get the store key and entry for an existing worktree path, if meta
/// recorded creating it. Keep the key: once the path is deleted it can no
/// longer be resolved to the same key.
pub fn store_get(worktree_path: &Path) -> Result<Option<(String, WorktreeStoreEntry)>> {
    let key = store_key(worktree_path);
    Ok(store_list()?
        .worktrees
        .remove(&key)
        .map(|entry| (key, entry)))
}

/// Recorded worktree sets inside `worktree_path`, not counting itself.
/// Deleting `worktree_path` would delete those sets too.
pub fn store_sets_inside(worktree_path: &Path) -> Result<Vec<String>> {
    let key = store_key(worktree_path);
    let root = Path::new(&key);
    Ok(store_list()?
        .worktrees
        .into_keys()
        .filter(|k| Path::new(k) != root && Path::new(k).starts_with(root))
        .collect())
}

/// Add repos to an existing worktree entry in the store.
pub fn store_extend_repos(worktree_path: &Path, repos: Vec<StoreRepoEntry>) -> Result<()> {
    let (data_path, lock_path) = store_paths();
    let key = store_key(worktree_path);

    meta_core::store::update::<WorktreeStoreData, _>(&data_path, &lock_path, move |store| {
        if let Some(entry) = store.worktrees.get_mut(&key) {
            entry.repos.extend(repos);
        }
    })
}

/// Remove multiple worktree entries from the store in a single lock cycle.
/// Each `(key, created_at)` pair removes the entry only if it is still the
/// one that was read, so a worktree recreated at the same path keeps its record.
pub fn store_remove_batch(entries: &[(String, String)]) -> Result<()> {
    let (data_path, lock_path) = store_paths();
    if !data_path.exists() {
        return Ok(());
    }

    meta_core::store::update::<WorktreeStoreData, _>(&data_path, &lock_path, |store| {
        for (key, created_at) in entries {
            if store
                .worktrees
                .get(key)
                .is_some_and(|e| e.created_at == *created_at)
            {
                store.worktrees.remove(key);
            }
        }
    })
}

/// Compute TTL remaining seconds for a store entry.
/// Returns `None` if no TTL is set. Negative means expired.
/// On malformed `created_at`, warns and treats as not expired.
pub fn entry_ttl_remaining(entry: &WorktreeStoreEntry, now_epoch: i64) -> Option<i64> {
    entry.ttl_seconds.map(|ttl| {
        let created = match chrono::DateTime::parse_from_rfc3339(&entry.created_at) {
            Ok(dt) => dt.timestamp(),
            Err(e) => {
                log::warn!(
                    "Malformed created_at '{}' in store entry '{}': {}",
                    entry.created_at,
                    entry.name,
                    e
                );
                return i64::MAX;
            }
        };
        let ttl = i64::try_from(ttl).unwrap_or(i64::MAX);
        created.saturating_add(ttl).saturating_sub(now_epoch)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_entry(created_at: &str, ttl_seconds: Option<u64>) -> WorktreeStoreEntry {
        WorktreeStoreEntry {
            name: "test-wt".to_string(),
            project: "/tmp/project".to_string(),
            created_at: created_at.to_string(),
            ephemeral: ttl_seconds.is_some(),
            ttl_seconds,
            repos: vec![],
            custom: HashMap::new(),
        }
    }

    // ── store_key ───────────────────────────────────────────

    #[test]
    fn store_key_returns_path_string() {
        let path = std::path::Path::new("/home/user/.worktrees/feat-1");
        // Note: For non-existent paths, store_key falls back to original path
        assert_eq!(store_key(path), "/home/user/.worktrees/feat-1");
    }

    #[test]
    fn store_key_canonicalizes_existing_paths() {
        // Create a temp directory to test canonicalization
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_path = temp_dir.path();

        // Canonicalize the temp dir to get the absolute, resolved path
        let canonical = temp_path.canonicalize().unwrap();

        // store_key should return the canonical path
        assert_eq!(
            store_key(temp_path),
            canonical.to_string_lossy().to_string()
        );
    }

    #[test]
    #[cfg(unix)]
    fn store_key_resolves_symlinks() {
        use std::os::unix::fs::symlink;

        let temp_dir = tempfile::tempdir().unwrap();
        let real_dir = temp_dir.path().join("real");
        let symlink_path = temp_dir.path().join("link");

        std::fs::create_dir(&real_dir).unwrap();
        symlink(&real_dir, &symlink_path).unwrap();

        // Both paths should resolve to the same canonical key
        let key_real = store_key(&real_dir);
        let key_symlink = store_key(&symlink_path);

        assert_eq!(
            key_real, key_symlink,
            "Symlink and real path should produce the same store key"
        );
    }

    #[test]
    fn store_key_handles_nonexistent_paths() {
        // For non-existent paths, should fall back to original path
        let path = std::path::Path::new("/nonexistent/path/to/worktree");
        assert_eq!(store_key(path), "/nonexistent/path/to/worktree");
    }

    #[test]
    fn store_key_normalizes_relative_components() {
        let temp_dir = tempfile::tempdir().unwrap();
        let real_path = temp_dir.path();

        // Create a subdirectory so the path exists
        let subdir = real_path.join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        // Create a path with .. components that resolves back to real_path
        let path_with_dots = subdir.join("..").join(".");

        // Both should resolve to the same canonical path
        let key_clean = store_key(real_path);
        let key_with_dots = store_key(&path_with_dots);

        assert_eq!(
            key_clean, key_with_dots,
            "Paths with . and .. should normalize to same key"
        );
    }

    // ── entry_ttl_remaining ─────────────────────────────────

    #[test]
    fn ttl_remaining_none_when_no_ttl() {
        let entry = make_entry("2025-01-01T00:00:00Z", None);
        assert!(entry_ttl_remaining(&entry, 1_700_000_000).is_none());
    }

    #[test]
    fn ttl_remaining_positive_when_not_expired() {
        // created_at = 2025-01-01T00:00:00Z = epoch 1735689600
        // ttl = 3600s (1 hour)
        // now = 1735689600 + 1800 (30 min later)
        // remaining = 1735689600 + 3600 - (1735689600 + 1800) = 1800
        let entry = make_entry("2025-01-01T00:00:00Z", Some(3600));
        let created_epoch = 1_735_689_600i64;
        let remaining = entry_ttl_remaining(&entry, created_epoch + 1800).unwrap();
        assert_eq!(remaining, 1800);
    }

    #[test]
    fn ttl_remaining_negative_when_expired() {
        // created + ttl < now → negative
        let entry = make_entry("2025-01-01T00:00:00Z", Some(3600));
        let created_epoch = 1_735_689_600i64;
        let remaining = entry_ttl_remaining(&entry, created_epoch + 7200).unwrap();
        assert_eq!(remaining, -3600);
    }

    #[test]
    fn ttl_remaining_zero_at_exact_expiry() {
        let entry = make_entry("2025-01-01T00:00:00Z", Some(3600));
        let created_epoch = 1_735_689_600i64;
        let remaining = entry_ttl_remaining(&entry, created_epoch + 3600).unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn ttl_remaining_huge_ttl_never_expires() {
        let entry = make_entry("2025-01-01T00:00:00Z", Some(u64::MAX));
        let remaining = entry_ttl_remaining(&entry, 1_800_000_000).unwrap();
        assert_eq!(remaining, i64::MAX - 1_800_000_000);
    }

    #[test]
    fn ttl_remaining_malformed_date_returns_max() {
        let entry = make_entry("not-a-date", Some(3600));
        let remaining = entry_ttl_remaining(&entry, 1_700_000_000).unwrap();
        assert_eq!(remaining, i64::MAX);
    }

    // ── Concurrent store access (file locking) ─────────────
    // Note: These tests use #[serial] because META_DATA_DIR is process-global.
    // Each test isolates its store via a unique temp directory.

    #[test]
    #[serial_test::serial]
    fn store_add_and_remove_sequential() {
        let temp_dir = tempfile::tempdir().unwrap();
        // Isolate store to temp dir
        let store_dir = temp_dir.path().join("meta-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::env::set_var("META_DATA_DIR", &store_dir);

        let wt_path = temp_dir.path().join("test-wt");
        std::fs::create_dir(&wt_path).unwrap();

        // Add entry
        let entry = make_entry("2025-01-01T00:00:00Z", None);
        store_add(&wt_path, entry.clone()).unwrap();

        // Verify it was added
        let data = store_list().unwrap();
        let key = store_key(&wt_path);
        assert!(data.worktrees.contains_key(&key));

        // Remove entry
        store_remove_batch(&[(key.clone(), "2025-01-01T00:00:00Z".to_string())]).unwrap();

        // Verify it was removed
        let data = store_list().unwrap();
        assert!(!data.worktrees.contains_key(&key));

        std::env::remove_var("META_DATA_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn store_remove_batch_removes_multiple() {
        let temp_dir = tempfile::tempdir().unwrap();
        // Isolate store to temp dir
        let store_dir = temp_dir.path().join("meta-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::env::set_var("META_DATA_DIR", &store_dir);

        let wt1 = temp_dir.path().join("wt1");
        let wt2 = temp_dir.path().join("wt2");
        let wt3 = temp_dir.path().join("wt3");

        std::fs::create_dir(&wt1).unwrap();
        std::fs::create_dir(&wt2).unwrap();
        std::fs::create_dir(&wt3).unwrap();

        // Add three entries
        store_add(&wt1, make_entry("2025-01-01T00:00:00Z", None)).unwrap();
        store_add(&wt2, make_entry("2025-01-01T00:00:00Z", None)).unwrap();
        store_add(&wt3, make_entry("2025-01-01T00:00:00Z", None)).unwrap();

        // Remove two in batch
        let keys_to_remove = vec![
            (store_key(&wt1), "2025-01-01T00:00:00Z".to_string()),
            (store_key(&wt2), "2025-01-01T00:00:00Z".to_string()),
        ];
        store_remove_batch(&keys_to_remove).unwrap();

        // An entry recreated since it was read keeps its record
        store_add(&wt3, make_entry("2026-01-01T00:00:00Z", None)).unwrap();
        store_remove_batch(&[(store_key(&wt3), "2025-01-01T00:00:00Z".to_string())]).unwrap();

        // Verify only wt3 remains
        let data = store_list().unwrap();
        assert!(!data.worktrees.contains_key(&store_key(&wt1)));
        assert!(!data.worktrees.contains_key(&store_key(&wt2)));
        assert!(data.worktrees.contains_key(&store_key(&wt3)));

        // Cleanup
        store_remove_batch(&[(store_key(&wt3), "2026-01-01T00:00:00Z".to_string())]).unwrap();
        std::env::remove_var("META_DATA_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn store_extend_repos_adds_to_existing_entry() {
        let temp_dir = tempfile::tempdir().unwrap();
        // Isolate store to temp dir
        let store_dir = temp_dir.path().join("meta-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::env::set_var("META_DATA_DIR", &store_dir);

        let wt_path = temp_dir.path().join("extend-wt");
        std::fs::create_dir(&wt_path).unwrap();

        // Canonicalize the path immediately after creating it to ensure consistency
        let wt_path = wt_path.canonicalize().unwrap();

        // Add entry with one repo
        let mut entry = make_entry("2025-01-01T00:00:00Z", None);
        entry.repos = vec![StoreRepoEntry {
            alias: "repo1".to_string(),
            branch: "main".to_string(),
            created_branch: false,
        }];
        store_add(&wt_path, entry).unwrap();

        // Extend with another repo
        let new_repos = vec![StoreRepoEntry {
            alias: "repo2".to_string(),
            branch: "main".to_string(),
            created_branch: false,
        }];
        store_extend_repos(&wt_path, new_repos).unwrap();

        // Verify both repos are present
        let data = store_list().unwrap();
        let key = store_key(&wt_path);
        let stored_entry = data.worktrees.get(&key).unwrap();
        assert_eq!(stored_entry.repos.len(), 2);
        assert!(stored_entry.repos.iter().any(|r| r.alias == "repo1"));
        assert!(stored_entry.repos.iter().any(|r| r.alias == "repo2"));

        // Cleanup
        store_remove_batch(&[(store_key(&wt_path), "2025-01-01T00:00:00Z".to_string())]).unwrap();
        std::env::remove_var("META_DATA_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn store_operations_handle_nonexistent_store_gracefully() {
        let temp_dir = tempfile::tempdir().unwrap();
        // Isolate store to temp dir - but don't create worktree.json
        let store_dir = temp_dir.path().join("meta-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::env::set_var("META_DATA_DIR", &store_dir);

        // List should return empty, not error (store file doesn't exist yet)
        let data = store_list().unwrap();
        assert!(data.worktrees.is_empty());

        // Remove on non-existent worktree should succeed
        let wt_path = temp_dir.path().join("nonexistent-wt");
        assert!(
            store_remove_batch(&[(store_key(&wt_path), "2025-01-01T00:00:00Z".to_string())])
                .is_ok()
        );

        std::env::remove_var("META_DATA_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn concurrent_store_adds_do_not_conflict() {
        use std::sync::Arc;
        use std::thread;

        let temp_dir = Arc::new(tempfile::tempdir().unwrap());
        // Isolate store to temp dir to avoid conflicts with parallel nextest processes
        let store_dir = temp_dir.path().join("meta-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::env::set_var("META_DATA_DIR", &store_dir);

        // Spawn multiple threads that add worktrees concurrently
        let handles: Vec<_> = (0..5)
            .map(|i| {
                let temp_dir = Arc::clone(&temp_dir);
                thread::spawn(move || {
                    let wt_path = temp_dir.path().join(format!("concurrent-add-{i}"));
                    std::fs::create_dir(&wt_path).unwrap();
                    let mut entry = make_entry("2025-01-01T00:00:00Z", None);
                    entry.name = format!("concurrent-add-{i}");
                    store_add(&wt_path, entry).unwrap();
                    wt_path
                })
            })
            .collect();

        // Wait for all threads
        let paths: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Verify all 5 entries were added (check each specific one exists)
        let data = store_list().unwrap();
        for path in &paths {
            let key = store_key(path);
            assert!(
                data.worktrees.contains_key(&key),
                "Concurrent add failed: {key} not found in store"
            );
        }

        // Cleanup - batch remove for efficiency
        let keys: Vec<String> = paths.iter().map(|p| store_key(p)).collect();
        let entries: Vec<_> = keys
            .iter()
            .map(|k| (k.clone(), "2025-01-01T00:00:00Z".to_string()))
            .collect();
        store_remove_batch(&entries).unwrap();

        // Verify cleanup worked
        let data_after = store_list().unwrap();
        for key in &keys {
            assert!(
                !data_after.worktrees.contains_key(key),
                "Cleanup failed: {key} still in store"
            );
        }
        std::env::remove_var("META_DATA_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn concurrent_batch_removes_handle_overlapping_keys() {
        use std::sync::Arc;
        use std::thread;

        let temp_dir = Arc::new(tempfile::tempdir().unwrap());
        // Isolate store to temp dir to avoid conflicts with parallel nextest processes
        let store_dir = temp_dir.path().join("meta-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::env::set_var("META_DATA_DIR", &store_dir);

        // Add 10 worktrees
        let paths: Vec<_> = (0..10)
            .map(|i| {
                let wt_path = temp_dir.path().join(format!("batch-rm-{i}"));
                std::fs::create_dir(&wt_path).unwrap();
                let mut entry = make_entry("2025-01-01T00:00:00Z", None);
                entry.name = format!("batch-rm-{i}");
                store_add(&wt_path, entry).unwrap();
                wt_path
            })
            .collect();

        let keys_before: Vec<String> = paths.iter().map(|p| store_key(p)).collect();

        // Spawn threads that remove overlapping batches
        let handles: Vec<_> = (0..3)
            .map(|batch_id| {
                let paths = paths.clone();
                thread::spawn(move || {
                    // Each thread removes a different subset
                    let start = batch_id * 3;
                    let end = std::cmp::min(start + 4, paths.len());
                    let keys: Vec<String> =
                        paths[start..end].iter().map(|p| store_key(p)).collect();
                    let entries: Vec<_> = keys
                        .into_iter()
                        .map(|k| (k, "2025-01-01T00:00:00Z".to_string()))
                        .collect();
                    store_remove_batch(&entries).unwrap();
                })
            })
            .collect();

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        // All entries should be removed (with possible duplicates in batches)
        let data = store_list().unwrap();
        for key in &keys_before {
            assert!(
                !data.worktrees.contains_key(key),
                "Key {key} should be removed"
            );
        }
        std::env::remove_var("META_DATA_DIR");
    }

    #[test]
    #[serial_test::serial]
    fn store_handles_corrupted_data_file() {
        // Isolate store to temp dir to avoid conflicts with parallel nextest processes
        let temp_dir = tempfile::tempdir().unwrap();
        let store_dir = temp_dir.path().join("meta-store");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::env::set_var("META_DATA_DIR", &store_dir);

        let store = store_path();

        // Clean up by ensuring a fresh empty store
        meta_core::data_dir::ensure_meta_dir().unwrap();
        std::fs::write(&store, b"{\"worktrees\":{}}").unwrap();

        // Write invalid JSON
        meta_core::data_dir::ensure_meta_dir().unwrap();
        std::fs::write(&store, b"not valid json").unwrap();

        // store_list should handle corruption gracefully
        // (either returns error or returns default empty store)
        let result = store_list();

        // Accept either behavior: error or default empty store
        match result {
            Ok(data) => {
                // If it returns a default, it should be empty
                assert!(
                    data.worktrees.is_empty(),
                    "Corrupted store should return empty data"
                );
            }
            Err(_) => {
                // Error is also acceptable
            }
        }

        std::env::remove_var("META_DATA_DIR");
        // temp_dir cleanup is automatic on drop
    }
}

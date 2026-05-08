use anyhow::{Context, Result};
use cargo_metadata::Metadata;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 1;
const PRUNE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60); // 30 days

#[derive(Serialize, Deserialize)]
struct CachedMetadata {
    schema_version: u32,
    manifest_mtimes: Vec<(PathBuf, u64)>,
    metadata: Metadata,
}

/// Return the cache directory for a given workspace root path.
fn cache_dir_for(workspace_root: &Path) -> PathBuf {
    let hash = fnv_hash(workspace_root.to_string_lossy().as_bytes());
    cache_base_dir().join(format!("{hash:016x}"))
}

fn cache_base_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg).join("rspeek")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache").join("rspeek")
    } else {
        PathBuf::from("/tmp").join("rspeek-cache")
    }
}

fn fnv_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn cache_file(workspace_root: &Path, no_deps: bool) -> PathBuf {
    let name = if no_deps {
        "metadata-nodeps.json"
    } else {
        "metadata-full.json"
    };
    cache_dir_for(workspace_root).join(name)
}

/// Collect mtime (as nanos since epoch) for each manifest path in the metadata,
/// plus the workspace Cargo.lock.
fn collect_mtimes(metadata: &Metadata) -> Vec<(PathBuf, u64)> {
    let mut entries = Vec::new();
    for pkg in &metadata.packages {
        let path = pkg.manifest_path.as_std_path().to_path_buf();
        if let Ok(mtime) = mtime_nanos(&path) {
            entries.push((path, mtime));
        }
    }
    let lock = metadata.workspace_root.as_std_path().join("Cargo.lock");
    if let Ok(mtime) = mtime_nanos(&lock) {
        entries.push((lock, mtime));
    }
    entries
}

fn mtime_nanos(path: &Path) -> Result<u64> {
    let meta = fs::metadata(path).context("stat")?;
    let mtime = meta.modified().context("mtime")?;
    Ok(mtime
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64)
}

/// Try to load cached metadata. Returns None on miss.
pub fn load(workspace_root: &Path, no_deps: bool) -> Option<Metadata> {
    let path = cache_file(workspace_root, no_deps);
    let data = fs::read(&path).ok()?;
    let cached: CachedMetadata = serde_json::from_slice(&data).ok()?;
    if cached.schema_version != SCHEMA_VERSION {
        return None;
    }
    // Validate mtimes
    for (manifest, cached_mtime) in &cached.manifest_mtimes {
        let current = mtime_nanos(manifest).ok()?;
        if current != *cached_mtime {
            return None;
        }
    }
    Some(cached.metadata)
}

/// Save metadata to cache. Best-effort; errors are silently ignored.
pub fn save(workspace_root: &Path, no_deps: bool, metadata: &Metadata) {
    let _ = save_inner(workspace_root, no_deps, metadata);
}

fn save_inner(workspace_root: &Path, no_deps: bool, metadata: &Metadata) -> Result<()> {
    let path = cache_file(workspace_root, no_deps);
    fs::create_dir_all(path.parent().unwrap()).context("create cache dir")?;
    let cached = CachedMetadata {
        schema_version: SCHEMA_VERSION,
        manifest_mtimes: collect_mtimes(metadata),
        metadata: metadata.clone(),
    };
    let data = serde_json::to_vec(&cached).context("serialize")?;
    // Atomic write: tmp file + rename
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &data).context("write tmp")?;
    fs::rename(&tmp, &path).context("rename")?;
    Ok(())
}

/// Remove cache entries older than 30 days. Best-effort.
pub fn prune() {
    let _ = prune_inner();
}

fn prune_inner() -> Result<()> {
    let base = cache_base_dir();
    let entries = fs::read_dir(&base).context("read cache dir")?;
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Check if all files in this dir are older than PRUNE_AGE
        let Ok(files) = fs::read_dir(&path) else {
            continue;
        };
        let all_old = files.flatten().all(|f| {
            f.metadata()
                .and_then(|m| m.modified())
                .map(|t| now.duration_since(t).unwrap_or_default() > PRUNE_AGE)
                .unwrap_or(true)
        });
        if all_old {
            let _ = fs::remove_dir_all(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    /// Create a minimal Cargo.toml + Cargo.lock in a tempdir and run
    /// `cargo metadata` to get a real Metadata value for testing.
    fn setup_workspace() -> (tempfile::TempDir, Metadata) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            r#"[package]
name = "test-pkg"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "").unwrap();

        let metadata = cargo_metadata::MetadataCommand::new()
            .current_dir(dir.path())
            .no_deps()
            .exec()
            .unwrap();
        (dir, metadata)
    }

    #[test]
    fn round_trip_save_and_load() {
        let (_dir, metadata) = setup_workspace();
        let root = metadata.workspace_root.as_std_path();
        save(root, true, &metadata);
        let loaded = load(root, true);
        assert!(loaded.is_some(), "cache hit expected after save");
        assert_eq!(loaded.unwrap().packages.len(), metadata.packages.len());
    }

    #[test]
    fn miss_on_mtime_change() {
        let (dir, metadata) = setup_workspace();
        let root = metadata.workspace_root.as_std_path();
        save(root, true, &metadata);

        // Touch the manifest to bump mtime
        thread::sleep(Duration::from_millis(20));
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, fs::read_to_string(&manifest).unwrap()).unwrap();

        let loaded = load(root, true);
        assert!(loaded.is_none(), "cache should miss after mtime change");
    }

    #[test]
    fn miss_on_corrupt_file() {
        let (_dir, metadata) = setup_workspace();
        let root = metadata.workspace_root.as_std_path();
        save(root, false, &metadata);

        let path = cache_file(root, false);
        fs::write(&path, b"not json").unwrap();

        let loaded = load(root, false);
        assert!(loaded.is_none(), "corrupt file should be a miss");
    }

    #[test]
    fn miss_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load(dir.path(), true);
        assert!(loaded.is_none(), "missing file should be a miss");
    }

    #[test]
    fn miss_on_schema_mismatch() {
        let (_dir, metadata) = setup_workspace();
        let root = metadata.workspace_root.as_std_path();
        save(root, true, &metadata);

        // Manually write a cache entry with a different schema version
        let path = cache_file(root, true);
        let mut cached: CachedMetadata = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        cached.schema_version = 999;
        fs::write(&path, serde_json::to_vec(&cached).unwrap()).unwrap();

        let loaded = load(root, true);
        assert!(loaded.is_none(), "schema mismatch should be a miss");
    }

    #[test]
    fn full_and_nodeps_are_independent() {
        let (_dir, metadata) = setup_workspace();
        let root = metadata.workspace_root.as_std_path();
        save(root, true, &metadata);

        assert!(load(root, true).is_some());
        assert!(
            load(root, false).is_none(),
            "nodeps save shouldn't hit full"
        );
    }

    #[test]
    fn prune_removes_old_entries() {
        let tmp = tempfile::tempdir().unwrap();
        // Override cache base via XDG_CACHE_HOME
        std::env::set_var("XDG_CACHE_HOME", tmp.path());

        let old_dir = cache_base_dir().join("0000000000000001");
        fs::create_dir_all(&old_dir).unwrap();
        let old_file = old_dir.join("metadata-full.json");
        fs::write(&old_file, b"{}").unwrap();

        // Set mtime to 31 days ago
        let old_time = SystemTime::now() - Duration::from_secs(31 * 24 * 60 * 60);
        filetime::set_file_mtime(&old_file, filetime::FileTime::from_system_time(old_time))
            .unwrap();

        prune();
        assert!(!old_dir.exists(), "old entry should be pruned");

        std::env::remove_var("XDG_CACHE_HOME");
    }

    #[test]
    fn prune_keeps_recent_entries() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", tmp.path());

        let recent_dir = cache_base_dir().join("0000000000000002");
        fs::create_dir_all(&recent_dir).unwrap();
        fs::write(recent_dir.join("metadata-full.json"), b"{}").unwrap();

        prune();
        assert!(recent_dir.exists(), "recent entry should be kept");

        std::env::remove_var("XDG_CACHE_HOME");
    }
}

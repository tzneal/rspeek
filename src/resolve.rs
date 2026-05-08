use anyhow::{bail, Context, Result};
use cargo_metadata::MetadataCommand;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A resolved crate with its source location.
#[derive(Debug)]
pub struct ResolvedCrate {
    pub name: String,
    pub version: String,
    /// Directories to search for items (typically `src/`, and `tests/` for workspace members).
    pub source_dirs: Vec<PathBuf>,
    pub is_workspace_member: bool,
    /// Build script OUT_DIR, if the crate has been built.
    pub out_dir: Option<PathBuf>,
    /// Direct dependency crate names.
    pub deps: Vec<String>,
}

/// Normalize crate name for comparison (treat `-` and `_` as equivalent).
pub fn normalize(name: &str) -> String {
    name.replace('-', "_")
}

/// Build a map from crate name → its most recently modified OUT_DIR by walking
/// `<target_dir>/*/build/*/out/` a single time.
///
/// Build-script output directories are named `<crate_name>-<16_hex_hash>`. We
/// strip the trailing `-<hash>` to recover the crate name. When multiple
/// directories exist for the same crate (different profiles, or stale hashes),
/// we keep the one with the newest mtime.
fn build_out_dir_map(
    target_dir: &Path,
) -> std::collections::HashMap<String, (PathBuf, std::time::SystemTime)> {
    let mut map: std::collections::HashMap<String, (PathBuf, std::time::SystemTime)> =
        std::collections::HashMap::new();
    let Ok(profiles) = std::fs::read_dir(target_dir) else {
        return map;
    };
    for profile in profiles.flatten() {
        let build_dir = profile.path().join("build");
        let Ok(entries) = std::fs::read_dir(&build_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let Some(crate_name) = strip_hash_suffix(name_str) else {
                continue;
            };
            let out = entry.path().join("out");
            if !out.is_dir() {
                continue;
            }
            let mtime = std::fs::metadata(&out)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            match map.get(crate_name) {
                Some((_, t)) if *t >= mtime => {}
                _ => {
                    map.insert(crate_name.to_string(), (out, mtime));
                }
            }
        }
    }
    map
}

/// Strip a trailing `-<16 hex chars>` suffix, returning the prefix. This
/// recovers the crate name from a cargo build-dir entry like
/// `async-trait-0abc123456789def`.
fn strip_hash_suffix(name: &str) -> Option<&str> {
    let (prefix, hash) = name.rsplit_once('-')?;
    if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(prefix)
    } else {
        None
    }
}

/// All resolved crates from `cargo metadata`.
pub struct Workspace {
    pub crates: Vec<ResolvedCrate>,
}

impl Workspace {
    pub fn load() -> Result<Self> {
        let metadata = MetadataCommand::new().exec().context(
            "failed to run `cargo metadata` — is there a Cargo.toml in the current directory?",
        )?;

        let resolve = metadata
            .resolve
            .context("no dependency resolution in metadata")?;
        let workspace_members: HashSet<_> = metadata.workspace_members.iter().collect();

        // Build a single index of all OUT_DIRs once, rather than re-scanning
        // `target/*/build/*` for every crate (O(N²) → O(N)).
        let out_dir_map = build_out_dir_map(metadata.target_directory.as_std_path());

        let mut crates = Vec::new();
        for node in &resolve.nodes {
            let pkg = metadata
                .packages
                .iter()
                .find(|p| p.id == node.id)
                .context("package not found in metadata")?;

            let manifest_dir = pkg
                .manifest_path
                .parent()
                .context("manifest has no parent dir")?;

            let manifest_std = manifest_dir.as_std_path();
            let src_dir = PathBuf::from(manifest_std).join("src");
            if src_dir.is_dir() {
                let is_member = workspace_members.contains(&node.id);
                let mut source_dirs = vec![src_dir];
                if is_member {
                    let tests_dir = PathBuf::from(manifest_std).join("tests");
                    if tests_dir.is_dir() {
                        source_dirs.push(tests_dir);
                    }
                }
                let out_dir = out_dir_map.get(pkg.name.as_str()).map(|(p, _)| p.clone());
                let deps: Vec<String> = node
                    .deps
                    .iter()
                    .filter(|d| {
                        d.dep_kinds
                            .iter()
                            .any(|k| k.kind == cargo_metadata::DependencyKind::Normal)
                    })
                    .map(|d| d.name.clone())
                    .collect();
                crates.push(ResolvedCrate {
                    name: pkg.name.to_string(),
                    version: pkg.version.to_string(),
                    source_dirs,
                    is_workspace_member: is_member,
                    out_dir,
                    deps,
                });
            }
        }

        if crates.is_empty() {
            bail!("no crates found");
        }

        Ok(Workspace { crates })
    }

    /// Check if a name matches any crate in the workspace.
    pub fn has_crate(&self, name: &str) -> bool {
        let norm = normalize(name);
        self.crates.iter().any(|c| normalize(&c.name) == norm)
    }

    /// Filter to crates matching the given name.
    pub fn filter(&self, name: &str) -> Vec<&ResolvedCrate> {
        let norm = normalize(name);
        self.crates
            .iter()
            .filter(|c| normalize(&c.name) == norm)
            .collect()
    }

    /// Names of all workspace member crates.
    pub fn member_names(&self) -> Vec<&str> {
        self.crates
            .iter()
            .filter(|c| c.is_workspace_member)
            .map(|c| c.name.as_str())
            .collect()
    }

    /// Workspace members that are not already dependencies of `crate_name`.
    pub fn available_members(&self, crate_name: &str, deps: &[String]) -> Vec<String> {
        let dep_set: HashSet<String> = deps.iter().map(|s| normalize(s)).collect();
        self.member_names()
            .into_iter()
            .filter(|&m| m != crate_name && !dep_set.contains(&normalize(m)))
            .map(|m| m.to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_hash_suffix_recognizes_cargo_build_dir_name() {
        assert_eq!(strip_hash_suffix("libc-0c691b6f9963e146"), Some("libc"));
        assert_eq!(
            strip_hash_suffix("async-trait-0abc123456789def"),
            Some("async-trait")
        );
        assert_eq!(
            strip_hash_suffix("aws-sdk-eks-0123456789abcdef"),
            Some("aws-sdk-eks")
        );
    }

    #[test]
    fn strip_hash_suffix_rejects_non_hash_suffix() {
        // Wrong length
        assert_eq!(strip_hash_suffix("libc-0c691b6f9963e14"), None);
        // Non-hex
        assert_eq!(strip_hash_suffix("libc-0c691b6fzzzzz146"), None);
        // No hyphen
        assert_eq!(strip_hash_suffix("libc"), None);
    }

    #[test]
    fn build_out_dir_map_collects_crates_across_profiles_and_picks_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path();

        // Two profiles, same crate name with different mtimes.
        let older = target
            .join("debug")
            .join("build")
            .join("mycrate-1111111111111111")
            .join("out");
        std::fs::create_dir_all(&older).unwrap();
        // Sleep to ensure distinct mtimes on fast filesystems.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = target
            .join("release")
            .join("build")
            .join("mycrate-2222222222222222")
            .join("out");
        std::fs::create_dir_all(&newer).unwrap();

        // An entry without an `out` subdir must be ignored.
        std::fs::create_dir_all(
            target
                .join("debug")
                .join("build")
                .join("nooutcrate-3333333333333333"),
        )
        .unwrap();

        let map = build_out_dir_map(target);
        assert_eq!(
            map.get("mycrate").map(|(p, _)| p.clone()),
            Some(newer),
            "expected the newer profile to win"
        );
        assert!(!map.contains_key("nooutcrate"));
    }
}

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
fn normalize(name: &str) -> String {
    name.replace('-', "_")
}

/// Find the OUT_DIR for a crate by scanning `<target_dir>/*/build/<crate>-*/out/`.
/// Returns the most recently modified match if multiple exist.
fn find_out_dir(target_dir: &Path, crate_name: &str) -> Option<PathBuf> {
    let prefix = format!("{crate_name}-");
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    // Check all profile dirs (debug, release, etc.)
    let Ok(profiles) = std::fs::read_dir(target_dir) else {
        return None;
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
            if !name_str.starts_with(&prefix) {
                continue;
            }
            let out = entry.path().join("out");
            if out.is_dir() {
                let mtime = std::fs::metadata(&out)
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().is_none_or(|(_, t)| mtime > *t) {
                    best = Some((out, mtime));
                }
            }
        }
    }
    best.map(|(p, _)| p)
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
                let out_dir = find_out_dir(metadata.target_directory.as_std_path(), &pkg.name);
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

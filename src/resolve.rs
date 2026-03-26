use anyhow::{bail, Context, Result};
use cargo_metadata::MetadataCommand;
use std::collections::HashSet;
use std::path::PathBuf;

/// A resolved crate with its source location.
#[derive(Debug)]
pub struct ResolvedCrate {
    pub name: String,
    pub version: String,
    /// Directories to search for items (typically `src/`, and `tests/` for workspace members).
    pub source_dirs: Vec<PathBuf>,
    pub is_workspace_member: bool,
}

/// Normalize crate name for comparison (treat `-` and `_` as equivalent).
fn normalize(name: &str) -> String {
    name.replace('-', "_")
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
                crates.push(ResolvedCrate {
                    name: pkg.name.to_string(),
                    version: pkg.version.to_string(),
                    source_dirs,
                    is_workspace_member: is_member,
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
}

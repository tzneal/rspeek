//! Re-export resolution: intra-crate `pub use` and cross-crate `pub use <crate>`.

use crate::index::IndexEntry;
use std::fs;
use std::path::Path;

/// A `pub use` re-export to resolve after indexing.
pub(crate) struct ReExport {
    /// The item name being re-exported (last segment of the use path).
    pub name: String,
    /// Optional alias (`pub use foo::Bar as Baz` → alias = "Baz").
    pub alias: Option<String>,
    /// Module path segments before the item name (e.g., `["errors"]` for `pub use errors::Error`).
    pub source_segments: Vec<String>,
    /// Module path where the re-export appears.
    pub reexport_module_path: String,
}

/// After all files are indexed, resolve `pub use` re-exports by finding
/// matching entries and creating aliases at the re-export's module path.
pub(crate) fn resolve_reexports(
    entries: &mut Vec<IndexEntry>,
    reexports: &[ReExport],
    name_filter: Option<&str>,
) {
    let mut new_entries = Vec::new();
    for re in reexports {
        let visible_name = re.alias.as_deref().unwrap_or(&re.name);
        if let Some(filter) = name_filter {
            if visible_name != filter {
                continue;
            }
        }
        // Find the original entry: name matches and module path ends with the source segments
        let original = entries.iter().find(|e| {
            e.name == re.name && reexport_path_matches(&e.module_path, &re.source_segments)
        });
        if let Some(orig) = original {
            // Don't create duplicate if already visible at this path
            let already_exists = entries
                .iter()
                .chain(new_entries.iter())
                .any(|e| e.name == visible_name && e.module_path == re.reexport_module_path);
            if !already_exists {
                new_entries.push(IndexEntry {
                    name: visible_name.to_string(),
                    kind: orig.kind,
                    module_path: re.reexport_module_path.clone(),
                    file: orig.file.clone(),
                    start_line: orig.start_line,
                    end_line: orig.end_line,
                    cfg: orig.cfg.clone(),
                });
            }
        }
    }
    entries.extend(new_entries);
}

/// Check if an entry's module path matches the source segments of a re-export.
/// `pub use errors::Error` in root → source_segments = ["errors"], matches module_path "errors".
/// `pub use sub::inner::Foo` in root → source_segments = ["sub", "inner"], matches "sub::inner".
fn reexport_path_matches(entry_module_path: &str, source_segments: &[String]) -> bool {
    if source_segments.is_empty() {
        return true;
    }
    let expected = source_segments.join("::");
    entry_module_path == expected || entry_module_path.ends_with(&format!("::{expected}"))
}

/// Recursively collect re-exported item names from a `use` tree.
pub(crate) fn collect_reexports(
    tree: &syn::UseTree,
    prefix: &[String],
    reexport_module_path: &str,
    reexports: &mut Vec<ReExport>,
) {
    match tree {
        syn::UseTree::Path(p) => {
            let seg = p.ident.to_string();
            // Skip `crate::`, `self::`, `super::` prefixes — just continue into the tree
            if seg == "crate" || seg == "self" || seg == "super" {
                collect_reexports(&p.tree, prefix, reexport_module_path, reexports);
            } else {
                let mut new_prefix = prefix.to_vec();
                new_prefix.push(seg);
                collect_reexports(&p.tree, &new_prefix, reexport_module_path, reexports);
            }
        }
        syn::UseTree::Name(n) => {
            let name = n.ident.to_string();
            reexports.push(ReExport {
                name,
                alias: None,
                source_segments: prefix.to_vec(),
                reexport_module_path: reexport_module_path.to_string(),
            });
        }
        syn::UseTree::Rename(r) => {
            reexports.push(ReExport {
                name: r.ident.to_string(),
                alias: Some(r.rename.to_string()),
                source_segments: prefix.to_vec(),
                reexport_module_path: reexport_module_path.to_string(),
            });
        }
        syn::UseTree::Group(g) => {
            for tree in &g.items {
                collect_reexports(tree, prefix, reexport_module_path, reexports);
            }
        }
        syn::UseTree::Glob(_) => {
            // Skip glob re-exports — too complex to resolve statically
        }
    }
}

/// Collect `pub use <crate_name>` re-exports from a crate's root source file.
/// Returns `(original_crate_name, optional_alias)` pairs for re-exports that
/// refer to external crates (not `crate::`, `self::`, or `super::` paths).
pub fn cross_crate_reexports(
    src_dir: &Path,
    _out_dir: Option<&Path>,
) -> Vec<(String, Option<String>)> {
    let lib_rs = src_dir.join("lib.rs");
    let path = if lib_rs.exists() {
        lib_rs
    } else {
        return Vec::new();
    };
    let Ok(source) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(file) = syn::parse_file(&source) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for item in &file.items {
        if let syn::Item::Use(u) = item {
            if !matches!(u.vis, syn::Visibility::Public(_)) {
                continue;
            }
            collect_extern_reexports(&u.tree, &mut result);
        }
    }
    result
}

/// Extract bare external crate re-exports from a use tree.
/// `pub use ttrpc;` → ("ttrpc", None)
/// `pub use other as aliased;` → ("other", Some("aliased"))
/// Skips paths starting with `crate`, `self`, `super`.
fn collect_extern_reexports(tree: &syn::UseTree, out: &mut Vec<(String, Option<String>)>) {
    match tree {
        syn::UseTree::Name(n) => {
            let name = n.ident.to_string();
            if !is_internal_path(&name) {
                out.push((name, None));
            }
        }
        syn::UseTree::Rename(r) => {
            let name = r.ident.to_string();
            if !is_internal_path(&name) {
                out.push((name, Some(r.rename.to_string())));
            }
        }
        syn::UseTree::Path(_) => {
            // `pub use ttrpc::context::with_timeout;` — not a bare crate re-export, skip
        }
        syn::UseTree::Group(g) => {
            for tree in &g.items {
                collect_extern_reexports(tree, out);
            }
        }
        syn::UseTree::Glob(_) => {}
    }
}

fn is_internal_path(name: &str) -> bool {
    matches!(name, "crate" | "self" | "super")
}

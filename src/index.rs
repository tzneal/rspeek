use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use syn::spanned::Spanned;
use syn::visit::Visit;

/// A found item in the source index.
#[derive(Debug, Serialize)]
pub struct IndexEntry {
    pub name: String,
    pub kind: &'static str,
    pub module_path: String,
    pub file: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
}

/// An `impl` block for a type.
#[derive(Debug, Serialize)]
pub struct ImplBlock {
    pub trait_name: Option<String>,
    pub file: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
}

/// Build an index of items under `dirs` matching `item_name`.
pub fn index_crate(dirs: &[PathBuf], item_name: &str) -> Result<Vec<IndexEntry>> {
    let mut entries = Vec::new();
    for dir in dirs {
        entries.extend(collect(dir, Some(item_name), false)?);
    }
    Ok(entries)
}

/// List all items under `dirs`. If `pub_only` is true, only public items.
pub fn list_items(dirs: &[PathBuf], pub_only: bool) -> Result<Vec<IndexEntry>> {
    let mut entries = Vec::new();
    for dir in dirs {
        entries.extend(collect(dir, None, pub_only)?);
    }
    Ok(entries)
}

/// Find all `impl` blocks for `type_name` under `dirs`.
pub fn find_impls(dirs: &[PathBuf], type_name: &str) -> Result<Vec<ImplBlock>> {
    let files = dirs
        .iter()
        .flat_map(|d| gather_rs_files(d))
        .collect::<Vec<_>>();
    let impls: Vec<ImplBlock> = files
        .par_iter()
        .flat_map(|path| find_impls_in_file(path, type_name).unwrap_or_default())
        .collect();
    Ok(impls)
}

fn find_impls_in_file(path: &Path, type_name: &str) -> Result<Vec<ImplBlock>> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let Ok(file) = syn::parse_file(&source) else {
        return Ok(vec![]);
    };
    let mut impls = Vec::new();
    for item in &file.items {
        if let syn::Item::Impl(imp) = item {
            if impl_is_for(imp, type_name) {
                let trait_name = imp.trait_.as_ref().map(|(_, p, _)| {
                    p.segments
                        .iter()
                        .map(|s| s.ident.to_string())
                        .collect::<Vec<_>>()
                        .join("::")
                });
                impls.push(ImplBlock {
                    trait_name,
                    file: path.to_path_buf(),
                    start_line: imp.span().start().line,
                    end_line: imp.span().end().line,
                });
            }
        }
    }
    Ok(impls)
}

/// Check if an impl block is for the given type name.
fn impl_is_for(imp: &syn::ItemImpl, type_name: &str) -> bool {
    if let syn::Type::Path(tp) = &*imp.self_ty {
        tp.path
            .segments
            .last()
            .is_some_and(|s| s.ident == type_name)
    } else {
        false
    }
}

fn collect(src_dir: &Path, name_filter: Option<&str>, pub_only: bool) -> Result<Vec<IndexEntry>> {
    let mut entries = Vec::new();
    let mut reexports = Vec::new();
    let mut visited = HashSet::new();

    let lib_rs = src_dir.join("lib.rs");
    let main_rs = src_dir.join("main.rs");
    let entry = if lib_rs.exists() {
        lib_rs
    } else if main_rs.exists() {
        main_rs
    } else {
        walk_all_rs(
            src_dir,
            name_filter,
            pub_only,
            &mut entries,
            &mut reexports,
            &mut visited,
        )?;
        resolve_reexports(&mut entries, &reexports, name_filter);
        return Ok(entries);
    };

    index_file(
        &entry,
        "",
        name_filter,
        pub_only,
        &mut entries,
        &mut reexports,
        &mut visited,
    )?;
    resolve_reexports(&mut entries, &reexports, name_filter);
    Ok(entries)
}

/// A `pub use` re-export to resolve after indexing.
struct ReExport {
    /// The item name being re-exported (last segment of the use path).
    name: String,
    /// Optional alias (`pub use foo::Bar as Baz` → alias = "Baz").
    alias: Option<String>,
    /// Module path segments before the item name (e.g., `["errors"]` for `pub use errors::Error`).
    source_segments: Vec<String>,
    /// Module path where the re-export appears.
    reexport_module_path: String,
}

/// After all files are indexed, resolve `pub use` re-exports by finding
/// matching entries and creating aliases at the re-export's module path.
fn resolve_reexports(
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

fn index_file(
    path: &Path,
    module_path: &str,
    name_filter: Option<&str>,
    pub_only: bool,
    entries: &mut Vec<IndexEntry>,
    reexports: &mut Vec<ReExport>,
    visited: &mut HashSet<PathBuf>,
) -> Result<()> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return Ok(());
    }

    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let file = syn::parse_file(&source).ok();
    let Some(file) = file else {
        return Ok(());
    };

    let mut visitor = ItemVisitor {
        name_filter,
        pub_only,
        module_path: module_path.to_string(),
        file_path: path.to_path_buf(),
        entries,
    };
    visitor.visit_file(&file);

    // Collect `pub use` re-exports and follow `mod` declarations
    let parent_dir = path.parent().unwrap();
    for item in &file.items {
        match item {
            syn::Item::Use(u) if is_pub(&u.vis) => {
                collect_reexports(&u.tree, &[], module_path, reexports);
            }
            syn::Item::Mod(m) if m.content.is_none() => {
                let mod_name = m.ident.to_string();
                let child_path = resolve_mod_file(parent_dir, &mod_name);
                if let Some(child) = child_path {
                    let child_mod = if module_path.is_empty() {
                        mod_name
                    } else {
                        format!("{module_path}::{mod_name}")
                    };
                    index_file(
                        &child,
                        &child_mod,
                        name_filter,
                        pub_only,
                        entries,
                        reexports,
                        visited,
                    )?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Recursively collect re-exported item names from a `use` tree.
fn collect_reexports(
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

fn resolve_mod_file(parent: &Path, mod_name: &str) -> Option<PathBuf> {
    let direct = parent.join(format!("{mod_name}.rs"));
    if direct.exists() {
        return Some(direct);
    }
    let nested = parent.join(mod_name).join("mod.rs");
    if nested.exists() {
        return Some(nested);
    }
    None
}

/// Recursively gather all `.rs` file paths under a directory.
fn gather_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    gather_rs_files_walk(dir, &mut files);
    files
}

fn gather_rs_files_walk(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            gather_rs_files_walk(&path, files);
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
}

fn walk_all_rs(
    dir: &Path,
    name_filter: Option<&str>,
    pub_only: bool,
    entries: &mut Vec<IndexEntry>,
    reexports: &mut Vec<ReExport>,
    visited: &mut HashSet<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_all_rs(&path, name_filter, pub_only, entries, reexports, visited)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            index_file(
                &path,
                "",
                name_filter,
                pub_only,
                entries,
                reexports,
                visited,
            )?;
        }
    }
    Ok(())
}

struct ItemVisitor<'a> {
    name_filter: Option<&'a str>,
    pub_only: bool,
    module_path: String,
    file_path: PathBuf,
    entries: &'a mut Vec<IndexEntry>,
}

impl ItemVisitor<'_> {
    fn check(&mut self, name: &str, kind: &'static str, is_pub: bool, start: usize, end: usize) {
        if self.pub_only && !is_pub {
            return;
        }
        if let Some(filter) = self.name_filter {
            if name != filter {
                return;
            }
        }
        self.entries.push(IndexEntry {
            name: name.to_string(),
            kind,
            module_path: self.module_path.clone(),
            file: self.file_path.clone(),
            start_line: start,
            end_line: end,
        });
    }
}

fn is_pub(vis: &syn::Visibility) -> bool {
    matches!(vis, syn::Visibility::Public(_))
}

fn doc_start(attrs: &[syn::Attribute], item_start: usize) -> usize {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .map(|a| a.span().start().line)
        .min()
        .unwrap_or(item_start)
}

impl<'ast> Visit<'ast> for ItemVisitor<'_> {
    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        let start = doc_start(&node.attrs, node.span().start().line);
        self.check(
            &node.ident.to_string(),
            "struct",
            is_pub(&node.vis),
            start,
            node.span().end().line,
        );
        syn::visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        let start = doc_start(&node.attrs, node.span().start().line);
        self.check(
            &node.ident.to_string(),
            "enum",
            is_pub(&node.vis),
            start,
            node.span().end().line,
        );
        syn::visit::visit_item_enum(self, node);
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        let start = doc_start(&node.attrs, node.span().start().line);
        self.check(
            &node.ident.to_string(),
            "trait",
            is_pub(&node.vis),
            start,
            node.span().end().line,
        );
        syn::visit::visit_item_trait(self, node);
    }

    fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
        let start = doc_start(&node.attrs, node.span().start().line);
        self.check(
            &node.ident.to_string(),
            "type",
            is_pub(&node.vis),
            start,
            node.span().end().line,
        );
        syn::visit::visit_item_type(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let start = doc_start(&node.attrs, node.span().start().line);
        self.check(
            &node.sig.ident.to_string(),
            "fn",
            is_pub(&node.vis),
            start,
            node.span().end().line,
        );
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if node.content.is_some() {
            let mod_name = node.ident.to_string();
            let old_path = self.module_path.clone();
            self.module_path = if old_path.is_empty() {
                mod_name
            } else {
                format!("{old_path}::{}", node.ident)
            };
            syn::visit::visit_item_mod(self, node);
            self.module_path = old_path;
        }
    }

    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        // Try to parse the macro body as Rust items.
        // Handles patterns like: ast_struct! { pub struct Foo { ... } }
        let tokens = node.mac.tokens.clone();
        if let Ok(body) = syn::parse2::<syn::File>(tokens) {
            for item in &body.items {
                self.visit_item(item);
            }
        }
    }
}

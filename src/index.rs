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

/// Options threaded through the recursive collect/index/walk functions.
#[derive(Clone, Copy)]
struct CollectOpts<'a> {
    name_filter: Option<&'a str>,
    pub_only: bool,
    out_dir: Option<&'a Path>,
    text_filter: Option<&'a str>,
}

/// Build an index of items under `dirs` matching `item_name`.
/// Uses a fast text pre-filter: only parses files containing `item_name`.
/// Falls back to full parse if the fast pass finds nothing.
pub fn index_crate(
    dirs: &[PathBuf],
    item_name: &str,
    pub_only: bool,
    out_dir: Option<&Path>,
) -> Result<Vec<IndexEntry>> {
    let opts = CollectOpts {
        name_filter: Some(item_name),
        pub_only,
        out_dir,
        text_filter: Some(item_name),
    };
    let mut entries = Vec::new();
    for dir in dirs {
        entries.extend(collect(dir, opts)?);
    }
    if entries.is_empty() {
        let opts = CollectOpts {
            text_filter: None,
            ..opts
        };
        for dir in dirs {
            entries.extend(collect(dir, opts)?);
        }
    }
    Ok(entries)
}

/// List all items under `dirs`. If `pub_only` is true, only public items.
pub fn list_items(
    dirs: &[PathBuf],
    pub_only: bool,
    out_dir: Option<&Path>,
) -> Result<Vec<IndexEntry>> {
    let opts = CollectOpts {
        name_filter: None,
        pub_only,
        out_dir,
        text_filter: None,
    };
    let mut entries = Vec::new();
    for dir in dirs {
        entries.extend(collect(dir, opts)?);
    }
    Ok(entries)
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
            if !is_pub(&u.vis) {
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

/// Find all `impl` blocks for `type_name` under `dirs`.
pub fn find_impls(
    dirs: &[PathBuf],
    type_name: &str,
    out_dir: Option<&Path>,
) -> Result<Vec<ImplBlock>> {
    let mut files: Vec<PathBuf> = dirs.iter().flat_map(|d| gather_rs_files(d)).collect();
    if let Some(od) = out_dir {
        if od.is_dir() {
            files.extend(gather_rs_files(od));
        }
    }
    let impls: Vec<ImplBlock> = files
        .par_iter()
        .flat_map(|path| find_impls_in_file(path, type_name).unwrap_or_default())
        .collect();
    Ok(impls)
}

fn find_impls_in_file(path: &Path, type_name: &str) -> Result<Vec<ImplBlock>> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if !source.contains(type_name) {
        return Ok(vec![]);
    }
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

fn collect(src_dir: &Path, opts: CollectOpts) -> Result<Vec<IndexEntry>> {
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
        walk_all_rs(src_dir, opts, &mut entries, &mut reexports, &mut visited)?;
        resolve_reexports(&mut entries, &reexports, opts.name_filter);
        return Ok(entries);
    };

    index_file(&entry, "", opts, &mut entries, &mut reexports, &mut visited)?;
    resolve_reexports(&mut entries, &reexports, opts.name_filter);
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
    opts: CollectOpts,
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

    // When text_filter is set and the file doesn't contain the text,
    // skip the expensive parse+visit but still follow mod declarations
    // by scanning for `mod <name>;` lines.
    if opts.text_filter.is_some_and(|tf| !source.contains(tf)) {
        let parent_dir = path.parent().unwrap();
        for line in source.lines() {
            let trimmed = line.trim();
            let rest = trimmed
                .strip_prefix("pub")
                .and_then(|s| {
                    if s.starts_with(' ') {
                        Some(s.trim_start())
                    } else if s.starts_with('(') {
                        s.find(')').map(|i| s[i + 1..].trim_start())
                    } else {
                        None
                    }
                })
                .unwrap_or(trimmed);
            let Some(rest) = rest.strip_prefix("mod ") else {
                continue;
            };
            let Some(mod_name) = rest.strip_suffix(';') else {
                continue;
            };
            let mod_name = mod_name.trim();
            if mod_name.is_empty() || mod_name.contains(' ') {
                continue;
            }
            if let Some(child) = resolve_mod_file(parent_dir, mod_name) {
                let child_mod = if module_path.is_empty() {
                    mod_name.to_string()
                } else {
                    format!("{module_path}::{mod_name}")
                };
                index_file(&child, &child_mod, opts, entries, reexports, visited)?;
            }
        }
        return Ok(());
    }

    let file = syn::parse_file(&source).ok();
    let Some(file) = file else {
        return Ok(());
    };

    let mut visitor = ItemVisitor {
        name_filter: opts.name_filter,
        pub_only: opts.pub_only,
        module_path: module_path.to_string(),
        file_path: path.to_path_buf(),
        out_dir: opts.out_dir.map(Path::to_path_buf),
        entries,
        included_files: Vec::new(),
    };
    visitor.visit_file(&file);
    let included_files = std::mem::take(&mut visitor.included_files);

    // Index any files discovered via include!() macros
    for (inc_path, inc_module) in included_files {
        index_file(&inc_path, &inc_module, opts, entries, reexports, visited)?;
    }

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
                    index_file(&child, &child_mod, opts, entries, reexports, visited)?;
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
    opts: CollectOpts,
    entries: &mut Vec<IndexEntry>,
    reexports: &mut Vec<ReExport>,
    visited: &mut HashSet<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_all_rs(&path, opts, entries, reexports, visited)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            index_file(&path, "", opts, entries, reexports, visited)?;
        }
    }
    Ok(())
}

struct ItemVisitor<'a> {
    name_filter: Option<&'a str>,
    pub_only: bool,
    module_path: String,
    file_path: PathBuf,
    out_dir: Option<PathBuf>,
    entries: &'a mut Vec<IndexEntry>,
    /// Files discovered via `include!()` that need indexing after the visit.
    included_files: Vec<(PathBuf, String)>,
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

/// Resolve the path argument of an `include!()` macro invocation.
/// Handles `include!("path.rs")` and `include!(concat!(env!("OUT_DIR"), "/file.rs"))`.
fn resolve_include_path(
    tokens: &proc_macro2::TokenStream,
    out_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Ok(lit) = syn::parse2::<syn::LitStr>(tokens.clone()) {
        return Some(PathBuf::from(lit.value()));
    }
    // For concat!(env!("OUT_DIR"), ...), work with the string representation
    // which is predictable: `concat ! (env ! ("OUT_DIR") , "/file.rs")`
    let s = tokens.to_string();
    if !s.starts_with("concat") {
        return None;
    }
    let out_str = out_dir?.to_string_lossy();
    let resolved = s
        // Strip the concat!(...) wrapper
        .strip_prefix("concat")?
        .trim()
        .strip_prefix("!")?
        .trim()
        .strip_prefix("(")?
        .strip_suffix(")")?;
    let mut result = String::new();
    for piece in resolved.split(',') {
        let piece = piece.trim();
        if piece.contains("env") && piece.contains("OUT_DIR") {
            result.push_str(&out_str);
        } else if let Ok(lit) = syn::parse_str::<syn::LitStr>(piece) {
            result.push_str(&lit.value());
        } else {
            return None;
        }
    }
    Some(PathBuf::from(result))
}

fn doc_start(attrs: &[syn::Attribute], item_start: usize) -> usize {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .map(|a| a.span().start().line)
        .min()
        .unwrap_or(item_start)
}

/// Check if a macro path is `include_proto`, `tonic::include_proto`, or `prost::include_proto`.
fn is_include_proto(path: &syn::Path) -> bool {
    let segs: Vec<_> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    segs == ["include_proto"]
        || segs == ["tonic", "include_proto"]
        || segs == ["prost", "include_proto"]
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

    fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
        let start = doc_start(&node.attrs, node.span().start().line);
        self.check(
            &node.ident.to_string(),
            "union",
            is_pub(&node.vis),
            start,
            node.span().end().line,
        );
        syn::visit::visit_item_union(self, node);
    }

    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        let start = doc_start(&node.attrs, node.span().start().line);
        self.check(
            &node.ident.to_string(),
            "const",
            is_pub(&node.vis),
            start,
            node.span().end().line,
        );
    }

    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        let start = doc_start(&node.attrs, node.span().start().line);
        self.check(
            &node.ident.to_string(),
            "static",
            is_pub(&node.vis),
            start,
            node.span().end().line,
        );
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
        // Handle include!() by resolving the path and queuing the file for indexing.
        if node.mac.path.is_ident("include") {
            if let Some(path) = resolve_include_path(&node.mac.tokens, self.out_dir.as_deref()) {
                if path.exists() {
                    self.included_files.push((path, self.module_path.clone()));
                    return;
                }
            }
        }
        // Handle tonic::include_proto!("package.name") → OUT_DIR/package.name.rs
        if is_include_proto(&node.mac.path) {
            if let Some(out_dir) = &self.out_dir {
                if let Ok(lit) = syn::parse2::<syn::LitStr>(node.mac.tokens.clone()) {
                    let path = out_dir.join(format!("{}.rs", lit.value()));
                    if path.exists() {
                        self.included_files.push((path, self.module_path.clone()));
                        return;
                    }
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn collect_from_source(source: &str) -> Vec<IndexEntry> {
        let dir = tempfile::tempdir().unwrap();
        let lib_rs = dir.path().join("lib.rs");
        let mut f = fs::File::create(&lib_rs).unwrap();
        f.write_all(source.as_bytes()).unwrap();
        collect(
            dir.path(),
            CollectOpts {
                name_filter: None,
                pub_only: false,
                out_dir: None,
                text_filter: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn finds_const() {
        let entries = collect_from_source("pub const FOO: u32 = 42;");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "FOO");
        assert_eq!(entries[0].kind, "const");
    }

    #[test]
    fn finds_static() {
        let entries = collect_from_source("pub static BAR: &str = \"hello\";");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "BAR");
        assert_eq!(entries[0].kind, "static");
    }

    #[test]
    fn finds_union() {
        let entries = collect_from_source("pub union MyUnion { pub f1: u32, pub f2: f32 }");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "MyUnion");
        assert_eq!(entries[0].kind, "union");
    }

    #[test]
    fn index_crate_filters_pub_crate_in_child_module() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        // lib.rs: public struct + private mod with pub(crate) struct of same name
        fs::write(src.join("lib.rs"), "pub struct Chain;\nmod inner;").unwrap();
        fs::write(src.join("inner.rs"), "pub(crate) struct Chain;").unwrap();
        let entries = index_crate(&[src], "Chain", true, None).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "expected only the pub struct, got: {entries:?}"
        );
        assert_eq!(entries[0].module_path, "");
    }

    #[test]
    fn text_filter_follows_pub_crate_mod() {
        // Foo exists in both `a.rs` (via `pub mod a`) and `b.rs` (via
        // `pub(crate) mod b`). The fast path finds the first Foo so the
        // fallback never triggers — if the text scanner can't follow
        // `pub(crate) mod`, the second Foo is silently lost.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("lib.rs"), "pub mod a;\npub(crate) mod b;").unwrap();
        fs::write(src.join("a.rs"), "pub struct Foo;").unwrap();
        fs::write(src.join("b.rs"), "pub struct Foo;").unwrap();
        let entries = index_crate(&[src], "Foo", false, None).unwrap();
        assert_eq!(
            entries.len(),
            2,
            "expected Foo from both modules, got: {entries:?}"
        );
    }

    #[test]
    fn pub_only_filters_private_const() {
        let dir = tempfile::tempdir().unwrap();
        let lib_rs = dir.path().join("lib.rs");
        fs::write(
            &lib_rs,
            "const PRIVATE: u32 = 1;\npub const PUBLIC: u32 = 2;",
        )
        .unwrap();
        let entries = collect(
            dir.path(),
            CollectOpts {
                name_filter: None,
                pub_only: true,
                out_dir: None,
                text_filter: None,
            },
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "PUBLIC");
    }

    #[test]
    fn cross_crate_reexports_finds_pub_use_crate() {
        let dir = tempfile::tempdir().unwrap();
        let lib_rs = dir.path().join("lib.rs");
        fs::write(
            &lib_rs,
            "pub use ttrpc;\npub use other_crate as aliased;\npub struct Local;",
        )
        .unwrap();
        let reexports = cross_crate_reexports(dir.path(), None);
        assert_eq!(reexports.len(), 2);
        assert!(reexports
            .iter()
            .any(|(name, alias)| name == "ttrpc" && alias.is_none()));
        assert!(reexports
            .iter()
            .any(|(name, alias)| name == "other_crate" && alias.as_deref() == Some("aliased")));
    }

    #[test]
    fn cross_crate_reexports_ignores_internal_reexports() {
        // `pub use crate::foo::Bar;` and `pub use self::baz;` are internal, not cross-crate
        let dir = tempfile::tempdir().unwrap();
        let lib_rs = dir.path().join("lib.rs");
        fs::write(
            &lib_rs,
            "pub use crate::foo::Bar;\npub use self::baz;\nmod foo { pub struct Bar; }",
        )
        .unwrap();
        let reexports = cross_crate_reexports(dir.path(), None);
        assert!(
            reexports.is_empty(),
            "internal re-exports should not appear: {reexports:?}"
        );
    }
}

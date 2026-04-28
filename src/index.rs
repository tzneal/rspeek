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
    /// Cfg predicates that gate this item (from `#[cfg(...)]` on the item
    /// itself, its parent modules, and cfg-gating macros). Empty when ungated.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cfg: Vec<String>,
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
#[derive(Clone)]
struct CollectOpts<'a> {
    name_filter: Option<&'a str>,
    pub_only: bool,
    out_dir: Option<&'a Path>,
    text_filter: Option<&'a str>,
    /// Cfg predicates inherited from parent modules/macros.
    inherited_cfg: Vec<String>,
    /// Cfg predicates extracted from `macro_rules!` definitions across the crate.
    macro_cfgs: MacroCfgMap,
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
    let macro_cfgs = prescan_macro_cfgs(dirs);
    let opts = CollectOpts {
        name_filter: Some(item_name),
        pub_only,
        out_dir,
        text_filter: Some(item_name),
        inherited_cfg: Vec::new(),
        macro_cfgs: macro_cfgs.clone(),
    };
    let mut entries = Vec::new();
    for dir in dirs {
        entries.extend(collect(dir, opts.clone())?);
    }
    if entries.is_empty() {
        let opts = CollectOpts {
            text_filter: None,
            ..opts
        };
        for dir in dirs {
            entries.extend(collect(dir, opts.clone())?);
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
    let macro_cfgs = prescan_macro_cfgs(dirs);
    let opts = CollectOpts {
        name_filter: None,
        pub_only,
        out_dir,
        text_filter: None,
        inherited_cfg: Vec::new(),
        macro_cfgs,
    };
    let mut entries = Vec::new();
    for dir in dirs {
        entries.extend(collect(dir, opts.clone())?);
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
        walk_all_rs(src_dir, &opts, &mut entries, &mut reexports, &mut visited)?;
        resolve_reexports(&mut entries, &reexports, opts.name_filter);
        return Ok(entries);
    };

    index_file(
        &entry,
        "",
        &opts,
        &mut entries,
        &mut reexports,
        &mut visited,
    )?;
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

fn index_file(
    path: &Path,
    module_path: &str,
    opts: &CollectOpts,
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
    // by scanning for `mod <name>;` lines. Honors a `#[path = "..."]`
    // attribute on the preceding line (or one separated by other attrs
    // like `#[cfg(...)]`), matching rustc's resolution rules.
    if opts.text_filter.is_some_and(|tf| !source.contains(tf)) {
        let parent_dir = submodule_dir(path);
        let mut pending_path: Option<String> = None;
        for line in source.lines() {
            let trimmed = line.trim();

            // Buffer `#[path = "..."]` for use on the next mod line.
            if let Some(p) = parse_path_attr_line(trimmed) {
                pending_path = Some(p);
                continue;
            }
            // Ignore other attributes / empty / comment lines between
            // `#[path]` and the `mod` line.
            if trimmed.is_empty()
                || trimmed.starts_with("//")
                || (trimmed.starts_with("#[") && trimmed.ends_with(']'))
            {
                continue;
            }

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
                // Any non-mod, non-attribute code line clears the buffered
                // path attribute (it was meant for something else).
                pending_path = None;
                continue;
            };
            let Some(mod_name) = rest.strip_suffix(';') else {
                pending_path = None;
                continue;
            };
            let mod_name = mod_name.trim();
            if mod_name.is_empty() || mod_name.contains(' ') {
                pending_path = None;
                continue;
            }
            let child = match pending_path.take() {
                Some(rel) => {
                    let candidate = parent_dir.join(&rel);
                    candidate.exists().then_some(candidate)
                }
                None => resolve_mod_file(&parent_dir, mod_name),
            };
            if let Some(child) = child {
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

    // Collect macro_rules! cfg definitions from this file and merge with crate-wide ones.
    let mut macro_cfgs = opts.macro_cfgs.clone();
    macro_cfgs.extend(collect_macro_cfg_defs(&file));

    let parent_dir = submodule_dir(path);
    let mut visitor = ItemVisitor {
        name_filter: opts.name_filter,
        pub_only: opts.pub_only,
        module_path: module_path.to_string(),
        file_path: path.to_path_buf(),
        out_dir: opts.out_dir.map(Path::to_path_buf),
        inherited_cfg: opts.inherited_cfg.clone(),
        macro_cfgs: &macro_cfgs,
        parent_dir,
        entries,
        included_files: Vec::new(),
        child_mods: Vec::new(),
    };
    visitor.visit_file(&file);
    let included_files = std::mem::take(&mut visitor.included_files);
    let child_mods = std::mem::take(&mut visitor.child_mods);

    // Index any files discovered via include!() macros
    for (inc_path, inc_module, inc_cfg) in included_files {
        let child_opts = CollectOpts {
            inherited_cfg: inc_cfg,
            ..opts.clone()
        };
        index_file(
            &inc_path,
            &inc_module,
            &child_opts,
            entries,
            reexports,
            visited,
        )?;
    }

    // Follow mod declarations and macro-body mod declarations
    for (child_path, child_mod, child_cfg) in child_mods {
        let child_opts = CollectOpts {
            inherited_cfg: child_cfg,
            ..opts.clone()
        };
        index_file(
            &child_path,
            &child_mod,
            &child_opts,
            entries,
            reexports,
            visited,
        )?;
    }

    // Collect `pub use` re-exports
    for item in &file.items {
        if let syn::Item::Use(u) = item {
            if is_pub(&u.vis) {
                collect_reexports(&u.tree, &[], module_path, reexports);
            }
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

/// Return the string value of the first `#[path = "..."]` attribute, if any.
fn extract_path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("path") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            {
                return Some(s.value());
            }
        }
    }
    None
}

/// Parse-free variant of `extract_path_attr` for the fast-path text scanner.
/// Accepts a single attribute line like `#[path = "foo.rs"]` and returns
/// the quoted value.
fn parse_path_attr_line(line: &str) -> Option<String> {
    let line = line.trim();
    let inner = line.strip_prefix("#[")?.strip_suffix("]")?;
    let inner = inner.strip_prefix("path")?.trim_start();
    let inner = inner.strip_prefix('=')?.trim();
    // Expect a quoted string; take the content between first and last "
    let start = inner.find('"')? + 1;
    let end = inner.rfind('"')?;
    if end <= start {
        return None;
    }
    Some(inner[start..end].to_string())
}

/// Directory where the submodules of the given module file live.
///
/// - For `lib.rs` / `main.rs` / `mod.rs`: submodules sit next to the file
///   (i.e. the file's own parent directory). This is the legacy layout.
/// - For any other module file `foo.rs` (Rust 2018+ "mod file" style):
///   submodules live in a sibling `foo/` directory. This is the layout used
///   by code-generated crates like the AWS SDK (e.g. `types.rs` with
///   children in `types/`).
fn submodule_dir(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new(""));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    match stem {
        "" | "lib" | "main" | "mod" => parent.to_path_buf(),
        _ => parent.join(stem),
    }
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
    opts: &CollectOpts,
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

/// Mapping from macro name → cfg predicates extracted from `macro_rules!` definitions.
type MacroCfgMap = std::collections::HashMap<String, Vec<String>>;

/// Pre-scan all `.rs` files under `dirs` for `macro_rules!` cfg-gating definitions.
fn prescan_macro_cfgs(dirs: &[PathBuf]) -> MacroCfgMap {
    let mut map = MacroCfgMap::new();
    for dir in dirs {
        for path in gather_rs_files(dir) {
            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            // Quick text check: skip files that don't contain macro_rules
            if !source.contains("macro_rules") {
                continue;
            }
            let Ok(file) = syn::parse_file(&source) else {
                continue;
            };
            map.extend(collect_macro_cfg_defs(&file));
        }
    }
    map
}

struct ItemVisitor<'a> {
    name_filter: Option<&'a str>,
    pub_only: bool,
    module_path: String,
    file_path: PathBuf,
    out_dir: Option<PathBuf>,
    /// Cfg predicates inherited from parent modules/macros.
    inherited_cfg: Vec<String>,
    /// Cfg predicates extracted from `macro_rules!` definitions in this file.
    macro_cfgs: &'a MacroCfgMap,
    /// Parent directory for resolving `mod foo;` declarations.
    parent_dir: PathBuf,
    entries: &'a mut Vec<IndexEntry>,
    /// Files discovered via `include!()` that need indexing after the visit.
    included_files: Vec<(PathBuf, String, Vec<String>)>,
    /// Child mod files discovered (path, module_path, cfg) to index after visit.
    child_mods: Vec<(PathBuf, String, Vec<String>)>,
}

impl ItemVisitor<'_> {
    fn check(
        &mut self,
        name: &str,
        kind: &'static str,
        is_pub: bool,
        attrs: &[syn::Attribute],
        start: usize,
        end: usize,
    ) {
        if self.pub_only && !is_pub {
            return;
        }
        if let Some(filter) = self.name_filter {
            if name != filter {
                return;
            }
        }
        let mut cfg = self.inherited_cfg.clone();
        cfg.extend(extract_cfg_attrs(attrs));
        cfg.dedup();
        self.entries.push(IndexEntry {
            name: name.to_string(),
            kind,
            module_path: self.module_path.clone(),
            file: self.file_path.clone(),
            start_line: start,
            end_line: end,
            cfg,
        });
    }
}

fn is_pub(vis: &syn::Visibility) -> bool {
    matches!(vis, syn::Visibility::Public(_))
}

/// Extract `#[cfg(...)]` predicate strings from a list of attributes.
fn extract_cfg_attrs(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut cfgs = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("cfg") {
            continue;
        }
        if let syn::Meta::List(list) = &attr.meta {
            cfgs.push(list.tokens.to_string());
        }
    }
    cfgs
}

/// Scan a file for `macro_rules!` definitions that follow the cfg-gating pattern:
/// ```ignore
/// macro_rules! cfg_xxx {
///     ($($item:item)*) => { $( #[cfg(PRED)] $item )* }
/// }
/// ```
/// Returns a map from macro name → vec of cfg predicate strings.
fn collect_macro_cfg_defs(file: &syn::File) -> MacroCfgMap {
    let mut map = MacroCfgMap::new();
    for item in &file.items {
        let syn::Item::Macro(m) = item else { continue };
        let Some(ident) = &m.ident else { continue };
        if !m.mac.path.is_ident("macro_rules") {
            continue;
        }
        let name = ident.to_string();
        if let Some(cfgs) = extract_cfg_from_macro_body(&m.mac.tokens) {
            if !cfgs.is_empty() {
                map.insert(name, cfgs);
            }
        }
    }
    map
}

/// Try to extract cfg predicates from a macro_rules! body.
/// Looks for `#[cfg(...)]` attributes before `$item` in the expansion.
fn extract_cfg_from_macro_body(tokens: &proc_macro2::TokenStream) -> Option<Vec<String>> {
    // proc_macro2 to_string() adds spaces between all tokens.
    // Strip whitespace for reliable matching, then normalize output.
    let s = tokens.to_string();
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let after_arrow = compact.split("=>").nth(1)?;
    let mut cfgs = Vec::new();
    let mut rest = after_arrow;
    while let Some(pos) = rest.find("#[cfg(") {
        let start = pos + 6;
        let substr = &rest[start..];
        let mut depth = 1i32;
        let mut end = 0;
        for (i, ch) in substr.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        if end > 0 {
            // Normalize: add space after commas and around = for readability
            let raw = &substr[..end];
            let pred = raw.replace(',', ", ").replace('=', " = ");
            cfgs.push(pred);
        }
        rest = &rest[start + end..];
    }
    Some(cfgs)
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
            &node.attrs,
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
            &node.attrs,
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
            &node.attrs,
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
            &node.attrs,
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
            &node.attrs,
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
            &node.attrs,
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
            &node.attrs,
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
            &node.attrs,
            start,
            node.span().end().line,
        );
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let mod_name = node.ident.to_string();
        let mut mod_cfg = self.inherited_cfg.clone();
        mod_cfg.extend(extract_cfg_attrs(&node.attrs));
        let path_override = extract_path_attr(&node.attrs);

        if node.content.is_some() {
            // Inline module — recurse with updated cfg + parent_dir context.
            // Any nested file-reference `mod bar;` inside this inline module
            // resolves relative to <current_parent>/<mod_name> (or the
            // `#[path = "..."]` override, treated as a directory for inline
            // mods). This matches rustc's rule for inline modules in
            // non-`mod.rs` files.
            let old_path = self.module_path.clone();
            let old_cfg = std::mem::replace(&mut self.inherited_cfg, mod_cfg);
            let subdir_name = path_override.as_deref().unwrap_or(&mod_name);
            let new_parent = self.parent_dir.join(subdir_name);
            let old_parent = std::mem::replace(&mut self.parent_dir, new_parent);
            self.module_path = if old_path.is_empty() {
                mod_name
            } else {
                format!("{old_path}::{}", node.ident)
            };
            syn::visit::visit_item_mod(self, node);
            self.module_path = old_path;
            self.inherited_cfg = old_cfg;
            self.parent_dir = old_parent;
        } else {
            // File-reference mod — queue for indexing with cfg context.
            // `#[path = "..."]` overrides the default `<mod_name>.rs` /
            // `<mod_name>/mod.rs` lookup with an explicit relative path.
            let child_mod = if self.module_path.is_empty() {
                mod_name.clone()
            } else {
                format!("{}::{mod_name}", self.module_path)
            };
            let child = match &path_override {
                Some(rel) => {
                    let candidate = self.parent_dir.join(rel);
                    candidate.exists().then_some(candidate)
                }
                None => resolve_mod_file(&self.parent_dir, &mod_name),
            };
            if let Some(child) = child {
                self.child_mods.push((child, child_mod, mod_cfg));
            }
        }
    }

    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        // Handle include!() by resolving the path and queuing the file for indexing.
        if node.mac.path.is_ident("include") {
            if let Some(path) = resolve_include_path(&node.mac.tokens, self.out_dir.as_deref()) {
                if path.exists() {
                    self.included_files.push((
                        path,
                        self.module_path.clone(),
                        self.inherited_cfg.clone(),
                    ));
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
                        self.included_files.push((
                            path,
                            self.module_path.clone(),
                            self.inherited_cfg.clone(),
                        ));
                        return;
                    }
                }
            }
        }
        // Try to parse the macro body as Rust items.
        // Handles patterns like: ast_struct! { pub struct Foo { ... } }
        // Also handles cfg-gating macros like: cfg_net_unix! { mod foo; pub mod bar { ... } }
        let tokens = node.mac.tokens.clone();
        if let Ok(body) = syn::parse2::<syn::File>(tokens) {
            // Check if this macro has known cfg predicates
            let macro_name = node
                .mac
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default();
            let macro_cfg = self
                .macro_cfgs
                .get(&macro_name)
                .cloned()
                .unwrap_or_default();

            let old_cfg = if !macro_cfg.is_empty() {
                let mut combined = self.inherited_cfg.clone();
                combined.extend(macro_cfg);
                Some(std::mem::replace(&mut self.inherited_cfg, combined))
            } else {
                None
            };

            for item in &body.items {
                self.visit_item(item);
            }

            if let Some(prev) = old_cfg {
                self.inherited_cfg = prev;
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
                inherited_cfg: Vec::new(),
                macro_cfgs: MacroCfgMap::new(),
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
                inherited_cfg: Vec::new(),
                macro_cfgs: MacroCfgMap::new(),
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

    #[test]
    fn cfg_attr_on_item() {
        let entries = collect_from_source("#[cfg(unix)]\npub struct UnixOnly;");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cfg, vec!["unix"]);
    }

    #[test]
    fn cfg_attr_inherited_from_module() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("lib.rs");
        fs::write(
            &src,
            "#[cfg(unix)]\nmod platform {\n    pub struct Inner;\n}",
        )
        .unwrap();
        let entries = collect(
            dir.path(),
            CollectOpts {
                name_filter: None,
                pub_only: false,
                out_dir: None,
                text_filter: None,
                inherited_cfg: Vec::new(),
                macro_cfgs: MacroCfgMap::new(),
            },
        )
        .unwrap();
        let inner = entries.iter().find(|e| e.name == "Inner").unwrap();
        assert_eq!(inner.cfg, vec!["unix"]);
    }

    #[test]
    fn cfg_gating_macro_items_found() {
        let source = r#"
macro_rules! cfg_net_unix {
    ($($item:item)*) => {
        $(
            #[cfg(all(unix, feature = "net"))]
            $item
        )*
    }
}

cfg_net_unix! {
    pub struct AsyncFd;
}
"#;
        let entries = collect_from_source(source);
        let entry = entries.iter().find(|e| e.name == "AsyncFd").unwrap();
        assert_eq!(entry.cfg, vec![r#"all(unix, feature = "net")"#]);
    }

    #[test]
    fn cfg_gating_macro_follows_mod_to_child_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            r#"
macro_rules! cfg_net_unix {
    ($($item:item)*) => {
        $(
            #[cfg(all(unix, feature = "net"))]
            $item
        )*
    }
}

cfg_net_unix! {
    mod child;
}
"#,
        )
        .unwrap();
        fs::write(dir.path().join("child.rs"), "pub struct ChildItem;").unwrap();
        let entries = collect(
            dir.path(),
            CollectOpts {
                name_filter: None,
                pub_only: false,
                out_dir: None,
                text_filter: None,
                inherited_cfg: Vec::new(),
                macro_cfgs: MacroCfgMap::new(),
            },
        )
        .unwrap();
        let child = entries.iter().find(|e| e.name == "ChildItem").unwrap();
        assert_eq!(child.module_path, "child");
        assert_eq!(child.cfg, vec![r#"all(unix, feature = "net")"#]);
    }

    #[test]
    fn no_cfg_when_ungated() {
        let entries = collect_from_source("pub struct Plain;");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].cfg.is_empty());
    }

    #[test]
    fn multiple_cfg_attrs_accumulated() {
        let entries =
            collect_from_source("#[cfg(unix)]\n#[cfg(feature = \"net\")]\npub struct Multi;");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cfg.len(), 2);
        assert!(entries[0].cfg.contains(&"unix".to_string()));
        assert!(entries[0].cfg.contains(&"feature = \"net\"".to_string()));
    }

    /// Rust 2018+ "mod file" style: a non-`mod.rs` parent file like `types.rs`
    /// sits alongside a `types/` directory that holds its submodules.
    /// `mod _inner;` inside `types.rs` should resolve to `types/_inner.rs`,
    /// NOT `_inner.rs` at the crate root.
    /// This layout is used pervasively by code-generated crates
    /// (e.g. the `aws-sdk-*` crates).
    #[test]
    fn mod_file_style_submodule_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("types")).unwrap();
        // lib.rs -> pub mod types;
        fs::write(src.join("lib.rs"), "pub mod types;").unwrap();
        // types.rs -> mod _inner; pub use crate::types::_inner::Inner;
        fs::write(
            src.join("types.rs"),
            "mod _inner;\npub use crate::types::_inner::Inner;\n",
        )
        .unwrap();
        // types/_inner.rs -> pub struct Inner;
        fs::write(src.join("types").join("_inner.rs"), "pub struct Inner;\n").unwrap();

        // Full-parse path: list_items with no text filter.
        let entries = list_items(std::slice::from_ref(&src), true, None).unwrap();
        let inner = entries
            .iter()
            .find(|e| e.name == "Inner" && e.module_path == "types::_inner")
            .unwrap_or_else(|| {
                panic!(
                    "expected Inner at types::_inner, got: {:#?}",
                    entries
                        .iter()
                        .map(|e| format!("{} @ {}", e.name, e.module_path))
                        .collect::<Vec<_>>()
                )
            });
        assert_eq!(inner.kind, "struct");

        // The `pub use` re-export should also make `Inner` visible at `types`.
        assert!(
            entries
                .iter()
                .any(|e| e.name == "Inner" && e.module_path == "types"),
            "expected re-exported Inner at module path `types`, got: {:#?}",
            entries
                .iter()
                .filter(|e| e.name == "Inner")
                .map(|e| format!("{} @ {}", e.name, e.module_path))
                .collect::<Vec<_>>()
        );

        // Filtered-search path: index_crate with the fast text pre-filter.
        // This exercises the `mod <name>;` line-scan fallback in index_file.
        let filtered = index_crate(std::slice::from_ref(&src), "Inner", true, None).unwrap();
        assert!(
            filtered
                .iter()
                .any(|e| e.name == "Inner" && e.module_path == "types::_inner"),
            "index_crate did not find Inner at types::_inner: {:#?}",
            filtered
        );
    }

    /// Classic `mod.rs` style must keep working: `foo/mod.rs` declaring
    /// `mod bar;` resolves to `foo/bar.rs` (not `foo/foo/bar.rs`).
    #[test]
    fn mod_rs_style_submodule_resolution_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("foo")).unwrap();
        fs::write(src.join("lib.rs"), "pub mod foo;").unwrap();
        fs::write(src.join("foo").join("mod.rs"), "pub mod bar;").unwrap();
        fs::write(src.join("foo").join("bar.rs"), "pub struct Baz;").unwrap();

        let entries = list_items(&[src], true, None).unwrap();
        let baz = entries
            .iter()
            .find(|e| e.name == "Baz")
            .expect("Baz missing");
        assert_eq!(baz.module_path, "foo::bar");
    }

    /// `#[path = "..."] mod foo;` at the crate root should resolve to the
    /// custom relative path, not `<parent_dir>/foo.rs`.
    #[test]
    fn path_attr_file_reference_mod() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("lib.rs"),
            "#[path = \"custom_name.rs\"]\npub mod foo;\n",
        )
        .unwrap();
        // Note: file name is `custom_name.rs`, NOT `foo.rs`. Without #[path]
        // support, rspeek would look for `foo.rs` and miss the file.
        fs::write(src.join("custom_name.rs"), "pub struct Custom;").unwrap();

        let entries = list_items(std::slice::from_ref(&src), true, None).unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.name == "Custom" && e.module_path == "foo"),
            "expected Custom at module path `foo`, got: {:#?}",
            entries
                .iter()
                .map(|e| format!("{} @ {}", e.name, e.module_path))
                .collect::<Vec<_>>()
        );

        // Fast-path text scanner should also follow #[path].
        let filtered = index_crate(std::slice::from_ref(&src), "Custom", true, None).unwrap();
        assert!(
            filtered
                .iter()
                .any(|e| e.name == "Custom" && e.module_path == "foo"),
            "fast-path did not find Custom: {:#?}",
            filtered
        );
    }

    /// The `libc` pattern: stacked `#[cfg(...)] #[path = "..."] mod imp;`
    /// declarations should each be indexed with their cfg predicate attached,
    /// so queries work regardless of platform.
    #[test]
    fn path_attr_stacked_cfg_variants_all_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("lib.rs"),
            r#"
#[cfg(unix)]
#[path = "unix.rs"]
pub mod imp;

#[cfg(windows)]
#[path = "windows.rs"]
pub mod imp;
"#,
        )
        .unwrap();
        fs::write(src.join("unix.rs"), "pub struct UnixItem;").unwrap();
        fs::write(src.join("windows.rs"), "pub struct WindowsItem;").unwrap();

        let entries = list_items(&[src], true, None).unwrap();
        let unix = entries
            .iter()
            .find(|e| e.name == "UnixItem")
            .expect("UnixItem missing");
        assert_eq!(unix.module_path, "imp");
        assert!(
            unix.cfg.iter().any(|c| c == "unix"),
            "UnixItem should carry cfg=unix, got: {:?}",
            unix.cfg
        );
        let win = entries
            .iter()
            .find(|e| e.name == "WindowsItem")
            .expect("WindowsItem missing");
        assert_eq!(win.module_path, "imp");
        assert!(
            win.cfg.iter().any(|c| c == "windows"),
            "WindowsItem should carry cfg=windows, got: {:?}",
            win.cfg
        );
    }

    /// Nested inline mod inside a non-`mod.rs` parent file. Rust resolves
    /// a file-reference mod declared inside an inline module by descending
    /// one more directory level per inline mod: `foo.rs` with
    /// `mod outer { mod bar; }` means `bar` lives at `foo/outer/bar.rs`
    /// (or `foo/outer/bar/mod.rs`).
    #[test]
    fn nested_inline_mod_inside_mod_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("foo").join("outer")).unwrap();
        fs::write(src.join("lib.rs"), "pub mod foo;").unwrap();
        fs::write(src.join("foo.rs"), "pub mod outer {\n    pub mod bar;\n}\n").unwrap();
        fs::write(
            src.join("foo").join("outer").join("bar.rs"),
            "pub struct Nested;",
        )
        .unwrap();

        let entries = list_items(&[src], true, None).unwrap();
        let nested = entries
            .iter()
            .find(|e| e.name == "Nested")
            .unwrap_or_else(|| {
                panic!(
                    "Nested not found. Entries: {:#?}",
                    entries
                        .iter()
                        .map(|e| format!("{} @ {}", e.name, e.module_path))
                        .collect::<Vec<_>>()
                )
            });
        assert_eq!(nested.module_path, "foo::outer::bar");
    }
}

use crate::index::{ImplBlock, IndexEntry};
use crate::resolve::ResolvedCrate;
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use syn::spanned::Spanned;

#[derive(Serialize)]
pub struct JsonEntry {
    pub name: String,
    pub kind: &'static str,
    pub module_path: String,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub crate_name: String,
    pub crate_version: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impls: Option<Vec<JsonImplBlock>>,
}

#[derive(Serialize)]
pub struct JsonImplBlock {
    pub trait_name: Option<String>,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub source: String,
}

#[derive(Serialize)]
pub struct JsonCrateOverview {
    pub crate_name: String,
    pub crate_version: String,
    pub src_dir: String,
    pub items: Vec<JsonItem>,
}

#[derive(Serialize)]
pub struct JsonItem {
    pub name: String,
    pub kind: &'static str,
    pub module_path: String,
}

pub fn read_source(entry: &IndexEntry) -> Result<String> {
    let source = fs::read_to_string(&entry.file)
        .with_context(|| format!("failed to read {}", entry.file.display()))?;
    let lines: Vec<&str> = source.lines().collect();
    let start = entry.start_line.saturating_sub(1);
    let end = entry.end_line.min(lines.len());
    Ok(lines[start..end].join("\n"))
}

/// Extract just the signature from source, stripping function bodies.
/// Structs, enums, and type aliases are returned as-is.
/// For fns: signature + ` { … }`. For traits: method bodies replaced with ` { … }`.
pub fn extract_signature(source: &str, kind: &str) -> String {
    match kind {
        "fn" => sig_fn(source),
        "trait" => sig_trait(source),
        _ => source.to_string(), // struct, enum, type — already signatures
    }
}

/// For a function: keep everything up to the body, append ` { … }`
fn sig_fn(source: &str) -> String {
    let source_lines: Vec<&str> = source.lines().collect();
    let brace_line = find_fn_body_start(&source_lines);
    if brace_line >= source_lines.len() {
        return source.to_string();
    }
    // Include lines before the brace line, plus the part of the brace line before `{`
    let mut result = String::new();
    for line in &source_lines[..brace_line] {
        result.push_str(line);
        result.push('\n');
    }
    let line = source_lines[brace_line];
    if let Some(pos) = find_body_brace(line) {
        let before = line[..pos].trim_end();
        if !before.is_empty() {
            result.push_str(before);
        }
    }
    // Trim trailing whitespace/newlines, then append marker
    let trimmed = result.trim_end();
    format!("{trimmed} {{ … }}")
}

/// Find the index of the body-opening `{` in a single line,
/// skipping braces inside angle brackets.
fn find_body_brace(line: &str) -> Option<usize> {
    let mut depth_angle = 0i32;
    let mut last = None;
    for (i, ch) in line.char_indices() {
        match ch {
            '<' => depth_angle += 1,
            '>' if depth_angle > 0 => depth_angle -= 1,
            '{' if depth_angle == 0 => last = Some(i),
            _ => {}
        }
    }
    last
}

/// Find the line index (0-based) where the fn body `{` starts.
/// Scans for the first `{` that isn't inside generics/parens.
fn find_fn_body_start(lines: &[&str]) -> usize {
    let mut depth_paren = 0i32;
    let mut depth_angle = 0i32;
    for (i, line) in lines.iter().enumerate() {
        for ch in line.chars() {
            match ch {
                '(' => depth_paren += 1,
                ')' => depth_paren -= 1,
                '<' => depth_angle += 1,
                '>' if depth_angle > 0 => depth_angle -= 1,
                '{' if depth_paren == 0 && depth_angle == 0 => return i,
                _ => {}
            }
        }
    }
    lines.len()
}

/// For a trait: keep the definition but replace default method bodies with ` { … }`
fn sig_trait(source: &str) -> String {
    let Ok(file) = syn::parse_file(source) else {
        return source.to_string();
    };
    let Some(syn::Item::Trait(tr)) = file.items.into_iter().next() else {
        return source.to_string();
    };

    let source_lines: Vec<&str> = source.lines().collect();

    // Collect (start, end) ranges of method bodies to replace (0-indexed line numbers)
    let mut replacements: Vec<(usize, usize)> = Vec::new();
    for item in &tr.items {
        if let syn::TraitItem::Fn(f) = item {
            if let Some(ref block) = f.default {
                let body_start = block.span().start().line - 1; // 0-indexed
                let body_end = block.span().end().line - 1;
                replacements.push((body_start, body_end));
            }
        }
    }

    if replacements.is_empty() {
        return source.to_string();
    }

    let mut result = String::new();
    let mut i = 0;
    for (body_start, body_end) in &replacements {
        // Emit lines before this body
        for line in &source_lines[i..*body_start] {
            result.push_str(line);
            result.push('\n');
        }
        // Replace body with ` { … }`
        // The line at body_start likely has the `{`, grab the leading whitespace
        let indent = source_lines[*body_start]
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(0);
        result.push_str(&" ".repeat(indent));
        result.push_str("{ … }\n");
        i = body_end + 1;
    }
    // Emit remaining lines
    for line in &source_lines[i..] {
        result.push_str(line);
        result.push('\n');
    }
    result.truncate(result.trim_end_matches('\n').len());
    result
}

fn read_lines(path: &std::path::Path, start: usize, end: usize) -> Result<String> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let lines: Vec<&str> = source.lines().collect();
    let start = start.saturating_sub(1);
    let end = end.min(lines.len());
    Ok(lines[start..end].join("\n"))
}

fn to_json_impls(impls: &[ImplBlock], signature: bool) -> Result<Vec<JsonImplBlock>> {
    impls
        .iter()
        .map(|imp| {
            let source = read_lines(&imp.file, imp.start_line, imp.end_line)?;
            let source = if signature {
                sig_impl_block(&source)
            } else {
                source
            };
            Ok(JsonImplBlock {
                trait_name: imp.trait_name.clone(),
                file: imp.file.display().to_string(),
                start_line: imp.start_line,
                end_line: imp.end_line,
                source,
            })
        })
        .collect()
}

/// Strip method bodies from an impl block, keeping only signatures.
fn sig_impl_block(source: &str) -> String {
    let Ok(file) = syn::parse_file(source) else {
        return source.to_string();
    };
    let Some(syn::Item::Impl(imp)) = file.items.first() else {
        return source.to_string();
    };

    let source_lines: Vec<&str> = source.lines().collect();
    let mut replacements: Vec<(usize, usize)> = Vec::new();

    for item in &imp.items {
        if let syn::ImplItem::Fn(f) = item {
            let body_start = f.block.span().start().line - 1;
            let body_end = f.block.span().end().line - 1;
            replacements.push((body_start, body_end));
        }
    }

    if replacements.is_empty() {
        return source.to_string();
    }

    let mut result = String::new();
    let mut i = 0;
    for (body_start, body_end) in &replacements {
        for line in &source_lines[i..*body_start] {
            result.push_str(line);
            result.push('\n');
        }
        let line = source_lines[*body_start];
        if let Some(pos) = find_body_brace(line) {
            let before = line[..pos].trim_end();
            if !before.is_empty() {
                result.push_str(before);
            }
        }
        result.push_str(" { … }\n");
        i = body_end + 1;
    }
    for line in &source_lines[i..] {
        result.push_str(line);
        result.push('\n');
    }
    result.truncate(result.trim_end_matches('\n').len());
    result
}

pub fn to_json_entry(
    entry: &IndexEntry,
    krate: &ResolvedCrate,
    impls: Option<&[ImplBlock]>,
    signature: bool,
) -> Result<JsonEntry> {
    let source = read_source(entry)?;
    let source = if signature {
        extract_signature(&source, entry.kind)
    } else {
        source
    };
    Ok(JsonEntry {
        name: entry.name.clone(),
        kind: entry.kind,
        module_path: entry.module_path.clone(),
        file: entry.file.display().to_string(),
        start_line: entry.start_line,
        end_line: entry.end_line,
        crate_name: krate.name.clone(),
        crate_version: krate.version.clone(),
        source,
        impls: impls.map(|i| to_json_impls(i, signature)).transpose()?,
    })
}

/// Extract source lines and format as markdown.
pub fn format_entry(
    entry: &IndexEntry,
    krate: &ResolvedCrate,
    impls: Option<&[ImplBlock]>,
    signature: bool,
) -> Result<String> {
    let source = read_source(entry)?;
    let source = if signature {
        extract_signature(&source, entry.kind)
    } else {
        source
    };
    let mut out = format!(
        "## `{}` ({})\n\
         **Source:** `{}:{}`\n\
         **Crate:** `{}` v{}\n\
         \n\
         ```rust\n\
         {}\n\
         ```",
        entry.name,
        entry.kind,
        entry.file.display(),
        entry.start_line,
        krate.name,
        krate.version,
        source,
    );

    if let Some(impls) = impls {
        for imp in impls {
            let label = match &imp.trait_name {
                Some(t) => format!("impl {t} for {}", entry.name),
                None => format!("impl {}", entry.name),
            };
            let imp_source = read_lines(&imp.file, imp.start_line, imp.end_line)?;
            let imp_source = if signature {
                sig_impl_block(&imp_source)
            } else {
                imp_source
            };
            out.push_str(&format!(
                "\n\n### `{label}`\n**Source:** `{}:{}`\n\n```rust\n{}\n```",
                imp.file.display(),
                imp.start_line,
                imp_source,
            ));
        }
    }

    Ok(out)
}

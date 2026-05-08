use anyhow::Result;
use clap::Parser;
use rayon::prelude::*;
use std::collections::HashSet;

use crate::index::{self, IndexEntry};
use crate::output;
use crate::query;
use crate::reexport;
use crate::resolve::{self, ResolvedCrate};
use crate::suggest;
use crate::Output;

/// Quickly inspect type definitions from Rust dependency source code.
#[derive(Parser, Debug)]
#[command(version)]
pub struct Cli {
    /// Crate name, fully qualified path, or item name
    first: Option<String>,
    /// Item name when first arg is a crate name
    second: Option<String>,
    /// Print extended usage examples for LLM tool integration
    #[arg(long)]
    pub llm_help: bool,
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
    /// Include impl blocks for matched types
    #[arg(long)]
    pub impls: bool,
    /// Show only signatures (no function bodies)
    #[arg(long)]
    pub signature: bool,
    /// Shorthand for --signature --impls (type API at a glance)
    #[arg(long)]
    pub api: bool,
    /// Filter to a specific crate version (e.g. --crate-version 0.29.0)
    #[arg(long)]
    pub crate_version: Option<String>,
}

/// Error with optional suggestions for "did you mean?" messages.
pub struct NotFound {
    pub message: String,
    pub suggestions: Vec<String>,
}

impl From<anyhow::Error> for NotFound {
    fn from(e: anyhow::Error) -> Self {
        NotFound {
            message: e.to_string(),
            suggestions: vec![],
        }
    }
}

pub const LLM_HELP: &str = "\
# rspeek — inspect type definitions from Rust dependency source code

## Usage

rspeek is a Rust-only tool. It parses Rust source files (.rs) and only
works with Cargo (Rust) projects. Do NOT use it for Go, Python, Java,
TypeScript, C++, or any other language.

It is designed to be called by LLMs to look up structs, enums, unions,
traits, type aliases, constants, statics, and functions from Rust crates
without building docs.

Run from any directory containing a Cargo.toml. Searches dependency crates
and workspace member crates (including integration tests in `tests/`).

## Query forms

  rspeek <crate>                    List public items in a crate
  rspeek <Item>                     Search all crates for an item
  rspeek <crate> <Item>             Search within one crate
  rspeek <crate>::<path>::<Item>    Match by module path

## Flags

  --json       Output as JSON (structured output for programmatic use)
  --impls      Include impl blocks for matched types
  --signature  Show only signatures (no function/method bodies)
  --api        Shorthand for --signature --impls (type API at a glance)
  --crate-version <VERSION>  Filter to a specific crate version
  --llm-help   Print this help text

## Examples

  rspeek anyhow                     List public items in anyhow
  rspeek Error                      Find all types named Error across deps
  rspeek anyhow Error               Find Error in the anyhow crate
  rspeek anyhow::Error              Same, using qualified syntax
  rspeek serde::de::Visitor         Find Visitor in serde's de module
  rspeek --json anyhow Error        JSON output with source code
  rspeek --impls anyhow Error       Include all impl blocks
  rspeek --signature anyhow Error   Signatures only (no fn bodies)
  rspeek --api anyhow Error         Signatures for type + all impl methods
  rspeek --crate-version 0.29.0 nix kill  Pin a specific crate version

## Output

Single match: full markdown with doc comments, source path, line number,
crate version, and the original source in a fenced code block.

Multiple matches: summary list with kind, qualified path, and location.
Narrow the search with a crate name or qualified path.

## Crate overview

`rspeek <crate>` lists public items and also shows:
- **Dependencies**: direct deps of the crate (already in Cargo.toml)
- **Workspace members (not yet deps)**: sibling crates that can be added
  with a one-line Cargo.toml edit

Use this to check what's already available before writing code from scratch.

## JSON output

--json returns an array of objects:
  [{\"name\", \"kind\", \"module_path\", \"file\", \"start_line\", \"end_line\",
    \"crate_name\", \"crate_version\", \"source\", \"impls\"?, \"other_versions\"?}]

Crate overview (rspeek --json <crate>) returns:
  [{\"crate_name\", \"crate_version\", \"src_dir\", \"items\": [...],
    \"deps\": [\"anyhow\", ...],
    \"available_workspace_members\": [\"my-utils\", ...]}]

Errors are also JSON: {\"error\": \"...\", \"suggestions\": [\"...\"]}

## Did you mean?

When no match is found, rspeek suggests similar names using Levenshtein
distance and prefix matching (like rustc). Suggestions appear in the
error message (plain text) or in the \"suggestions\" array (JSON).

## Scope

- Dependency crates: searches `src/`, public items only
- Workspace member crates: searches `src/` and `tests/`, all items (including tests)

## Limitations

- Only finds items defined as regular Rust syntax (struct, enum, union, trait, type, fn, const, static)
- Macro bodies are parsed for item definitions (e.g. syn's ast_struct!), but
  procedural macros and complex macro_rules! patterns are not expanded
- Re-exports: `pub use` within a crate are followed; cross-crate `pub use <crate>`
  re-exports are followed for qualified lookups; glob re-exports are not
";

fn not_found(message: impl Into<String>, query: &str, candidates: &[String]) -> NotFound {
    NotFound {
        message: message.into(),
        suggestions: suggest::suggestions(query, candidates.iter().map(|s| s.as_str()))
            .into_iter()
            .map(String::from)
            .collect(),
    }
}

pub fn run(cli: &Cli, out: &mut Output) -> std::result::Result<(), NotFound> {
    let first = cli.first.as_deref().ok_or_else(|| NotFound {
        message: "missing argument: item name or crate name\n\nUsage: rspeek [OPTIONS] <FIRST> [SECOND]\n\nTry --help or --llm-help for usage".into(),
        suggestions: vec![],
    })?;

    // Phase 1: try to resolve using workspace members only (cargo metadata
    // --no-deps, ~20× faster than full). Skip phase 1 if the query references
    // a crate name that isn't a workspace member, since it can't succeed.
    let members = resolve::Workspace::load_members_only().map_err(NotFound::from)?;
    let query = query::Query::parse(first, cli.second.as_deref(), &members);
    let can_try_phase1 = query
        .crate_filter()
        .is_none_or(|name| members.has_crate(name));

    if can_try_phase1 {
        let mut buf = Output::default();
        if dispatch(cli, &members, &query, &mut buf).is_ok() {
            out.stdout.push_str(&buf.stdout);
            out.stderr.push_str(&buf.stderr);
            return Ok(());
        }
        // Phase 1 error is swallowed; phase 2 retries with full metadata.
    }

    // Phase 2: full metadata, re-parse query (so a bare `rspeek anyhow`
    // reclassifies from Unscoped to CrateOnly once deps are known), and
    // dispatch.
    let ws = resolve::Workspace::load().map_err(NotFound::from)?;
    let query = query::Query::parse(first, cli.second.as_deref(), &ws);
    dispatch(cli, &ws, &query, out)
}

/// Dispatch a parsed query to either the crate-overview or item-search path.
fn dispatch(
    cli: &Cli,
    ws: &resolve::Workspace,
    query: &query::Query,
    out: &mut Output,
) -> std::result::Result<(), NotFound> {
    if let query::Query::CrateOnly { crate_name } = query {
        run_crate_overview(cli, ws, crate_name, out)
    } else {
        run_item_search(cli, ws, query, out)
    }
}

fn run_crate_overview(
    cli: &Cli,
    ws: &resolve::Workspace,
    crate_name: &str,
    out: &mut Output,
) -> std::result::Result<(), NotFound> {
    let filtered = ws.filter(crate_name);
    if filtered.is_empty() {
        let crate_names: Vec<String> = ws.crates.iter().map(|c| c.name.clone()).collect();
        return Err(not_found(
            format!("crate `{crate_name}` not found"),
            crate_name,
            &crate_names,
        ));
    }
    if cli.json {
        let mut overviews = Vec::new();
        for c in filtered {
            let pub_only = !c.is_workspace_member;
            let items =
                index::list_items(&c.source_dirs, pub_only, c.out_dir.as_deref()).map_err(|e| {
                    NotFound {
                        message: e.to_string(),
                        suggestions: vec![],
                    }
                })?;
            overviews.push(output::JsonCrateOverview {
                crate_name: c.name.clone(),
                crate_version: c.version.clone(),
                src_dir: c
                    .source_dirs
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                items: items
                    .into_iter()
                    .map(|i| output::JsonItem {
                        name: i.name,
                        kind: i.kind,
                        module_path: i.module_path,
                    })
                    .collect(),
                deps: c.deps.clone(),
                available_workspace_members: ws.available_members(&c.name, &c.deps),
            });
        }
        out.println(&serde_json::to_string(&overviews).unwrap());
    } else {
        for c in filtered {
            out.println(&format!("## `{}` v{}", c.name, c.version));
            out.println(&format!(
                "**Source:** `{}`\n",
                c.source_dirs
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            if !c.deps.is_empty() {
                out.println(&format!("**Dependencies:** {}\n", c.deps.join(", ")));
            }
            let available = ws.available_members(&c.name, &c.deps);
            if !available.is_empty() {
                out.println(&format!(
                    "**Workspace members (not yet deps):** {}\n",
                    available.join(", ")
                ));
            }
            let pub_only = !c.is_workspace_member;
            let items =
                index::list_items(&c.source_dirs, pub_only, c.out_dir.as_deref()).map_err(|e| {
                    NotFound {
                        message: e.to_string(),
                        suggestions: vec![],
                    }
                })?;
            if items.is_empty() {
                out.println("No items found.\n");
            } else {
                let label = if pub_only { "Public items" } else { "Items" };
                out.println(&format!("{label}:\n"));
                for item in &items {
                    let mod_display = if item.module_path.is_empty() {
                        String::new()
                    } else {
                        format!("{}::", item.module_path)
                    };
                    out.println(&format!("- {} `{}{}`", item.kind, mod_display, item.name));
                }
                out.println(&format!("\nUsage: `rspeek {} <Item>`", c.name));
            }
        }
    }
    Ok(())
}

/// When the same item appears in multiple versions of the same crate, keep only
/// the newest version. Returns the deduped matches and a parallel vec of other
/// versions that were collapsed into each kept entry.
fn dedup_versions(
    matches: Vec<(&ResolvedCrate, IndexEntry)>,
) -> (Vec<(&ResolvedCrate, IndexEntry)>, Vec<Vec<String>>) {
    use cargo_metadata::semver::Version;
    use std::collections::HashMap;

    // Group indices by (crate_name, item_name, module_path).
    let mut groups: HashMap<(&str, &str, &str), Vec<usize>> = HashMap::new();
    for (i, (c, entry)) in matches.iter().enumerate() {
        groups
            .entry((&c.name, &entry.name, &entry.module_path))
            .or_default()
            .push(i);
    }

    let mut skip = HashSet::new();
    // Map from kept index -> list of other version strings.
    let mut others: HashMap<usize, Vec<String>> = HashMap::new();
    for indices in groups.values() {
        if indices.len() <= 1 {
            continue;
        }
        let best = *indices
            .iter()
            .max_by(|&&a, &&b| {
                let va = Version::parse(&matches[a].0.version).ok();
                let vb = Version::parse(&matches[b].0.version).ok();
                va.cmp(&vb)
            })
            .unwrap();
        let mut vers: Vec<String> = indices
            .iter()
            .filter(|&&i| i != best)
            .map(|&i| matches[i].0.version.clone())
            .collect();
        vers.sort_by(|a, b| {
            Version::parse(a)
                .unwrap_or(Version::new(0, 0, 0))
                .cmp(&Version::parse(b).unwrap_or(Version::new(0, 0, 0)))
        });
        others.insert(best, vers);
        for &i in indices {
            if i != best {
                skip.insert(i);
            }
        }
    }

    let mut deduped = Vec::new();
    let mut other_versions = Vec::new();
    for (i, m) in matches.into_iter().enumerate() {
        if skip.contains(&i) {
            continue;
        }
        other_versions.push(others.remove(&i).unwrap_or_default());
        deduped.push(m);
    }
    (deduped, other_versions)
}

/// Check if a qualified query's first module segment matches a `pub use <crate>`
/// re-export in any of the search crates, and if so, build a redirected query
/// targeting that crate with the remaining path segments.
fn cross_crate_redirect(
    query: &query::Query,
    search_crates: &[&ResolvedCrate],
    ws: &resolve::Workspace,
) -> Option<query::Query> {
    let query::Query::Qualified {
        module_segments,
        item,
        ..
    } = query
    else {
        return None;
    };
    let first_seg = module_segments.first()?;
    for c in search_crates {
        let Some(src_dir) = c.source_dirs.first() else {
            continue;
        };
        let reexports = reexport::cross_crate_reexports(src_dir, c.out_dir.as_deref());
        for (crate_name, alias) in &reexports {
            let visible = alias.as_deref().unwrap_or(crate_name);
            if resolve::normalize(visible) != resolve::normalize(first_seg) {
                continue;
            }
            if ws.filter(crate_name).is_empty() {
                continue;
            }
            return Some(query::Query::Qualified {
                crate_name: crate_name.clone(),
                module_segments: module_segments[1..].to_vec(),
                item: item.clone(),
            });
        }
    }
    None
}

/// Run `index_crate` in parallel across the given crates, filtering entries by
/// the query's module path, and collecting the results. If the text-filtered
/// pass finds nothing across ALL crates, falls back to full-parse (global
/// fallback) to catch items hidden inside macro bodies.
fn search_crates_for<'a>(
    crates: &[&'a ResolvedCrate],
    item_name: &str,
    query: &query::Query,
) -> std::result::Result<Vec<(&'a ResolvedCrate, IndexEntry)>, NotFound> {
    let do_pass = |use_text_filter: bool| -> std::result::Result<Vec<_>, NotFound> {
        let results: Vec<std::result::Result<Vec<(&ResolvedCrate, IndexEntry)>, NotFound>> = crates
            .par_iter()
            .map(|c| {
                let entries = index::index_crate(
                    &c.source_dirs,
                    item_name,
                    !c.is_workspace_member,
                    c.out_dir.as_deref(),
                    use_text_filter,
                )
                .map_err(NotFound::from)?;
                Ok(entries
                    .into_iter()
                    .filter(|entry| query.matches_module_path(&entry.module_path))
                    .map(|entry| (*c, entry))
                    .collect())
            })
            .collect();
        let mut matches = Vec::new();
        for r in results {
            matches.extend(r?);
        }
        Ok(matches)
    };

    let matches = do_pass(true)?;
    // Global fallback: if the text-filtered pass found nothing, retry with
    // full parse to catch items hidden in macro bodies.
    if matches.is_empty() {
        do_pass(false)
    } else {
        Ok(matches)
    }
}

fn run_item_search(
    cli: &Cli,
    ws: &resolve::Workspace,
    query: &query::Query,
    out: &mut Output,
) -> std::result::Result<(), NotFound> {
    let item_name = query.item_name().unwrap();
    let search_crates: Vec<&ResolvedCrate> = match query.crate_filter() {
        Some(name) => {
            let mut filtered = ws.filter(name);
            if filtered.is_empty() {
                let crate_names: Vec<String> = ws.crates.iter().map(|c| c.name.clone()).collect();
                return Err(not_found(
                    format!("crate `{name}` not found"),
                    name,
                    &crate_names,
                ));
            }
            if let Some(ver) = &cli.crate_version {
                let ver = ver.strip_prefix('v').unwrap_or(ver);
                filtered.retain(|c| c.version == *ver);
                if filtered.is_empty() {
                    let versions: Vec<String> =
                        ws.filter(name).iter().map(|c| c.version.clone()).collect();
                    return Err(NotFound {
                        message: format!(
                            "version `{ver}` not found for crate `{name}`. Available: {}",
                            versions.join(", ")
                        ),
                        suggestions: versions,
                    });
                }
            }
            filtered
        }
        None => ws.crates.iter().collect(),
    };

    // Workspace-first strategy for bare-name queries: search workspace members
    // first and only fall through to dependency crates if no match is found.
    // This skips the expensive dep scan for local symbols (the common case
    // when an LLM is inspecting user code).
    let (initial, fallback): (Vec<&ResolvedCrate>, Vec<&ResolvedCrate>) =
        if query.crate_filter().is_none() {
            search_crates
                .iter()
                .copied()
                .partition(|c| c.is_workspace_member)
        } else {
            (search_crates.clone(), Vec::new())
        };

    let mut matches = search_crates_for(&initial, item_name, query)?;
    if matches.is_empty() && !fallback.is_empty() {
        matches = search_crates_for(&fallback, item_name, query)?;
    }

    // Deduplicate entries pointing to the same source location, keeping the
    // shortest module path (i.e. the re-exported, user-facing path).
    matches.sort_by_key(|(_, a)| a.module_path.len());
    let mut seen = HashSet::new();
    matches.retain(|(_, e)| seen.insert((e.file.clone(), e.start_line, e.end_line)));

    // When the same item exists in multiple versions of a crate, keep the newest.
    let (matches, other_versions) = dedup_versions(matches);

    if matches.is_empty() {
        // Cross-crate re-export fallback: if this is a qualified query like
        // `wrapper::ttrpc::context::with_timeout`, check if the first module
        // segment matches a `pub use <crate>` in the target crate and retry
        // the search in that crate with the remaining path.
        if let Some(redirected) = cross_crate_redirect(query, &search_crates, ws) {
            return run_item_search(cli, ws, &redirected, out);
        }

        let all_names: Vec<String> = search_crates
            .iter()
            .flat_map(|c| {
                // Fast path: use cached name index for registry (immutable) crates.
                if !c.is_workspace_member {
                    if let Some(dir) = c.source_dirs.first() {
                        if crate::cache::is_registry_path(dir) {
                            if let Some(names) = crate::cache::read_name_index(dir) {
                                return names;
                            }
                        }
                    }
                }
                index::list_items(&c.source_dirs, !c.is_workspace_member, c.out_dir.as_deref())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| e.name)
                    .collect()
            })
            .collect();
        return Err(not_found(
            format!("no item `{item_name}` found in dependencies"),
            item_name,
            &all_names,
        ));
    }

    if cli.json {
        let json_entries: Vec<output::JsonEntry> = matches
            .iter()
            .enumerate()
            .map(|(i, (c, entry))| {
                let impls = if cli.impls {
                    Some(
                        index::find_impls(&c.source_dirs, &entry.name, c.out_dir.as_deref())
                            .unwrap_or_default(),
                    )
                } else {
                    None
                };
                output::to_json_entry(
                    entry,
                    c,
                    impls.as_deref(),
                    cli.signature,
                    other_versions[i].clone(),
                )
            })
            .collect::<Result<_>>()
            .map_err(NotFound::from)?;
        out.println(&serde_json::to_string(&json_entries).unwrap());
    } else if matches.len() == 1 {
        let (c, entry) = &matches[0];
        let impls = if cli.impls {
            Some(
                index::find_impls(&c.source_dirs, &entry.name, c.out_dir.as_deref())
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        out.println(
            &output::format_entry(
                entry,
                c,
                impls.as_deref(),
                cli.signature,
                &other_versions[0],
            )
            .map_err(NotFound::from)?,
        );
    } else {
        out.println(&format!(
            "Found {} matches for `{}`:\n",
            matches.len(),
            item_name
        ));
        for (i, (c, entry)) in matches.iter().enumerate() {
            let mod_display = if entry.module_path.is_empty() {
                String::new()
            } else {
                format!("::{}", entry.module_path)
            };
            let ver_note = if other_versions[i].is_empty() {
                String::new()
            } else {
                format!(
                    " (also in {})",
                    other_versions[i]
                        .iter()
                        .map(|v| format!("v{v}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            out.println(&format!(
                "- {} `{}{}::{}` v{} at {}:{}{}{}",
                entry.kind,
                c.name,
                mod_display,
                entry.name,
                c.version,
                entry.file.display(),
                entry.start_line,
                ver_note,
                if entry.cfg.is_empty() {
                    String::new()
                } else {
                    format!(" [cfg: {}]", entry.cfg.join(", "))
                },
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Simulate the cross-crate re-export scenario:
    /// - "wrapper" crate has `pub use inner_crate;`
    /// - "inner_crate" crate has `pub fn target_fn() {}`  in module `ctx`
    /// - Query: `wrapper::inner_crate::ctx::target_fn` should find the fn
    #[test]
    fn qualified_lookup_follows_cross_crate_reexport() {
        let dir = tempfile::tempdir().unwrap();

        // Create "inner_crate" source
        let inner_src = dir.path().join("inner_crate_src");
        fs::create_dir_all(&inner_src).unwrap();
        fs::write(inner_src.join("lib.rs"), "pub mod ctx;").unwrap();
        fs::write(inner_src.join("ctx.rs"), "pub fn target_fn() {}").unwrap();

        // Create "wrapper" crate source with `pub use inner_crate;`
        let wrapper_src = dir.path().join("wrapper_src");
        fs::create_dir_all(&wrapper_src).unwrap();
        fs::write(wrapper_src.join("lib.rs"), "pub use inner_crate;").unwrap();

        let ws = resolve::Workspace {
            crates: vec![
                resolve::ResolvedCrate {
                    name: "wrapper".to_string(),
                    version: "0.1.0".to_string(),
                    source_dirs: vec![wrapper_src],
                    is_workspace_member: false,
                    out_dir: None,
                    deps: vec!["inner_crate".to_string()],
                },
                resolve::ResolvedCrate {
                    name: "inner_crate".to_string(),
                    version: "0.1.0".to_string(),
                    source_dirs: vec![inner_src],
                    is_workspace_member: false,
                    out_dir: None,
                    deps: vec![],
                },
            ],
        };

        let query = query::Query::Qualified {
            crate_name: "wrapper".to_string(),
            module_segments: vec!["inner_crate".to_string(), "ctx".to_string()],
            item: "target_fn".to_string(),
        };

        let cli = Cli::parse_from(["rspeek", "wrapper::inner_crate::ctx::target_fn"]);
        let mut out = Output::default();
        let result = run_item_search(&cli, &ws, &query, &mut out);
        assert!(
            result.is_ok(),
            "expected match via cross-crate re-export, got: {:?}",
            result.err().map(|e| e.message)
        );
        assert!(
            out.stdout.contains("target_fn"),
            "output should contain target_fn: {}",
            out.stdout
        );
    }

    /// When the re-exported crate name uses hyphens but the `pub use` uses underscores,
    /// the lookup should still work (crate names normalize `-` to `_`).
    #[test]
    fn cross_crate_reexport_normalizes_hyphens() {
        let dir = tempfile::tempdir().unwrap();

        let inner_src = dir.path().join("inner_src");
        fs::create_dir_all(&inner_src).unwrap();
        fs::write(inner_src.join("lib.rs"), "pub fn my_func() {}").unwrap();

        let wrapper_src = dir.path().join("wrapper_src");
        fs::create_dir_all(&wrapper_src).unwrap();
        // `pub use my_crate;` — the actual crate is named "my-crate" with a hyphen
        fs::write(wrapper_src.join("lib.rs"), "pub use my_crate;").unwrap();

        let ws = resolve::Workspace {
            crates: vec![
                resolve::ResolvedCrate {
                    name: "wrapper".to_string(),
                    version: "0.1.0".to_string(),
                    source_dirs: vec![wrapper_src],
                    is_workspace_member: false,
                    out_dir: None,
                    deps: vec!["my-crate".to_string()],
                },
                resolve::ResolvedCrate {
                    name: "my-crate".to_string(),
                    version: "0.1.0".to_string(),
                    source_dirs: vec![inner_src],
                    is_workspace_member: false,
                    out_dir: None,
                    deps: vec![],
                },
            ],
        };

        let query = query::Query::Qualified {
            crate_name: "wrapper".to_string(),
            module_segments: vec!["my_crate".to_string()],
            item: "my_func".to_string(),
        };

        let cli = Cli::parse_from(["rspeek", "wrapper::my_crate::my_func"]);
        let mut out = Output::default();
        let result = run_item_search(&cli, &ws, &query, &mut out);
        assert!(
            result.is_ok(),
            "expected match via cross-crate re-export with hyphen normalization, got: {:?}",
            result.err().map(|e| e.message)
        );
        assert!(
            out.stdout.contains("my_func"),
            "output should contain my_func: {}",
            out.stdout
        );
    }

    /// `pub use real_crate as aliased;` — querying via the alias name should redirect
    /// to the real crate.
    #[test]
    fn cross_crate_reexport_follows_alias() {
        let dir = tempfile::tempdir().unwrap();

        let inner_src = dir.path().join("inner_src");
        fs::create_dir_all(&inner_src).unwrap();
        fs::write(inner_src.join("lib.rs"), "pub fn aliased_fn() {}").unwrap();

        let wrapper_src = dir.path().join("wrapper_src");
        fs::create_dir_all(&wrapper_src).unwrap();
        fs::write(wrapper_src.join("lib.rs"), "pub use real_crate as nice;").unwrap();

        let ws = resolve::Workspace {
            crates: vec![
                resolve::ResolvedCrate {
                    name: "wrapper".to_string(),
                    version: "0.1.0".to_string(),
                    source_dirs: vec![wrapper_src],
                    is_workspace_member: false,
                    out_dir: None,
                    deps: vec!["real_crate".to_string()],
                },
                resolve::ResolvedCrate {
                    name: "real_crate".to_string(),
                    version: "0.1.0".to_string(),
                    source_dirs: vec![inner_src],
                    is_workspace_member: false,
                    out_dir: None,
                    deps: vec![],
                },
            ],
        };

        let query = query::Query::Qualified {
            crate_name: "wrapper".to_string(),
            module_segments: vec!["nice".to_string()],
            item: "aliased_fn".to_string(),
        };

        let cli = Cli::parse_from(["rspeek", "wrapper::nice::aliased_fn"]);
        let mut out = Output::default();
        let result = run_item_search(&cli, &ws, &query, &mut out);
        assert!(
            result.is_ok(),
            "expected match via aliased re-export, got: {:?}",
            result.err().map(|e| e.message)
        );
        assert!(
            out.stdout.contains("aliased_fn"),
            "output should contain aliased_fn: {}",
            out.stdout
        );
    }

    /// For a bare-name query (no crate filter), a symbol in a workspace member
    /// should be returned even when a dep also defines the same symbol, and the
    /// dep version should not appear in the output.
    #[test]
    fn bare_name_query_prefers_workspace_over_dep() {
        let dir = tempfile::tempdir().unwrap();

        // Workspace member defines `Target` as a struct.
        let member_src = dir.path().join("member_src");
        fs::create_dir_all(&member_src).unwrap();
        fs::write(member_src.join("lib.rs"), "pub struct Target;").unwrap();

        // Dep also defines `Target` as a struct.
        let dep_src = dir.path().join("dep_src");
        fs::create_dir_all(&dep_src).unwrap();
        fs::write(dep_src.join("lib.rs"), "pub struct Target;").unwrap();

        let ws = resolve::Workspace {
            crates: vec![
                resolve::ResolvedCrate {
                    name: "my_member".to_string(),
                    version: "0.1.0".to_string(),
                    source_dirs: vec![member_src.clone()],
                    is_workspace_member: true,
                    out_dir: None,
                    deps: vec![],
                },
                resolve::ResolvedCrate {
                    name: "some_dep".to_string(),
                    version: "1.0.0".to_string(),
                    source_dirs: vec![dep_src.clone()],
                    is_workspace_member: false,
                    out_dir: None,
                    deps: vec![],
                },
            ],
        };

        let query = query::Query::parse("Target", None, &ws);
        let cli = Cli::parse_from(["rspeek", "Target"]);
        let mut out = Output::default();
        let result = run_item_search(&cli, &ws, &query, &mut out);
        assert!(
            result.is_ok(),
            "expected match, got: {:?}",
            result.err().map(|e| e.message)
        );
        // The workspace version must be present and the dep version must NOT appear.
        assert!(
            out.stdout.contains("my_member"),
            "workspace match should appear in output: {}",
            out.stdout
        );
        assert!(
            !out.stdout.contains("some_dep"),
            "dep match should NOT appear when workspace match exists: {}",
            out.stdout
        );
    }

    /// For a bare-name query, if no workspace member defines the symbol, the
    /// search should fall through to deps and find the symbol there.
    #[test]
    fn bare_name_query_falls_back_to_dep_when_not_in_workspace() {
        let dir = tempfile::tempdir().unwrap();

        // Workspace member has unrelated content.
        let member_src = dir.path().join("member_src");
        fs::create_dir_all(&member_src).unwrap();
        fs::write(member_src.join("lib.rs"), "pub struct SomethingElse;").unwrap();

        // Only the dep defines `DepOnly`.
        let dep_src = dir.path().join("dep_src");
        fs::create_dir_all(&dep_src).unwrap();
        fs::write(dep_src.join("lib.rs"), "pub struct DepOnly;").unwrap();

        let ws = resolve::Workspace {
            crates: vec![
                resolve::ResolvedCrate {
                    name: "my_member".to_string(),
                    version: "0.1.0".to_string(),
                    source_dirs: vec![member_src],
                    is_workspace_member: true,
                    out_dir: None,
                    deps: vec![],
                },
                resolve::ResolvedCrate {
                    name: "some_dep".to_string(),
                    version: "1.0.0".to_string(),
                    source_dirs: vec![dep_src],
                    is_workspace_member: false,
                    out_dir: None,
                    deps: vec![],
                },
            ],
        };

        let query = query::Query::parse("DepOnly", None, &ws);
        let cli = Cli::parse_from(["rspeek", "DepOnly"]);
        let mut out = Output::default();
        let result = run_item_search(&cli, &ws, &query, &mut out);
        assert!(
            result.is_ok(),
            "expected fallback to find dep match, got: {:?}",
            result.err().map(|e| e.message)
        );
        assert!(
            out.stdout.contains("DepOnly"),
            "output should contain DepOnly from dep: {}",
            out.stdout
        );
    }
}

mod index;
mod output;
mod query;
mod resolve;
mod suggest;

use anyhow::Result;
use clap::Parser;
use index::IndexEntry;
use rayon::prelude::*;
use resolve::ResolvedCrate;
use std::collections::HashSet;
use std::process;

/// Quickly inspect type definitions from Rust dependency source code.
#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    /// Crate name, fully qualified path, or item name
    first: Option<String>,
    /// Item name when first arg is a crate name
    second: Option<String>,
    /// Print extended usage examples for LLM tool integration
    #[arg(long)]
    llm_help: bool,
    /// Output as JSON
    #[arg(long)]
    json: bool,
    /// Include impl blocks for matched types
    #[arg(long)]
    impls: bool,
    /// Show only signatures (no function bodies)
    #[arg(long)]
    signature: bool,
}

const LLM_HELP: &str = r#"# rspeek — inspect type definitions from Rust dependency source code

## Usage

rspeek is designed to be called by LLMs to look up structs, enums, traits,
type aliases, and functions from Rust crates without building docs.

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
  rspeek --signature --impls anyhow Error  Signatures for type + impl methods

## Output

Single match: full markdown with doc comments, source path, line number,
crate version, and the original source in a fenced code block.

Multiple matches: summary list with kind, qualified path, and location.
Narrow the search with a crate name or qualified path.

## JSON output

--json returns an array of objects:
  [{"name", "kind", "module_path", "file", "start_line", "end_line",
    "crate_name", "crate_version", "source", "impls"?}]

Errors are also JSON: {"error": "...", "suggestions": ["..."]}

## Did you mean?

When no match is found, rspeek suggests similar names using Levenshtein
distance and prefix matching (like rustc). Suggestions appear in the
error message (plain text) or in the "suggestions" array (JSON).

## Scope

- Dependency crates: searches `src/`, public items only
- Workspace member crates: searches `src/` and `tests/`, all items (including tests)

## Limitations

- Only finds items defined as regular Rust syntax (struct, enum, trait, type, fn)
- Macro bodies are parsed for item definitions (e.g. syn's ast_struct!), but
  procedural macros and complex macro_rules! patterns are not expanded
- Re-exports: `pub use` within a crate are followed; glob re-exports and
  cross-crate re-exports are not
"#;

/// Error with optional suggestions for "did you mean?" messages.
struct NotFound {
    message: String,
    suggestions: Vec<String>,
}

fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(&cli) {
        if cli.json {
            let json = serde_json::json!({
                "error": e.message,
                "suggestions": e.suggestions,
            });
            println!("{json}");
        } else {
            eprint!("Error: {}", e.message);
            let hint = match e.suggestions.len() {
                0 => String::new(),
                1 => format!("\n\ndid you mean `{}`?", e.suggestions[0]),
                _ => {
                    let list: Vec<String> =
                        e.suggestions.iter().map(|s| format!("`{s}`")).collect();
                    format!("\n\ndid you mean one of {}?", list.join(", "))
                }
            };
            eprintln!("{hint}");
        }
        process::exit(1);
    }
}

fn not_found(message: impl Into<String>, query: &str, candidates: &[String]) -> NotFound {
    NotFound {
        message: message.into(),
        suggestions: suggest::suggestions(query, candidates.iter().map(|s| s.as_str()))
            .into_iter()
            .map(String::from)
            .collect(),
    }
}

fn run(cli: &Cli) -> std::result::Result<(), NotFound> {
    if cli.llm_help {
        print!("{LLM_HELP}");
        return Ok(());
    }

    let first = cli.first.as_deref().ok_or_else(|| NotFound {
        message: "missing argument: item name or crate name\n\nUsage: rspeek [OPTIONS] <FIRST> [SECOND]\n\nTry --help or --llm-help for usage".into(),
        suggestions: vec![],
    })?;

    let ws = resolve::Workspace::load().map_err(|e| NotFound {
        message: e.to_string(),
        suggestions: vec![],
    })?;
    let query = query::Query::parse(first, cli.second.as_deref(), &ws);

    // Handle crate-only query: show overview
    if let query::Query::CrateOnly { crate_name } = &query {
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
                let items = index::list_items(&c.source_dirs, pub_only, c.out_dir.as_deref())
                    .map_err(|e| NotFound {
                        message: e.to_string(),
                        suggestions: vec![],
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
                });
            }
            println!("{}", serde_json::to_string(&overviews).unwrap());
        } else {
            for c in filtered {
                println!("## `{}` v{}", c.name, c.version);
                println!(
                    "**Source:** `{}`\n",
                    c.source_dirs
                        .iter()
                        .map(|d| d.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let pub_only = !c.is_workspace_member;
                let items = index::list_items(&c.source_dirs, pub_only, c.out_dir.as_deref())
                    .map_err(|e| NotFound {
                        message: e.to_string(),
                        suggestions: vec![],
                    })?;
                if items.is_empty() {
                    println!("No items found.\n");
                } else {
                    let label = if pub_only { "Public items" } else { "Items" };
                    println!("{label}:\n");
                    for item in &items {
                        let mod_display = if item.module_path.is_empty() {
                            String::new()
                        } else {
                            format!("{}::", item.module_path)
                        };
                        println!("- {} `{}{}`", item.kind, mod_display, item.name);
                    }
                    println!("\nUsage: `rspeek {} <Item>`", c.name);
                }
            }
        }
        return Ok(());
    }

    let item_name = query.item_name().unwrap();
    let search_crates: Vec<&ResolvedCrate> = match query.crate_filter() {
        Some(name) => {
            let filtered = ws.filter(name);
            if filtered.is_empty() {
                let crate_names: Vec<String> = ws.crates.iter().map(|c| c.name.clone()).collect();
                return Err(not_found(
                    format!("crate `{name}` not found"),
                    name,
                    &crate_names,
                ));
            }
            filtered
        }
        None => ws.crates.iter().collect(),
    };

    let results: Vec<Result<Vec<(&ResolvedCrate, IndexEntry)>, _>> = search_crates
        .par_iter()
        .map(|c| {
            let entries = index::index_crate(&c.source_dirs, item_name, c.out_dir.as_deref())
                .map_err(|e| NotFound {
                    message: e.to_string(),
                    suggestions: vec![],
                })?;
            Ok(entries
                .into_iter()
                .filter(|entry| query.matches_module_path(&entry.module_path))
                .map(|entry| (*c, entry))
                .collect())
        })
        .collect();
    let mut matches: Vec<(&ResolvedCrate, IndexEntry)> = Vec::new();
    for r in results {
        matches.extend(r?);
    }

    // Deduplicate entries pointing to the same source location, keeping the
    // shortest module path (i.e. the re-exported, user-facing path).
    matches.sort_by_key(|(_, a)| a.module_path.len());
    let mut seen = HashSet::new();
    matches.retain(|(_, e)| seen.insert((e.file.clone(), e.start_line, e.end_line)));

    if matches.is_empty() {
        let all_names: Vec<String> = search_crates
            .iter()
            .flat_map(|c| {
                index::list_items(&c.source_dirs, !c.is_workspace_member, c.out_dir.as_deref())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| e.name)
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
            .map(|(c, entry)| {
                let impls = if cli.impls {
                    Some(
                        index::find_impls(&c.source_dirs, &entry.name, c.out_dir.as_deref())
                            .unwrap_or_default(),
                    )
                } else {
                    None
                };
                output::to_json_entry(entry, c, impls.as_deref(), cli.signature)
            })
            .collect::<Result<_>>()
            .map_err(|e| NotFound {
                message: e.to_string(),
                suggestions: vec![],
            })?;
        println!("{}", serde_json::to_string(&json_entries).unwrap());
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
        println!(
            "{}",
            output::format_entry(entry, c, impls.as_deref(), cli.signature).map_err(|e| {
                NotFound {
                    message: e.to_string(),
                    suggestions: vec![],
                }
            })?
        );
    } else {
        println!("Found {} matches for `{}`:\n", matches.len(), item_name);
        for (c, entry) in &matches {
            let mod_display = if entry.module_path.is_empty() {
                String::new()
            } else {
                format!("::{}", entry.module_path)
            };
            println!(
                "- {} `{}{}::{}` at {}:{}",
                entry.kind,
                c.name,
                mod_display,
                entry.name,
                entry.file.display(),
                entry.start_line,
            );
        }
    }

    Ok(())
}

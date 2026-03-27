# rspeek

Inspect type definitions from Rust crate source code without building docs.

Searches dependency crates and workspace member crates for structs, enums,
unions, traits, type aliases, constants, statics, and functions — including
integration tests in `tests/`.

## Installation

```bash
cargo install --path .
```

### Kiro CLI setup

Generate a steering file so the Kiro CLI knows how to use rspeek:

```bash
rspeek --llm-help > ~/.kiro/steering/rspeek.md
```

## Usage

Run from any directory containing a `Cargo.toml`.

```
rspeek <crate>                    List public items in a crate
rspeek <Item>                     Search all crates for an item
rspeek <crate> <Item>             Search within one crate
rspeek <crate>::<path>::<Item>    Match by module path
```

### Flags

- `--json` — structured JSON output (for LLMs and scripts)
- `--impls` — include impl blocks for matched types
- `--signature` — show only signatures (no function/method bodies)
- `--api` — shorthand for `--signature --impls` (type API at a glance)
- `--crate-version <VERSION>` — filter to a specific crate version (accepts `v` prefix)
- `--llm-help` — extended usage documentation for LLM tool integration

## Examples

```bash
# List public items in a crate
rspeek anyhow

# Find a type across all dependencies
rspeek Error

# Find a specific type in a crate
rspeek anyhow Error
rspeek anyhow::Error              # equivalent

# Match by module path
rspeek serde::de::Visitor
rspeek cargo_metadata::dependency::DependencyKind

# JSON output
rspeek --json anyhow Error

# Include impl blocks
rspeek --impls anyhow Error

# Signatures only (no function bodies)
rspeek --signature anyhow Error

# Signatures for type + impl methods
rspeek --signature --impls anyhow Error

# Pin a specific crate version
rspeek --crate-version 0.29.0 nix kill
```

## Output

Single match — full markdown with doc comments, source path, line number, crate version, and original source:

````
## `Error` (struct)
**Source:** `/home/user/.cargo/registry/src/.../anyhow-1.0.102/src/lib.rs:288`
**Crate:** `anyhow` v1.0.102

```rust
/// The `Error` type, a wrapper around a dynamic error type.
pub struct Error {
    inner: Own<ErrorImpl>,
}
```
````

Multiple matches — summary list with qualified paths and locations.
When the same item exists in multiple versions of a crate, only the newest
is shown with the other versions noted:

```
Found 7 matches for `Error`:

- struct `anyhow::Error` v1.0.102 at /home/user/.cargo/.../lib.rs:288
- enum `cargo_metadata::errors::Error` v0.23.1 at /home/user/.cargo/.../errors.rs:6
- struct `serde_json::error::Error` v1.0.149 at /home/user/.cargo/.../error.rs:15 (also in v1.0.140, v1.0.145)
```

Crate overview — source path, dependencies, and item listing:

```
## `serde_json` v1.0.149
**Source:** `/home/user/.cargo/registry/src/.../serde_json-1.0.149/src`

**Dependencies:** itoa, memchr, serde, serde_core, zmij

**Workspace members (not yet deps):** my-utils, my-core

Public items:

- struct `de::Deserializer`
- fn `de::from_reader`
- fn `de::from_slice`
- fn `de::from_str`
- struct `error::Error`
- enum `value::Value`

Usage: `rspeek serde_json <Item>`
```

For workspace members, all items are shown (not just public).
**Dependencies** and **Workspace members (not yet deps)** help you discover
what's already usable and what can be added with a one-line Cargo.toml edit.

### JSON output

`--json` returns an array of objects:

```json
[{
  "name": "Error",
  "kind": "struct",
  "module_path": "",
  "file": "/path/to/src/lib.rs",
  "start_line": 288,
  "end_line": 392,
  "crate_name": "anyhow",
  "crate_version": "1.0.102",
  "source": "pub struct Error { ... }",
  "impls": [{"trait_name": null, "file": "...", "start_line": 19, "end_line": 670, "source": "..."}],
  "other_versions": ["1.0.98", "1.0.100"]
}]
```

The `impls` field is only present when `--impls` is used.
The `other_versions` field is only present when older versions were collapsed.

Errors are also JSON when `--json` is set:

```json
{"error": "no item `Contxt` found in dependencies", "suggestions": ["Context"]}
```

### Did you mean?

When no match is found, rspeek suggests similar names using Levenshtein distance and prefix matching (like rustc):

```
Error: no item `Contxt` found in dependencies

did you mean `Context`?
```

## LLM Integration

Use `--llm-help` for extended usage documentation suitable for tool descriptions.

## Scope

- Dependency crates: searches `src/`, public items only
- Workspace member crates: searches `src/` and `tests/`, all items (including tests)

## Limitations

- Only finds items defined as regular Rust syntax (`struct`, `enum`, `union`, `trait`, `type`, `fn`, `const`, `static`)
- Macro bodies are parsed for item definitions (e.g., syn's `ast_struct!`), but procedural macros and complex `macro_rules!` patterns are not expanded
- Re-exports: `pub use` within a crate are followed; glob re-exports and cross-crate re-exports are not

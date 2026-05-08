//! Cfg-predicate extraction from attributes and macro_rules! definitions.

use std::collections::HashMap;

/// Mapping from macro name → cfg predicates extracted from `macro_rules!` definitions.
pub(crate) type MacroCfgMap = HashMap<String, Vec<String>>;

/// Extract `#[cfg(...)]` predicate strings from a list of attributes.
pub(crate) fn extract_cfg_attrs(attrs: &[syn::Attribute]) -> Vec<String> {
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
pub(crate) fn collect_macro_cfg_defs(file: &syn::File) -> MacroCfgMap {
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
pub(crate) fn extract_cfg_from_macro_body(
    tokens: &proc_macro2::TokenStream,
) -> Option<Vec<String>> {
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
            let raw = &substr[..end];
            let pred = raw.replace(',', ", ").replace('=', " = ");
            cfgs.push(pred);
        }
        rest = &rest[start + end..];
    }
    Some(cfgs)
}

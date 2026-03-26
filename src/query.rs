use crate::resolve::Workspace;

/// Parsed query representing what the user is looking for.
#[derive(Debug)]
pub enum Query {
    /// `rspeek <item>` — search all crates
    Unscoped { item: String },
    /// `rspeek <crate> <item>` — search within one crate
    Scoped { crate_name: String, item: String },
    /// `rspeek crate::path::Item` — match by module path
    Qualified {
        crate_name: String,
        module_segments: Vec<String>,
        item: String,
    },
    /// `rspeek <crate>` — bare crate name, show overview
    CrateOnly { crate_name: String },
}

impl Query {
    pub fn parse(first: &str, second: Option<&str>, ws: &Workspace) -> Self {
        if let Some(item) = second {
            return Query::Scoped {
                crate_name: first.to_string(),
                item: item.to_string(),
            };
        }

        if first.contains("::") {
            let parts: Vec<&str> = first.split("::").collect();
            return Query::Qualified {
                crate_name: parts[0].to_string(),
                module_segments: parts[1..parts.len() - 1]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                item: parts.last().unwrap().to_string(),
            };
        }

        if ws.has_crate(first) {
            return Query::CrateOnly {
                crate_name: first.to_string(),
            };
        }

        Query::Unscoped {
            item: first.to_string(),
        }
    }

    pub fn crate_filter(&self) -> Option<&str> {
        match self {
            Query::Scoped { crate_name, .. }
            | Query::Qualified { crate_name, .. }
            | Query::CrateOnly { crate_name, .. } => Some(crate_name),
            Query::Unscoped { .. } => None,
        }
    }

    pub fn item_name(&self) -> Option<&str> {
        match self {
            Query::Scoped { item, .. }
            | Query::Qualified { item, .. }
            | Query::Unscoped { item } => Some(item),
            Query::CrateOnly { .. } => None,
        }
    }

    pub fn matches_module_path(&self, entry_module_path: &str) -> bool {
        match self {
            Query::Qualified {
                module_segments, ..
            } if !module_segments.is_empty() => {
                let entry_parts: Vec<&str> = if entry_module_path.is_empty() {
                    vec![]
                } else {
                    entry_module_path.split("::").collect()
                };
                if module_segments.len() > entry_parts.len() {
                    return false;
                }
                let offset = entry_parts.len() - module_segments.len();
                module_segments
                    .iter()
                    .zip(&entry_parts[offset..])
                    .all(|(a, b)| a == b)
            }
            _ => true,
        }
    }
}

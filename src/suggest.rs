//! Suggest similar item names when an exact match fails.
//! Uses Levenshtein distance and prefix matching, similar to rustc.

/// Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let b_len = b.len();
    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0; b_len + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_len]
}

/// Find similar names from `candidates` for the given `query`.
/// Returns up to 5 suggestions, sorted by relevance (prefix matches first,
/// then by edit distance). Uses the same threshold as rustc: distance <= max(query.len(), 3) / 3.
pub fn suggestions<'a>(query: &str, candidates: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let query_lower = query.to_lowercase();
    let threshold = query.len().max(3) / 3;

    let mut scored: Vec<(&str, usize, bool)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for name in candidates {
        if !seen.insert(name) {
            continue;
        }
        let name_lower = name.to_lowercase();
        let is_prefix = name_lower.starts_with(&query_lower);
        let dist = levenshtein(&query_lower, &name_lower);

        if is_prefix || dist <= threshold {
            scored.push((name, dist, is_prefix));
        }
    }

    // Prefix matches first, then by distance, then alphabetically
    scored.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)).then(a.0.cmp(b.0)));
    scored
        .into_iter()
        .take(5)
        .map(|(name, _, _)| name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_prefix() {
        let candidates = vec!["Error", "ErrorKind", "Result"];
        let suggs = suggestions("Err", candidates.iter().copied());
        assert!(suggs.contains(&"Error"));
        assert!(suggs.contains(&"ErrorKind"));
    }

    #[test]
    fn typo() {
        let candidates = vec!["Context", "Chain", "Error", "Result"];
        let suggs = suggestions("Contxt", candidates.iter().copied());
        assert!(suggs.contains(&"Context"));
    }

    #[test]
    fn no_match() {
        let candidates = vec!["Error", "Result"];
        let suggs = suggestions("Zzzzzzzzz", candidates.iter().copied());
        assert!(suggs.is_empty());
    }

    #[test]
    fn case_insensitive() {
        let candidates = vec!["Error", "ErrorKind"];
        let suggs = suggestions("error", candidates.iter().copied());
        assert!(suggs.contains(&"Error"));
    }
}

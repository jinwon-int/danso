//! Wiki-maintained token/phrase aliases, ported from the bash
//! implementation's `alias_python_lib` (issue #164, consumed by the `find`
//! port in issue #121 slice 2).
//!
//! The alias page (`pages/aliases.md`) holds lines like
//! `representative: alias one, alias two`. Terms may be single tokens or
//! bounded phrases of up to eight latin/hangul tokens. A missing or malformed
//! page fails open to no expansion: the search must degrade, never die, on a
//! wiki content problem.

use crate::text;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

fn alias_term_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"^(?:[a-z0-9][a-z0-9_-]*|[가-힣]+)",
            r"(?:\s+(?:[a-z0-9][a-z0-9_-]*|[가-힣]+)){0,7}$"
        ))
        .expect("static regex")
    })
}

/// Whitespace-collapsed lowercase, the shared term normalizer.
pub fn normalize_alias_term(term: &str) -> String {
    term.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AliasTable {
    /// term → equivalents (the whole group minus the term itself).
    groups: BTreeMap<String, BTreeSet<String>>,
    /// term → the group's line representative(s).
    representatives: BTreeMap<String, BTreeSet<String>>,
}

impl AliasTable {
    /// `load_alias_groups()` — parse `representative: a, b` lines. Anything
    /// unreadable or malformed leaves an empty table (fail open).
    pub fn load(path: &Path) -> AliasTable {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return AliasTable::default();
        };
        AliasTable::parse(&raw)
    }

    pub fn parse(raw: &str) -> AliasTable {
        let mut table = AliasTable::default();
        for line in raw.lines() {
            let text = line.trim().trim_start_matches(['-', '*', '+', ' ']).trim();
            if text.is_empty() || text.starts_with('#') {
                continue;
            }
            for separator in [':', '='] {
                if !text.contains(separator) {
                    continue;
                }
                let (left, right) = match text.split_once(separator) {
                    Some((left, right)) => (left, right),
                    None => continue,
                };
                let mut normalized: Vec<String> = Vec::new();
                for raw_term in std::iter::once(left).chain(right.split(',')) {
                    let term = normalize_alias_term(raw_term);
                    if !term.is_empty()
                        && term.chars().count() <= 96
                        && alias_term_re().is_match(&term)
                        && !normalized.contains(&term)
                    {
                        normalized.push(term);
                    }
                }
                if normalized.len() >= 2 {
                    let representative = normalized[0].clone();
                    let group: BTreeSet<String> = normalized.iter().cloned().collect();
                    for term in normalized {
                        let equivalents: BTreeSet<String> = group
                            .difference(&BTreeSet::from([term.clone()]))
                            .cloned()
                            .collect();
                        table
                            .groups
                            .entry(term.clone())
                            .or_default()
                            .extend(equivalents);
                        table
                            .representatives
                            .entry(term)
                            .or_default()
                            .insert(representative.clone());
                    }
                }
                break;
            }
        }
        table
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// `alias_query_matches` — every single token of the query plus every
    /// multi-word alias phrase that occurs in it.
    pub fn query_matches(&self, query_text: &str) -> BTreeSet<String> {
        let normalized_query = normalize_alias_term(query_text);
        let mut matches = text::token_set(&normalized_query);
        for term in self.groups.keys() {
            if term.contains(' ') && normalized_query.contains(term.as_str()) {
                matches.insert(term.clone());
            }
        }
        matches
    }

    /// `expand_query` — append sorted, bounded (≤12) equivalents and return
    /// the expanded query with the added terms.
    pub fn expand(&self, query_text: &str, enabled: bool) -> (String, Vec<String>) {
        if !enabled || self.groups.is_empty() {
            return (query_text.to_string(), Vec::new());
        }
        let normalized_query = normalize_alias_term(query_text);
        let matched = self.query_matches(query_text);
        let mut extras: Vec<String> = Vec::new();
        for matched_term in &matched {
            for alias in self.groups.get(matched_term).into_iter().flatten() {
                if matched.contains(alias) || extras.contains(alias) {
                    continue;
                }
                if normalized_query.contains(alias.as_str()) {
                    continue;
                }
                extras.push(alias.clone());
            }
        }
        extras.truncate(12);
        if extras.is_empty() {
            return (query_text.to_string(), Vec::new());
        }
        (format!("{query_text} {}", extras.join(" ")), extras)
    }

    /// `resolve_alias_exact_terms` — aliases with exactly one representative
    /// resolve to that representative for the exact-match boost.
    pub fn resolve_exact_terms(&self, query_text: &str, enabled: bool) -> Vec<String> {
        if !enabled || self.groups.is_empty() {
            return Vec::new();
        }
        let mut resolved = BTreeSet::new();
        for matched in self.query_matches(query_text) {
            if let Some(representatives) = self.representatives.get(&matched)
                && representatives.len() == 1
            {
                resolved.extend(representatives.iter().cloned());
            }
        }
        resolved.into_iter().collect()
    }

    /// `alias_exact_boost` — general exact evidence for alias-introduced
    /// terms: path 0.09, heading 0.06, snippet 0.02 when any resolved term
    /// occurs. Returns `(path, heading, snippet)`.
    pub fn exact_boost(
        &self,
        exact_terms: &[String],
        path: &str,
        heading: &str,
        snippet: &str,
    ) -> (f64, f64, f64) {
        if exact_terms.is_empty() {
            return (0.0, 0.0, 0.0);
        }
        let term_present = |term: &str, haystack: &str| -> bool {
            let term = normalize_alias_term(term);
            let normalized = normalize_alias_term(haystack);
            if term.contains(' ') {
                normalized.contains(&term)
            } else {
                text::token_set(&normalized).contains(&term)
            }
        };
        let any = |haystack: &str| exact_terms.iter().any(|term| term_present(term, haystack));
        (
            if any(path) { 0.09 } else { 0.0 },
            if any(heading) { 0.06 } else { 0.0 },
            if any(snippet) { 0.02 } else { 0.0 },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "\
# [DOC-950] 별칭

요약 줄은 무시됩니다.

## [AL-01] 노드
- vps6: vps-six, 여섯번째
- daegyo: 대교
- broken: 여섯번째vps, 순욱
멀티: alpha, beta, gamma
=basic: termux, 안드로이드
";

    #[test]
    fn parses_representative_lines_with_both_separators() {
        let table = AliasTable::parse(PAGE);
        assert!(table.groups.contains_key("vps6"));
        assert!(table.groups["vps6"].contains("여섯번째"));
        assert!(
            table.groups["termux"].contains("안드로이드"),
            "= prefix stripped"
        );
        assert_eq!(
            table.representatives["daegyo"],
            BTreeSet::from(["daegyo".to_string()])
        );
        // A mixed hangul+latin token fails the term rule exactly like the
        // reference `ALIAS_TERM_RE.fullmatch` — dropped, line still parsed.
        assert!(!table.groups["broken"].contains("여섯번째vps"));
        assert!(table.groups["broken"].contains("순욱"));
        // Comments, headings and prose lines without separators contribute.
        assert!(!table.groups.contains_key("요약"));
    }

    #[test]
    fn expands_bounded_and_skips_known_words() {
        let table = AliasTable::parse(PAGE);
        let (expanded, extras) = table.expand("vps6 문제", true);
        assert_eq!(expanded, "vps6 문제 vps-six 여섯번째");
        assert_eq!(extras, vec!["vps-six".to_string(), "여섯번째".to_string()]);
        // Every equivalent already present in the query → nothing to add
        // (bash skips aliases the query already carries).
        let (unchanged, none) = table.expand("vps6 vps-six 여섯번째", true);
        assert_eq!(unchanged, "vps6 vps-six 여섯번째");
        assert!(none.is_empty());
        let (disabled, none) = table.expand("vps6", false);
        assert_eq!(disabled, "vps6");
        assert!(none.is_empty());
    }

    #[test]
    fn resolves_only_unambiguous_representatives() {
        let table = AliasTable::parse(PAGE);
        assert_eq!(table.resolve_exact_terms("daegyo", true), vec!["daegyo"]);
        assert!(table.resolve_exact_terms("없는토큰", true).is_empty());
    }

    #[test]
    fn exact_boost_scores_path_over_heading_over_snippet() {
        let table = AliasTable::parse(PAGE);
        // Resolving the alias 대교 yields the line representative itself
        // (`resolve_alias_exact_terms`); the boost checks that representative
        // against path / heading / snippet.
        let terms = table.resolve_exact_terms("daegyo", true);
        assert_eq!(terms, vec!["daegyo".to_string()]);
        let (p, h, s) = table.exact_boost(&terms, "pages/nodes/daegyo.md", "daegyo 노드", "메모");
        assert_eq!((p, h, s), (0.09, 0.06, 0.0));
        let (p, h, s) = table.exact_boost(&[], "pages/nodes/daegyo.md", "대교", "대교");
        assert_eq!((p, h, s), (0.0, 0.0, 0.0));
    }

    #[test]
    fn missing_page_fails_open() {
        let table = AliasTable::load(Path::new("/nonexistent/aliases.md"));
        assert!(table.is_empty());
        let (expanded, extras) = table.expand("vps6", true);
        assert_eq!(expanded, "vps6");
        assert!(extras.is_empty());
    }
}

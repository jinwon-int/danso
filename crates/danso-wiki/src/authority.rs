//! Document authority signals and ranking, ported from the bash
//! implementation's `authority_python_lib` (issue #164, consumed by the
//! `find` port in issue #121 slice 2).
//!
//! The classifier runs on the path plus the *first indexed chunk's* lead
//! (heading + snippet): forwarding markers, `raw/`·`archive/` path shapes,
//! `status:` declarations and canonical-source claims place a document in one
//! of seven tiers. Only policy-intent queries (`운영규칙`, `정책`, "policy…")
//! receive the tier multipliers; everything else keeps 1.0.

use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

pub const AUTHORITY_SCHEMA: &str = "document-authority-v1";

const POLICY_TERMS: &[&str] = &[
    "운영규칙",
    "운영 규칙",
    "정책",
    "정본",
    "헌장",
    "source of truth",
    "canonical source",
];

const HISTORICAL_STATUSES: &[&str] = &[
    "retired",
    "historical",
    "superseded",
    "archived",
    "inactive",
];

/// Tier → multiplier. Applied only when the query carries authority intent.
pub fn multiplier_for(tier: &str) -> f64 {
    match tier {
        "canonical" => 1.30,
        "active" => 1.05,
        "standard" => 1.00,
        "raw" => 0.80,
        "archive" => 0.78,
        "historical" => 0.72,
        "forwarding" => 0.55,
        _ => 1.0,
    }
}

/// `authority_score_multiplier` — the on/off and intent gates.
pub fn score_multiplier(authority: &Authority, query_has_intent: bool, enabled: bool) -> f64 {
    if !enabled || !query_has_intent {
        return 1.0;
    }
    multiplier_for(&authority.tier)
}

/// `" ".join(query.lower().split())` with substring policy terms or the
/// english policy-word regex.
pub fn query_intent(query_text: &str) -> bool {
    let normalized: String = query_text
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if POLICY_TERMS.iter().any(|term| normalized.contains(term)) {
        return true;
    }
    static POLICY_WORDS: OnceLock<Regex> = OnceLock::new();
    POLICY_WORDS
        .get_or_init(|| {
            Regex::new(r"(?i)\b(?:policy|policies|rule|rules)\b").expect("static regex")
        })
        .is_match(&normalized)
}

/// The authority record carried on every result's `explain`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Authority {
    pub schema: &'static str,
    pub tier: String,
    pub canonical: bool,
    pub forwarding: bool,
    pub raw: bool,
    pub archive: bool,
    pub historical: bool,
    pub status: String,
    pub verified: Option<String>,
    pub canonical_target: Option<String>,
    pub signals: Vec<String>,
}

impl Authority {
    /// `classify_document_authority(path, record)` — `heading`/`snippet` are
    /// the first chunk's lead fields (records are emitted in path/line order,
    /// so callers cache per path and reuse for every later section).
    pub fn classify(path: &str, heading: &str, snippet: &str) -> Authority {
        let path_l = path.to_lowercase();
        let lead = format!("{heading} {snippet}");
        let lead_l: String = lead
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        static FORWARDING: OnceLock<Regex> = OnceLock::new();
        let forwarding = FORWARDING
            .get_or_init(|| {
                Regex::new(concat!(
                    r"(?i)(?:\bforwarding stub\b|\bcanonical path\s*:",
                    r"|^#?\s*[^#]{0,160}\bmoved\b)"
                ))
                .expect("static regex")
            })
            .is_match(&lead_l);

        let raw = path_l.starts_with("raw/") || path_l.contains("/raw/");
        static ARCHIVE_PATH: OnceLock<Regex> = OnceLock::new();
        let archive = path_l.starts_with("archive/")
            || path_l.starts_with("pages/archive/")
            || path_l.contains("/archive/")
            || ARCHIVE_PATH
                .get_or_init(|| {
                    Regex::new(r"(?:^|/)log-archive-|(?:^|/)[^/]*archive[^/]*\.md$")
                        .expect("static regex")
                })
                .is_match(&path_l);

        static STATUS: OnceLock<Regex> = OnceLock::new();
        let status = STATUS
            .get_or_init(|| {
                Regex::new(concat!(
                    r"(?i)\bstatus\b\s*(?:\*\*)?\s*[:|]\s*[`*_]*",
                    r"(active|retired|historical|superseded|archived|inactive)\b"
                ))
                .expect("static regex")
            })
            .captures(&lead_l)
            .map(|c| c[1].to_lowercase())
            .unwrap_or_else(|| "unknown".to_string());
        let mut status = status;
        if status == "unknown" {
            static HISTORICAL: OnceLock<Regex> = OnceLock::new();
            if HISTORICAL
                .get_or_init(|| {
                    Regex::new(r"(?i)\bretired runtime\b|\bhistorical\b|역사 문서|폐기|퇴역")
                        .expect("static regex")
                })
                .is_match(&lead_l)
            {
                status = "historical".to_string();
            }
        }

        static VERIFIED: OnceLock<Regex> = OnceLock::new();
        let verified = VERIFIED
            .get_or_init(|| {
                Regex::new(concat!(
                    r"(?i)\bverified\b\s*(?:\*\*)?\s*[:|]\s*[`*_]*",
                    r"(\d{4}-\d{2}-\d{2})(?:\s+KST)?"
                ))
                .expect("static regex")
            })
            .captures(&lead_l)
            .map(|c| c[1].to_string())
            .unwrap_or_default();

        // The canonical target is matched against the *original-case* lead.
        static CANONICAL_TARGET: OnceLock<Regex> = OnceLock::new();
        let canonical_target = CANONICAL_TARGET
            .get_or_init(|| {
                Regex::new(concat!(
                    r"(?i)\bcanonical path\s*:\s*.*?",
                    r"((?:pages|raw)/[a-z0-9_./-]+\.md)"
                ))
                .expect("static regex")
            })
            .captures(&lead)
            .map(|c| c[1].to_string())
            .unwrap_or_default();

        let mut canonical = false;
        if !forwarding {
            static CANONICAL_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
            let patterns = CANONICAL_PATTERNS.get_or_init(|| {
                [
                    concat!(
                        r"(?i)(?:이\s*문서|this\s+(?:page|document))[^.!?]{0,140}",
                        r"(?:최상위\s+운영\s+헌장|운영\s+정본|official\s+source\s+of\s+truth|canonical\s+(?:source|document))"
                    ),
                    r"(?i)최상위\s+운영\s+헌장",
                    r"(?i)\bstatus\b\s*(?:\*\*)?\s*[:|]\s*[`*_]*canonical\b",
                ]
                .iter()
                .map(|p| Regex::new(p).expect("static regex"))
                .collect()
            });
            canonical = patterns.iter().any(|re| re.is_match(&lead_l));
        }

        let historical = HISTORICAL_STATUSES.contains(&status.as_str());
        let mut signals = Vec::new();
        if canonical {
            signals.push("declares-canonical".to_string());
        }
        if forwarding {
            signals.push("forwarding-stub".to_string());
        }
        if raw {
            signals.push("path:raw".to_string());
        }
        if archive {
            signals.push("path:archive".to_string());
        }
        if status != "unknown" {
            signals.push(format!("status:{status}"));
        }
        if !verified.is_empty() {
            signals.push("verified".to_string());
        }
        if !canonical_target.is_empty() {
            signals.push("canonical-target".to_string());
        }

        let tier = if forwarding {
            "forwarding"
        } else if historical {
            "historical"
        } else if archive {
            "archive"
        } else if raw {
            "raw"
        } else if canonical {
            "canonical"
        } else if status == "active" {
            "active"
        } else {
            "standard"
        }
        .to_string();

        Authority {
            schema: AUTHORITY_SCHEMA,
            tier,
            canonical,
            forwarding,
            raw,
            archive,
            historical,
            status,
            verified: if verified.is_empty() {
                None
            } else {
                Some(verified)
            },
            canonical_target: if canonical_target.is_empty() {
                None
            } else {
                Some(canonical_target)
            },
            signals,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_precedence_follows_the_reference_order() {
        let forwarding = Authority::classify(
            "archive/old.md",
            "Moved",
            "forwarding stub. canonical path: pages/current.md",
        );
        assert_eq!(forwarding.tier, "forwarding");
        assert!(forwarding.signals.contains(&"forwarding-stub".to_string()));

        let historical = Authority::classify("pages/x.md", "Status: **retired**", "구 런타임");
        assert_eq!(historical.tier, "historical");

        let archive = Authority::classify("pages/log-archive-2026-06.md", "", "운영 기록");
        assert_eq!(archive.tier, "archive");

        let raw = Authority::classify("raw/dump.md", "", "");
        assert_eq!(raw.tier, "raw");

        let canonical = Authority::classify(
            "pages/charter.md",
            "Status: canonical",
            "이 문서는 최상위 운영 헌장입니다",
        );
        assert_eq!(canonical.tier, "canonical");

        let active = Authority::classify("pages/service.md", "Status: **active**", "운영 중");
        assert_eq!(active.tier, "active");

        let standard = Authority::classify("pages/notes.md", "", "메모");
        assert_eq!(standard.tier, "standard");
    }

    #[test]
    fn multipliers_apply_only_with_intent_and_enabled() {
        let authority = Authority::classify("pages/x.md", "", "");
        assert_eq!(score_multiplier(&authority, false, true), 1.0);
        assert_eq!(score_multiplier(&authority, true, false), 1.0);
        let canonical = Authority::classify("pages/charter.md", "Status: canonical", "");
        assert!((score_multiplier(&canonical, true, true) - 1.30).abs() < 1e-12);
        assert!((score_multiplier(&canonical, false, true) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn intent_matches_korean_and_english_policy_words() {
        assert!(query_intent("위키 운영규칙 알려줘"));
        assert!(query_intent("what is the policy for keys"));
        assert!(!query_intent("postgres 연결 실패"));
    }
}

//! Calibrated semantic abstention, ported from the bash implementation's
//! `abstention_python_lib` (issue #164, consumed by the `find` port in
//! issue #121 slice 2).
//!
//! The calibration intentionally covers only the measured full-wiki arms:
//! the hashed vectorizer and the measured jina model. Unknown backends,
//! models, or reranked candidates report `calibrated: false` and keep their
//! results — the reader refuses to guess a cutoff it was never calibrated
//! for.

use serde::Serialize;
use std::collections::BTreeMap;

pub const ABSTENTION_SCHEMA: &str = "semantic-abstention-v1";
pub const CALIBRATED_JINA_MODEL: &str = "jina-embeddings-v5-text-small";
/// Backend → calibrated vectorizer identity.
pub const CALIBRATED_VECTORIZERS: [(&str, &str); 2] = [
    ("hashed", "hashed-v3-d2048"),
    ("jina-api", "jina-api-v3-d2048"),
];

const WEAK_AUTHORITY_TIERS: &[&str] = &["archive", "historical", "raw", "forwarding"];

fn thresholds() -> BTreeMap<&'static str, f64> {
    BTreeMap::from([
        ("queryTermCoverage", 0.35),
        ("sparseVector", 0.175),
        ("denseCosine", 0.43),
    ])
}

fn calibrated_vectorizer(backend: &str) -> Option<&'static str> {
    CALIBRATED_VECTORIZERS
        .iter()
        .find(|(name, _)| *name == backend)
        .map(|(_, vectorizer)| *vectorizer)
}

/// What the decision needs from the top-ranked candidate. The bash code
/// digs these out of the result's `explain` map; the port takes them as a
/// plain struct so the caller owns the extraction.
pub struct TopEvidence {
    pub matched_terms: u64,
    pub sparse_vector: f64,
    /// The raw dense cosine, when the top chunk carried a dense vector.
    pub dense_cosine: Option<f64>,
    pub authority_tier: String,
    /// headingExact+snippetExact+pathExact+alias*Exact of the top result.
    pub exact_boost: f64,
}

pub struct DecisionInput<'a> {
    pub backend: &'a str,
    pub fusion: &'a str,
    pub enabled: bool,
    /// `meta.json`'s `denseModel` (may be empty).
    pub dense_model: &'a str,
    /// `meta.json`'s `vectorizer` (may be empty).
    pub vectorizer: &'a str,
    /// The Rust read path never reranks; an operator's reranker setting is
    /// warned about and ignored before this point, so this is always false.
    pub reranked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Abstention {
    pub schema: &'static str,
    pub enabled: bool,
    pub calibrated: bool,
    pub calibration: Option<String>,
    pub backend: String,
    pub fusion: String,
    pub dense_model: Option<String>,
    pub vectorizer: Option<String>,
    pub reranker: Option<String>,
    pub confidence: f64,
    pub threshold: f64,
    pub would_abstain: bool,
    pub abstained: bool,
    pub reason: String,
    pub signals: Signals,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Signals {
    pub query_terms: usize,
    pub matched_terms: u64,
    pub query_term_coverage: f64,
    pub sparse_vector: f64,
    pub dense_cosine: Option<f64>,
    pub authority_tier: String,
    pub exact_boost: f64,
    pub component_strengths: BTreeMap<String, f64>,
    pub thresholds: BTreeMap<&'static str, f64>,
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

/// `semantic_abstention_decision` over the top-N candidates (bash passes the
/// ranked prefix; only the first item's evidence is read).
pub fn decide(
    results_empty: bool,
    top: Option<&TopEvidence>,
    query_term_count: usize,
    input: &DecisionInput<'_>,
) -> Abstention {
    let fusion_calibrated = input.fusion == "weighted" || input.fusion == "rrf";
    let mut calibrated = false;
    let mut calibration: Option<String> = None;
    let hashed_vectorizer = calibrated_vectorizer("hashed").unwrap_or("");
    let jina_vectorizer = calibrated_vectorizer("jina-api").unwrap_or("");
    if input.backend == "hashed" && fusion_calibrated && input.vectorizer == hashed_vectorizer {
        calibrated = true;
        calibration = Some(format!("full-wiki-2026-07-22:hashed:{}:v1", input.fusion));
    } else if input.backend == "jina-api"
        && fusion_calibrated
        && input.dense_model == CALIBRATED_JINA_MODEL
        && input.vectorizer == jina_vectorizer
    {
        calibrated = true;
        calibration = Some(format!(
            "full-wiki-2026-07-22:jina-v5-small:{}:v1",
            input.fusion
        ));
    }

    let evidence = top;
    let matched_terms = evidence.map(|e| e.matched_terms).unwrap_or(0);
    let query_coverage = matched_terms as f64 / query_term_count.max(1) as f64;
    let sparse_vector = evidence.map(|e| e.sparse_vector).unwrap_or(0.0);
    let dense_cosine = evidence.and_then(|e| e.dense_cosine);
    let authority_tier = evidence
        .map(|e| e.authority_tier.clone())
        .unwrap_or_else(|| "standard".to_string());
    let exact_boost = evidence.map(|e| e.exact_boost).unwrap_or(0.0);

    let mut strengths = BTreeMap::from([
        (
            "queryTermCoverage",
            (query_coverage / thresholds()["queryTermCoverage"]).min(1.0),
        ),
        (
            "sparseVector",
            (sparse_vector / thresholds()["sparseVector"]).min(1.0),
        ),
        ("exactMatch", if exact_boost > 0.0 { 1.0 } else { 0.0 }),
        ("denseCosine", 0.0),
    ]);
    if input.backend == "jina-api"
        && dense_cosine.is_some()
        && !WEAK_AUTHORITY_TIERS.contains(&authority_tier.as_str())
    {
        strengths.insert(
            "denseCosine",
            (dense_cosine.unwrap_or(0.0) / thresholds()["denseCosine"]).min(1.0),
        );
    }
    let confidence = if results_empty {
        0.0
    } else {
        round6(strengths.values().cloned().fold(0.0, f64::max))
    };
    let would_abstain = calibrated && !input.reranked && (results_empty || confidence < 1.0);
    let abstained = input.enabled && would_abstain;

    let reason = if input.reranked {
        "reranker-not-calibrated"
    } else if !calibrated {
        "backend-model-not-calibrated"
    } else if !input.enabled {
        "disabled"
    } else if results_empty {
        "no-candidates"
    } else if abstained {
        "insufficient-cross-signal-evidence"
    } else {
        "sufficient-cross-signal-evidence"
    };

    Abstention {
        schema: ABSTENTION_SCHEMA,
        enabled: input.enabled,
        calibrated,
        calibration,
        backend: input.backend.to_string(),
        fusion: input.fusion.to_string(),
        dense_model: if input.dense_model.is_empty() {
            None
        } else {
            Some(input.dense_model.into())
        },
        vectorizer: if input.vectorizer.is_empty() {
            None
        } else {
            Some(input.vectorizer.into())
        },
        reranker: None,
        confidence,
        threshold: 1.0,
        would_abstain,
        abstained,
        reason: reason.to_string(),
        signals: Signals {
            query_terms: query_term_count,
            matched_terms,
            query_term_coverage: round6(query_coverage),
            sparse_vector: round6(sparse_vector),
            dense_cosine: dense_cosine.map(round6),
            authority_tier,
            exact_boost: round6(exact_boost),
            component_strengths: strengths
                .into_iter()
                .map(|(key, value)| (key.to_string(), round6(value)))
                .collect(),
            thresholds: thresholds(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashed_input() -> DecisionInput<'static> {
        DecisionInput {
            backend: "hashed",
            fusion: "weighted",
            enabled: true,
            dense_model: "",
            vectorizer: "hashed-v3-d2048",
            reranked: false,
        }
    }

    #[test]
    fn calibrated_hashed_arm_abstains_without_evidence() {
        let decision = decide(true, None, 3, &hashed_input());
        assert!(decision.calibrated);
        assert_eq!(
            decision.calibration.as_deref(),
            Some("full-wiki-2026-07-22:hashed:weighted:v1")
        );
        assert!(decision.abstained);
        assert_eq!(decision.reason, "no-candidates");
        assert_eq!(decision.confidence, 0.0);
    }

    #[test]
    fn strong_sparse_evidence_keeps_results() {
        let top = TopEvidence {
            matched_terms: 3,
            sparse_vector: 0.3,
            dense_cosine: None,
            authority_tier: "standard".into(),
            exact_boost: 0.0,
        };
        let decision = decide(false, Some(&top), 3, &hashed_input());
        assert!(!decision.abstained);
        assert_eq!(decision.reason, "sufficient-cross-signal-evidence");
        assert_eq!(decision.confidence, 1.0);
    }

    #[test]
    fn weak_evidence_abstains() {
        let top = TopEvidence {
            matched_terms: 1,
            sparse_vector: 0.05,
            dense_cosine: None,
            authority_tier: "archive".into(),
            exact_boost: 0.0,
        };
        let decision = decide(false, Some(&top), 3, &hashed_input());
        assert!(decision.abstained);
        assert_eq!(decision.reason, "insufficient-cross-signal-evidence");
        // sparse 0.05/0.175 ≈ 0.2857 dominates coverage 0.333/0.35... both
        // below 1; coverage 1/3 / 0.35 ≈ 0.952.
        assert!(
            (decision.signals.component_strengths["queryTermCoverage"] - 0.952381).abs() < 1e-5
        );
    }

    #[test]
    fn unknown_vectorizer_is_not_calibrated() {
        let input = DecisionInput {
            vectorizer: "hashed-v2-d1024",
            ..hashed_input()
        };
        let decision = decide(true, None, 1, &input);
        assert!(!decision.calibrated);
        assert!(!decision.abstained);
        assert_eq!(decision.reason, "backend-model-not-calibrated");
    }

    #[test]
    fn disabled_keeps_results_but_reports_the_gate() {
        let input = DecisionInput {
            enabled: false,
            ..hashed_input()
        };
        let decision = decide(true, None, 1, &input);
        assert!(decision.calibrated);
        assert!(!decision.abstained);
        assert_eq!(decision.reason, "disabled");
    }

    #[test]
    fn jina_arm_needs_both_model_and_vectorizer() {
        let input = DecisionInput {
            backend: "jina-api",
            dense_model: "jina-embeddings-v5-text-small",
            vectorizer: "jina-api-v3-d2048",
            ..hashed_input()
        };
        let decision = decide(true, None, 1, &input);
        assert!(decision.calibrated);
        assert_eq!(
            decision.calibration.as_deref(),
            Some("full-wiki-2026-07-22:jina-v5-small:weighted:v1")
        );
    }
}

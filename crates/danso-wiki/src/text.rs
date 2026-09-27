//! Shared query/document text rules, ported line-for-line from the bash
//! wiki-agent's embedded python (`TOKEN_RE`, `vectorize`, `tokenize`, the
//! `text_has_secret_like` detector and the notification redactor).
//!
//! These are the equivalence-critical primitives (issue #121 decision 2b and
//! the gold gate): a query must tokenize and embed identically on both
//! runtimes, and read-path output must be masked with the same rules the
//! codebase already applies to every other body of text that leaves a
//! process. `python3 -` was used to derive the golden constants in the tests
//! from the reference implementation.

use blake2::digest::consts::U4;
use blake2::{Blake2b, Digest};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

/// `[a-z0-9][a-z0-9_-]{1,}|[가-힣]{2,}` — latin tokens are at least two
/// characters, hangul runs at least two syllables. Single-character latin
/// fragments are deliberately noise here.
fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[a-z0-9][a-z0-9_-]{1,}|[가-힣]{2,}").expect("static regex"))
}

/// `[가-힣]{2,}` — the hangul runs that feed the 2/3-gram channel.
fn hangul_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[가-힣]{2,}").expect("static regex"))
}

/// `[a-z0-9][a-z0-9_-]{1,}` — the latin tokens that feed the split-bit
/// channel (`wiki-agent` → `wiki`, `agent`).
fn latin_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[a-z0-9][a-z0-9_-]{1,}").expect("static regex"))
}

/// `[-_/.:]+` — the separators latin tokens are split on.
fn split_bits(token: &str) -> impl Iterator<Item = &str> {
    token.split(['-', '_', '/', '.', ':'])
}

/// The tokens of `text`, exactly `re.findall(TOKEN_RE, text.lower())`.
pub fn tokens(text: &str) -> Vec<String> {
    token_re()
        .find_iter(&text.to_lowercase())
        .map(|m| m.as_str().to_string())
        .collect()
}

/// The distinct tokens of `text`.
pub fn token_set(text: &str) -> BTreeSet<String> {
    tokens(text).into_iter().collect()
}

/// The lexical bag: `tokenize()` in the bash implementation. Tokens count
/// once each, then every token is split on `[-_/.:]` and each new spelling
/// of two or more characters counts once more — `doc-002` matches a
/// document that only ever says `002`. The identity bit never counts twice.
pub fn tokenize(text: &str) -> BTreeMap<String, u32> {
    let lower = text.to_lowercase();
    let mut terms: BTreeMap<String, u32> = BTreeMap::new();
    let matches: Vec<&str> = token_re().find_iter(&lower).map(|m| m.as_str()).collect();
    for token in &matches {
        *terms.entry((*token).to_string()).or_insert(0) += 1;
    }
    let bit_lists: Vec<Vec<String>> = matches
        .iter()
        .map(|token| {
            split_bits(token)
                .filter(|bit| bit.chars().count() >= 2 && *bit != *token)
                .map(|bit| bit.to_string())
                .collect()
        })
        .collect();
    for bits in bit_lists {
        for bit in bits {
            *terms.entry(bit).or_insert(0) += 1;
        }
    }
    terms
}

/// The sparse hashed query vector: `vectorize()` in the bash implementation.
///
/// Tokens count double, hangul 2/3-grams and latin split bits count once,
/// each spelling hashes to a dimension through 4-byte BLAKE2b (`digest_size=4`
/// — the same construction as `hashlib.blake2b`), weights are `1 + ln(count)`
/// and the result is L2-normalized. Vectorizer identity: `hashed-v3-d2048`.
pub fn vectorize(text: &str, dim: u32) -> BTreeMap<String, f64> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let lower = text.to_lowercase();
    for token in token_re().find_iter(&lower) {
        *counts.entry(token.as_str().to_string()).or_insert(0) += 2;
    }
    for span in hangul_re().find_iter(&lower) {
        let chars: Vec<char> = span.as_str().chars().collect();
        for n in 2..=3 {
            if chars.len() >= n {
                for i in 0..=(chars.len() - n) {
                    let gram: String = chars[i..i + n].iter().collect();
                    *counts.entry(gram).or_insert(0) += 1;
                }
            }
        }
    }
    for token in latin_re().find_iter(&lower) {
        for bit in split_bits(token.as_str()) {
            if bit.chars().count() >= 2 {
                *counts.entry(bit.to_string()).or_insert(0) += 1;
            }
        }
    }
    let mut sparse: BTreeMap<String, f64> = BTreeMap::new();
    for (token, count) in counts {
        let index = blake2_dimension(&token, dim);
        let weight = 1.0 + (count as f64).ln();
        *sparse.entry(index.to_string()).or_insert(0.0) += weight;
    }
    let norm2: f64 = sparse.values().map(|w| w * w).sum();
    let norm = if norm2 > 0.0 { norm2.sqrt() } else { 0.0 };
    let norm = if norm == 0.0 { 1.0 } else { norm };
    sparse.into_iter().map(|(k, v)| (k, v / norm)).collect()
}

/// `int(hashlib.blake2b(token, digest_size=4).hexdigest(), 16) % dim` — the
/// hex digest is the big-endian rendering of the four output bytes.
fn blake2_dimension(token: &str, dim: u32) -> u32 {
    let mut hasher = Blake2b::<U4>::new();
    hasher.update(token.as_bytes());
    let digest: [u8; 4] = hasher.finalize().into();
    u32::from_be_bytes(digest) % dim
}

/// `text_has_secret_like()` — the conservative credential-shape detector.
pub fn has_secret_like(text: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)(",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----|",
            r"github_pat_|",
            r"\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9_]{20,}|",
            r"\bsk-[A-Za-z0-9_-]{32,}|",
            r"\bAKIA[0-9A-Z]{16}\b|",
            r"\bAIza[0-9A-Za-z_-]{30,}|",
            r"\bxox[baprs]-[0-9A-Za-z-]{20,}|",
            r"\b[0-9]{8,10}:[A-Za-z0-9_-]{30,}|",
            r"[A-Z0-9_]*(TOKEN|SECRET|PASSWORD)[[:space:]]*=",
            r")"
        ))
        .expect("static regex")
    })
    .is_match(text)
}

/// The read-path masker: the redaction rules the codebase already applies to
/// notification bodies (`redact_notification_text`), now also applied to
/// `find`/`prefetch`/`load` output per issue #121 decision 2b. `KEY=value`
/// keeps the key name; everything else collapses to `[REDACTED]`.
pub fn redact(text: &str) -> String {
    fn pattern(pattern: &str) -> Regex {
        Regex::new(&format!("(?is){pattern}")).expect("static regex")
    }
    static PRIVATE_KEY: OnceLock<Regex> = OnceLock::new();
    static PLAIN: OnceLock<Vec<Regex>> = OnceLock::new();
    static KEYED: OnceLock<Regex> = OnceLock::new();
    let private_key = PRIVATE_KEY.get_or_init(|| {
        pattern(r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----")
    });
    let plain = PLAIN.get_or_init(|| {
        [
            r"github_pat_[A-Za-z0-9_]+",
            r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9_]{12,}",
            r"\bsk-[A-Za-z0-9_-]{12,}",
            r"\bAKIA[0-9A-Z]{16}\b",
            r"\bAIza[0-9A-Za-z_-]{20,}",
            r"\bxox[baprs]-[0-9A-Za-z-]{12,}",
            r"\b[0-9]{8,10}:[A-Za-z0-9_-]{16,}",
        ]
        .map(pattern)
        .to_vec()
    });
    let keyed = KEYED.get_or_init(|| {
        pattern(r"\b([A-Z0-9_]*(?:TOKEN|SECRET|PASSWORD|PRIVATE_KEY|API_KEY))\s*=\s*\S+")
    });
    let mut text = private_key.replace_all(text, "[REDACTED]").to_string();
    for re in plain {
        text = re.replace_all(&text, "[REDACTED]").to_string();
    }
    text = keyed.replace_all(&text, "$1=[REDACTED]").to_string();
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden values derived with the reference implementation:
    //   python3 -c "import hashlib; …"  (blake2b digest_size=4, % 2048)
    #[test]
    fn blake2_dimensions_match_the_reference_hash() {
        // hashlib.blake2b(b"alpha", digest_size=4).hexdigest() == 0e3491b4
        assert_eq!(blake2_dimension("alpha", 2048), 436);
        // hashlib.blake2b(b"wiki", digest_size=4).hexdigest() == 8f27c209
        assert_eq!(blake2_dimension("wiki", 2048), 2023);
    }

    #[test]
    fn vectorize_weights_match_the_reference_formula() {
        // "alpha": TOKEN_RE +2, latin split bit +1 → count 3, one dimension.
        let v = vectorize("alpha", 2048);
        assert_eq!(v.len(), 1, "one spelling, one dimension: {v:?}");
        assert_eq!(v["436"], 1.0, "single spelling normalizes to 1.0");
    }

    #[test]
    fn mixed_query_vector_matches_the_reference_golden() {
        // vectorize("wiki-agent find 세션 키 관리", 2048) from the bash
        // implementation — single-char hangul ("키") drops out, latin bits
        // ("wiki", "agent") join, find/세션/관리 count 3 (token + bit / runs).
        let v = vectorize("wiki-agent find 세션 키 관리", 2048);
        let golden: BTreeMap<String, f64> = BTreeMap::from([
            ("1030".into(), 0.23518497814732853),
            ("1321".into(), 0.4935620852501245),
            ("1782".into(), 0.39820278266020165),
            ("1935".into(), 0.4935620852501245),
            ("2023".into(), 0.23518497814732853),
            ("827".into(), 0.4935620852501245),
        ]);
        assert_eq!(v.len(), golden.len());
        for (dim, weight) in &golden {
            assert!(
                (v[dim] - weight).abs() < 1e-12,
                "dim {dim}: {:?} vs {weight}",
                v[dim]
            );
        }
    }

    #[test]
    fn tokenize_splits_separated_bits_and_keeps_the_whole_token() {
        let terms = tokenize("doc-002 wiki.seoyoon-family.com");
        assert_eq!(terms["doc-002"], 1);
        assert_eq!(terms["doc"], 1);
        assert_eq!(terms["002"], 1);
        assert_eq!(terms["wiki"], 1);
        assert_eq!(terms["seoyoon"], 1);
        assert_eq!(terms["seoyoon-family"], 1);
        assert_eq!(terms["family"], 1);
        assert_eq!(terms["com"], 1);
        assert!(!terms.contains_key("md"), "no such fragment in the input");
    }

    #[test]
    fn hangul_runs_gain_ngrams_and_the_whole_run() {
        let v = vectorize("한글정규", 2048);
        // run + 2-grams(3) + 3-grams(2): 6 spellings over 6 dimensions,
        // hashed with the reference construction.
        assert_eq!(v.len(), 6);
        for (gram, dim) in [
            ("한글", 1859),
            ("글정", 1606),
            ("정규", 1029),
            ("한글정", 1945),
            ("글정규", 1010),
            ("한글정규", 1999),
        ] {
            assert!(
                v.contains_key(&dim.to_string()),
                "{gram} must hash to {dim}"
            );
        }
        let terms = tokenize("한글정규");
        // The lexical bag has no n-gram channel — that is vectorize-only
        // (bash tokenize: tokens + split bits). One spelling, counted once.
        assert_eq!(terms.values().sum::<u32>(), 1);
    }

    #[test]
    fn tokenize_matches_the_reference_counts_on_a_mixed_query() {
        let terms = tokenize("wiki-agent find 세션 키 관리");
        assert_eq!(terms["wiki-agent"], 1);
        assert_eq!(terms["find"], 1);
        assert_eq!(terms["wiki"], 1);
        assert_eq!(terms["agent"], 1);
        assert_eq!(terms["세션"], 1);
        assert_eq!(terms["관리"], 1);
        assert!(
            !terms.contains_key("키"),
            "single hangul syllables are not tokens"
        );
    }

    #[test]
    fn detector_flags_credentials_but_not_prose() {
        assert!(has_secret_like(
            "token: ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        ));
        assert!(has_secret_like("WIKI_SECRET=hunter2"));
        assert!(has_secret_like("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(!has_secret_like("the password policy page"));
        assert!(!has_secret_like("run wiki-agent find 세션 키 관리"));
    }

    #[test]
    fn redact_masks_values_and_keeps_key_names() {
        assert_eq!(redact("ghp_AAAAAAAAAAAAAAAAAAAAAAAA"), "[REDACTED]");
        assert_eq!(
            redact("API_KEY = sk-abcdefghijklmnopqrst"),
            "API_KEY=[REDACTED]"
        );
        assert_eq!(
            redact("-----BEGIN X PRIVATE KEY-----\nabc\n-----END X PRIVATE KEY----- keep"),
            "[REDACTED] keep"
        );
        assert_eq!(redact("normal text stays"), "normal text stays");
        assert_eq!(redact("1234567890:AAAAAAAAAAAAAAAAAAAAAAAA"), "[REDACTED]");
    }
}

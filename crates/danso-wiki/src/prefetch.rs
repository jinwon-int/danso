//! `prefetch` — budget-capped Wiki context for agent runtimes (issue #121
//! slice 3), ported 1:1 from the bash `cmd_prefetch` python. One find pass
//! (in-process, exactly like the bash original calling `cmd_find` as a shell
//! function — a subprocess would re-parse the 143 MB JSONL for nothing) with
//! at most 3 snippet candidates inside a hard character budget.
//!
//! Fail-open: every failure path still prints a `wiki-agent-prefetch-v1`
//! payload and exits 0 — prefetch must never block the user-facing response.
//! Like the bash tool, snippets are candidates only; every result carries
//! its `load` verification command.

use crate::find::{self, FindOptions};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

pub struct PrefetchOptions {
    pub query: String,
    pub index_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// Emit the machine-readable payload instead of the text rendering.
    pub json: bool,
    /// Maximum snippets; clamped to 1..=3 like the bash python did.
    pub top: usize,
    /// Hard snippet budget in characters (python `len` — code points).
    pub budget_chars: usize,
    /// `None` — default (on) unless `WIKI_SEMANTIC_ABSTENTION` says otherwise.
    pub abstention: Option<bool>,
}

/// Entry point: prints the payload (JSON or text) and always exits 0.
pub fn run(options: &PrefetchOptions) -> i32 {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    run_into(options, &mut out)
}

fn run_into(options: &PrefetchOptions, out: &mut impl Write) -> i32 {
    let top = options.top.clamp(1, 3);
    let budget_chars = options.budget_chars.max(1);

    let mut payload = json!({
        "schema": "wiki-agent-prefetch-v1",
        "status": "unavailable",
        "query": options.query,
        "budget": {"maxSnippets": top, "maxChars": budget_chars, "usedChars": 0, "truncated": false},
        "results": [],
        "diagnostic": "",
    });

    let find_options = FindOptions {
        query: options.query.clone(),
        index_dir: options.index_dir.clone(),
        cache_dir: options.cache_dir.clone(),
        json: true,
        refresh: false,
        abstention: options.abstention,
        top,
        // The bash prefetch leaves grep_top at its default.
        grep_top: 8,
        include: Vec::new(),
        exclude: Vec::new(),
    };
    let find_payload = find::report(&find_options).ok().filter(|found| {
        found.payload().get("schema").and_then(Value::as_str) == Some("wiki-agent-find-v1")
    });

    match find_payload {
        None => {
            // The bash find exit code here is 2 (`run` maps every error to
            // 2); the diagnostic text is the bash one, verbatim.
            payload["diagnostic"] = json!(
                "wiki-agent find failed (exit 2); prefetch fails open. Answer without wiki \
                 context; for manual retrieval use wiki-agent grep or build/import the \
                 semantic index (wiki-agent semantic-index --import-from <build-node>)."
            );
        }
        Some(found) => {
            let find_payload = found.payload();
            let semantic = find_payload
                .get("semantic")
                .cloned()
                .unwrap_or_else(|| json!({}));
            payload["confidence"] = semantic.get("confidence").cloned().unwrap_or(Value::Null);
            payload["abstained"] = json!(
                semantic
                    .get("abstained")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            );
            payload["abstentionReason"] = semantic
                .get("abstentionReason")
                .cloned()
                .unwrap_or(Value::Null);
            payload["abstention"] = semantic.get("abstention").cloned().unwrap_or(Value::Null);

            let mut candidates: Vec<Value> = Vec::new();
            let mut seen_paths: HashSet<String> = HashSet::new();
            // Semantic candidates first (source "semantic"), then exact-text
            // matches (source "text"), deduped by path across both lists.
            let semantic_results = semantic
                .get("results")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for item in &semantic_results {
                let Some(path) = item.get("path").and_then(Value::as_str) else {
                    continue;
                };
                if path.is_empty() || !seen_paths.insert(path.to_string()) {
                    continue;
                }
                candidates.push(json!({
                    "path": path,
                    "heading": str_field(item, "heading"),
                    "startLine": item.get("startLine").and_then(Value::as_u64).unwrap_or(0),
                    "endLine": item.get("endLine").and_then(Value::as_u64).unwrap_or(0),
                    "score": item.get("score").and_then(Value::as_f64).unwrap_or(0.0),
                    "snippet": str_field(item, "snippet"),
                    "loadCommand": str_field(item, "loadCommand"),
                    "source": "semantic",
                }));
            }
            let text_matches = find_payload
                .get("textMatches")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for item in &text_matches {
                let Some(path) = item.get("path").and_then(Value::as_str) else {
                    continue;
                };
                if path.is_empty() || !seen_paths.insert(path.to_string()) {
                    continue;
                }
                let line = item.get("line").and_then(Value::as_u64).unwrap_or(0);
                candidates.push(json!({
                    "path": path,
                    "heading": "",
                    "startLine": line,
                    "endLine": line,
                    "score": 0,
                    "snippet": str_field(item, "text"),
                    "loadCommand": str_field(item, "loadCommand"),
                    "source": "text",
                }));
            }

            let mut used = 0usize;
            let mut truncated = false;
            let mut results: Vec<Value> = Vec::new();
            for mut item in candidates {
                if results.len() >= top {
                    break;
                }
                let remaining = budget_chars.saturating_sub(used);
                if remaining == 0 {
                    truncated = true;
                    break;
                }
                let snippet = str_field(&item, "snippet");
                if py_len(&snippet) > remaining {
                    let cut: String = snippet.chars().take(remaining.saturating_sub(1)).collect();
                    item["snippet"] = json!(format!("{}…", cut.trim_end()));
                    truncated = true;
                }
                used += py_len(str_field(&item, "snippet").as_str());
                results.push(item);
            }

            payload["results"] = Value::Array(results.clone());
            payload["budget"]["usedChars"] = json!(used);
            payload["budget"]["truncated"] = json!(truncated);
            let abstained = payload["abstained"].as_bool().unwrap_or(false);
            if abstained {
                payload["status"] = json!("low-confidence");
                payload["diagnostic"] = json!(
                    "calibrated retrieval confidence was below threshold; answer without wiki context"
                );
            } else if !results.is_empty() {
                payload["status"] = json!("ok");
            } else {
                payload["status"] = json!("empty");
                payload["diagnostic"] =
                    json!("no wiki candidates for this query; answer without wiki context");
            }
        }
    }

    if options.json {
        let _ = writeln!(out, "{payload}");
    } else {
        let _ = write!(out, "{}", to_text(&options.query, &payload));
    }
    // Fail-open: prefetch never fails the caller's turn.
    0
}

fn str_field(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// python `len()` — code points, not bytes; the budget is a context share,
/// so a CJK snippet must not eat three times its share.
fn py_len(text: &str) -> usize {
    text.chars().count()
}

/// The bash text rendering, line for line — including the python `True`/
/// `False` spelling of the truncated flag.
fn to_text(query: &str, payload: &Value) -> String {
    let status = payload["status"].as_str().unwrap_or_default();
    let results = payload["results"].as_array().cloned().unwrap_or_default();
    let used = payload["budget"]["usedChars"].as_u64().unwrap_or(0);
    let max = payload["budget"]["maxChars"].as_u64().unwrap_or(0);
    let truncated = payload["budget"]["truncated"].as_bool().unwrap_or(false);
    let mut out = format!(
        "prefetch: {query} (status={status} snippets={} budget={used}/{max} truncated={})\n",
        results.len(),
        if truncated { "True" } else { "False" },
    );
    for (index, item) in results.iter().enumerate() {
        let path = item["path"].as_str().unwrap_or_default();
        let start = item["startLine"].as_u64().unwrap_or(0);
        let end = item["endLine"].as_u64().unwrap_or(0);
        let location = if start != 0 {
            format!("{path}:{start}-{end}")
        } else {
            path.to_string()
        };
        out.push_str(&format!("{}. {location}\n", index + 1));
        if let Some(heading) = item["heading"].as_str()
            && !heading.is_empty()
        {
            out.push_str(&format!("   {heading}\n"));
        }
        if let Some(snippet) = item["snippet"].as_str()
            && !snippet.is_empty()
        {
            out.push_str(&format!("   {snippet}\n"));
        }
        if let Some(load_command) = item["loadCommand"].as_str()
            && !load_command.is_empty()
        {
            out.push_str(&format!("   verify: {load_command}\n"));
        }
    }
    if let Some(diagnostic) = payload["diagnostic"].as_str()
        && !diagnostic.is_empty()
    {
        out.push_str(&format!("diagnostic: {diagnostic}\n"));
    }
    if !results.is_empty() {
        out.push_str(
            "snippets are candidates; verify with the listed load commands before operational claims\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A minimal on-disk index shaped like the real one (same fixture shape
    /// as the find contract tests): hashed vectors, one cached page for the
    /// exact-text fallback, no aliases.md.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().expect("temp");
        let index = temp.path().join("index");
        let cache = temp.path().join("cache");
        std::fs::create_dir_all(&index).expect("index dir");
        std::fs::write(
            index.join("meta.json"),
            r#"{"version":3,"backend":"hashed","dims":2048,"chunks":2,"files":2,"builtAt":"2026-09-27T00:00:00Z","builtOn":"test","vectorizer":"hashed-v3-d2048"}"#,
        )
        .expect("meta");
        std::fs::write(
            index.join("manifest.jsonl"),
            concat!(
                r#"{"path":"pages/a.md","fileHash":"hash-a","mtime":1758900000.0,"bytes":120,"chunks":1,"backend":"hashed","indexVersion":3,"indexedAt":"2026-09-27T00:00:00Z"}"#,
                "\n",
                r#"{"path":"pages/b.md","fileHash":"hash-b","mtime":1758900001.0,"bytes":90,"chunks":1,"backend":"hashed","indexVersion":3,"indexedAt":"2026-09-27T00:00:00Z"}"#,
                "\n",
            ),
        )
        .expect("manifest");
        std::fs::write(
            index.join("chunks.jsonl"),
            concat!(
                r#"{"id":"a-0","path":"pages/a.md","startLine":1,"endLine":4,"heading":"Session keys","headingStack":["Session keys"],"level":1,"split":0,"bytes":120,"mtime":1758900000.0,"fileHash":"hash-a","snippet":"session key rotation policy, API_KEY = sk-abcdefghijklmnopqrst inside","terms":{"session":2.0,"key":2.0,"rotation":1.0,"policy":1.0},"vector":{"11":0.25}}"#,
                "\n",
                r#"{"id":"b-0","path":"pages/b.md","startLine":3,"endLine":4,"heading":"Bread","headingStack":["Bread"],"level":1,"split":1,"bytes":90,"mtime":1758900001.0,"fileHash":"hash-b","snippet":"bread baking recipes","terms":{"bread":1.0,"baking":1.0},"vector":{}}"#,
                "\n",
            ),
        )
        .expect("chunks");
        std::fs::create_dir_all(cache.join("pages")).expect("cache tree");
        std::fs::write(
            cache.join("pages/a.md"),
            "# Session keys\n\nrotate the session key every 30 days\nAPI_KEY = sk-abcdefghijklmnopqrst\n",
        )
        .expect("cached page");
        std::fs::write(cache.join("pages/b.md"), "bread baking recipes\n").expect("cached page");
        crate::cache::build(&index, &cache).expect("cache build");
        (temp, index, cache)
    }

    fn options(query: &str, index: &Path, cache: &Path) -> PrefetchOptions {
        PrefetchOptions {
            query: query.to_string(),
            index_dir: index.to_path_buf(),
            cache_dir: cache.to_path_buf(),
            json: true,
            top: 3,
            budget_chars: 3200,
            // Pinned off: the calibrated gate would abstain on partial
            // evidence, and these tests are about the candidate pipeline.
            abstention: Some(false),
        }
    }

    #[test]
    fn prefetch_reports_ok_candidates_with_budget_accounting() {
        let (_temp, index, cache) = fixture();
        let mut opts = options("session key rotation", &index, &cache);
        opts.budget_chars = 10_000;
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0, "fail-open: always exit 0");
        let payload: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(payload["schema"], "wiki-agent-prefetch-v1");
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["budget"]["maxSnippets"], 3);
        assert_eq!(payload["budget"]["truncated"], false);
        let results = payload["results"].as_array().expect("results");
        assert!(!results.is_empty());
        // The semantic candidate is deduped against its own text match.
        let paths: Vec<&str> = results
            .iter()
            .filter_map(|item| item["path"].as_str())
            .collect();
        let unique: HashSet<&str> = paths.iter().copied().collect();
        assert_eq!(paths.len(), unique.len(), "results are deduped by path");
        assert_eq!(results[0]["source"], "semantic");
        assert!(
            results[0]["loadCommand"]
                .as_str()
                .expect("loadCommand")
                .starts_with("wiki-agent load --lines ")
        );
        // usedChars is the python-len sum of the emitted snippets.
        let used: usize = results
            .iter()
            .map(|item| item["snippet"].as_str().expect("snippet").chars().count())
            .sum();
        assert_eq!(payload["budget"]["usedChars"], json!(used as u64));
    }

    #[test]
    fn prefetch_truncates_over_budget_snippets() {
        let (_temp, index, cache) = fixture();
        let mut opts = options("session key rotation", &index, &cache);
        opts.budget_chars = 40;
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        let payload: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["budget"]["truncated"], true);
        assert!(payload["budget"]["usedChars"].as_u64().expect("used") <= 40);
        let snippet = payload["results"][0]["snippet"].as_str().expect("snippet");
        assert!(
            snippet.ends_with('…'),
            "over-budget snippets end with the ellipsis"
        );
        assert!(snippet.chars().count() <= 40);
    }

    #[test]
    fn prefetch_clamps_top_to_three() {
        let (_temp, index, cache) = fixture();
        let mut opts = options("session key rotation", &index, &cache);
        opts.top = 99;
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        let payload: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(payload["budget"]["maxSnippets"], 3);
        let results = payload["results"].as_array().expect("results");
        assert!(results.len() <= 3);

        // top=0 is clamped to 1, like the bash max(1, min(3, top)).
        let mut opts = options("session key rotation", &index, &cache);
        opts.top = 0;
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        let payload: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(payload["budget"]["maxSnippets"], 1);
        assert!(
            payload["results"].as_array().expect("results").len() <= 1,
            "top=0 clamps to one snippet"
        );
    }

    #[test]
    fn prefetch_fails_open_when_the_index_is_unavailable() {
        let temp = tempfile::tempdir().expect("temp");
        let cache = temp.path().join("cache");
        std::fs::create_dir_all(&cache).expect("cache");
        let opts = options("anything", &temp.path().join("absent-index"), &cache);
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        let payload: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(payload["status"], "unavailable");
        assert_eq!(payload["results"], json!([]));
        let diagnostic = payload["diagnostic"].as_str().expect("diagnostic");
        assert!(diagnostic.contains("prefetch fails open"));
        assert!(diagnostic.contains("wiki-agent grep"));
        assert!(diagnostic.contains("--import-from"));
    }

    #[test]
    fn prefetch_reports_empty_when_nothing_matches() {
        let (_temp, index, cache) = fixture();
        // Abstention off: with the calibrated gate on, a zero-evidence query
        // abstains (low-confidence) instead of reaching the empty status.
        let mut opts = options("zzzqqq unrelated widgets", &index, &cache);
        opts.abstention = Some(false);
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        let payload: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(payload["status"], "empty");
        assert_eq!(
            payload["diagnostic"],
            "no wiki candidates for this query; answer without wiki context"
        );
    }

    #[test]
    fn prefetch_marks_low_confidence_when_abstained() {
        let (_temp, index, cache) = fixture();
        let mut opts = options("session gardening workshops", &index, &cache);
        // 1 of 3 query terms matches: the calibrated gate must abstain.
        opts.abstention = Some(true);
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        let payload: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(payload["abstained"], true);
        assert_eq!(payload["status"], "low-confidence");
        assert_eq!(
            payload["diagnostic"],
            "calibrated retrieval confidence was below threshold; answer without wiki context"
        );
        assert!(payload["results"].as_array().expect("results").is_empty());
        // The bash payload copies the abstention decision object through.
        assert!(payload["abstention"].is_object());
    }

    #[test]
    fn prefetch_text_render_matches_the_bash_layout() {
        let (_temp, index, cache) = fixture();
        let mut opts = options("session key rotation", &index, &cache);
        opts.json = false;
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        let text = String::from_utf8(out).expect("utf8");
        let mut lines = text.lines();
        let summary = lines.next().expect("summary line");
        assert!(summary.starts_with("prefetch: session key rotation (status=ok snippets="));
        assert!(summary.contains("budget="));
        assert!(summary.contains(" truncated=False)"));
        let first = lines.next().expect("numbered result");
        assert!(
            first.starts_with("1. pages/a.md:"),
            "numbered location line, got: {first}"
        );
        let verify: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("   verify: wiki-agent load --lines "))
            .collect();
        assert!(
            !verify.is_empty(),
            "each snippet carries its verify command"
        );
        assert!(
            text.ends_with(
                "snippets are candidates; verify with the listed load commands before operational claims\n"
            )
        );

        // An unavailable prefetch renders its diagnostic instead.
        let temp = tempfile::tempdir().expect("temp");
        let cache = temp.path().join("cache");
        std::fs::create_dir_all(&cache).expect("cache");
        let mut opts = options("anything", &temp.path().join("absent-index"), &cache);
        opts.json = false;
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(run_into(&opts, &mut out), 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.starts_with(
            "prefetch: anything (status=unavailable snippets=0 budget=0/3200 truncated=False)\n"
        ));
        assert!(text.contains("diagnostic: wiki-agent find failed (exit 2); prefetch fails open."));
        assert!(!text.contains("snippets are candidates"));
    }
}

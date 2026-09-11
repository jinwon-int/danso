//! Successful-path integration coverage for public `danso::usage` aggregation.
use danso::usage::{TokenUsage, Usage};
use serde_json::{Value, json};

fn exact(summary: Value) -> String {
    // Serialize to a canonical string so structural checks are exact, not fuzzy.
    summary.to_string()
}

#[test]
fn zero_usage_summary_is_exactly_the_zero_contract() {
    let summary = Usage::default().summary();
    assert_eq!(
        exact(summary.clone()),
        exact(json!({
            "requests": 0,
            "inputTokens": 0,
            "outputTokens": 0,
            "cacheReadTokens": 0,
            "cacheWriteTokens": 0,
            "totalTokens": 0,
            "costUsd": 0,
            "models": []
        }))
    );
    // costUsd stays a literal 0 placeholder (JSON number zero, not null/string).
    assert_eq!(summary["costUsd"], Value::from(0));
}

#[test]
fn successful_calls_aggregate_all_four_counters_across_calls() {
    let mut usage = Usage::default();
    usage
        .add(
            "alpha",
            "m1",
            TokenUsage {
                input: 11,
                output: 7,
                cache_read: 3,
                cache_write: 5,
            },
        )
        .unwrap();
    usage
        .add(
            "beta",
            "m2",
            TokenUsage {
                input: 101,
                output: 23,
                cache_read: 17,
                cache_write: 9,
            },
        )
        .unwrap();
    let summary = usage.summary();
    assert_eq!(summary["requests"], 2);
    assert_eq!(summary["inputTokens"], 112);
    assert_eq!(summary["outputTokens"], 30);
    assert_eq!(summary["cacheReadTokens"], 20);
    assert_eq!(summary["cacheWriteTokens"], 14);
    assert_eq!(summary["totalTokens"], 176);
    assert_eq!(summary["costUsd"], 0);
    assert_eq!(summary["models"], json!(["alpha/m1", "beta/m2"]));
}

#[test]
fn models_dedupe_in_first_seen_order_across_providers() {
    let mut usage = Usage::default();
    // First seen: openai/gpt-x
    usage.add("openai", "gpt-x", tok(1, 0, 0, 0)).unwrap();
    // Same model name on a different provider is a distinct pair.
    usage.add("anthropic", "gpt-x", tok(0, 2, 0, 0)).unwrap();
    // Repeated pair is deduplicated, no new entry.
    usage.add("openai", "gpt-x", tok(0, 0, 4, 0)).unwrap();
    // New pair appended at the end.
    usage.add("zeta", "m9", tok(0, 0, 0, 8)).unwrap();
    // Repeated again: still deduplicated, order unchanged.
    usage.add("anthropic", "gpt-x", tok(2, 2, 0, 0)).unwrap();

    let summary = usage.summary();
    assert_eq!(
        summary["models"],
        json!(["openai/gpt-x", "anthropic/gpt-x", "zeta/m9"])
    );
    // Repeated calls still count as requests and tokens aggregate.
    assert_eq!(summary["requests"], 5);
    assert_eq!(summary["inputTokens"], 3);
    assert_eq!(summary["outputTokens"], 4);
    assert_eq!(summary["cacheReadTokens"], 4);
    assert_eq!(summary["cacheWriteTokens"], 8);
    assert_eq!(summary["totalTokens"], 19);
}

#[test]
fn zero_token_successful_call_still_counts_as_a_request() {
    let mut usage = Usage::default();
    usage.add("noop", "silent", TokenUsage::default()).unwrap();
    usage
        .add(
            "noop",
            "loud",
            TokenUsage {
                input: 5,
                ..Default::default()
            },
        )
        .unwrap();
    let summary = usage.summary();
    assert_eq!(summary["requests"], 2);
    assert_eq!(summary["totalTokens"], 5);
    assert_eq!(summary["inputTokens"], 5);
    assert_eq!(summary["models"], json!(["noop/silent", "noop/loud"]));
}

/// #52 §4.6 (#86): `memory_requests` is a DANSO_BUDGET field. Pin the whole
/// record so a future counter cannot silently land in the Piri-fixed
/// DANSO_USAGE record instead, and so the hand-written format literal cannot
/// drift out of argument order.
#[test]
fn budget_record_is_exactly_the_budget_contract() {
    let mut usage = Usage::default();
    usage.record_summary_request();
    usage.record_memory_request();
    usage.record_memory_request();
    usage.record_length_stop();
    usage.add("noop", "silent", TokenUsage::default()).unwrap();

    let record = danso::output::budget_record(&usage, 12, 4096);
    assert_eq!(
        record,
        "{\"version\":1,\"requests_used\":1,\"requests_total\":12,\"summary_requests\":1,\"memory_requests\":2,\"output_tokens_max\":4096,\"length_stops\":1,\"continuations\":0}"
    );
    // It must parse as JSON: the literal is hand-escaped.
    let parsed: Value = serde_json::from_str(&record).unwrap();
    assert_eq!(parsed["memory_requests"], json!(2));
    // The counter never widens DANSO_USAGE / PIRI_USAGE.
    assert!(usage.summary().get("memoryRequests").is_none());
}

fn tok(input: u64, output: u64, cache_read: u64, cache_write: u64) -> TokenUsage {
    TokenUsage {
        input,
        output,
        cache_read,
        cache_write,
    }
}

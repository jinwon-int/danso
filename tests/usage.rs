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

fn tok(input: u64, output: u64, cache_read: u64, cache_write: u64) -> TokenUsage {
    TokenUsage {
        input,
        output,
        cache_read,
        cache_write,
    }
}

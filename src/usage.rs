//! Provider-neutral usage aggregation; formatting belongs to the output adapter.
use anyhow::{Context, Result};
use serde_json::{Value, json};

#[derive(Default)]
pub struct Usage {
    pub attempted: bool,
    requests: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    models: Vec<String>,
    total: u64,
}

#[derive(Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn add(&mut self, provider: &str, model: &str, tokens: TokenUsage) -> Result<()> {
        // Validate every aggregate before mutating state. Error reporting must
        // still be able to print the last valid summary without wrapping/panic.
        let add = |a: u64, b: u64| a.checked_add(b).context("provider usage overflow");
        let requests = add(self.requests, 1)?;
        let input = add(self.input, tokens.input)?;
        let output = add(self.output, tokens.output)?;
        let cache_read = add(self.cache_read, tokens.cache_read)?;
        let cache_write = add(self.cache_write, tokens.cache_write)?;
        let total = add(add(add(input, output)?, cache_read)?, cache_write)?;
        self.requests = requests;
        self.input = input;
        self.output = output;
        self.cache_read = cache_read;
        self.cache_write = cache_write;
        self.total = total;
        let pair = format!("{provider}/{model}");
        if !self.models.contains(&pair) {
            self.models.push(pair);
        }
        Ok(())
    }

    pub fn summary(&self) -> Value {
        json!({"requests":self.requests,"inputTokens":self.input,"outputTokens":self.output,"cacheReadTokens":self.cache_read,"cacheWriteTokens":self.cache_write,"totalTokens":self.total,"costUsd":0,"models":self.models})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overflowing_usage_leaves_a_printable_unchanged_summary() {
        for extra in [1, 2] {
            let mut usage = Usage::default();
            usage
                .add(
                    "test",
                    "first",
                    TokenUsage {
                        input: u64::MAX - 1,
                        output: 1,
                        ..Default::default()
                    },
                )
                .unwrap();
            let before = usage.summary();
            assert!(
                usage
                    .add(
                        "test",
                        "rejected",
                        TokenUsage {
                            input: extra,
                            ..Default::default()
                        }
                    )
                    .is_err()
            );
            assert_eq!(usage.summary(), before);
            assert_eq!(usage.summary()["totalTokens"], u64::MAX);
        }
    }
}

//! Provider-neutral usage aggregation; formatting belongs to the output adapter.
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
}

#[derive(Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn add(&mut self, provider: &str, model: &str, tokens: TokenUsage) {
        self.requests += 1;
        self.input += tokens.input;
        self.output += tokens.output;
        self.cache_read += tokens.cache_read;
        self.cache_write += tokens.cache_write;
        let pair = format!("{provider}/{model}");
        if !self.models.contains(&pair) {
            self.models.push(pair);
        }
    }

    pub fn summary(&self) -> Value {
        json!({"requests":self.requests,"inputTokens":self.input,"outputTokens":self.output,"cacheReadTokens":self.cache_read,"cacheWriteTokens":self.cache_write,"totalTokens":self.input+self.output+self.cache_read+self.cache_write,"costUsd":0,"models":self.models})
    }
}

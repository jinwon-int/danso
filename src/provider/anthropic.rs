use crate::{
    provider::{ModelRequest, Provider},
    session::millis,
    usage::{TokenUsage, Usage},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::time::Duration;

pub struct Anthropic {
    client: reqwest::Client,
    url: reqwest::Url,
    key: String,
    model: String,
}
impl Anthropic {
    pub fn new(model: String, key: String, base: &str) -> Result<Self> {
        Self::new_with_timeout(model, key, base, 60)
    }
    pub fn new_with_timeout(
        model: String,
        key: String,
        base: &str,
        timeout_seconds: u64,
    ) -> Result<Self> {
        ensure!(
            (1..=300).contains(&timeout_seconds),
            "invalid provider timeout"
        );
        ensure!(!key.is_empty(), "ANTHROPIC_API_KEY is empty");
        // Only HTTPS or literal loopback HTTP is allowed; never follow redirects
        // carrying a credential to a different host.
        let url = reqwest::Url::parse(&format!("{}/v1/messages", base.trim_end_matches('/')))?;
        ensure!(
            url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))),
            "provider endpoint requires HTTPS (loopback HTTP is allowed for tests)"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "endpoint must not contain credentials"
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_seconds))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            url,
            key,
            model,
        })
    }
    fn body(&self, request: &ModelRequest<'_>) -> Result<Value> {
        let definitions: Vec<Value> = request
            .tools
            .iter()
            .map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.parameters}))
            .collect();
        let body = json!({"model":self.model,"max_tokens":4096,"system":request.system,"messages":provider_messages(request.messages)?,"tools":definitions});
        Ok(body)
    }
}
impl Provider for Anthropic {
    fn validate_history(&self, messages: &[Value]) -> Result<()> {
        provider_messages(messages).map(|_| ())
    }
    fn request_bytes(&self, request: &ModelRequest<'_>) -> Result<usize> {
        Ok(serde_json::to_vec(&self.body(request)?)?.len())
    }
    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        let body = self.body(&request)?;
        ensure!(
            serde_json::to_vec(&body)?.len() <= 512 * 1024,
            "request context exceeds 512 KiB; start a new session"
        );
        usage.attempted = true;
        let response = self
            .client
            .post(self.url.clone())
            .header("x-api-key", &self.key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .context("provider request failed")?;
        ensure!(
            response.status().is_success(),
            "provider request failed: HTTP {}",
            response.status().as_u16()
        );
        // Bounded response accumulation, including chunked responses.
        let mut response = response;
        let mut bytes = vec![];
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 1024 * 1024,
                "provider response exceeds 1 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        let response: Value = serde_json::from_slice(&bytes)?;
        let response_model = response["model"].as_str().unwrap_or(&self.model);
        let u = &response["usage"];
        usage.add(
            "anthropic",
            response_model,
            TokenUsage {
                input: u["input_tokens"].as_u64().unwrap_or(0),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            },
        )?;
        assistant(&response, response_model)
    }
}

fn provider_messages(messages: &[Value]) -> Result<Vec<Value>> {
    let mut result = Vec::<Value>::new();
    for m in messages {
        let (role, content) = match m["role"].as_str() {
            Some("user") => {
                let content = if let Some(text) = m["content"].as_str() {
                    json!([{"type":"text","text":text}])
                } else {
                    m["content"].clone()
                };
                ensure!(
                    content
                        .as_array()
                        .is_some_and(|blocks| blocks.iter().all(|b| b["type"] == "text")),
                    "v0 supports text user messages only"
                );
                ("user", content)
            }
            Some("assistant") => {
                let mut blocks = vec![];
                for b in m["content"]
                    .as_array()
                    .context("invalid assistant content")?
                {
                    match b["type"].as_str() {
                        Some("text") => blocks.push(b.clone()),
                        Some("toolCall") => blocks.push(json!({"type":"tool_use", "id":b["id"], "name":b["name"], "input":b["arguments"]})),
                        _ => bail!("unsupported assistant block in v0"),
                    }
                }
                ("assistant", json!(blocks))
            }
            Some("toolResult") => (
                "user",
                json!([{"type":"tool_result", "tool_use_id":m["toolCallId"], "content":m["content"], "is_error":m["isError"]}]),
            ),
            _ => bail!("unsupported message role in v0"),
        };
        if let Some(last) = result.last_mut().filter(|e| e["role"] == role) {
            last["content"].as_array_mut().unwrap().extend(
                content
                    .as_array()
                    .context("invalid message content")?
                    .iter()
                    .cloned(),
            );
        } else {
            result.push(json!({"role":role, "content":content}));
        }
    }
    Ok(result)
}

fn assistant(response: &Value, model: &str) -> Result<Value> {
    let mut content = vec![];
    for b in response["content"]
        .as_array()
        .context("provider response lacks content")?
    {
        match b["type"].as_str() {
            Some("text") => {
                ensure!(b["text"].is_string(), "invalid provider text");
                content.push(b.clone());
            }
            Some("tool_use") => {
                ensure!(
                    b["id"].is_string() && b["name"].is_string() && b["input"].is_object(),
                    "invalid provider tool call"
                );
                content.push(json!({"type":"toolCall", "id":b["id"], "name":b["name"], "arguments":b["input"]}));
            }
            _ => bail!("unsupported provider content block"),
        }
    }
    let stop = match response["stop_reason"].as_str() {
        Some("end_turn" | "stop_sequence") => "stop",
        Some("tool_use") => "toolUse",
        Some("max_tokens") => "length",
        _ => bail!("unsupported provider stop reason"),
    };
    ensure!(!content.is_empty(), "empty provider response");
    let has_calls = content.iter().any(|b| b["type"] == "toolCall");
    ensure!(
        has_calls == (stop == "toolUse"),
        "inconsistent provider stop reason"
    );
    let u = &response["usage"];
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Ok(
        json!({"role":"assistant", "content":content,"api":"anthropic-messages","provider":"anthropic","model":model,"timestamp":millis(),"stopReason":stop,"usage":{"input":n("input_tokens"),"output":n("output_tokens"),"cacheRead":n("cache_read_input_tokens"),"cacheWrite":n("cache_creation_input_tokens"),"totalTokens":n("input_tokens")+n("output_tokens")+n("cache_read_input_tokens")+n("cache_creation_input_tokens"),"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}),
    )
}

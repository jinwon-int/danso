use crate::{
    provider::{ModelRequest, Provider, http::Http},
    session::millis,
    usage::{TokenUsage, Usage},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

pub struct Anthropic {
    http: Http,
    headers: reqwest::header::HeaderMap,
    model: String,
    max_output_tokens: u32,
}
impl Anthropic {
    pub fn new(model: String, key: String, base: &str) -> Result<Self> {
        Self::new_with_timeout(model, key, base, super::MAX_OUTPUT_TOKENS_DEFAULT, 180)
    }
    pub fn new_with_timeout(
        model: String,
        key: String,
        base: &str,
        max_output_tokens: u32,
        timeout_seconds: u64,
    ) -> Result<Self> {
        // Anthropic authenticates with x-api-key rather than a Bearer token,
        // but every other transport rule (HTTPS-or-loopback, no credentials,
        // query or fragment in the base URL, no redirects, connect timeout,
        // sensitive credential header, bounded request and response) is the
        // shared one.
        let http = Http::with_auth(
            base,
            "v1/messages",
            reqwest::header::HeaderName::from_static("x-api-key"),
            &key,
            &key,
            timeout_seconds,
        )?;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("anthropic-version"),
            reqwest::header::HeaderValue::from_static("2023-06-01"),
        );
        Ok(Self {
            http,
            headers,
            model,
            max_output_tokens,
        })
    }
    /// Bounded wire-retry budget (issue #67 B); 0 disables.
    pub fn set_retries(&mut self, retries: u32) {
        self.http.set_retries(retries);
    }
    fn body(&self, request: &ModelRequest<'_>) -> Result<Value> {
        let mut definitions: Vec<Value> = request
            .tools
            .iter()
            .map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.parameters}))
            .collect();
        // Issue #69 E: the stable system prefix and the last tool definition
        // carry the two ephemeral cache marks (limit 4); the volatile budget
        // guidance lands after the cached prefix, so only it is re-read.
        if let Some(last_tool) = definitions.last_mut() {
            last_tool["cache_control"] = json!({"type":"ephemeral"});
        }
        let mut system_blocks = Vec::<Value>::new();
        if !request.system.stable.is_empty() {
            system_blocks.push(json!({
                "type":"text","text":request.system.stable,
                "cache_control":{"type":"ephemeral"}
            }));
        }
        if !request.system.volatile.is_empty() {
            system_blocks.push(json!({"type":"text","text":request.system.volatile}));
        }
        let body = json!({"model":self.model,"max_tokens":self.max_output_tokens,"system":system_blocks,"messages":provider_messages(request.messages)?,"tools":definitions});
        Ok(body)
    }
}
impl Provider for Anthropic {
    fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }
    fn validate_history(&self, messages: &[Value]) -> Result<()> {
        provider_messages(messages).map(|_| ())
    }
    fn request_bytes(&self, request: &ModelRequest<'_>) -> Result<usize> {
        Ok(serde_json::to_vec(&self.body(request)?)?.len())
    }
    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        let body = self.body(&request)?;
        // Http enforces the 512 KiB request bound, the 1 MiB response bound,
        // HTTP status and credential-safe transport errors.
        let bytes = self
            .http
            .post_bytes(&body, usage, self.headers.clone())
            .await?;
        let response: Value = serde_json::from_slice(&bytes).context("invalid provider JSON")?;
        let response_model = response["model"]
            .as_str()
            .unwrap_or(&self.model)
            .to_string();
        let tokens = tokens(&response["usage"])?;
        usage.add("anthropic", &response_model, tokens)?;
        assistant(&response, &response_model, &tokens)
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

/// Anthropic reports four flat counters and, unlike the Responses API, does
/// not nest cache reads inside the input count. Absent or non-numeric fields
/// are an error rather than a silent zero: a silent zero is indistinguishable
/// from a genuinely free turn in DANSO_USAGE.
fn tokens(u: &Value) -> Result<TokenUsage> {
    let field = |k: &str| -> Result<u64> {
        match &u[k] {
            Value::Null => Ok(0),
            v => v.as_u64().with_context(|| format!("invalid {k} usage")),
        }
    };
    let input = u["input_tokens"].as_u64().context("missing input usage")?;
    let output = u["output_tokens"]
        .as_u64()
        .context("missing output usage")?;
    let cache_read = field("cache_read_input_tokens")?;
    let cache_write = field("cache_creation_input_tokens")?;
    ensure!(
        input
            .checked_add(output)
            .and_then(|t| t.checked_add(cache_read))
            .and_then(|t| t.checked_add(cache_write))
            .is_some(),
        "invalid usage totals"
    );
    Ok(TokenUsage {
        input,
        output,
        cache_read,
        cache_write,
    })
}

fn assistant(response: &Value, model: &str, t: &TokenUsage) -> Result<Value> {
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
    let total = t.input + t.output + t.cache_read + t.cache_write;
    Ok(
        json!({"role":"assistant", "content":content,"api":"anthropic-messages","provider":"anthropic","model":model,"timestamp":millis(),"stopReason":stop,"usage":{"input":t.input,"output":t.output,"cacheRead":t.cache_read,"cacheWrite":t.cache_write,"totalTokens":total,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic(base: &str) -> Result<Anthropic> {
        Anthropic::new("m".into(), "k".into(), base)
    }

    #[test]
    fn base_url_rejects_credentials_query_and_fragment() {
        assert!(anthropic("https://api.example.com").is_ok());
        // Previously only username/password were rejected here, while the
        // shared transport rejected query and fragment as well.
        for bad in [
            "https://user:pw@api.example.com",
            "https://api.example.com/?key=leak",
            "https://api.example.com/#leak",
            "http://api.example.com",
        ] {
            assert!(anthropic(bad).is_err(), "accepted {bad}");
        }
        assert!(anthropic("http://127.0.0.1:8080").is_ok());
    }

    #[test]
    fn usage_is_validated_rather_than_silently_zeroed() {
        let ok = tokens(&json!({
            "input_tokens": 10, "output_tokens": 5,
            "cache_read_input_tokens": 3, "cache_creation_input_tokens": 2
        }))
        .unwrap();
        assert_eq!(
            (ok.input, ok.output, ok.cache_read, ok.cache_write),
            (10, 5, 3, 2)
        );

        // Absent cache counters are genuinely zero and stay accepted.
        let sparse = tokens(&json!({"input_tokens": 1, "output_tokens": 2})).unwrap();
        assert_eq!((sparse.cache_read, sparse.cache_write), (0, 0));

        // A missing or non-numeric core counter is an error, not a zero:
        // a silent zero is indistinguishable from a free turn in DANSO_USAGE.
        assert!(tokens(&json!({"output_tokens": 5})).is_err());
        assert!(tokens(&json!({"input_tokens": 10})).is_err());
        assert!(tokens(&json!({"input_tokens": "10", "output_tokens": 5})).is_err());
        assert!(
            tokens(&json!({
                "input_tokens": 1, "output_tokens": 2, "cache_read_input_tokens": -1
            }))
            .is_err()
        );
        // Totals must not wrap.
        assert!(
            tokens(&json!({
                "input_tokens": u64::MAX, "output_tokens": 1
            }))
            .is_err()
        );
    }

    #[test]
    fn assistant_reports_the_validated_usage() {
        let t = TokenUsage {
            input: 10,
            output: 5,
            cache_read: 3,
            cache_write: 2,
        };
        let response = json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            // A usage block here must not be re-read: the validated value wins.
            "usage": {"input_tokens": 999}
        });
        let m = assistant(&response, "m", &t).unwrap();
        assert_eq!(m["usage"]["input"], 10);
        assert_eq!(m["usage"]["totalTokens"], 20);
    }
}

#[cfg(test)]
mod cache_split_tests {
    use super::*;
    use crate::provider::SystemParts;

    /// Issue #69 E: the stable system block and the last tool definition
    /// carry the two ephemeral marks; the volatile block stays uncached.
    #[test]
    fn system_split_renders_cache_marked_blocks() {
        let adapter = Anthropic::new("m".into(), "k".into(), "https://api.example.com").unwrap();
        let tools = vec![crate::contracts::ToolDefinition {
            name: "read".into(),
            description: "fixture".into(),
            parameters: json!({"type":"object"}),
        }];
        let body = adapter
            .body(&ModelRequest {
                system: SystemParts {
                    stable: "STABLE_PREFIX",
                    volatile: "VOLATILE_GUIDANCE",
                },
                messages: std::slice::from_ref(&json!({
                    "role":"user","content":[{"type":"text","text":"hi"}]
                })),
                tools: &tools,
            })
            .unwrap();
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], json!("STABLE_PREFIX"));
        assert_eq!(system[0]["cache_control"], json!({"type":"ephemeral"}));
        assert_eq!(system[1]["text"], json!("VOLATILE_GUIDANCE"));
        assert!(system[1].get("cache_control").is_none());
        let wire_tools = body["tools"].as_array().unwrap();
        assert_eq!(wire_tools[0]["cache_control"], json!({"type":"ephemeral"}));
        // request_bytes serializes the block form, so compaction budget
        // checks measure exactly what the adapter puts on the wire.
        let bytes = crate::provider::Provider::request_bytes(
            &adapter,
            &ModelRequest {
                system: SystemParts {
                    stable: "STABLE_PREFIX",
                    volatile: "VOLATILE_GUIDANCE",
                },
                messages: std::slice::from_ref(&json!({
                    "role":"user","content":[{"type":"text","text":"hi"}]
                })),
                tools: &tools,
            },
        )
        .unwrap();
        assert!(bytes > 0);
    }

    /// No tools and no volatile tail: a single marked block, no tool mark.
    #[test]
    fn minimal_request_carries_only_the_system_mark() {
        let adapter = Anthropic::new("m".into(), "k".into(), "https://api.example.com").unwrap();
        let body = adapter
            .body(&ModelRequest {
                system: SystemParts::single("ONLY"),
                messages: std::slice::from_ref(&json!({
                    "role":"user","content":[{"type":"text","text":"hi"}]
                })),
                tools: &[],
            })
            .unwrap();
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["cache_control"], json!({"type":"ephemeral"}));
        assert!(body["tools"].as_array().unwrap().is_empty());
    }
}

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
    request_budget_bytes: usize,
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
        let request_budget_bytes = super::effective_request_budget("anthropic", &model);
        let http = Http::with_auth_with_budget(
            base,
            "v1/messages",
            reqwest::header::HeaderName::from_static("x-api-key"),
            &key,
            &key,
            timeout_seconds,
            request_budget_bytes,
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
            request_budget_bytes,
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
        let body = json!({"model":self.model,"max_tokens":self.max_output_tokens,"system":system_blocks,"messages":provider_messages(request.messages)?,"tools":definitions,"stream":true});
        Ok(body)
    }
    fn non_streaming_body(&self, request: &ModelRequest<'_>) -> Result<Value> {
        let mut body = self.body(request)?;
        body["stream"] = json!(false);
        Ok(body)
    }
}
impl Provider for Anthropic {
    fn request_budget_bytes(&self) -> usize {
        self.request_budget_bytes
    }

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
        let body = self.non_streaming_body(&request)?;
        // Http enforces the provider/model request budget, the 1 MiB response
        // bound, HTTP status and credential-safe transport errors.
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
    async fn complete_streaming(
        &mut self,
        request: ModelRequest<'_>,
        usage: &mut Usage,
        on_delta: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<Value> {
        let body = self.body(&request)?;
        let mut stream = AnthropicStream::default();
        self.http
            .post_sse(&body, usage, self.headers.clone(), |frame| {
                stream
                    .event(frame, on_delta)
                    .map_err(crate::failure::provider_context(
                        crate::failure::ProviderReason::InvalidStream,
                    ))
            })
            .await?;
        let response = stream.response()?;
        let response_model = response["model"]
            .as_str()
            .unwrap_or(&self.model)
            .to_string();
        let tokens = tokens(&response["usage"])?;
        usage.add("anthropic", &response_model, tokens)?;
        assistant(&response, &response_model, &tokens)
    }
}

#[derive(Default)]
struct AnthropicStream {
    model: Option<String>,
    usage: Option<Value>,
    blocks: Vec<AnthropicBlock>,
    stop_reason: Option<String>,
    message_started: bool,
    message_delta_seen: bool,
    message_stopped: bool,
}

struct AnthropicBlock {
    kind: &'static str,
    text: String,
    id: Option<String>,
    name: Option<String>,
    input_json: String,
    stopped: bool,
}

impl AnthropicStream {
    fn event(
        &mut self,
        frame: &[u8],
        on_delta: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<bool> {
        let Some((event, value)) = parse_sse_frame(frame)? else {
            return Ok(false);
        };
        let kind = value["type"]
            .as_str()
            .context("missing Anthropic SSE event type")?;
        ensure!(
            event.as_deref().is_none_or(|event| event == kind),
            "Anthropic SSE event type mismatch"
        );
        match kind {
            "message_start" => {
                ensure!(!self.message_started, "duplicate Anthropic message_start");
                let message = &value["message"];
                ensure!(
                    message["role"] == "assistant",
                    "invalid Anthropic message role"
                );
                self.model = message["model"].as_str().map(str::to_owned);
                ensure!(
                    message["usage"].is_object(),
                    "missing Anthropic start usage"
                );
                self.usage = Some(message["usage"].clone());
                self.message_started = true;
            }
            "content_block_start" => {
                ensure!(
                    self.message_started,
                    "Anthropic content before message_start"
                );
                let index = value["index"]
                    .as_u64()
                    .context("missing Anthropic block index")?;
                ensure!(
                    index == self.blocks.len() as u64,
                    "out-of-order Anthropic content block"
                );
                let block = &value["content_block"];
                match block["type"].as_str() {
                    Some("text") => {
                        let text = block["text"].as_str().unwrap_or_default().to_owned();
                        if !text.is_empty() {
                            on_delta(&text)?;
                        }
                        self.blocks.push(AnthropicBlock {
                            kind: "text",
                            text,
                            id: None,
                            name: None,
                            input_json: String::new(),
                            stopped: false,
                        });
                    }
                    Some("tool_use") => {
                        let id = block["id"].as_str().context("missing Anthropic tool id")?;
                        let name = block["name"]
                            .as_str()
                            .context("missing Anthropic tool name")?;
                        ensure!(block["input"].is_object(), "invalid Anthropic tool input");
                        ensure!(
                            block["input"]
                                .as_object()
                                .is_some_and(|input| input.is_empty()),
                            "nonempty Anthropic initial tool input"
                        );
                        self.blocks.push(AnthropicBlock {
                            kind: "tool_use",
                            text: String::new(),
                            id: Some(id.to_owned()),
                            name: Some(name.to_owned()),
                            input_json: String::new(),
                            stopped: false,
                        });
                    }
                    _ => {
                        return Err(crate::failure::provider_error(
                            crate::failure::ProviderReason::UnsupportedStreamEvent,
                        ));
                    }
                }
            }
            "content_block_delta" => {
                let index = value["index"]
                    .as_u64()
                    .context("missing Anthropic block index")?;
                let block = self
                    .blocks
                    .get_mut(usize::try_from(index).context("invalid Anthropic block index")?)
                    .context("Anthropic delta for unknown block")?;
                ensure!(!block.stopped, "Anthropic delta after block stop");
                let delta = &value["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        ensure!(block.kind == "text", "Anthropic text delta for tool block");
                        let text = delta["text"]
                            .as_str()
                            .context("missing Anthropic text delta")?;
                        if !text.is_empty() {
                            on_delta(text)?;
                        }
                        block.text.push_str(text);
                    }
                    Some("input_json_delta") => {
                        ensure!(
                            block.kind == "tool_use",
                            "Anthropic input delta for text block"
                        );
                        block.input_json.push_str(
                            delta["partial_json"]
                                .as_str()
                                .context("missing Anthropic input delta")?,
                        );
                    }
                    _ => {
                        return Err(crate::failure::provider_error(
                            crate::failure::ProviderReason::UnsupportedStreamEvent,
                        ));
                    }
                }
            }
            "content_block_stop" => {
                let index = value["index"]
                    .as_u64()
                    .context("missing Anthropic block index")?;
                let block = self
                    .blocks
                    .get_mut(usize::try_from(index).context("invalid Anthropic block index")?)
                    .context("Anthropic stop for unknown block")?;
                ensure!(!block.stopped, "duplicate Anthropic block stop");
                block.stopped = true;
            }
            "message_delta" => {
                ensure!(self.message_started, "Anthropic delta before message_start");
                ensure!(
                    !self.message_delta_seen,
                    "duplicate Anthropic message_delta"
                );
                self.stop_reason = Some(
                    value["delta"]["stop_reason"]
                        .as_str()
                        .context("missing Anthropic stop reason")?
                        .to_owned(),
                );
                let output = value["usage"]["output_tokens"]
                    .as_u64()
                    .context("missing Anthropic output usage")?;
                self.usage
                    .as_mut()
                    .context("missing Anthropic start usage")?["output_tokens"] = json!(output);
                self.message_delta_seen = true;
            }
            "message_stop" => {
                ensure!(self.message_started, "Anthropic stop before message_start");
                ensure!(!self.message_stopped, "duplicate Anthropic message_stop");
                ensure!(
                    self.message_delta_seen,
                    "Anthropic stop without message_delta"
                );
                ensure!(
                    self.blocks.iter().all(|block| block.stopped),
                    "Anthropic block did not stop"
                );
                self.message_stopped = true;
                return Ok(true);
            }
            "ping" => {}
            "error" => {
                return Err(crate::failure::provider_error(
                    crate::failure::ProviderReason::ResponseError,
                ));
            }
            _ => {
                return Err(crate::failure::provider_error(
                    crate::failure::ProviderReason::UnsupportedStreamEvent,
                ));
            }
        }
        Ok(false)
    }

    fn response(&self) -> Result<Value> {
        ensure!(self.message_stopped, "incomplete Anthropic stream");
        let mut content = Vec::with_capacity(self.blocks.len());
        for block in &self.blocks {
            match block.kind {
                "text" => content.push(json!({"type":"text", "text":block.text})),
                "tool_use" => {
                    let input = if block.input_json.is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&block.input_json)
                            .context("invalid Anthropic streamed tool input")?
                    };
                    ensure!(
                        input.is_object(),
                        "Anthropic streamed tool input is not an object"
                    );
                    content.push(
                        json!({"type":"tool_use","id":block.id,"name":block.name,"input":input}),
                    );
                }
                _ => unreachable!("validated Anthropic block kind"),
            }
        }
        Ok(json!({
            "model": self.model.as_deref().unwrap_or(""),
            "content": content,
            "stop_reason": self.stop_reason,
            "usage": self.usage.clone().context("missing Anthropic usage")?
        }))
    }
}

fn parse_sse_frame(frame: &[u8]) -> Result<Option<(Option<String>, Value)>> {
    let text = std::str::from_utf8(frame).context("invalid Anthropic SSE encoding")?;
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = normalized.trim_end_matches('\n');
    let mut event = None;
    let mut data = Vec::new();
    for line in normalized.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            ensure!(event.is_none(), "duplicate Anthropic SSE event field");
            event = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        } else if !line.is_empty() && !line.starts_with(':') {
            bail!("unsupported Anthropic SSE field");
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    let value = serde_json::from_str(&data.join("\n")).context("invalid Anthropic SSE JSON")?;
    Ok(Some((event, value)))
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

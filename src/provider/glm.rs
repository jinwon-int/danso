//! Z.AI Chat Completions (GLM). Preserve reasoning across tool turns.
//! Endpoint presets and the thinking toggle are issue #70 A/B.
use super::{ModelRequest, Provider, http::Http, wire};
use crate::usage::Usage;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

pub const GLM_GENERAL_BASE_URL: &str = "https://api.z.ai/api/paas/v4";
pub const GLM_CODING_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

/// Resolve the thinking toggle (issue #70 A): explicit flag first, then
/// `DANSO_GLM_THINKING`, then enabled. Anything other than
/// enabled|disabled is a configuration error, never a silent default.
pub fn resolve_thinking(flag: Option<&str>) -> Result<bool> {
    let from_env = || -> Result<Option<bool>> {
        let Some(raw) = std::env::var_os("DANSO_GLM_THINKING") else {
            return Ok(None);
        };
        let raw = raw.to_string_lossy().trim().to_string();
        if raw.is_empty() {
            return Ok(None);
        }
        Some(parse_thinking(&raw)).transpose()
    };
    match flag {
        Some(flag) => parse_thinking(flag),
        None => Ok(from_env()?.unwrap_or(true)),
    }
}

fn parse_thinking(value: &str) -> Result<bool> {
    ensure!(
        matches!(value, "enabled" | "disabled"),
        "GLM thinking must be enabled or disabled"
    );
    Ok(value == "enabled")
}

/// Resolve the GLM base URL (issue #70 B): an explicit `DANSO_GLM_BASE_URL`
/// wins, but when an endpoint preset is also selected and the two disagree
/// the configuration is ambiguous and fails closed. Unambiguous presets map
/// to the documented Z.AI endpoints.
pub fn resolve_base_url(endpoint: Option<&str>, explicit_base: Option<&str>) -> Result<String> {
    let parse_endpoint = |value: &str| -> Result<&'static str> {
        match value {
            "general" => Ok(GLM_GENERAL_BASE_URL),
            "coding" => Ok(GLM_CODING_BASE_URL),
            _ => bail!("GLM endpoint must be general or coding"),
        }
    };
    let preset = match endpoint {
        Some(value) => Some(parse_endpoint(value)?),
        None => match std::env::var("DANSO_GLM_ENDPOINT") {
            Ok(raw) if !raw.trim().is_empty() => Some(parse_endpoint(raw.trim())?),
            _ => None,
        },
    };
    match explicit_base {
        Some(base) => {
            if let Some(preset) = preset {
                ensure!(
                    base == preset,
                    "DANSO_GLM_BASE_URL conflicts with the selected GLM endpoint preset"
                );
            }
            Ok(base.to_string())
        }
        None => Ok(preset.unwrap_or(GLM_GENERAL_BASE_URL).to_string()),
    }
}

pub struct Glm {
    http: Http,
    model: String,
    effort: Option<String>,
    max_output_tokens: u32,
    thinking: bool,
    request_budget_bytes: usize,
}
impl Glm {
    pub fn new(model: String, key: String, base: &str, effort: Option<String>) -> Result<Self> {
        Self::new_with_timeout(
            model,
            key,
            base,
            effort,
            super::MAX_OUTPUT_TOKENS_DEFAULT,
            true,
            180,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_timeout(
        model: String,
        key: String,
        base: &str,
        effort: Option<String>,
        max_output_tokens: u32,
        thinking: bool,
        timeout_seconds: u64,
    ) -> Result<Self> {
        let request_budget_bytes = super::effective_request_budget("glm", &model);
        let mut http = Http::new_with_budget(
            base,
            "chat/completions",
            &key,
            timeout_seconds,
            request_budget_bytes,
        )?;
        http.enable_zai_diagnostics();
        Ok(Self {
            http,
            model,
            effort,
            max_output_tokens,
            thinking,
            request_budget_bytes,
        })
    }
    /// Bounded wire-retry budget (issue #67 B); 0 disables.
    pub fn set_retries(&mut self, retries: u32) {
        self.http.set_retries(retries);
    }
    fn body(&self, request: &ModelRequest<'_>) -> Result<Value> {
        let mut messages = vec![json!({"role":"system","content":request.system.joined()})];
        messages.extend(history(request.messages)?);
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({"type":"function","function":{
            "name":t.name,"description":t.description,"parameters":t.parameters}})
            })
            .collect();
        let mut body = json!({"model":self.model,"messages":messages,"tools":tools,"stream":true,"stream_options":{"include_usage":true},
            "max_tokens":self.max_output_tokens,"thinking":{"type":if self.thinking {"enabled"} else {"disabled"},"clear_thinking":false}});
        if let Some(effort) = &self.effort {
            body["reasoning_effort"] = json!(effort);
        }
        Ok(body)
    }
    fn non_streaming_body(&self, request: &ModelRequest<'_>) -> Result<Value> {
        let mut body = self.body(request)?;
        body["stream"] = json!(false);
        body.as_object_mut()
            .context("invalid GLM request body")?
            .remove("stream_options");
        Ok(body)
    }
}
impl Provider for Glm {
    fn request_budget_bytes(&self) -> usize {
        self.request_budget_bytes
    }

    fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }
    fn validate_history(&self, messages: &[Value]) -> Result<()> {
        history(messages).map(|_| ())
    }
    fn request_bytes(&self, request: &ModelRequest<'_>) -> Result<usize> {
        Ok(serde_json::to_vec(&self.body(request)?)?.len())
    }
    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        let body = self.non_streaming_body(&request)?;
        let response = self.http.post(&body, usage).await?;
        response_message(response, &self.model, usage, "glm")
    }
    async fn complete_streaming(
        &mut self,
        request: ModelRequest<'_>,
        usage: &mut Usage,
        on_delta: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<Value> {
        let body = self.body(&request)?;
        let mut stream = GlmStream::default();
        self.http
            .post_sse(&body, usage, reqwest::header::HeaderMap::new(), |frame| {
                stream
                    .event(frame, on_delta)
                    .map_err(crate::failure::provider_context(
                        crate::failure::ProviderReason::InvalidStream,
                    ))
            })
            .await?;
        let response = stream.response()?;
        response_message(response, &self.model, usage, "glm")
    }
}

fn response_message(
    response: Value,
    default_model: &str,
    usage: &mut Usage,
    provider: &str,
) -> Result<Value> {
    let t = wire::tokens(
        &response["usage"],
        "prompt_tokens",
        "completion_tokens",
        "prompt_tokens_details",
    )?;
    let model = response["model"].as_str().unwrap_or(default_model);
    usage.add(
        provider,
        model,
        crate::usage::TokenUsage {
            input: t.input,
            output: t.output,
            cache_read: t.cache_read,
            cache_write: 0,
        },
    )?;
    let choices = response["choices"]
        .as_array()
        .context("missing GLM choices")?;
    ensure!(choices.len() == 1, "expected exactly one GLM choice");
    let choice = &choices[0];
    let m = &choice["message"];
    ensure!(m["role"] == "assistant", "invalid GLM message role");
    let mut content = vec![];
    if !m["content"].is_null() {
        let text = wire::string(m, "content")?;
        if !text.is_empty() {
            content.push(json!({"type":"text","text":text}));
        }
    }
    if !m["tool_calls"].is_null() {
        for c in m["tool_calls"]
            .as_array()
            .context("invalid GLM tool calls")?
        {
            ensure!(c["type"] == "function", "unsupported GLM tool type");
            content.push(wire::call(
                wire::nonempty(c, "id")?,
                wire::nonempty(&c["function"], "name")?,
                wire::arguments(&c["function"]["arguments"])?,
            )?);
        }
    }
    let has_calls = content.iter().any(|b| b["type"] == "toolCall");
    let stop = match choice["finish_reason"].as_str() {
        Some(reason) if has_calls && reason == "tool_calls" => "toolUse",
        Some(reason) if !has_calls && (reason == "stop" || reason == "length") => reason,
        _ => bail!("GLM response did not complete consistently"),
    };
    let mut message = wire::message(content, "glm", "openai-completions", model, &t)?;
    message["stopReason"] = json!(stop);
    if !m["reasoning_content"].is_null() {
        message["dansoGlmReasoning"] = json!(wire::string(m, "reasoning_content")?);
    }
    Ok(message)
}

#[derive(Default)]
struct GlmStream {
    model: Option<String>,
    content: String,
    reasoning: String,
    tool_calls: Vec<GlmToolCall>,
    finish_reason: Option<String>,
    usage: Option<Value>,
}

#[derive(Default)]
struct GlmToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl GlmStream {
    fn event(
        &mut self,
        frame: &[u8],
        on_delta: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<bool> {
        let Some(data) = parse_sse_frame(frame)? else {
            return Ok(false);
        };
        if data.trim() == "[DONE]" {
            ensure!(
                self.finish_reason.is_some(),
                "GLM stream ended without finish reason"
            );
            ensure!(
                self.usage.as_ref().is_some_and(Value::is_object),
                "GLM stream ended without usage"
            );
            return Ok(true);
        }
        let value: Value = serde_json::from_str(&data).context("invalid GLM SSE JSON")?;
        if value["error"].is_object() {
            return Err(crate::failure::provider_error(
                crate::failure::ProviderReason::ResponseError,
            ));
        }
        if let Some(model) = value["model"].as_str() {
            if let Some(previous) = &self.model {
                ensure!(previous == model, "GLM stream model changed");
            } else {
                self.model = Some(model.to_owned());
            }
        }
        let choices = value["choices"]
            .as_array()
            .context("missing GLM streamed choices")?;
        ensure!(
            choices.len() <= 1,
            "expected at most one GLM streamed choice"
        );
        if let Some(choice) = choices.first() {
            ensure!(
                choice["index"].as_u64().unwrap_or(0) == 0,
                "invalid GLM streamed choice index"
            );
            let delta = &choice["delta"];
            ensure!(delta.is_object(), "missing GLM streamed delta");
            if let Some(role) = delta["role"].as_str() {
                ensure!(role == "assistant", "invalid GLM streamed role");
            }
            if let Some(text) = delta["content"].as_str() {
                if !text.is_empty() {
                    on_delta(text)?;
                }
                self.content.push_str(text);
            }
            if let Some(text) = delta["reasoning_content"].as_str() {
                self.reasoning.push_str(text);
            }
            if let Some(tool_calls) = delta["tool_calls"].as_array() {
                for tool in tool_calls {
                    let index = tool["index"].as_u64().context("missing GLM tool index")?;
                    ensure!(
                        index <= self.tool_calls.len() as u64,
                        "out-of-order GLM tool call"
                    );
                    if index == self.tool_calls.len() as u64 {
                        self.tool_calls.push(GlmToolCall::default());
                    }
                    let call = &mut self.tool_calls
                        [usize::try_from(index).context("invalid GLM tool index")?];
                    if let Some(id) = tool["id"].as_str() {
                        if !call.id.is_empty() {
                            ensure!(call.id == id, "GLM tool call id changed");
                        } else {
                            call.id = id.to_owned();
                        }
                    }
                    let function = &tool["function"];
                    if let Some(name) = function["name"].as_str() {
                        if !call.name.is_empty() {
                            ensure!(call.name == name, "GLM tool name changed");
                        } else {
                            call.name = name.to_owned();
                        }
                    }
                    if let Some(arguments) = function["arguments"].as_str() {
                        call.arguments.push_str(arguments);
                    }
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                if let Some(previous) = &self.finish_reason {
                    ensure!(previous == reason, "GLM finish reason changed");
                } else {
                    self.finish_reason = Some(reason.to_owned());
                }
            }
        }
        if !value["usage"].is_null() {
            ensure!(value["usage"].is_object(), "invalid GLM streamed usage");
            self.usage = Some(value["usage"].clone());
        }
        Ok(false)
    }

    fn response(self) -> Result<Value> {
        let finish_reason = self
            .finish_reason
            .context("GLM stream has no finish reason")?;
        let usage = self.usage.context("GLM stream has no usage")?;
        let mut message = json!({
            "role": "assistant",
            "content": if self.content.is_empty() { Value::Null } else { json!(self.content) }
        });
        if !self.tool_calls.is_empty() {
            let mut calls = Vec::with_capacity(self.tool_calls.len());
            for call in self.tool_calls {
                ensure!(
                    !call.id.is_empty() && !call.name.is_empty(),
                    "incomplete GLM tool call"
                );
                calls.push(json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}));
            }
            message["tool_calls"] = json!(calls);
        }
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = json!(self.reasoning);
        }
        Ok(json!({
            "model": self.model.unwrap_or_default(),
            "choices": [{"index":0,"message":message,"finish_reason":finish_reason}],
            "usage": usage
        }))
    }
}

fn parse_sse_frame(frame: &[u8]) -> Result<Option<String>> {
    let text = std::str::from_utf8(frame).context("invalid GLM SSE encoding")?;
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = normalized.trim_end_matches('\n');
    let mut data = Vec::new();
    for line in normalized.lines() {
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        } else if !line.is_empty() && !line.starts_with(':') && !line.starts_with("event:") {
            bail!("unsupported GLM SSE field");
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(data.join("\n")))
}
fn history(messages: &[Value]) -> Result<Vec<Value>> {
    let mut result = vec![];
    for m in messages {
        match m["role"].as_str() {
            Some("user") => result.push(json!({"role":"user","content":wire::text(&m["content"])?})),
            Some("toolResult") => result.push(json!({"role":"tool","tool_call_id":wire::nonempty(m,"toolCallId")?,
                "content":serde_json::to_string(&json!({"isError":m["isError"],"output":wire::text(&m["content"])?}))?})),
            Some("assistant") => {
                let mut texts = vec![];
                let mut calls = vec![];
                for b in wire::assistant_blocks(m)? {
                    if b["type"] == "text" { texts.push(wire::string(b,"text")?); }
                    else { calls.push(json!({"id":b["id"],"type":"function","function":{
                        "name":b["name"],"arguments":serde_json::to_string(&b["arguments"])?}})); }
                }
                let mut msg = json!({"role":"assistant","content":texts.join("\n")});
                if !calls.is_empty() { msg["tool_calls"] = json!(calls); }
                if m.get("dansoGlmReasoning").is_some() { msg["reasoning_content"] = json!(wire::string(m,"dansoGlmReasoning")?); }
                result.push(msg);
            }
            _ => bail!("unsupported message role"),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_resolution_is_flag_then_env_then_enabled() {
        // SAFETY(unsafe): this test binary's only reader/writer of
        // DANSO_GLM_THINKING; single test owns the process-global variable.
        unsafe { std::env::remove_var("DANSO_GLM_THINKING") };
        assert!(resolve_thinking(None).unwrap());
        assert!(!resolve_thinking(Some("disabled")).unwrap());
        assert!(resolve_thinking(Some("enabled")).unwrap());
        unsafe { std::env::set_var("DANSO_GLM_THINKING", "disabled") };
        assert!(!resolve_thinking(None).unwrap());
        // Explicit flag wins over the environment.
        assert!(resolve_thinking(Some("enabled")).unwrap());
        unsafe { std::env::set_var("DANSO_GLM_THINKING", "true") };
        assert!(resolve_thinking(None).is_err());
        unsafe { std::env::remove_var("DANSO_GLM_THINKING") };
        assert!(resolve_thinking(Some("true")).is_err());
    }

    #[test]
    fn base_url_resolves_presets_and_fails_closed_on_conflicts() {
        // SAFETY(unsafe): this test binary's only reader/writer of
        // DANSO_GLM_ENDPOINT; single test owns the process-global variable.
        unsafe { std::env::remove_var("DANSO_GLM_ENDPOINT") };
        assert_eq!(resolve_base_url(None, None).unwrap(), GLM_GENERAL_BASE_URL);
        assert_eq!(
            resolve_base_url(Some("coding"), None).unwrap(),
            GLM_CODING_BASE_URL
        );
        // Explicit base wins when the preset is absent...
        assert_eq!(
            resolve_base_url(None, Some("http://127.0.0.1:9/api")).unwrap(),
            "http://127.0.0.1:9/api"
        );
        // ...and conflicts are configuration errors, never silent rewrites.
        assert!(resolve_base_url(Some("coding"), Some("http://127.0.0.1:9/api")).is_err());
        assert_eq!(
            resolve_base_url(Some("coding"), Some(GLM_CODING_BASE_URL)).unwrap(),
            GLM_CODING_BASE_URL,
            "an explicit base matching the preset is not a conflict"
        );
        assert!(resolve_base_url(Some("staging"), None).is_err());
        unsafe { std::env::set_var("DANSO_GLM_ENDPOINT", "coding") };
        assert_eq!(resolve_base_url(None, None).unwrap(), GLM_CODING_BASE_URL);
        assert!(
            resolve_base_url(None, Some("http://127.0.0.1:9/api")).is_err(),
            "env preset and explicit base must agree"
        );
        unsafe { std::env::remove_var("DANSO_GLM_ENDPOINT") };
    }

    #[test]
    fn body_reflects_thinking_toggle() {
        let build = |thinking: bool| {
            Glm::new_with_timeout(
                "m".into(),
                "k".into(),
                GLM_GENERAL_BASE_URL,
                None,
                4096,
                thinking,
                60,
            )
            .unwrap()
            .body(&ModelRequest {
                system: crate::provider::SystemParts::single("s"),
                messages: std::slice::from_ref(&json!({"role":"user","content":"hi"})),
                tools: &[],
            })
            .unwrap()
        };
        assert_eq!(build(true)["thinking"]["type"], json!("enabled"));
        assert_eq!(build(false)["thinking"]["type"], json!("disabled"));
        assert_eq!(build(false)["thinking"]["clear_thinking"], json!(false));
        assert_eq!(build(false)["max_tokens"], json!(4096));
    }
}

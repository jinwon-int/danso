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
        let mut http = Http::new(base, "chat/completions", &key, timeout_seconds)?;
        http.enable_zai_diagnostics();
        Ok(Self {
            http,
            model,
            effort,
            max_output_tokens,
            thinking,
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
        let mut body = json!({"model":self.model,"messages":messages,"tools":tools,"stream":false,
            "max_tokens":self.max_output_tokens,"thinking":{"type":if self.thinking {"enabled"} else {"disabled"},"clear_thinking":false}});
        if let Some(effort) = &self.effort {
            body["reasoning_effort"] = json!(effort);
        }
        Ok(body)
    }
}
impl Provider for Glm {
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
        let body = self.body(&request)?;
        let response = self.http.post(&body, usage).await?;
        let t = wire::tokens(
            &response["usage"],
            "prompt_tokens",
            "completion_tokens",
            "prompt_tokens_details",
        )?;
        let model = response["model"].as_str().unwrap_or(&self.model);
        usage.add(
            "glm",
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

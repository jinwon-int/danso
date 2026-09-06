//! Z.AI Chat Completions (GLM). Preserve reasoning across tool turns.
use super::{ModelRequest, Provider, http::Http, wire};
use crate::usage::Usage;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

pub struct Glm {
    http: Http,
    model: String,
    effort: Option<String>,
}
impl Glm {
    pub fn new(model: String, key: String, base: &str, effort: Option<String>) -> Result<Self> {
        Self::new_with_timeout(model, key, base, effort, 60)
    }
    pub fn new_with_timeout(
        model: String,
        key: String,
        base: &str,
        effort: Option<String>,
        timeout_seconds: u64,
    ) -> Result<Self> {
        Ok(Self {
            http: Http::new(base, "chat/completions", &key, timeout_seconds)?,
            model,
            effort,
        })
    }
    fn body(&self, request: &ModelRequest<'_>) -> Result<Value> {
        let mut messages = vec![json!({"role":"system","content":request.system})];
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
            "max_tokens":4096,"thinking":{"type":"enabled","clear_thinking":false}});
        if let Some(effort) = &self.effort {
            body["reasoning_effort"] = json!(effort);
        }
        Ok(body)
    }
}
impl Provider for Glm {
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
        ensure!(
            if has_calls {
                choice["finish_reason"] == "tool_calls"
            } else {
                choice["finish_reason"] == "stop"
            },
            "GLM response did not complete consistently"
        );
        let mut message = wire::message(content, "glm", "openai-completions", model, &t)?;
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

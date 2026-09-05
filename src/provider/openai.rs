//! OpenAI Responses API, stateless history with opaque reasoning preserved.
use super::{ModelRequest, Provider, http::Http, wire};
use crate::usage::Usage;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

pub struct OpenAi {
    http: Http,
    model: String,
    effort: Option<String>,
}
impl OpenAi {
    pub fn new(model: String, key: String, base: &str, effort: Option<String>) -> Result<Self> {
        Ok(Self {
            http: Http::new(base, "responses", &key)?,
            model,
            effort,
        })
    }
    fn body(&self, request: &ModelRequest<'_>) -> Result<Value> {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({"type":"function","name":t.name,
            "description":t.description,"parameters":t.parameters,"strict":false})
            })
            .collect();
        let mut body = json!({"model":self.model,"instructions":request.system,"input":history(request.messages)?,
            "tools":tools,"store":false,"include":["reasoning.encrypted_content"],"max_output_tokens":4096});
        if let Some(effort) = &self.effort {
            body["reasoning"] = json!({"effort":effort});
        }
        Ok(body)
    }
}
impl Provider for OpenAi {
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
            "input_tokens",
            "output_tokens",
            "input_tokens_details",
        )?;
        let model = response["model"].as_str().unwrap_or(&self.model);
        usage.add(
            "openai",
            model,
            crate::usage::TokenUsage {
                input: t.input,
                output: t.output,
                cache_read: t.cache_read,
                cache_write: 0,
            },
        )?;
        ensure!(
            response["status"] == "completed" && response["error"].is_null(),
            "OpenAI response did not complete"
        );
        let output = response["output"]
            .as_array()
            .context("missing OpenAI output")?;
        let content = output_content(output)?;
        let mut message = wire::message(content, "openai", "openai-responses", model, &t)?;
        message["dansoOpenAIOutput"] = json!(output);
        Ok(message)
    }
}

fn output_content(output: &[Value]) -> Result<Vec<Value>> {
    let mut content = vec![];
    for item in output {
        match item["type"].as_str() {
            Some("reasoning") => {
                wire::nonempty(item, "id")?;
                wire::nonempty(item, "encrypted_content")?;
                ensure!(item["summary"].is_array(), "invalid reasoning summary");
            }
            Some("message") => {
                ensure!(
                    item["role"] == "assistant" && item["status"] == "completed",
                    "incomplete assistant message"
                );
                for b in item["content"]
                    .as_array()
                    .context("missing message content")?
                {
                    match b["type"].as_str() {
                        Some("output_text") => {
                            content.push(json!({"type":"text","text":wire::string(b,"text")?}))
                        }
                        Some("refusal") => {
                            content.push(json!({"type":"text","text":wire::string(b,"refusal")?}))
                        }
                        _ => bail!("unsupported OpenAI message content"),
                    }
                }
            }
            Some("function_call") => {
                ensure!(
                    item["status"].is_null() || item["status"] == "completed",
                    "incomplete function call"
                );
                content.push(wire::call(
                    wire::nonempty(item, "call_id")?,
                    wire::nonempty(item, "name")?,
                    wire::arguments(&item["arguments"])?,
                )?);
            }
            _ => bail!("unsupported OpenAI output item"),
        }
    }
    Ok(content)
}
fn history(messages: &[Value]) -> Result<Vec<Value>> {
    let mut input = vec![];
    for m in messages {
        match m["role"].as_str() {
            Some("user") => input.push(json!({"role":"user","content":wire::text(&m["content"])?})),
            Some("toolResult") => input.push(json!({"type":"function_call_output","call_id":wire::nonempty(m,"toolCallId")?,
                "output":serde_json::to_string(&json!({"isError":m["isError"],"output":wire::text(&m["content"])?}))?})),
            Some("assistant") => {
                let blocks = wire::assistant_blocks(m)?;
                if let Some(output) = m.get("dansoOpenAIOutput") {
                    let output = output.as_array().context("invalid saved OpenAI output")?;
                    ensure!(output_content(output)? == *blocks, "saved OpenAI output disagrees with transcript");
                    input.extend(output.iter().cloned());
                } else {
                    ensure!(m["api"] != "openai-responses", "OpenAI session lacks preserved output");
                    for b in blocks {
                        if b["type"] == "text" { input.push(json!({"role":"assistant","content":b["text"]})); }
                        else { input.push(json!({"type":"function_call","call_id":b["id"],"name":b["name"],
                            "arguments":serde_json::to_string(&b["arguments"])?})); }
                    }
                }
            }
            _ => bail!("unsupported message role"),
        }
    }
    Ok(input)
}

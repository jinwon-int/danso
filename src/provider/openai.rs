//! OpenAI Responses API, stateless history with opaque reasoning preserved.
use super::{ModelRequest, Provider, http::Http, wire};
use crate::usage::Usage;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

pub struct OpenAi {
    http: Option<Http>,
    chatgpt: Option<super::chatgpt::ChatGpt>,
    model: String,
    effort: Option<String>,
}
impl OpenAi {
    pub fn new(model: String, key: String, base: &str, effort: Option<String>) -> Result<Self> {
        Self::new_with_timeout(model, key, base, effort, 180)
    }
    pub fn new_with_timeout(
        model: String,
        key: String,
        base: &str,
        effort: Option<String>,
        timeout_seconds: u64,
    ) -> Result<Self> {
        Ok(Self {
            http: Some(Http::new(base, "responses", &key, timeout_seconds)?),
            chatgpt: None,
            model,
            effort,
        })
    }
    pub fn new_chatgpt(
        model: String,
        auth_file: &std::path::Path,
        base: &str,
        effort: Option<String>,
        timeout_seconds: u64,
    ) -> Result<Self> {
        Ok(Self {
            http: None,
            chatgpt: Some(super::chatgpt::ChatGpt::new(
                auth_file,
                base,
                timeout_seconds,
            )?),
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
        if self.chatgpt.is_some() {
            body.as_object_mut().unwrap().remove("max_output_tokens");
            body["stream"] = json!(true);
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
        let response = match &self.chatgpt {
            Some(auth) => auth.post(&body, usage).await?,
            None => {
                self.http
                    .as_ref()
                    .context("missing OpenAI transport")?
                    .post(&body, usage)
                    .await?
            }
        };
        let (provider, api) = if self.chatgpt.is_some() {
            ("openai-codex", "openai-codex-responses")
        } else {
            ("openai", "openai-responses")
        };
        let t = wire::tokens(
            &response["usage"],
            "input_tokens",
            "output_tokens",
            "input_tokens_details",
        )?;
        let model = response["model"].as_str().unwrap_or(&self.model);
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
        ensure!(
            response["status"] == "completed" && response["error"].is_null(),
            "OpenAI response did not complete"
        );
        let output = response["output"]
            .as_array()
            .context("missing OpenAI output")?;
        let content = output_content(output)?;
        let mut message = wire::message(content, provider, api, model, &t)?;
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
                    ensure!(m["api"] != "openai-responses" && m["api"] != "openai-codex-responses", "OpenAI session lacks preserved output");
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

#[cfg(test)]
mod image_admission_tests {
    use super::*;

    #[test]
    fn experimental_images_remain_rejected_by_production_history() {
        // Wire-foundation fixtures are deliberately not decodable pictures.
        // Neither the API nor ChatGPT path may admit them before ingress,
        // capability, journal and compaction gates are implemented.
        let image = json!({"type":"image","mimeType":"image/jpeg","data":"/9j/"});
        for role in ["user", "toolResult", "assistant"] {
            let message = json!({
                "role":role, "content":[image.clone()],
                "toolCallId":"synthetic-call", "isError":false
            });
            assert!(history(&[message]).is_err(), "accepted image in {role}");
        }
    }

    #[test]
    fn mixed_image_history_fails_without_mutation_or_payload_echo() {
        let secret = "PRIVATE_SYNTHETIC_IMAGE_DO_NOT_ECHO";
        for role in ["user", "toolResult", "assistant"] {
            let messages = vec![
                json!({"role":"user","content":"previous text"}),
                json!({
                    "role":role, "toolCallId":"synthetic-call", "isError":false,
                    "content":[
                        {"type":"text","text":"caption"},
                        {"type":"image","mimeType":"image/jpeg","data":secret}
                    ]
                }),
            ];
            let original = messages.clone();
            let error = history(&messages).unwrap_err();
            assert_eq!(messages, original);
            assert!(!format!("{error:#}").contains(secret));
        }
    }

    #[tokio::test]
    async fn image_rejection_precedes_transport_resolution_and_usage_changes() {
        // Deliberately no transport: complete must return the history error,
        // not "missing OpenAI transport". No credentials, sockets or model calls.
        // This exercises shared adapter entry points, not a ChatGPT HTTP capture.
        let mut provider = OpenAi {
            http: None,
            chatgpt: None,
            model: "synthetic-model".into(),
            effort: None,
        };
        let secret = "PRIVATE_SYNTHETIC_IMAGE_DO_NOT_ECHO";
        for role in ["user", "toolResult", "assistant"] {
            let messages = vec![json!({
                "role":role, "toolCallId":"synthetic-call", "isError":false,
                "content":[{"type":"image","mimeType":"image/jpeg","data":secret}]
            })];
            let original = messages.clone();
            let request = ModelRequest {
                system: "synthetic instructions",
                messages: &messages,
                tools: &[],
            };
            let expected = format!("{:#}", history(&messages).unwrap_err());
            let validation = provider.validate_history(&messages).unwrap_err();
            let sizing = provider.request_bytes(&request).unwrap_err();
            let mut usage = Usage::default();
            let before = usage.summary();
            let completion = provider.complete(request, &mut usage).await.unwrap_err();
            for error in [validation, sizing, completion] {
                let error = format!("{error:#}");
                assert_eq!(error, expected);
                assert!(!error.contains(secret));
                assert!(!error.contains("missing OpenAI transport"));
            }
            assert!(!usage.attempted);
            assert_eq!(usage.summary(), before);
            assert_eq!(messages, original);
        }
    }

    #[test]
    fn text_user_and_tool_results_keep_existing_shape() {
        let input = history(&[
            json!({"role":"user","content":[
                {"type":"text","text":"first"}, {"type":"text","text":"second"}
            ]}),
            json!({"role":"toolResult","toolCallId":"synthetic-call",
                "isError":false,"content":"tool text"}),
        ])
        .unwrap();
        assert_eq!(input[0], json!({"role":"user","content":"first\nsecond"}));
        assert_eq!(input[1]["type"], "function_call_output");
        let output: Value = serde_json::from_str(input[1]["output"].as_str().unwrap()).unwrap();
        assert_eq!(output, json!({"isError":false,"output":"tool text"}));
    }
}

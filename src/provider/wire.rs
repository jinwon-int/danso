//! Text-only Pi conversion helpers. No provider-specific request selection.
use crate::usage::TokenUsage;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

pub fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .context("missing or invalid provider string")
}
pub fn nonempty<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    let s = string(v, key)?;
    ensure!(!s.is_empty(), "empty provider identifier");
    Ok(s)
}
pub fn text(v: &Value) -> Result<String> {
    if let Some(s) = v.as_str() {
        return Ok(s.into());
    }
    let mut strings = vec![];
    for b in v.as_array().context("expected text content")? {
        ensure!(b["type"] == "text", "only text content is supported");
        strings.push(string(b, "text")?);
    }
    Ok(strings.join("\n"))
}
pub fn arguments(v: &Value) -> Result<Value> {
    let args = if let Some(s) = v.as_str() {
        serde_json::from_str(s).context("invalid tool arguments JSON")?
    } else {
        v.clone()
    };
    ensure!(args.is_object(), "tool arguments must be an object");
    Ok(args)
}
pub fn call(id: &str, name: &str, args: Value) -> Result<Value> {
    ensure!(
        !id.is_empty() && !name.is_empty() && args.is_object(),
        "invalid tool call"
    );
    Ok(json!({"type":"toolCall", "id":id,"name":name,"arguments":args}))
}
pub fn assistant_blocks(m: &Value) -> Result<&Vec<Value>> {
    let blocks = m["content"]
        .as_array()
        .context("invalid assistant content")?;
    for b in blocks {
        match b["type"].as_str() {
            Some("text") => {
                string(b, "text")?;
            }
            Some("toolCall") => {
                call(
                    nonempty(b, "id")?,
                    nonempty(b, "name")?,
                    b["arguments"].clone(),
                )?;
            }
            _ => bail!("unsupported assistant content"),
        }
    }
    Ok(blocks)
}
pub fn tokens(u: &Value, input: &str, output: &str, details: &str) -> Result<TokenUsage> {
    let input = u[input].as_u64().context("missing input usage")?;
    let output = u[output].as_u64().context("missing output usage")?;
    let cached = match &u[details]["cached_tokens"] {
        Value::Null => 0,
        v => v.as_u64().context("invalid cache usage")?,
    };
    ensure!(
        cached <= input && input.checked_add(output).is_some(),
        "invalid usage totals"
    );
    Ok(TokenUsage {
        input: input - cached,
        output,
        cache_read: cached,
        cache_write: 0,
    })
}
pub fn message(
    content: Vec<Value>,
    provider: &str,
    api: &str,
    model: &str,
    t: &TokenUsage,
) -> Result<Value> {
    ensure!(!content.is_empty(), "empty provider response");
    let stop = if content.iter().any(|b| b["type"] == "toolCall") {
        "toolUse"
    } else {
        "stop"
    };
    Ok(
        json!({"role":"assistant","content":content,"provider":provider,"api":api,"model":model,
        "timestamp":crate::session::millis(),"stopReason":stop,
        "usage":{"input":t.input,"output":t.output,"cacheRead":t.cache_read,"cacheWrite":0,
        "totalTokens":t.input+t.output+t.cache_read,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}),
    )
}

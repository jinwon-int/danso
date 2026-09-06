use super::Tool;
use crate::contracts::ToolDefinition;
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub struct ReadTool;
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".into(),
            description: "Read a UTF-8 file (256 KiB maximum). Prefer offset (1-based line) and limit for targeted reads; omitted range returns the entire file.".into(),
            parameters: json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "minimum": 1},
                "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
            }, "required": ["path"], "additionalProperties": false}),
        }
    }
    fn execute(&self, args: &Value) -> Result<()> {
        let output = render(args)?;
        print!("{output}");
        Ok(())
    }
}

fn positive(args: &Value, key: &str, default: u64, max: u64) -> Result<u64> {
    match args.get(key) {
        None => Ok(default),
        Some(value) => {
            let number = value.as_u64().filter(|n| *n > 0 && *n <= max);
            ensure!(number.is_some(), "invalid read {key}");
            Ok(number.unwrap())
        }
    }
}

fn render(args: &Value) -> Result<String> {
    let offset = positive(args, "offset", 1, usize::MAX as u64)? as usize;
    let limit = positive(args, "limit", 200, 2000)? as usize;
    let text = crate::context::read_bounded(
        std::path::Path::new(super::files::string(args, "path")?),
        crate::context::FILE_LIMIT,
    )?;
    if args.get("offset").is_none() && args.get("limit").is_none() {
        return Ok(text);
    }
    // Keep original line endings, including a final line without a newline.
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let start = offset - 1;
    ensure!(start <= lines.len(), "read offset exceeds end of file");
    let end = start.saturating_add(limit).min(lines.len());
    let next = if end < lines.len() {
        format!("next offset {}", end + 1)
    } else {
        "EOF".into()
    };
    let header = if start == end {
        format!(
            "[read: no lines at offset {offset}; {} total lines; {next}]\n",
            lines.len()
        )
    } else {
        format!("[read: lines {offset}-{end} of {}; {next}]\n", lines.len())
    };
    Ok(header + &lines[start..end].concat())
}

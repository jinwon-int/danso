use super::Tool;
use crate::contracts::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};

pub struct WriteTool;
impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".into(),
            description: "Write UTF-8 content to a file, creating parent directories".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"], "additionalProperties": false}),
        }
    }
    fn execute(&self, args: &Value) -> Result<()> {
        super::files::replace_or_write(args, false)
    }
}

use super::Tool;
use crate::contracts::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};

pub struct EditTool;
impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".into(),
            description: "Replace exactly one occurrence of oldText in a UTF-8 file".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}, "oldText": {"type": "string"}, "newText": {"type": "string"}}, "required": ["path", "oldText", "newText"], "additionalProperties": false}),
        }
    }
    fn execute(&self, args: &Value) -> Result<()> {
        super::files::replace_or_write(args, true)
    }
}

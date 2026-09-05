use super::Tool;
use crate::contracts::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};

pub struct ReadTool;
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".into(),
            description: "Read a UTF-8 file (256 KiB maximum)".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false}),
        }
    }
    fn execute(&self, args: &Value) -> Result<()> {
        print!(
            "{}",
            crate::context::read_bounded(
                std::path::Path::new(super::files::string(args, "path")?),
                crate::context::FILE_LIMIT
            )?
        );
        Ok(())
    }
}

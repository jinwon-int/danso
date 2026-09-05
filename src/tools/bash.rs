use super::Tool;
use crate::contracts::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};

pub struct BashTool;
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".into(),
            description: "Run a shell command in the workspace; network is disabled in the sandbox"
                .into(),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"], "additionalProperties": false}),
        }
    }
    fn execute(&self, args: &Value) -> Result<()> {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new("/bin/bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                super::files::string(args, "command")?,
            ])
            .exec();
        Err(error.into())
    }
}

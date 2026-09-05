//! Compile-time tool registry. Definitions and dispatch share the same entries.
//! Tool implementations execute only inside the worker chosen by the executor.
mod bash;
mod edit;
mod files;
mod read;
mod runner;
mod write;

use crate::contracts::ToolDefinition;
use anyhow::{Context, Result, ensure};
pub use runner::Runner;
use serde_json::Value;

pub const OUTPUT_LIMIT: usize = 64 * 1024;

pub trait Tool {
    fn definition(&self) -> ToolDefinition;
    fn execute(&self, arguments: &Value) -> Result<()>;
}

struct Registration {
    definition: ToolDefinition,
    implementation: Box<dyn Tool>,
}

#[derive(Default)]
pub struct Registry {
    entries: Vec<Registration>,
}
impl Registry {
    pub fn register(&mut self, tool: impl Tool + 'static) -> Result<()> {
        let definition = tool.definition();
        ensure!(!definition.name.is_empty(), "tool name must not be empty");
        ensure!(
            !self
                .entries
                .iter()
                .any(|e| e.definition.name == definition.name),
            "duplicate tool name: {}",
            definition.name
        );
        self.entries.push(Registration {
            definition,
            implementation: Box::new(tool),
        });
        Ok(())
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.entries.iter().map(|e| e.definition.clone()).collect()
    }

    pub fn execute(&self, name: &str, arguments: &Value) -> Result<()> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.definition.name == name)
            .context("unknown tool")?;
        entry.implementation.execute(arguments)
    }
}

/// The only production registration site, shared by parent advertisement and
/// child dispatch. v0 deliberately keeps exactly four builtins enabled.
pub fn builtins() -> Registry {
    let mut registry = Registry::default();
    registry.register(read::ReadTool).expect("unique builtin");
    registry.register(bash::BashTool).expect("unique builtin");
    registry.register(edit::EditTool).expect("unique builtin");
    registry.register(write::WriteTool).expect("unique builtin");
    registry
}

pub fn worker(call: Value) -> Result<()> {
    let name = call["name"].as_str().context("missing tool name")?;
    builtins().execute(name, &call["arguments"])
}

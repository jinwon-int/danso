//! Provider extension failures keep their source category through compaction.
use anyhow::{Result, bail};
use danso::{
    contracts::{Event, EventSink, ToolCall, ToolDefinition, ToolExecutor, ToolOutcome},
    failure::{self, Kind},
    provider::{ModelRequest, Provider},
    runtime::{self, RunInput},
    session::Session,
    usage::Usage,
};
use serde_json::{Value, json};
use std::{cell::Cell, path::Path};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Point {
    Before,
    Bare,
    Summary,
    After,
}
struct FailingSizer {
    point: Point,
    action_sizes: Cell<usize>,
    completions: usize,
}
impl Provider for FailingSizer {
    fn validate_history(&self, _: &[Value]) -> Result<()> {
        Ok(())
    }
    fn request_bytes(&self, request: &ModelRequest<'_>) -> Result<usize> {
        if request
            .system
            .starts_with("You are a headless coding worker")
        {
            let number = self.action_sizes.get() + 1;
            self.action_sizes.set(number);
            if matches!(
                (number, self.point),
                (1, Point::Before) | (2, Point::Bare) | (3, Point::After)
            ) {
                bail!("PRIVATE request sizing failure");
            }
            Ok(if number == 1 { 9000 } else { 100 })
        } else {
            if self.point == Point::Summary {
                bail!("PRIVATE request sizing failure");
            }
            Ok(100)
        }
    }
    async fn complete(&mut self, _: ModelRequest<'_>, _: &mut Usage) -> Result<Value> {
        self.completions += 1;
        let summary = json!({"objective":"continue", "constraints":[], "changes":[], "tests":[], "pending":[]});
        Ok(
            json!({"role":"assistant", "stopReason":"stop", "content":[{"type":"text", "text": summary.to_string()}]}),
        )
    }
}
struct EmptyExecutor;
impl ToolExecutor for EmptyExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    async fn preflight(&self) -> Result<()> {
        Ok(())
    }
    async fn execute(&self, _: &ToolCall) -> Result<ToolOutcome> {
        unreachable!()
    }
}
struct Sink;
impl EventSink for Sink {
    fn emit(&mut self, _: Event<'_>) -> Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn provider_sizing_failure_keeps_category_at_every_compaction_boundary() {
    for point in [Point::Before, Point::Bare, Point::Summary, Point::After] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
        let mut provider = FailingSizer {
            point,
            action_sizes: Cell::new(0),
            completions: 0,
        };
        let error = runtime::run(
            RunInput {
                prompt: "continue",
                context: "",
                max_turns: 2,
                compact_at_bytes: Some(8192),
            },
            &mut provider,
            &EmptyExecutor,
            &mut session,
            &mut Sink,
            &mut Usage::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            failure::category(&error),
            Some(Kind::Provider),
            "{point:?}: {error:#}"
        );
        assert_eq!(provider.completions, usize::from(point == Point::After));
        assert!(
            !std::fs::read_to_string(path)
                .unwrap()
                .contains("danso.compaction.v1")
        );
    }
}

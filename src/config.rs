//! Danso configuration file (docs/unified-design.md §6.1).
//!
//! `$DANSO_HOME/config.toml` (default `~/.danso/config.toml`) is the one
//! place operators describe a node. Every section is optional; every key
//! inside a section is declared here, and unknown keys are errors so a typo
//! cannot silently disable a setting. Secrets never live in this file: the
//! `[provider]` and `[telegram]` sections name credential *files* or rely
//! on the process environment.
//!
//! Precedence, once the run and bridge paths consume this file, is
//! CLI flag > environment > file > default. This module owns parsing and
//! validation and the body-free `config check` projection; the CLI keeps
//! its flag-driven path unchanged until the bridge stage lands.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "config.toml";
/// Upper bound on the file size; a configuration is small by construction.
pub const MAX_BYTES: u64 = 64 * 1024;

/// `$DANSO_HOME`, defaulting to `$HOME/.danso`.
pub fn home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("DANSO_HOME") {
        let home = PathBuf::from(home);
        ensure!(home.is_absolute(), "DANSO_HOME must be an absolute path");
        return Ok(home);
    }
    let home = std::env::var_os("HOME").context("HOME is required")?;
    let home = PathBuf::from(home);
    ensure!(home.is_absolute(), "HOME must be an absolute path");
    Ok(home.join(".danso"))
}

pub fn default_path() -> Result<PathBuf> {
    Ok(home()?.join(FILE_NAME))
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub core: Core,
    #[serde(default)]
    pub provider: Provider,
    #[serde(default)]
    pub memory: Memory,
    #[serde(default)]
    pub telegram: Telegram,
    #[serde(default)]
    pub cron: Cron,
    #[serde(default)]
    pub service: Service,
    #[serde(default)]
    pub update: Update,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Core {
    /// Default workspace for bridge and scheduled runs; absolute.
    pub workspace: Option<PathBuf>,
    /// `host` (default) or `bubblewrap`.
    pub sandbox: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub max_turns: Option<u32>,
    pub tool_timeout_seconds: Option<u64>,
    pub tool_home: Option<PathBuf>,
    pub long_task: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// `anthropic`, `openai`, `openai-codex` or `glm`.
    pub name: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Endpoint override; HTTPS, or literal loopback HTTP for tests.
    pub base_url: Option<String>,
    /// GLM endpoint preset: `general` or `coding`.
    pub endpoint: Option<String>,
    /// GLM thinking toggle: `enabled` or `disabled`.
    pub thinking: Option<String>,
    pub retries: Option<u32>,
    pub timeout_seconds: Option<u64>,
    pub max_output_tokens: Option<u32>,
    /// `openai-codex` only: the Danso-managed auth file. Never the token.
    pub auth_file: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    /// `off` (default), `read` or `read-write`.
    pub mode: Option<String>,
    pub dir: Option<PathBuf>,
    pub scope: Option<String>,
    pub max_bytes: Option<usize>,
    /// `per-run` or `per-request`.
    pub refresh: Option<String>,
    /// `queue`, `inline` or `off`.
    pub distill: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Telegram {
    /// Owner-only file holding the bot token; never the token itself.
    pub token_file: Option<PathBuf>,
    #[serde(default)]
    pub allowed_user_ids: Vec<i64>,
    /// `per-user-chat` (default) or `shared-groups`.
    pub session_scope: Option<String>,
    /// `off` (default) or `audience-scoped`.
    pub memory_mode: Option<String>,
    pub heartbeat_seconds: Option<u64>,
    pub stall_seconds: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Cron {
    pub store: Option<PathBuf>,
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub unit: Option<String>,
    /// `system` or `user`.
    pub scope: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Update {
    /// Base64 ed25519 public key that signs release manifests.
    pub public_key: Option<String>,
    pub channel: Option<String>,
    pub enforce_signature: Option<bool>,
}

fn one_of(field: &str, value: Option<&str>, allowed: &[&str]) -> Result<()> {
    if let Some(value) = value {
        ensure!(
            allowed.contains(&value),
            "{field} must be one of {}",
            allowed.join(", ")
        );
    }
    Ok(())
}

fn absolute(field: &str, path: Option<&Path>) -> Result<()> {
    if let Some(path) = path {
        ensure!(path.is_absolute(), "{field} must be an absolute path");
    }
    Ok(())
}

impl Config {
    /// Parse and validate. Values are checked for shape and range only;
    /// existence of files is a `doctor` concern.
    pub fn parse(text: &str) -> Result<Self> {
        // The parser's default rendering quotes the offending source line,
        // which would echo a mistyped secret; keep only the message and
        // the line number.
        let config: Config = toml::from_str(text).map_err(|error| {
            let line = error
                .span()
                .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
            match line {
                Some(line) => {
                    anyhow::anyhow!("invalid config.toml at line {line}: {}", error.message())
                }
                None => anyhow::anyhow!("invalid config.toml: {}", error.message()),
            }
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "config path must be absolute");
        let meta = std::fs::symlink_metadata(path).context("config file is missing")?;
        ensure!(meta.is_file(), "config path must be a regular file");
        ensure!(
            meta.len() <= MAX_BYTES,
            "config file exceeds {MAX_BYTES} bytes"
        );
        let text = std::fs::read_to_string(path).context("read config file")?;
        Self::parse(&text)
    }

    pub fn validate(&self) -> Result<()> {
        absolute("core.workspace", self.core.workspace.as_deref())?;
        absolute("core.tool_home", self.core.tool_home.as_deref())?;
        one_of(
            "core.sandbox",
            self.core.sandbox.as_deref(),
            &["host", "bubblewrap"],
        )?;
        if let Some(turns) = self.core.max_turns {
            ensure!((1..=128).contains(&turns), "core.max_turns must be 1..128");
        }
        if let Some(seconds) = self.core.timeout_seconds {
            ensure!(
                (1..=crate::long_task::MAX_WALL_SECONDS).contains(&seconds),
                "core.timeout_seconds must be 1..{}",
                crate::long_task::MAX_WALL_SECONDS
            );
        }
        if let Some(seconds) = self.core.tool_timeout_seconds {
            ensure!(
                (1..=crate::tools::HOST_TOOL_TIMEOUT_MAX_SECONDS).contains(&seconds),
                "core.tool_timeout_seconds must be 1..{}",
                crate::tools::HOST_TOOL_TIMEOUT_MAX_SECONDS
            );
        }
        one_of(
            "provider.name",
            self.provider.name.as_deref(),
            &["anthropic", "openai", "openai-codex", "glm"],
        )?;
        if let Some(model) = &self.provider.model {
            ensure!(!model.trim().is_empty(), "provider.model must not be empty");
        }
        one_of(
            "provider.reasoning_effort",
            self.provider.reasoning_effort.as_deref(),
            &["none", "minimal", "low", "medium", "high", "xhigh", "max"],
        )?;
        if self.provider.name.as_deref() == Some("anthropic") {
            ensure!(
                self.provider.reasoning_effort.is_none(),
                "provider.reasoning_effort is unsupported by the Anthropic adapter"
            );
        }
        one_of(
            "provider.endpoint",
            self.provider.endpoint.as_deref(),
            &["general", "coding"],
        )?;
        one_of(
            "provider.thinking",
            self.provider.thinking.as_deref(),
            &["enabled", "disabled"],
        )?;
        if let Some(url) = &self.provider.base_url {
            ensure!(
                url.starts_with("https://") || url.starts_with("http://127.0.0.1"),
                "provider.base_url must be HTTPS (literal loopback HTTP allowed for tests)"
            );
        }
        if let Some(retries) = self.provider.retries {
            ensure!(retries <= 5, "provider.retries must be 0..=5");
        }
        if let Some(seconds) = self.provider.timeout_seconds {
            ensure!(
                (1..=300).contains(&seconds),
                "provider.timeout_seconds must be 1..300"
            );
        }
        if let Some(cap) = self.provider.max_output_tokens {
            ensure!(
                (256..=131072).contains(&cap),
                "provider.max_output_tokens must be 256..131072"
            );
        }
        absolute("provider.auth_file", self.provider.auth_file.as_deref())?;
        one_of(
            "memory.mode",
            self.memory.mode.as_deref(),
            &["off", "read", "read-write"],
        )?;
        absolute("memory.dir", self.memory.dir.as_deref())?;
        if let Some(scope) = &self.memory.scope {
            ensure!(
                crate::memory::paths::valid_scope(scope),
                "memory.scope must be global, shared or private-<32 hex>"
            );
        }
        if let Some(bytes) = self.memory.max_bytes {
            ensure!(
                (1..=crate::memory::snapshot::SNAPSHOT_MAX_BYTES_MAX).contains(&bytes),
                "memory.max_bytes must be 1..{}",
                crate::memory::snapshot::SNAPSHOT_MAX_BYTES_MAX
            );
        }
        one_of(
            "memory.refresh",
            self.memory.refresh.as_deref(),
            &["per-run", "per-request"],
        )?;
        one_of(
            "memory.distill",
            self.memory.distill.as_deref(),
            &["queue", "inline", "off"],
        )?;
        absolute("telegram.token_file", self.telegram.token_file.as_deref())?;
        ensure!(
            self.telegram.allowed_user_ids.iter().all(|id| *id > 0),
            "telegram.allowed_user_ids must be positive"
        );
        one_of(
            "telegram.session_scope",
            self.telegram.session_scope.as_deref(),
            &["per-user-chat", "shared-groups"],
        )?;
        one_of(
            "telegram.memory_mode",
            self.telegram.memory_mode.as_deref(),
            &["off", "audience-scoped"],
        )?;
        if self.telegram.memory_mode.as_deref() == Some("audience-scoped") {
            ensure!(
                self.memory.mode.as_deref() != Some("off"),
                "telegram.memory_mode=audience-scoped requires memory.mode read or read-write"
            );
        }
        for (field, value) in [
            (
                "telegram.heartbeat_seconds",
                self.telegram.heartbeat_seconds,
            ),
            ("telegram.stall_seconds", self.telegram.stall_seconds),
        ] {
            if let Some(value) = value {
                ensure!((0..=86400).contains(&value), "{field} must be 0..86400");
            }
        }
        absolute("cron.store", self.cron.store.as_deref())?;
        if let Some(unit) = &self.service.unit {
            ensure!(
                !unit.is_empty()
                    && unit
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.@:".contains(c)),
                "service.unit must be a plain unit name"
            );
        }
        one_of(
            "service.scope",
            self.service.scope.as_deref(),
            &["system", "user"],
        )?;
        if let Some(key) = &self.update.public_key {
            ensure!(
                key.len() == 44 && key.ends_with('=') && key.bytes().all(|b| b.is_ascii()),
                "update.public_key must be a base64 ed25519 public key"
            );
        }
        if let Some(channel) = &self.update.channel {
            ensure!(!channel.is_empty(), "update.channel must not be empty");
        }
        Ok(())
    }

    /// Body-free projection for `danso config check`: which keys are set,
    /// never their values. Secrets cannot leak because values are not
    /// rendered at all.
    pub fn report(&self, path: &Path) -> Value {
        fn present<'a>(pairs: &[(&'a str, bool)]) -> Vec<&'a str> {
            pairs
                .iter()
                .filter(|(_, set)| *set)
                .map(|(name, _)| *name)
                .collect()
        }
        let c = self;
        let keys = [
            ("core.workspace", c.core.workspace.is_some()),
            ("core.sandbox", c.core.sandbox.is_some()),
            ("core.timeout_seconds", c.core.timeout_seconds.is_some()),
            ("core.max_turns", c.core.max_turns.is_some()),
            (
                "core.tool_timeout_seconds",
                c.core.tool_timeout_seconds.is_some(),
            ),
            ("core.tool_home", c.core.tool_home.is_some()),
            ("core.long_task", c.core.long_task.is_some()),
            ("provider.name", c.provider.name.is_some()),
            ("provider.model", c.provider.model.is_some()),
            (
                "provider.reasoning_effort",
                c.provider.reasoning_effort.is_some(),
            ),
            ("provider.base_url", c.provider.base_url.is_some()),
            ("provider.endpoint", c.provider.endpoint.is_some()),
            ("provider.thinking", c.provider.thinking.is_some()),
            ("provider.retries", c.provider.retries.is_some()),
            (
                "provider.timeout_seconds",
                c.provider.timeout_seconds.is_some(),
            ),
            (
                "provider.max_output_tokens",
                c.provider.max_output_tokens.is_some(),
            ),
            ("provider.auth_file", c.provider.auth_file.is_some()),
            ("memory.mode", c.memory.mode.is_some()),
            ("memory.dir", c.memory.dir.is_some()),
            ("memory.scope", c.memory.scope.is_some()),
            ("memory.max_bytes", c.memory.max_bytes.is_some()),
            ("memory.refresh", c.memory.refresh.is_some()),
            ("memory.distill", c.memory.distill.is_some()),
            ("telegram.token_file", c.telegram.token_file.is_some()),
            (
                "telegram.allowed_user_ids",
                !c.telegram.allowed_user_ids.is_empty(),
            ),
            ("telegram.session_scope", c.telegram.session_scope.is_some()),
            ("telegram.memory_mode", c.telegram.memory_mode.is_some()),
            (
                "telegram.heartbeat_seconds",
                c.telegram.heartbeat_seconds.is_some(),
            ),
            ("telegram.stall_seconds", c.telegram.stall_seconds.is_some()),
            ("cron.store", c.cron.store.is_some()),
            ("cron.enabled", c.cron.enabled.is_some()),
            ("service.unit", c.service.unit.is_some()),
            ("service.scope", c.service.scope.is_some()),
            ("update.public_key", c.update.public_key.is_some()),
            ("update.channel", c.update.channel.is_some()),
            (
                "update.enforce_signature",
                c.update.enforce_signature.is_some(),
            ),
        ];
        json!({
            "version": 1,
            "kind": "config_check",
            "path": path.display().to_string(),
            "valid": true,
            "set_keys": present(&keys),
            "allowed_user_count": c.telegram.allowed_user_ids.len(),
        })
    }
}

/// `danso config check` entry point: parse, validate, and report without
/// echoing values. Missing file is a configuration error.
pub fn check(path: Option<&Path>) -> Result<Value> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => default_path()?,
    };
    if std::fs::symlink_metadata(&path).is_err() {
        bail!("config file is missing: {}", path.display());
    }
    let config = Config::load(&path)?;
    Ok(config.report(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
[core]
workspace = "/srv/work"
sandbox = "host"
timeout_seconds = 3600
max_turns = 32
[provider]
name = "glm"
model = "glm-5.3-flash"
reasoning_effort = "medium"
endpoint = "coding"
retries = 0
[memory]
mode = "read-write"
scope = "shared"
max_bytes = 12000
[telegram]
token_file = "/var/lib/danso/telegram.token"
allowed_user_ids = [12345]
session_scope = "per-user-chat"
memory_mode = "audience-scoped"
[cron]
store = "/var/lib/danso/cron/tasks.json"
[service]
unit = "danso-bridge.service"
scope = "system"
[update]
public_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
"#;

    #[test]
    fn full_config_parses_and_reports_keys_only() {
        let config = Config::parse(FULL).unwrap();
        assert_eq!(config.provider.model.as_deref(), Some("glm-5.3-flash"));
        let report = config.report(Path::new("/x/config.toml"));
        let rendered = report.to_string();
        assert!(rendered.contains("provider.model"));
        assert!(!rendered.contains("glm-5.3-flash"));
        assert!(!rendered.contains("12345"));
        assert!(!rendered.contains("/var/lib/danso"), "paths are values");
        assert_eq!(report["allowed_user_count"], 1);
        assert_eq!(report["valid"], true);
    }

    #[test]
    fn empty_config_is_valid_and_unknown_keys_are_errors() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let error = Config::parse("[telegram]\ntokn = \"123:SECRET-VALUE\"\n").unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("line 2"), "{rendered}");
        assert!(!rendered.contains("SECRET-VALUE"), "{rendered}");
        assert!(Config::parse("[bridge]\nx = 1\n").is_err());
    }

    #[test]
    fn validation_is_fail_closed() {
        for (bad, reason) in [
            ("[core]\nsandbox = \"docker\"\n", "sandbox enum"),
            ("[core]\nworkspace = \"relative\"\n", "relative workspace"),
            ("[core]\nmax_turns = 0\n", "turn range"),
            ("[provider]\nname = \"mistral\"\n", "provider enum"),
            (
                "[provider]\nname = \"anthropic\"\nreasoning_effort = \"high\"\n",
                "anthropic effort",
            ),
            (
                "[provider]\nbase_url = \"http://example.com\"\n",
                "plain http",
            ),
            ("[provider]\nretries = 6\n", "retry range"),
            ("[memory]\nscope = \"private-short\"\n", "scope shape"),
            ("[memory]\nmode = \"write\"\n", "memory mode"),
            ("[telegram]\nallowed_user_ids = [0]\n", "user id"),
            (
                "[telegram]\nmemory_mode = \"audience-scoped\"\n[memory]\nmode = \"off\"\n",
                "memory off with audience scope",
            ),
            ("[service]\nunit = \"../evil\"\n", "unit name"),
            ("[update]\npublic_key = \"short\"\n", "public key shape"),
        ] {
            assert!(Config::parse(bad).is_err(), "{reason} must be rejected");
        }
    }

    #[test]
    fn load_requires_a_bounded_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(Config::load(&path).is_err());
        std::fs::write(&path, "[core]\nmax_turns = 4\n").unwrap();
        assert_eq!(Config::load(&path).unwrap().core.max_turns, Some(4));
        assert!(Config::load(Path::new("relative.toml")).is_err());
        let report = check(Some(&path)).unwrap();
        assert_eq!(report["set_keys"], json!(["core.max_turns"]));
        assert!(check(Some(&dir.path().join("missing.toml"))).is_err());
    }
}

use super::OUTPUT_LIMIT;
use crate::contracts::{ToolCall, ToolDefinition, ToolExecutor, ToolOutcome};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
pub struct Runner {
    pub cwd: PathBuf,
    pub readable: Vec<PathBuf>,
    pub unsafe_no_sandbox: bool,
    pub timeout: Duration,
}

impl Runner {
    fn command(&self) -> Result<Command> {
        let exe = std::env::current_exe()?;
        let mut cmd;
        if self.unsafe_no_sandbox {
            cmd = Command::new(exe);
        } else {
            cmd = Command::new("/usr/bin/bwrap");
            cmd.args([
                "--unshare-all",
                "--die-with-parent",
                "--new-session",
                "--cap-drop",
                "ALL",
                "--clearenv",
            ]);
            for dir in ["/usr", "/bin", "/lib", "/lib64"] {
                if Path::new(dir).exists() {
                    cmd.arg("--ro-bind").arg(dir).arg(dir);
                }
            }
            cmd.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
            cmd.arg("--bind").arg(&self.cwd).arg(&self.cwd);
            for file in &self.readable {
                if !file.starts_with(&self.cwd) {
                    cmd.arg("--ro-bind").arg(file).arg(file);
                }
            }
            cmd.arg("--ro-bind").arg(exe).arg("/danso-worker");
            cmd.arg("--chdir").arg(&self.cwd);
            cmd.args([
                "--setenv",
                "PATH",
                "/usr/bin:/bin",
                "--setenv",
                "HOME",
                "/tmp",
                "--",
                "/danso-worker",
            ]);
        }
        cmd.arg("__tool")
            .current_dir(&self.cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/tmp");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // RLIMIT_AS is a virtual-memory cap, not an RSS claim. Sequential tools
        // cap concurrent bash at one. The bubblewrap PID namespace reaps descendants.
        unsafe {
            cmd.pre_exec(|| {
                for (resource, limit) in [
                    (libc::RLIMIT_AS, 512 * 1024 * 1024),
                    (libc::RLIMIT_FSIZE, 16 * 1024 * 1024),
                    (libc::RLIMIT_NOFILE, 128),
                    (libc::RLIMIT_CPU, 30),
                ] {
                    let r = libc::rlimit {
                        rlim_cur: limit,
                        rlim_max: limit,
                    };
                    if libc::setrlimit(resource, &r) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(cmd)
    }

    pub async fn run(&self, call: &Value) -> Result<(String, bool)> {
        let mut child = self
            .command()?
            .spawn()
            .context("sandbox/tool launch failed (requires bubblewrap and user namespaces)")?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let input = serde_json::to_vec(call)?;
        let work = async {
            let send = async {
                stdin.write_all(&input).await?;
                drop(stdin);
                Ok::<_, std::io::Error>(())
            };
            let drain = async {
                let (out, err) = tokio::try_join!(bounded_output(stdout), bounded_output(stderr))?;
                Ok::<_, anyhow::Error>((out, err))
            };
            let (_, (out, err)) =
                tokio::try_join!(async { send.await.map_err(anyhow::Error::from) }, drain)?;
            let status = child.wait().await?;
            Ok::<_, anyhow::Error>((
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&out),
                    String::from_utf8_lossy(&err)
                ),
                !status.success(),
            ))
        };
        match tokio::time::timeout(self.timeout, work).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => {
                child.kill().await.ok();
                Err(error)
            }
            Err(_) => {
                child.kill().await.ok();
                bail!("tool timed out");
            }
        }
    }
}

async fn bounded_output(mut reader: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut out = vec![];
    let mut buf = [0; 4096];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(out);
        }
        ensure!(out.len() + n <= OUTPUT_LIMIT, "tool output exceeds 64 KiB");
        out.extend_from_slice(&buf[..n]);
    }
}

impl ToolExecutor for Runner {
    fn definitions(&self) -> Vec<ToolDefinition> {
        super::builtins().definitions()
    }
    async fn preflight(&self) -> Result<()> {
        let (_, failed) = self
            .run(&json!({"name":"bash","arguments":{"command":"true"}}))
            .await?;
        ensure!(
            !failed,
            "sandbox preflight failed; no provider request sent"
        );
        Ok(())
    }
    async fn execute(&self, call: &ToolCall) -> Result<ToolOutcome> {
        let (output, is_error) = self.run(&serde_json::to_value(call)?).await?;
        Ok(ToolOutcome { output, is_error })
    }
}

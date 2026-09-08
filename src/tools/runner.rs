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
pub(crate) const SYSTEM_MOUNTS: [&str; 4] = ["/usr", "/bin", "/lib", "/lib64"];

pub struct Runner {
    pub cwd: PathBuf,
    pub readable: Vec<PathBuf>,
    pub backend: crate::app::Backend,
    pub timeout: Duration,
}

impl Runner {
    fn command(&self) -> Result<Command> {
        let exe = std::env::current_exe()?;
        let mut cmd;
        if self.backend.is_host() {
            cmd = Command::new(exe);
            cmd.arg("__supervise").arg(std::process::id().to_string());
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
            for dir in SYSTEM_MOUNTS {
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
        if !self.backend.is_host() {
            cmd.arg("__tool");
        }
        cmd.current_dir(&self.cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/tmp");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(!self.backend.is_host());
        // RLIMIT_AS is a virtual-memory cap, not an RSS claim. Sequential tools
        // cap concurrent bash at one. Host mode uses a subreaper; bubblewrap a PID namespace.
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
            .context("tool launch failed for selected execution backend")?;
        let mut cleanup = if self.backend.is_host() {
            match HostCleanup::new(child.id().expect("new child PID")) {
                Ok(guard) => guard,
                Err(error) => {
                    child.kill().await.ok();
                    return Err(error.into());
                }
            }
        } else {
            HostCleanup(None)
        };
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
            cleanup.0 = None;
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
                if self.backend.is_host() {
                    cleanup.stop();
                    if child.wait().await.is_ok() {
                        cleanup.0 = None;
                    }
                } else {
                    child.kill().await.ok();
                }
                Err(error)
            }
            Err(_) => {
                if self.backend.is_host() {
                    cleanup.stop();
                    if child.wait().await.is_ok() {
                        cleanup.0 = None;
                    }
                } else {
                    child.kill().await.ok();
                }
                bail!("tool timed out");
            }
        }
    }
}

// A pidfd pins identity even if Tokio reaps a child before future cancellation.
struct HostCleanup(Option<std::os::fd::OwnedFd>);
impl HostCleanup {
    fn new(pid: u32) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(Some(unsafe {
            std::os::fd::OwnedFd::from_raw_fd(fd as i32)
        })))
    }
    fn stop(&self) {
        use std::os::fd::AsRawFd;
        if let Some(fd) = &self.0 {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    libc::SIGTERM,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }
}
impl Drop for HostCleanup {
    fn drop(&mut self) {
        self.stop();
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
        let mut definitions = super::builtins().definitions();
        if self.backend.is_host() {
            for definition in &mut definitions {
                if definition.name == "bash" {
                    definition.description = "Run Bash with pipefail in the workspace using current-user host permissions. Filesystem and network are not sandboxed. Check actual test results.".into();
                }
            }
        }
        definitions
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

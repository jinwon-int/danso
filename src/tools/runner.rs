use super::OUTPUT_LIMIT;
use crate::contracts::{ToolCall, ToolDefinition, ToolExecutor, ToolOutcome};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
pub(crate) const SYSTEM_MOUNTS: [&str; 4] = ["/usr", "/bin", "/lib", "/lib64"];
pub const HOST_TOOL_TIMEOUT_DEFAULT_SECONDS: u64 = 900;
pub const HOST_TOOL_TIMEOUT_MAX_SECONDS: u64 = 3600;
pub const BUBBLEWRAP_TOOL_TIMEOUT_DEFAULT_SECONDS: u64 = 30;
pub const BUBBLEWRAP_TOOL_TIMEOUT_MAX_SECONDS: u64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimits {
    pub address_space_bytes: u64,
    pub file_size_bytes: u64,
    pub open_files: u64,
    pub cpu_seconds: u64,
}

pub fn tool_timeout_default(backend: crate::app::Backend) -> u64 {
    if backend.is_host() {
        HOST_TOOL_TIMEOUT_DEFAULT_SECONDS
    } else {
        BUBBLEWRAP_TOOL_TIMEOUT_DEFAULT_SECONDS
    }
}

pub fn tool_timeout_max(backend: crate::app::Backend) -> u64 {
    if backend.is_host() {
        HOST_TOOL_TIMEOUT_MAX_SECONDS
    } else {
        BUBBLEWRAP_TOOL_TIMEOUT_MAX_SECONDS
    }
}

pub fn resource_limits(backend: crate::app::Backend, timeout: Duration) -> ResourceLimits {
    if backend.is_host() {
        ResourceLimits {
            address_space_bytes: 32 * 1024 * 1024 * 1024,
            file_size_bytes: 4 * 1024 * 1024 * 1024,
            open_files: 4096,
            // Keep the kernel CPU budget aligned with the configured per-tool
            // wall budget.  A sub-second library timeout still gets one CPU
            // second so the rlimit remains valid and fail-closed.
            cpu_seconds: timeout.as_secs().max(1),
        }
    } else {
        ResourceLimits {
            address_space_bytes: 512 * 1024 * 1024,
            file_size_bytes: 16 * 1024 * 1024,
            open_files: 128,
            cpu_seconds: 30,
        }
    }
}

/// Bound on each stage of the post-timeout host reap. The supervisor sends
/// SIGKILL and is a subreaper, so a normal tree converges in tens of
/// milliseconds; this only caps the pathological case where a descendant is
/// stuck in uninterruptible sleep and can never be reaped.
const REAP_GRACE: Duration = Duration::from_secs(5);

pub struct Runner {
    pub cwd: PathBuf,
    pub readable: Vec<PathBuf>,
    pub backend: crate::app::Backend,
    pub home: PathBuf,
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
        let (path, home): (OsString, OsString) = if self.backend.is_host() {
            let cargo_bin = self.home.join(".cargo/bin");
            let path = std::env::join_paths([
                cargo_bin.as_os_str(),
                std::ffi::OsStr::new("/usr/local/bin"),
                std::ffi::OsStr::new("/usr/bin"),
                std::ffi::OsStr::new("/bin"),
            ])?;
            (path, self.home.as_os_str().to_os_string())
        } else {
            (OsString::from("/usr/bin:/bin"), OsString::from("/tmp"))
        };
        cmd.current_dir(&self.cwd)
            .env_clear()
            .env("PATH", path)
            .env("HOME", home);
        if self.backend.is_host() {
            // Keep development builds bounded on hosts with many CPUs without
            // inheriting an arbitrary caller setting.
            cmd.env("CARGO_BUILD_JOBS", "2");
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(!self.backend.is_host());
        // RLIMIT_AS is a virtual-memory cap, not an RSS claim. Sequential tools
        // cap concurrent bash at one. Host mode uses a subreaper; bubblewrap a PID namespace.
        let limits = resource_limits(self.backend, self.timeout);
        unsafe {
            cmd.pre_exec(move || {
                for (resource, limit) in [
                    (libc::RLIMIT_AS, limits.address_space_bytes),
                    (libc::RLIMIT_FSIZE, limits.file_size_bytes),
                    (libc::RLIMIT_NOFILE, limits.open_files),
                    (libc::RLIMIT_CPU, limits.cpu_seconds),
                ] {
                    let mut inherited = std::mem::MaybeUninit::<libc::rlimit>::uninit();
                    if libc::getrlimit(resource, inherited.as_mut_ptr()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let inherited = inherited.assume_init();
                    // Never raise a hard limit inherited from the caller.  A
                    // tighter caller limit remains tighter; an unlimited
                    // caller gets exactly the backend cap below.
                    let limit = limit.min(inherited.rlim_max);
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
                    reap_host(&mut cleanup, &mut child).await;
                } else {
                    child.kill().await.ok();
                }
                Err(error)
            }
            Err(_) => {
                if self.backend.is_host() {
                    reap_host(&mut cleanup, &mut child).await;
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
        self.signal(libc::SIGTERM);
    }
    fn signal(&self, signal: i32) {
        use std::os::fd::AsRawFd;
        if let Some(fd) = &self.0 {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    signal,
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

/// Terminate the host supervisor and reap it without blocking indefinitely.
///
/// The supervisor kills its descendants with SIGKILL under
/// PR_SET_CHILD_SUBREAPER, so this normally completes in tens of
/// milliseconds. It cannot reap a descendant stuck in uninterruptible sleep,
/// and its reap loop has no exit for that case. An unbounded wait here would
/// hand a per-tool timeout up to the whole-run timeout, so escalate to
/// SIGKILL and then give up rather than stall the agent loop.
///
/// Giving up leaves the guard armed, so Drop signals once more.
async fn reap_host(cleanup: &mut HostCleanup, child: &mut tokio::process::Child) {
    async fn settled(child: &mut tokio::process::Child) -> bool {
        matches!(
            tokio::time::timeout(REAP_GRACE, child.wait()).await,
            Ok(Ok(_))
        )
    }
    cleanup.signal(libc::SIGTERM);
    if settled(child).await {
        cleanup.0 = None;
        return;
    }
    cleanup.signal(libc::SIGKILL);
    if settled(child).await {
        cleanup.0 = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_defaults_and_limits_are_distinct() {
        assert_eq!(
            tool_timeout_default(crate::app::Backend::Host),
            HOST_TOOL_TIMEOUT_DEFAULT_SECONDS
        );
        assert_eq!(
            tool_timeout_default(crate::app::Backend::Bubblewrap),
            BUBBLEWRAP_TOOL_TIMEOUT_DEFAULT_SECONDS
        );
        assert_eq!(
            tool_timeout_max(crate::app::Backend::Host),
            HOST_TOOL_TIMEOUT_MAX_SECONDS
        );
        assert_eq!(
            tool_timeout_max(crate::app::Backend::Bubblewrap),
            BUBBLEWRAP_TOOL_TIMEOUT_MAX_SECONDS
        );
        assert_eq!(
            resource_limits(crate::app::Backend::Host, Duration::from_secs(900)),
            ResourceLimits {
                address_space_bytes: 32 * 1024 * 1024 * 1024,
                file_size_bytes: 4 * 1024 * 1024 * 1024,
                open_files: 4096,
                cpu_seconds: 900,
            }
        );
        assert_eq!(
            resource_limits(crate::app::Backend::Bubblewrap, Duration::from_secs(300)),
            ResourceLimits {
                address_space_bytes: 512 * 1024 * 1024,
                file_size_bytes: 16 * 1024 * 1024,
                open_files: 128,
                cpu_seconds: 30,
            }
        );
    }
}

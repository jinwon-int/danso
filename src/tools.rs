use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

pub const OUTPUT_LIMIT: usize = 64 * 1024;

pub fn definitions() -> Value {
    json!([
        {"name":"read", "description":"Read a UTF-8 file (256 KiB maximum)", "input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}},
        {"name":"bash", "description":"Run a shell command in the workspace; network is disabled in the sandbox", "input_schema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false}},
        {"name":"edit", "description":"Replace exactly one occurrence of oldText in a UTF-8 file", "input_schema":{"type":"object","properties":{"path":{"type":"string"},"oldText":{"type":"string"},"newText":{"type":"string"}},"required":["path","oldText","newText"],"additionalProperties":false}},
        {"name":"write", "description":"Write UTF-8 content to a file, creating parent directories", "input_schema":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false}}
    ])
}

fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .with_context(|| format!("missing string {key}"))
}

/// Called only in a short-lived worker process. The parent supplies the OS
/// sandbox; these checks give useful errors but are not the security boundary.
pub fn worker(call: Value) -> Result<()> {
    let args = &call["arguments"];
    match call["name"].as_str() {
        Some("read") => print!(
            "{}",
            crate::context::read_bounded(
                Path::new(string(args, "path")?),
                crate::context::FILE_LIMIT
            )?
        ),
        Some("write" | "edit") => {
            let path = PathBuf::from(string(args, "path")?);
            let cwd = std::env::current_dir()?;
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            // A dangling symlink may point into the sandbox's private /tmp.
            // Reject it as well: success there would falsely claim a workspace
            // edit even though the host target remained unchanged.
            if fs::symlink_metadata(&path).is_ok() {
                ensure!(
                    path.canonicalize()?.starts_with(&cwd),
                    "writes must stay inside the workspace"
                );
            }
            // Resolve the closest existing ancestor before creating anything.
            let existing = path
                .ancestors()
                .find(|p| p.exists())
                .context("no parent")?
                .canonicalize()?;
            ensure!(
                existing.starts_with(&cwd),
                "writes must stay inside the workspace"
            );
            ensure!(
                !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
                "parent traversal is not allowed"
            );
            let content = if call["name"] == "edit" {
                let old = string(args, "oldText")?;
                ensure!(!old.is_empty(), "oldText must not be empty");
                let current = crate::context::read_bounded(&path, crate::context::FILE_LIMIT)?;
                ensure!(
                    current.matches(old).count() == 1,
                    "oldText must match exactly once"
                );
                current.replacen(old, string(args, "newText")?, 1)
            } else {
                string(args, "content")?.to_string()
            };
            ensure!(
                content.len() <= crate::context::FILE_LIMIT as usize,
                "write exceeds byte budget"
            );
            fs::create_dir_all(path.parent().context("missing parent")?)?;
            fs::write(path, content)?;
            println!("ok");
        }
        Some("bash") => {
            use std::os::unix::process::CommandExt;
            let error = std::process::Command::new("/bin/bash")
                .args(["--noprofile", "--norc", "-c", string(args, "command")?])
                .exec();
            return Err(error.into());
        }
        _ => bail!("unknown tool"),
    }
    Ok(())
}

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

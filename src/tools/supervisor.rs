//! Host-mode lifecycle supervision, not a security boundary. Single-threaded
//! entrypoint before Tokio; subreaping also catches children that call setsid.
use anyhow::{Context, Result, bail};
use std::{
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

pub fn run(parent: libc::pid_t) -> Result<i32> {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = stop as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        if libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0
            || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if libc::getppid() != parent || STOP.load(Ordering::Relaxed) {
            return Ok(143);
        }
    }
    // Check proc support before allowing any tool effects.
    let children = format!("/proc/self/task/{}/children", std::process::id());
    std::fs::read_to_string(&children).context("host supervisor requires procfs")?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("__tool")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .context("host tool launch failed")?;
    let outcome = loop {
        if STOP.load(Ordering::Relaxed) {
            break Ok(143);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                break Ok(status.code().unwrap_or(128 + status.signal().unwrap_or(1)));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => break Err(error),
        }
    };
    // Kill immediate children, reap, then kill newly adopted descendants. A
    // child PID cannot be reused while it is our unreaped child. No global scan.
    loop {
        let pids = std::fs::read_to_string(&children)?;
        for pid in pids.split_whitespace() {
            let pid: libc::pid_t = pid.parse()?;
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        loop {
            let result = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if result > 0 {
                continue;
            }
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ECHILD) {
                    return Ok(outcome?);
                }
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                bail!("host child reaping failed: {error}");
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

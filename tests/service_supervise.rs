//! `danso service run --supervise` survives a killed service (#118).
//!
//! Measured on yukson 2026-09-17 during the #118 acceptance run: `kill -9` on
//! the supervised child left no process, no restart, a stale `service.pid` and
//! an empty log. Supervision had classified *every* signal death as an orderly
//! stop, so the one platform where this loop is the restart policy — Termux,
//! which has no systemd — was unsupervised against the death it is most likely
//! to suffer, the OOM killer's `SIGKILL`.
//!
//! Only a real process can show this. The classification itself is unit-tested
//! in `danso_ops::supervise`; what is here is that the loop actually restarts.

use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// The service needs these to start at all; a stub API keeps the run local.
///
/// The workspace is a sibling of the state root, not its parent: journals live
/// under the state root and the service refuses to keep them inside the
/// workspace it runs commands in.
fn supervised(data_dir: &Path, api_base_url: &str) -> Child {
    let workspace = data_dir.parent().expect("state root parent").join("ws");
    std::fs::create_dir_all(&workspace).expect("workspace");
    Command::new(env!("CARGO_BIN_EXE_danso"))
        .args(["service", "run", "--data-dir"])
        .arg(data_dir)
        .arg("--supervise")
        .env("DANSO_TELEGRAM_BOT_TOKEN", "000000:TEST-NOT-A-REAL-TOKEN")
        .env("DANSO_TELEGRAM_API_BASE_URL", api_base_url)
        .env("DANSO_TELEGRAM_ALLOWED_USER_IDS", "1")
        .env("DANSO_TELEGRAM_WORKSPACE", &workspace)
        .env("DANSO_TELEGRAM_PROVIDER", "anthropic")
        .env("DANSO_TELEGRAM_MODEL", "claude-opus-5")
        .env("DANSO_HOME", data_dir.parent().unwrap())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start the supervisor")
}

/// A local stand-in for the Telegram API, so the test contacts nothing.
fn stub_api() -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let addr = listener.local_addr().expect("stub address");
    let handle = std::thread::spawn(move || {
        use std::io::{Read, Write};
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer);
            let body = br#"{"ok":true,"result":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
        }
    });
    (addr, handle)
}

fn service_pid(data_dir: &Path) -> Option<u32> {
    let raw = std::fs::read(data_dir.join("service.pid")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    value.get("pid")?.as_u64().map(|pid| pid as u32)
}

/// Wait for a pid record that is live and not `excluding`.
fn wait_for_service(data_dir: &Path, excluding: Option<u32>, budget: Duration) -> Option<u32> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if let Some(pid) = service_pid(data_dir)
            && Some(pid) != excluding
            && Path::new(&format!("/proc/{pid}")).exists()
        {
            return Some(pid);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn kill(pid: u32, signal: i32) {
    // SAFETY: `kill` with a pid this test started and is about to reap.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_killed_service_is_restarted() {
    let root = tempfile::tempdir().expect("temporary root");
    let data_dir = root.path().join("data");
    let (addr, _stub) = stub_api();
    let base = format!("http://{addr}");

    let mut supervisor = Reaped(supervised(&data_dir, &base));
    let first = wait_for_service(&data_dir, None, Duration::from_secs(30))
        .expect("the service should come up under supervision");

    // The OOM killer's signal. Before this fix supervision read it as an
    // orderly stop and exited without saying anything.
    kill(first, libc::SIGKILL);

    let second = wait_for_service(&data_dir, Some(first), Duration::from_secs(30))
        .expect("a killed service must be restarted, not silently abandoned");
    assert_ne!(second, first);
    assert!(
        supervisor.0.try_wait().expect("poll supervisor").is_none(),
        "the supervisor is still supervising"
    );
}

#[test]
fn a_kill_that_a_stop_escalated_to_is_not_restarted() {
    let root = tempfile::tempdir().expect("temporary root");
    let data_dir = root.path().join("data");
    let (addr, _stub) = stub_api();
    let base = format!("http://{addr}");

    let mut supervisor = Reaped(supervised(&data_dir, &base));
    let pid = wait_for_service(&data_dir, None, Duration::from_secs(30))
        .expect("the service should come up under supervision");

    // `service stop` escalates to SIGKILL when the grace budget runs out, and
    // ccc-node's contract is that a restart must not launch on top of a
    // teardown that never ran. The marker is the only thing that separates
    // that kill from the OOM killer's, so this writes the marker the stop
    // would have written and then kills exactly as the stop would.
    let marker = serde_json::json!({
        "schema": "danso.service.stopping.v1",
        "pid": pid,
        "at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        data_dir.join("stopping.json"),
        serde_json::to_vec(&marker).unwrap(),
    )
    .expect("write the stop marker");
    kill(pid, libc::SIGKILL);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut exited = false;
    while Instant::now() < deadline {
        if supervisor.0.try_wait().expect("poll supervisor").is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        exited,
        "a kill an operator's stop escalated to must end supervision, not \
         start a replacement over an unfinished teardown"
    );
    assert_eq!(
        wait_for_service(&data_dir, Some(pid), Duration::from_secs(2)),
        None,
        "and nothing may be running in its place"
    );
}

#[test]
fn a_service_stopped_on_purpose_is_not_restarted() {
    let root = tempfile::tempdir().expect("temporary root");
    let data_dir = root.path().join("data");
    let (addr, _stub) = stub_api();
    let base = format!("http://{addr}");

    let mut supervisor = Reaped(supervised(&data_dir, &base));
    let pid = wait_for_service(&data_dir, None, Duration::from_secs(30))
        .expect("the service should come up under supervision");

    let stop = Command::new(env!("CARGO_BIN_EXE_danso"))
        .args(["service", "stop", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("run service stop");
    assert_eq!(stop.status.code(), Some(0), "a drained stop is success");

    // Supervision must end with the service, not fight the operator who just
    // stopped it. This is the behaviour the old signal rule got right and the
    // fix has to keep.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut exited = false;
    while Instant::now() < deadline {
        if supervisor.0.try_wait().expect("poll supervisor").is_some() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        exited,
        "supervision should finish when the service is stopped"
    );
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "and nothing should have been started in its place"
    );
}

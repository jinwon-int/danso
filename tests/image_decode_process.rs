//! Linux-only isolated decoding experiment, NOT a production ingress.
//! Bubblewrap is mandatory; request mounts are input and preopened output files.
//! System libraries/executables are read-only mounts, not a minimal runtime image.
//! Durable ownership remains absent; async cancellation probes are test-only.
#![cfg(target_os = "linux")]
#[path = "../src/provider/image_pixels.rs"]
mod image_pixels;

use std::{
    fs,
    io::{Cursor, Read, Seek, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const CAP: u64 = 192 * 1024;
const MEMORY: u64 = 256 * 1024 * 1024;
const PROBE_FD: libc::c_int = 198;
const PRIVATE_DESCRIPTOR: &[u8] = b"SYNTHETIC_PRIVATE_DESCRIPTOR_ONLY";

// dup2 runs only after fork: never clear CLOEXEC in the multithreaded parent.
// Caller retains the source file until spawn completes. The fixed target is a
// deliberate exec-time leak fixture, not an image transport protocol.
fn inherit_probe(command: &mut Command, file: &fs::File) {
    let source = file.as_raw_fd();
    assert_ne!(source, PROBE_FD);
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(source, PROBE_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

// Runs only in a fresh test executable, never in the multithreaded parent.
#[test]
fn decode_child_entry() {
    let Some(dir) = std::env::var_os("DANSO_IMAGE_TEST_CHILD") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    match std::env::var("DANSO_IMAGE_TEST_MODE").unwrap().as_str() {
        "descriptor-control" => {
            assert_ne!(unsafe { libc::fcntl(PROBE_FD, libc::F_GETFD) }, -1);
            let mut file = unsafe { fs::File::from_raw_fd(PROBE_FD) };
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            assert!(bytes == PRIVATE_DESCRIPTOR);
            return;
        }
        "descriptors" => {
            assert_eq!(unsafe { libc::fcntl(PROBE_FD, libc::F_GETFD) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
            return;
        }
        "descendant-lock" => {
            // A fresh executable, not Rust allocations after a raw fork. The
            // launcher made this process a new session/process-group leader.
            assert_eq!(unsafe { libc::getsid(0) }, unsafe { libc::getpid() });
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(dir.join("output"))
                .unwrap();
            assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
            file.write_all(b"L").unwrap();
            // No explicit unlock: early lock release must come from process
            // teardown, not a cooperative exit. Finite fallback bounds failures.
            thread::sleep(Duration::from_secs(30));
            std::process::exit(3);
        }
        "descendants" => {
            let mut command = Command::new("/decoder");
            command
                .args(["--exact", "decode_child_entry", "--test-threads=1"])
                // Inherit the launcher's null stdio descriptors: /dev/null
                // intentionally is not mounted inside this sandbox.
                .env("DANSO_IMAGE_TEST_MODE", "descendant-lock");
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let _descendant = ChildGuard(command.spawn().unwrap());
            thread::sleep(Duration::from_secs(30));
            return;
        }
        "sleep" => thread::sleep(Duration::from_secs(30)),
        "memory" => {
            // Verify enforced address-space rejection, without aborting allocator.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    (MEMORY * 2) as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_eq!(ptr, libc::MAP_FAILED);
            return;
        }
        "output" => {
            // Ignore the signal to assert the actual kernel write error and size.
            // This disposition is confined to this fresh child executable.
            unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
            let mut file = fs::File::create(dir.join("output")).unwrap();
            let error = file.write_all(&vec![0; CAP as usize + 1]).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EFBIG));
            assert_eq!(file.metadata().unwrap().len(), CAP);
            return;
        }
        "cpu" => loop {
            std::hint::black_box((0..10000u64).fold(0u64, u64::wrapping_add));
        },
        "isolation" => {
            assert!(!std::path::Path::new("/home").exists());
            assert!(fs::write("/work/unexpected", b"no").is_err());
            assert!(fs::write(dir.join("input"), b"no").is_err());
            assert!(fs::remove_file(dir.join("output")).is_err());
            let port = std::env::var("DANSO_IMAGE_TEST_PORT").unwrap();
            assert!(std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_err());
            return;
        }
        "empty" => return,
        "invalid" => {
            fs::write(dir.join("output"), b"PRIVATE_INVALID_OUTPUT").unwrap();
            return;
        }
        "decode" => {}
        _ => std::process::exit(2),
    }
    let input = fs::read(dir.join("input")).unwrap();
    let mime = std::env::var("DANSO_IMAGE_TEST_MIME").unwrap();
    let output = match image_pixels::normalized_png(&input, &mime, CAP as usize) {
        Ok(output) => output,
        Err(_) => std::process::exit(1),
    };
    fs::write(dir.join("output"), output).unwrap();
}

// The PID namespace contains descendants. Explicit shutdown reports errors;
// Drop is bounded best effort only (not a crash-safe or async ownership protocol).
struct ChildGuard(Child);
// Small fault-injection seam: no Runner/runtime policy is involved.
trait CleanupProcess {
    fn reaped(&mut self) -> std::io::Result<bool>;
    fn kill(&mut self) -> std::io::Result<()>;
}
impl CleanupProcess for Child {
    fn reaped(&mut self) -> std::io::Result<bool> {
        self.try_wait().map(|status| status.is_some())
    }
    fn kill(&mut self) -> std::io::Result<()> {
        Child::kill(self)
    }
}

fn stop_process(process: &mut impl CleanupProcess, budget: Duration) -> Result<(), &'static str> {
    if process.reaped().map_err(|_| "image worker reap failed")? {
        return Ok(());
    }
    if process.kill().is_err() {
        // Exit may race with kill. Only confirmed reap resolves ownership;
        // ESRCH (or any other kill error) alone is not a successful cleanup.
        return match process.reaped() {
            Ok(true) => Ok(()),
            Ok(false) => Err("image worker kill failed"),
            Err(_) => Err("image worker reap failed"),
        };
    }
    let end = Instant::now() + budget;
    loop {
        match process.reaped() {
            Ok(true) => return Ok(()),
            Ok(false) if Instant::now() < end => thread::sleep(Duration::from_millis(5)),
            Ok(false) => return Err("image worker cleanup unresolved"),
            Err(_) => return Err("image worker reap failed"),
        }
    }
}

impl ChildGuard {
    fn stop(&mut self) -> Result<(), &'static str> {
        stop_process(&mut self.0, Duration::from_secs(1))
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn read_output(file: &mut fs::File) -> Result<Vec<u8>, &'static str> {
    let rejected = "image worker output rejected";
    let meta = file.metadata().map_err(|_| rejected)?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.permissions().mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() == 0
        || meta.len() > CAP
    {
        return Err(rejected);
    }
    file.rewind().map_err(|_| rejected)?;
    let mut bytes = Vec::new();
    file.take(CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| rejected)?;
    if bytes.len() as u64 != meta.len() || bytes.len() as u64 > CAP {
        return Err(rejected);
    }
    // Output remains untrusted: this is framing, NOT normalized provenance.
    // Do not run another image decoder in the unsandboxed supervisor.
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(rejected);
    }
    Ok(bytes)
}

fn supervised(
    input: &[u8],
    mime: &str,
    mode: &str,
    deadline: Duration,
) -> Result<Vec<u8>, &'static str> {
    let dir = tempfile::tempdir().map_err(|_| "image worker failed")?;
    supervised_in(input, mime, mode, deadline, dir.path())
}

// The crash probe supplies an outer-owned directory: supervisor death must not
// unlink the observation file or rely on TempDir/ChildGuard destructors.
fn supervised_in(
    input: &[u8],
    mime: &str,
    mode: &str,
    deadline: Duration,
    dir: &std::path::Path,
) -> Result<Vec<u8>, &'static str> {
    supervised_controlled(input, mime, mode, deadline, dir, None)
}

#[derive(Clone, Copy, PartialEq)]
enum CancelStage {
    BeforeSpawn,
    Ready,
}

struct CancelProbe {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stage: CancelStage,
    reached: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}
impl CancelProbe {
    fn cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }

    // Deterministic scheduling seam, not a production blocking protocol.
    fn checkpoint(&self, stage: CancelStage) -> Result<(), &'static str> {
        if self.stage == stage {
            self.reached.send(()).map_err(|_| "probe disconnected")?;
            self.resume
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| "probe resume timeout")?;
        }
        Ok(())
    }
}

fn supervised_controlled(
    input: &[u8],
    mime: &str,
    mode: &str,
    deadline: Duration,
    dir: &std::path::Path,
    probe: Option<&CancelProbe>,
) -> Result<Vec<u8>, &'static str> {
    if input.is_empty()
        || input.len() > CAP as usize
        || !["image/png", "image/jpeg"].contains(&mime)
    {
        return Err("image input rejected");
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(dir.join("input"), input).unwrap();
    fs::set_permissions(dir.join("input"), fs::Permissions::from_mode(0o600)).unwrap();
    let output_path = dir.join("output");
    let mut output = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&output_path)
        .map_err(|_| "image worker failed")?;
    // A live host listener makes the network isolation check non-vacuous.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|_| "image worker failed")?;
    let mut command = Command::new("/usr/bin/bwrap");
    command.args([
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--cap-drop",
        "ALL",
        "--clearenv",
    ]);
    for path in ["/usr", "/lib", "/lib64"] {
        if std::path::Path::new(path).exists() {
            command.args(["--ro-bind", path, path]);
        }
    }
    command
        .arg("--ro-bind")
        .arg(std::env::current_exe().unwrap())
        .arg("/decoder")
        .args(["--dir", "/work", "--ro-bind"])
        .arg(dir.join("input"))
        .arg("/work/input")
        .arg("--bind")
        .arg(&output_path)
        .arg("/work/output")
        .args([
            "--setenv",
            "DANSO_IMAGE_TEST_CHILD",
            "/work",
            "--setenv",
            "DANSO_IMAGE_TEST_MODE",
            mode,
            "--setenv",
            "DANSO_IMAGE_TEST_MIME",
            mime,
            "--setenv",
            "DANSO_IMAGE_TEST_PORT",
        ])
        .arg(listener.local_addr().unwrap().port().to_string())
        .args(["--remount-ro", "/", "--chdir", "/work", "--", "/decoder"]);
    command
        .args(["--exact", "decode_child_entry", "--test-threads=1"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            for (resource, value) in [
                (libc::RLIMIT_AS, MEMORY),
                (libc::RLIMIT_CPU, 2),
                (libc::RLIMIT_FSIZE, CAP),
                (libc::RLIMIT_CORE, 0),
            ] {
                let limit = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    // This descriptor is not a sandbox mount. The launcher must close an
    // intentionally non-CLOEXEC descriptor even when its payload is private.
    let mut descriptor_probe = tempfile::tempfile().map_err(|_| "image worker failed")?;
    if mode == "descriptors" {
        descriptor_probe.write_all(PRIVATE_DESCRIPTOR).unwrap();
        descriptor_probe.rewind().unwrap();
        inherit_probe(&mut command, &descriptor_probe);
    }
    // Fail closed on kernels without close_range(CLOEXEC). Mark rather than
    // close here: Rust's exec-error pipe must survive until exec succeeds.
    // Registered last so even the deliberate leak fixture is sanitized.
    unsafe {
        command.pre_exec(|| {
            if libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    if let Some(probe) = probe {
        probe.checkpoint(CancelStage::BeforeSpawn)?;
        if probe.cancelled() {
            return Err("image request cancelled before spawn");
        }
    }
    let mut child = ChildGuard(command.spawn().map_err(|_| "image worker failed")?);
    let start = Instant::now();
    let mut witnessed = false;
    loop {
        if let Some(probe) = probe {
            if !witnessed && mode == "descendants" {
                output.rewind().map_err(|_| "descendant probe failed")?;
                let mut ready = [0];
                if output.read_exact(&mut ready).is_ok() && ready == *b"L" && lock_is_held(&output)?
                {
                    witnessed = true;
                    probe.checkpoint(CancelStage::Ready)?;
                }
            }
            if probe.cancelled() {
                // Cancellation is not success and cannot bypass cleanup errors.
                // This owner, not the aborted async waiter, retains the Child.
                child.stop()?;
                if witnessed {
                    let end = Instant::now() + Duration::from_secs(1);
                    while lock_is_held(&output)? {
                        if Instant::now() >= end {
                            return Err("descendant cleanup unresolved");
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                }
                return Err("image request cancelled after cleanup");
            }
        }
        match child.0.try_wait() {
            Ok(Some(status)) => {
                if mode == "cpu"
                    && (status.signal() == Some(libc::SIGKILL)
                        || status.code() == Some(128 + libc::SIGKILL))
                {
                    return Ok(Vec::new());
                }
                if !status.success() {
                    return Err("image worker failed");
                }
                break;
            }
            Ok(None) if start.elapsed() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => {
                if mode == "descendants" {
                    // Require both the readiness byte and an independently
                    // opened descriptor's conflicting lock before shutdown.
                    // This cannot pass merely because the descendant failed
                    // to launch. The parent never inherits the lock owner FD.
                    output.rewind().map_err(|_| "descendant probe failed")?;
                    let mut ready = [0];
                    output
                        .read_exact(&mut ready)
                        .map_err(|_| "descendant not ready")?;
                    if ready != *b"L" || !lock_is_held(&output)? {
                        return Err("descendant not ready");
                    }
                    child.stop()?;
                    let end = Instant::now() + Duration::from_secs(1);
                    while lock_is_held(&output)? {
                        if Instant::now() >= end {
                            return Err("descendant cleanup unresolved");
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    return Ok(Vec::new());
                }
                child.stop()?;
                return Err("image worker stopped");
            }
        }
    }
    if ["memory", "output", "isolation", "descriptors"].contains(&mode) {
        return Ok(Vec::new());
    }
    read_output(&mut output)
}

// Lock liveness avoids host PID discovery/reuse and does not add any sandbox
// mount. It proves release of this holder's descriptor, not universal reaping.
fn lock_is_held(file: &fs::File) -> Result<bool, &'static str> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err("descendant probe failed");
        }
        return Ok(false);
    }
    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(true)
    } else {
        Err("descendant probe failed")
    }
}

#[test]
fn timeout_releases_detached_descendant_lock() {
    let input = fixture(image::ImageFormat::Png);
    assert_eq!(
        supervised(&input, "image/png", "descendants", Duration::from_secs(2)),
        Ok(Vec::new())
    );
}

// Fresh executable only; no process-global subreaper in the outer harness.
#[test]
fn crash_supervisor_entry() {
    let Some(dir) = std::env::var_os("DANSO_IMAGE_TEST_SUPERVISOR") else {
        return;
    };
    supervised_in(
        &fixture(image::ImageFormat::Png),
        "image/png",
        "descendants",
        Duration::from_secs(10),
        std::path::Path::new(&dir),
    )
    .unwrap();
}

#[test]
fn supervisor_sigkill_releases_detached_descendant_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut supervisor = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_supervisor_entry", "--test-threads=1"])
            .env_clear()
            .env("DANSO_IMAGE_TEST_SUPERVISOR", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let end = Instant::now() + Duration::from_secs(5);
    let observation = loop {
        assert!(
            supervisor.0.try_wait().unwrap().is_none(),
            "supervisor exited before crash"
        );
        if let Ok(mut file) = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir.path().join("output"))
        {
            let mut ready = [0];
            if file.read_exact(&mut ready).is_ok() && ready == *b"L" && lock_is_held(&file).unwrap()
            {
                break file;
            }
        }
        assert!(Instant::now() < end, "descendant never became ready");
        thread::sleep(Duration::from_millis(5));
    };
    // Child::kill targets the still-owned, unreaped direct child, not a PID
    // discovered in host /proc. SIGKILL bypasses all supervisor destructors.
    supervisor.0.kill().unwrap();
    let end = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(status) = supervisor.0.try_wait().unwrap() {
            assert_eq!(status.signal(), Some(libc::SIGKILL));
            break;
        }
        assert!(Instant::now() < end, "supervisor reap unresolved");
        thread::sleep(Duration::from_millis(5));
    }
    let end = Instant::now() + Duration::from_secs(1);
    while lock_is_held(&observation).unwrap() {
        assert!(Instant::now() < end, "crash descendant cleanup unresolved");
        thread::sleep(Duration::from_millis(5));
    }
    // Witness only: not universal descendant reap or durable orphan recovery.
    // On failure ChildGuard bounds direct-child cleanup; the synthetic holder
    // has its existing 30-second finite fallback, not a production owner.
}

// The outer test owns and joins the OS thread. The async waiter owns only a
// cancellation signal and result receiver; abort cannot drop Child/TempDir.
// This is in-memory ownership, NOT a durable registry or crash recovery service.
// Test-only scope owner: even assertion unwinding cancels, unblocks and joins
// before removing the directory. Join deliberately has no detach-on-timeout:
// a stuck fixture must fail the external test-job timeout, not abandon ownership.
// This blocking destructor is NOT suitable for a production async executor.
struct CancellationOwner {
    dir: tempfile::TempDir,
    thread: Option<thread::JoinHandle<Result<Vec<u8>, &'static str>>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    resume: std::sync::mpsc::Sender<()>,
}
impl CancellationOwner {
    fn is_finished(&self) -> bool {
        self.thread.as_ref().unwrap().is_finished()
    }

    fn join(&mut self) -> thread::Result<Result<Vec<u8>, &'static str>> {
        self.thread.take().unwrap().join()
    }
}
impl Drop for CancellationOwner {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.resume.send(());
        if let Some(owner) = self.thread.take() {
            // Do not panic again if the owner itself panicked while unwinding.
            let _ = owner.join();
        }
    }
}

struct CancelOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

#[tokio::test]
async fn aborted_waiter_preserves_owner_before_spawn_and_after_readiness() {
    use std::sync::{Arc, atomic::AtomicBool, mpsc};
    for stage in [CancelStage::BeforeSpawn, CancelStage::Ready] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (reached_tx, reached_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let probe = CancelProbe {
            cancelled: cancelled.clone(),
            stage,
            reached: reached_tx,
            resume: resume_rx,
        };
        let owner = thread::spawn(move || {
            let result = supervised_controlled(
                &fixture(image::ImageFormat::Png),
                "image/png",
                "descendants",
                Duration::from_secs(10),
                &path,
                Some(&probe),
            );
            // Absent receiver does not change cleanup or its result.
            let _ = result_tx.send(result.clone());
            result
        });
        let mut owner = CancellationOwner {
            dir,
            thread: Some(owner),
            cancelled: cancelled.clone(),
            resume: resume_tx,
        };
        let signal = CancelOnDrop(cancelled.clone());
        let waiter = tokio::spawn(async move {
            let _signal = signal;
            loop {
                match result_rx.try_recv() {
                    Ok(result) => return result,
                    Err(mpsc::TryRecvError::Empty) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => panic!("owner lost"),
                }
            }
        });
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if reached_rx.try_recv().is_ok() {
                break;
            }
            assert!(Instant::now() < end, "owner checkpoint missing");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!owner.is_finished());
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
        // Owner is deliberately parked: abort has not reaped it, published
        // success, or transferred ownership back to the canceled waiter.
        assert!(!owner.is_finished());
        owner.resume.send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        while !owner.is_finished() {
            assert!(Instant::now() < end, "cancellation owner unresolved");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let expected = if stage == CancelStage::BeforeSpawn {
            "image request cancelled before spawn"
        } else {
            "image request cancelled after cleanup"
        };
        assert_eq!(owner.join().unwrap(), Err(expected));
        let output = fs::File::open(owner.dir.path().join("output")).unwrap();
        assert!(!lock_is_held(&output).unwrap());
        if stage == CancelStage::BeforeSpawn {
            assert_eq!(output.metadata().unwrap().len(), 0);
        }
    }
}

#[test]
fn cancellation_owner_joins_on_assertion_unwind() {
    use std::sync::{Arc, atomic::AtomicBool, mpsc};
    for stage in [CancelStage::BeforeSpawn, CancelStage::Ready] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let observed_path = path.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (reached_tx, reached_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let probe = CancelProbe {
            cancelled: cancelled.clone(),
            stage,
            reached: reached_tx,
            resume: resume_rx,
        };
        let owner = thread::spawn(move || {
            let result = supervised_controlled(
                &fixture(image::ImageFormat::Png),
                "image/png",
                "descendants",
                Duration::from_secs(10),
                &path,
                Some(&probe),
            );
            // Directory must still exist when cleanup has completed.
            done_tx.send((result.clone(), path.exists())).unwrap();
            result
        });
        let guard = CancellationOwner {
            dir,
            thread: Some(owner),
            cancelled: cancelled.clone(),
            resume: resume_tx,
        };
        reached_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let output = fs::File::open(observed_path.join("output")).unwrap();
        if stage == CancelStage::Ready {
            assert!(lock_is_held(&output).unwrap());
        }
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _owner = guard;
            panic!("synthetic assertion failure while owner parked");
        }));
        assert!(unwind.is_err());
        assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
        let expected = if stage == CancelStage::BeforeSpawn {
            "image request cancelled before spawn"
        } else {
            "image request cancelled after cleanup"
        };
        assert_eq!(done_rx.try_recv().unwrap(), (Err(expected), true));
        assert!(!lock_is_held(&output).unwrap());
        assert!(!observed_path.exists());
    }
}

fn fixture(format: image::ImageFormat) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(3, 2)
        .write_to(&mut bytes, format)
        .unwrap();
    bytes.into_inner()
}

#[test]
fn real_decode_in_limited_child() {
    for (format, mime) in [
        (image::ImageFormat::Png, "image/png"),
        (image::ImageFormat::Jpeg, "image/jpeg"),
    ] {
        let input = fixture(format);
        let output = supervised(&input, mime, "decode", Duration::from_secs(5)).unwrap();
        assert_eq!(
            image::guess_format(&output).unwrap(),
            image::ImageFormat::Png
        );
        assert_eq!(image::load_from_memory(&output).unwrap().width(), 3);
    }
}

#[test]
fn private_failure_and_resource_limits() {
    let input = fixture(image::ImageFormat::Png);
    assert_eq!(
        supervised(
            b"PRIVATE_IMAGE",
            "image/png",
            "decode",
            Duration::from_secs(5)
        ),
        Err("image worker failed")
    );
    assert!(supervised(&input, "image/png", "memory", Duration::from_secs(5)).is_ok());
    assert!(supervised(&input, "image/png", "output", Duration::from_secs(5)).is_ok());
    let start = Instant::now();
    assert_eq!(
        supervised(&input, "image/png", "sleep", Duration::from_millis(100)),
        Err("image worker stopped")
    );
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn cpu_burn_is_killed_before_wall_deadline() {
    let input = fixture(image::ImageFormat::Png);
    // Bubblewrap forwards child SIGKILL as exit 137; neither identifies its source.
    // A wall timeout remains an error, never a successful resource check.
    // This is an enforcement fixture, not a decoder CPU-safety proof.
    assert!(supervised(&input, "image/png", "cpu", Duration::from_secs(15)).is_ok());
}

#[test]
fn dropping_guard_reaps_direct_child() {
    let child = Command::new("/bin/sleep")
        .arg("30")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id() as libc::pid_t;
    drop(ChildGuard(child));
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn isolated_files_and_network_fail_closed() {
    let input = fixture(image::ImageFormat::Png);
    assert!(supervised(&input, "image/png", "isolation", Duration::from_secs(5)).is_ok());
    for mode in ["empty", "invalid"] {
        assert_eq!(
            supervised(&input, "image/png", mode, Duration::from_secs(5)),
            Err("image worker output rejected")
        );
    }
}

#[test]
fn owned_output_does_not_follow_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("output");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all(&fixture(image::ImageFormat::Png)).unwrap();
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
    assert_eq!(read_output(&mut file), Err("image worker output rejected"));
}

#[test]
fn output_descriptor_rejects_growth_and_public_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("output");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.set_len(CAP + 1).unwrap();
    assert_eq!(read_output(&mut file), Err("image worker output rejected"));
    file.set_len(0).unwrap();
    file.write_all(&fixture(image::ImageFormat::Png)).unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o644))
        .unwrap();
    assert_eq!(read_output(&mut file), Err("image worker output rejected"));
}

#[test]
fn unwinding_guard_reaps_direct_child() {
    let child = Command::new("/bin/sleep")
        .arg("30")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id() as libc::pid_t;
    let result = std::panic::catch_unwind(move || {
        let _guard = ChildGuard(child);
        panic!("synthetic supervisor unwind");
    });
    assert!(result.is_err());
    assert_eq!(
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

// Faults are injected without signals against unrelated host processes.
struct CleanupFixture {
    polls: std::collections::VecDeque<std::io::Result<bool>>,
    kill_error: Option<i32>,
    kills: usize,
}
impl CleanupProcess for CleanupFixture {
    fn reaped(&mut self) -> std::io::Result<bool> {
        self.polls.pop_front().expect("unexpected cleanup poll")
    }
    fn kill(&mut self) -> std::io::Result<()> {
        self.kills += 1;
        self.kill_error
            .map_or(Ok(()), |code| Err(std::io::Error::from_raw_os_error(code)))
    }
}
fn cleanup_fixture(
    polls: Vec<std::io::Result<bool>>,
    kill_error: Option<i32>,
    expected: Result<(), &'static str>,
    kills: usize,
) {
    let mut process = CleanupFixture {
        polls: polls.into(),
        kill_error,
        kills: 0,
    };
    assert_eq!(stop_process(&mut process, Duration::ZERO), expected);
    assert_eq!(process.kills, kills);
    assert!(process.polls.is_empty());
}

#[test]
fn cleanup_requires_reap_even_when_kill_reports_missing_process() {
    cleanup_fixture(vec![Ok(true)], None, Ok(()), 0);
    cleanup_fixture(vec![Ok(false), Ok(true)], Some(libc::ESRCH), Ok(()), 1);
    cleanup_fixture(
        vec![Ok(false), Ok(false)],
        Some(libc::ESRCH),
        Err("image worker kill failed"),
        1,
    );
}

#[test]
fn cleanup_reports_kill_reap_and_unresolved_failures() {
    cleanup_fixture(
        vec![Ok(false), Ok(false)],
        Some(libc::EPERM),
        Err("image worker kill failed"),
        1,
    );
    for polls in [
        vec![Err(std::io::Error::from_raw_os_error(libc::ECHILD))],
        vec![
            Ok(false),
            Err(std::io::Error::from_raw_os_error(libc::ECHILD)),
        ],
    ] {
        let kills = polls.len() - 1;
        cleanup_fixture(polls, None, Err("image worker reap failed"), kills);
    }
    cleanup_fixture(
        vec![
            Ok(false),
            Err(std::io::Error::from_raw_os_error(libc::ECHILD)),
        ],
        Some(libc::ESRCH),
        Err("image worker reap failed"),
        1,
    );
    cleanup_fixture(
        vec![Ok(false), Ok(false)],
        None,
        Err("image worker cleanup unresolved"),
        1,
    );
    cleanup_fixture(vec![Ok(false), Ok(true)], None, Ok(()), 1);
}

#[test]
fn inherited_private_descriptor_is_closed_at_sandbox_launch() {
    let mut private = tempfile::tempfile().unwrap();
    private.write_all(PRIVATE_DESCRIPTOR).unwrap();
    private.rewind().unwrap();
    let mut control = Command::new(std::env::current_exe().unwrap());
    control
        .args(["--exact", "decode_child_entry", "--test-threads=1"])
        .env_clear()
        .env("DANSO_IMAGE_TEST_CHILD", "/unused")
        .env("DANSO_IMAGE_TEST_MODE", "descriptor-control")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    inherit_probe(&mut control, &private);
    let mut child = ChildGuard(control.spawn().unwrap());
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "descriptor positive control failed");
            break;
        }
        assert!(
            Instant::now() < end,
            "descriptor positive control timed out"
        );
        thread::sleep(Duration::from_millis(5));
    }
    // The same injection helper is used across the real sandbox boundary.
    assert!(
        supervised(
            &fixture(image::ImageFormat::Png),
            "image/png",
            "descriptors",
            Duration::from_secs(5),
        )
        .is_ok()
    );
    // Injection did not clear CLOEXEC on the parent's source descriptor.
    assert_ne!(
        unsafe { libc::fcntl(private.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
}

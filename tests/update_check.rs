//! `danso update check` against a release source (#119, §6.3).
//!
//! The unit tests in `danso::fetch` cover which URLs are allowed. What only a
//! process can show is the part an operator and a cron wrapper see: that the
//! signature is checked before anything in the manifest is believed, that a
//! `check` downloads no archive and writes nothing, and what it exits with.
//!
//! The source is a loopback HTTP server, which is the reason `fetch` allows
//! loopback at all — a scheme rule that could only be exercised by weakening
//! it would not stay exercised.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const FIXTURE_KEY: &str = "RWRDmslv0TCmdcfE0s2lpt3hpMuvKCA0wKzLgDP7W81iToI0T/bZXQC7";
/// `docs/unified-design.md` §6.3.
const EXIT_UPDATE_AVAILABLE: i32 = 10;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/release")
}

/// What the stub serves, and what it was asked for.
struct Source {
    address: String,
    requested: Arc<Mutex<Vec<String>>>,
}

/// A loopback release source serving a directory.
///
/// Records every path it was asked for, so a test can assert that `check`
/// fetched the manifest and *not* the archive — the difference between a read
/// and a download is not visible any other way.
fn serve(dir: PathBuf) -> Source {
    serve_with(dir, Behaviour::Files)
}

/// What the stub does instead of serving a file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    Files,
    /// Answer every request with a redirect to a scheme the fetcher refuses.
    RedirectElsewhere,
    /// Answer the manifest with more bytes than the fetcher will accept.
    Oversized,
    /// Redirect to itself, forever — every hop stays on loopback, so the
    /// scheme check allows each one and only the hop limit can stop it.
    RedirectLoop,
}

fn serve_with(dir: PathBuf, behaviour: Behaviour) -> Source {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the release source");
    let address = format!("http://{}", listener.local_addr().expect("address"));
    let requested = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requested);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let path = request
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .trim_start_matches('/')
                .to_string();
            log.lock().expect("log").push(path.clone());

            if behaviour == Behaviour::RedirectLoop {
                let next = path.rsplit('-').next().and_then(|n| n.parse::<u32>().ok());
                let next = next.unwrap_or(0) + 1;
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: /hop-{next}\r\n\
                         Content-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
                continue;
            }
            if behaviour == Behaviour::RedirectElsewhere {
                let _ = stream.write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://example.invalid/x\r\n\
                      Content-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            // Deliberately naive, and safe because it never leaves `dir`:
            // anything with a separator is refused rather than joined.
            let body = match path.contains('/') || path.contains("..") {
                true => None,
                false => std::fs::read(dir.join(&path)).ok(),
            };
            let body = match behaviour == Behaviour::Oversized && path == "SHA256SUMS" {
                // One byte past what the fetcher accepts for a manifest.
                true => Some(vec![b'x'; 64 * 1024 + 1]),
                false => body,
            };
            let response = match &body {
                Some(bytes) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            };
            let _ = stream.write_all(response.as_bytes());
            if let Some(bytes) = body {
                let _ = stream.write_all(&bytes);
            }
        }
    });
    Source { address, requested }
}

fn release_copy(into: &Path) -> PathBuf {
    let dir = into.join("release");
    std::fs::create_dir_all(&dir).expect("release dir");
    for entry in std::fs::read_dir(fixture_dir()).expect("read fixture") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("file type").is_file() {
            std::fs::copy(entry.path(), dir.join(entry.file_name())).expect("copy fixture");
        }
    }
    dir
}

fn configure(home: &Path, source: &str, key: &str) {
    std::fs::create_dir_all(home).expect("home");
    std::fs::write(
        home.join("config.toml"),
        format!("[update]\npublic_key = \"{key}\"\nsource = \"{source}\"\n"),
    )
    .expect("config");
}

fn check(home: &Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
        .args(["update", "check", "--json"])
        .env("DANSO_HOME", home)
        .env_remove("HOME")
        .output()
        .expect("run danso update check")
}

fn code(output: &std::process::Output) -> i32 {
    output.status.code().expect("exit code")
}

fn report(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout was not json: {:?} / stderr {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn a_release_the_source_serves_is_reported_without_downloading_it() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let source = serve(release_copy(temp.path()));
    configure(&home, &source.address, FIXTURE_KEY);

    let output = check(&home);
    // The fixture names no artifact for this target, which is itself the
    // honest answer — and exit 0, because a release that carries nothing for
    // this machine is a fact about the release, not a failure.
    assert_eq!(
        code(&output),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report(&output)["result"], "nothing_for_this_target");

    let asked = source.requested.lock().expect("log").clone();
    assert!(
        asked.contains(&"SHA256SUMS".to_string())
            && asked.contains(&"SHA256SUMS.minisig".to_string()),
        "{asked:?}"
    );
    assert!(
        !asked.iter().any(|path| path.ends_with(".tar.gz")),
        "a check must not download an archive: {asked:?}"
    );
    assert!(
        !home.join("bin").exists() && !home.join("state").exists(),
        "a check must write nothing"
    );
}

#[test]
fn an_artifact_for_this_target_is_offered() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let dir = release_copy(temp.path());

    // Re-sign a manifest naming an artifact for the target this test binary
    // was built for. The fixture's own key cannot sign — its private half was
    // destroyed — so this uses a throwaway pair, which is the same practice
    // `tests/fixtures/release/README.md` documents.
    let keys = temp.path().join("keys");
    std::fs::create_dir_all(&keys).expect("keys");
    let generated = std::process::Command::new("minisign")
        .args(["-G", "-W", "-p"])
        .arg(keys.join("pub"))
        .arg("-s")
        .arg(keys.join("sec"))
        .output();
    let Ok(generated) = generated else {
        // minisign is a contract of the release runner, not of every
        // developer machine. Skipping loudly beats a green run that proved
        // nothing, so say so.
        eprintln!("SKIPPED: minisign is not installed on this machine");
        return;
    };
    assert!(generated.status.success(), "generate a throwaway key");

    // `build.rs` sets this for the whole package, integration tests included,
    // so the test and the binary agree on the triple by construction.
    let name = format!("danso-9.9.9-{}.tar.gz", env!("DANSO_TARGET"));
    std::fs::copy(dir.join("danso-9.9.9-ok.tar.gz"), dir.join(&name)).expect("name for target");

    let digest = std::process::Command::new("sha256sum")
        .arg(&name)
        .current_dir(&dir)
        .output()
        .expect("sha256sum");
    std::fs::write(dir.join("SHA256SUMS"), &digest.stdout).expect("manifest");
    std::fs::remove_file(dir.join("SHA256SUMS.minisig")).expect("drop the old signature");
    let signed = std::process::Command::new("minisign")
        .args(["-S", "-s"])
        .arg(keys.join("sec"))
        .args(["-m", "SHA256SUMS"])
        .current_dir(&dir)
        .output()
        .expect("sign");
    assert!(signed.status.success(), "sign the manifest");

    let key = std::fs::read_to_string(keys.join("pub")).expect("public key");
    let key = key.lines().last().expect("key line").trim().to_string();
    let source = serve(dir);
    configure(&home, &source.address, &key);

    let output = check(&home);
    assert_eq!(
        code(&output),
        EXIT_UPDATE_AVAILABLE,
        "nothing recorded what this node was installed from, so it cannot be \
         compared — that is an update to consider, not 'up to date'; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = report(&output);
    assert_eq!(report["result"], "unknown");
    assert_eq!(report["artifact"], name);

    // The assertion that matters, and it has to be here rather than on a
    // release with nothing for this target: there, `check` returns before it
    // ever reaches the point where it could download.
    let asked = source.requested.lock().expect("log").clone();
    assert!(
        !asked.iter().any(|path| path.ends_with(".tar.gz")),
        "a check reports what is available; downloading it is `apply`'s job: {asked:?}"
    );
}

#[test]
fn a_source_that_redirects_off_the_allowed_schemes_is_refused() {
    // A redirect is a second URL, and only the first one went through the
    // configuration check. Without re-checking each hop, `https://…` in
    // config.toml is a promise about the first request only.
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let source = serve_with(release_copy(temp.path()), Behaviour::RedirectElsewhere);
    configure(&home, &source.address, FIXTURE_KEY);

    let output = check(&home);
    assert_eq!(code(&output), 2);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unreachable"), "{stderr}");
    assert!(
        !stderr.contains("example.invalid"),
        "and the redirect target is not echoed either: {stderr}"
    );
}

#[test]
fn a_redirect_loop_stops_at_the_hop_limit() {
    // Every hop is loopback, so the scheme check allows all of them: only the
    // bound ends this. Counting the hops is what tells a wired-in policy from
    // one the client never consults — a refused hop and a followed hop that
    // fails both surface as the same error.
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let source = serve_with(release_copy(temp.path()), Behaviour::RedirectLoop);
    configure(&home, &source.address, FIXTURE_KEY);

    let output = check(&home);
    assert_eq!(code(&output), 2);
    let hops = source.requested.lock().expect("log").len();
    assert!(
        hops <= 8,
        "{hops} hops before giving up; the limit is 5, so a client that never \
         consults the policy would still be going"
    );
}

#[test]
fn a_source_that_answers_with_more_than_the_cap_is_refused() {
    // The manifest declares digests, never sizes, so nothing downstream
    // bounds this. Without the cap a hostile or broken source runs the node
    // out of memory before anything gets the chance to reject it.
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let source = serve_with(release_copy(temp.path()), Behaviour::Oversized);
    configure(&home, &source.address, FIXTURE_KEY);

    let output = check(&home);
    assert_eq!(code(&output), 2);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("larger than"),
        "the failure must name the cap, not a parse error further down: {stderr}"
    );
}

#[test]
fn a_manifest_the_key_did_not_sign_is_refused_before_it_is_read() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let dir = release_copy(temp.path());
    // Correctly signed, but for a different key than the one configured.
    let source = serve(dir);
    configure(
        &home,
        &source.address,
        // A valid minisign line that did not sign this manifest.
        "RWRMUffxvBQUP7oZ5RSwwkRp/UjAH5GUEQ6x1NAu4mGZvD+NqXHMVcMq",
    );

    let output = check(&home);
    assert_ne!(code(&output), 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("update check failed"), "{stderr}");
}

#[test]
fn a_tampered_manifest_is_refused() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let dir = release_copy(temp.path());
    let manifest = dir.join("SHA256SUMS");
    let mut text = std::fs::read_to_string(&manifest).expect("manifest");
    text.push_str("0000000000000000000000000000000000000000000000000000000000000000  evil\n");
    std::fs::write(&manifest, text).expect("tamper");

    let source = serve(dir);
    configure(&home, &source.address, FIXTURE_KEY);

    let output = check(&home);
    assert_ne!(code(&output), 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("evil"),
        "a rejected manifest must not echo its contents: {stderr}"
    );
}

#[test]
fn no_configured_source_is_an_error_that_says_what_to_do() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::write(
        home.join("config.toml"),
        format!("[update]\npublic_key = \"{FIXTURE_KEY}\"\n"),
    )
    .expect("config");

    let output = check(&home);
    assert_eq!(code(&output), 2);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("source"), "{stderr}");
    assert!(stderr.contains("--artifact-dir"), "{stderr}");
}

#[test]
fn a_plain_http_source_is_refused_by_configuration() {
    // Not at fetch time: a node whose release source is unusable should say so
    // when the config is read, not on the first cron tick that needed it.
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    configure(&home, "http://example.invalid/releases", FIXTURE_KEY);

    let output = check(&home);
    assert_eq!(code(&output), 2);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("https"), "{stderr}");
}

#[test]
fn a_source_that_is_not_there_fails_without_naming_it() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    // Port 1 on loopback: refused immediately, no timeout needed.
    configure(&home, "http://127.0.0.1:1/releases", FIXTURE_KEY);

    let output = check(&home);
    assert_eq!(code(&output), 2);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unreachable"), "{stderr}");
    assert!(
        !stderr.contains("127.0.0.1"),
        "a source can carry a token in its URL: {stderr}"
    );
}

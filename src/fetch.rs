//! Fetching a release over the network (`docs/unified-design.md` §6.3).
//!
//! This lives in the root package and not in `danso-ops` on purpose. That crate
//! is deliberately free of a network stack — it verifies and installs, and a
//! transport belongs on the other side of that line. Here the two meet: this
//! module downloads bytes, `danso_ops::release` decides whether they are worth
//! anything.
//!
//! ## The signature is the boundary, not the transport
//!
//! Worth stating plainly, because the opposite assumption is what makes
//! updaters dangerous. Whoever controls this connection can serve whatever
//! they like; `verify_manifest` refuses it, because the manifest is checked
//! against the release key before a single name inside it is read. HTTPS here
//! buys confidentiality and stops a casual tamper-and-DoS, and it is *not*
//! what makes an update safe. Nothing downloaded is trusted until it verifies.
//!
//! What this module is responsible for is the part a signature cannot cover:
//! not being made to download forever, not being redirected somewhere
//! surprising, not writing what it got somewhere another user can reach, and
//! not hanging a cron job until somebody notices.

use anyhow::{Context, Result, bail, ensure};
use std::path::Path;
use std::time::Duration;

/// A manifest is a few lines per artifact; this is room for hundreds.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// A minisign signature file is two lines.
pub const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
/// The release archive. Measured: the 0.1.0 `x86_64-unknown-linux-gnu` archive
/// is about 5 MB, so this is ample headroom and still bounded — the manifest
/// declares a digest, never a size, so without a cap a hostile or broken
/// server can make this run until the disk is full.
pub const MAX_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Small files: a server that cannot produce two lines in this long is not
/// going to produce an archive either.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);
const ARTIFACT_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_REDIRECTS: usize = 5;

/// Whether a URL may be fetched from.
///
/// `https` anywhere; `http` only to loopback. A loopback URL is not a network
/// hop — it is a local mirror or a test — and refusing it would mean the only
/// way to exercise this code is to weaken it. Everything else is refused
/// including `file:` and `ftp:`, not because the signature would accept them
/// but because a release URL that is not a release URL is a configuration
/// mistake worth naming.
/// Parsed rather than pattern-matched. Splitting a URL on `/`, `@` and `:` by
/// hand gets `http://[::1]:9/x` wrong — it did, in the first version of this
/// function — and every other way of getting a host out of a string by hand is
/// wrong in some case somebody else has already found.
pub fn is_allowed(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        // `host_str` gives the bracketed form for IPv6, which is what a URL
        // carries; both spellings are listed rather than stripped, because
        // stripping is the kind of hand-parsing this function stopped doing.
        "http" => matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1"),
        _ => false,
    }
}

fn check_url(url: &str) -> Result<()> {
    ensure!(
        is_allowed(url),
        "release source must be an https URL (or http to loopback)"
    );
    Ok(())
}

/// What to do with a redirect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirect {
    Follow,
    TooMany,
    Refused,
}

/// Whether to follow one hop.
///
/// Separated from the client so it can be tested. It cannot be tested through
/// a real redirect: a hop that is refused and a hop that is followed to a host
/// that does not resolve both come back as "unreachable", so an integration
/// test cannot tell the policy working from the policy missing. That was
/// measured — the mutation removing this check passed the redirect test.
pub fn redirect_decision(url: &str, hops: usize) -> Redirect {
    if hops >= MAX_REDIRECTS {
        return Redirect::TooMany;
    }
    match is_allowed(url) {
        true => Redirect::Follow,
        false => Redirect::Refused,
    }
}

fn client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        // Redirects are followed, but every hop is checked again: a redirect
        // to `http://` elsewhere would otherwise silently undo the policy
        // above, and the first request's URL is not the one that serves.
        .redirect(reqwest::redirect::Policy::custom(
            |attempt| match redirect_decision(attempt.url().as_str(), attempt.previous().len()) {
                Redirect::Follow => attempt.follow(),
                Redirect::TooMany => attempt.error("too many redirects"),
                Redirect::Refused => {
                    attempt.error("redirected to a scheme this updater will not fetch")
                }
            },
        ))
        .user_agent(concat!("danso/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build the release fetcher")
}

/// Fetch a URL into memory, refusing anything past `limit`.
///
/// Read incrementally rather than trusting `Content-Length`: the header is
/// whatever the server said, and a body that keeps coming after it is exactly
/// the case a cap exists for.
pub fn get(url: &str, limit: u64, timeout: Duration) -> Result<Vec<u8>> {
    use std::io::Read;
    check_url(url)?;
    let response = client(timeout)?
        .get(url)
        .send()
        // Body-free: the error carries the failure, never the URL — a release
        // source can hold a token in a query string.
        .map_err(|_| anyhow::anyhow!("release source unreachable"))?;
    ensure!(
        response.status().is_success(),
        "release source answered {}",
        response.status().as_u16()
    );
    let mut bytes = Vec::new();
    // One byte past the limit is enough to know it was exceeded.
    response
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("release source stopped mid-transfer"))?;
    if bytes.len() as u64 > limit {
        bail!("release artifact is larger than this updater will download");
    }
    Ok(bytes)
}

/// Fetch the signed manifest and its signature.
///
/// Returned as bytes, not parsed: parsing is `danso_ops::release`'s job and it
/// does it *after* checking the signature over exactly these bytes.
pub fn manifest(base: &str) -> Result<(Vec<u8>, String)> {
    let base = base.trim_end_matches('/');
    let manifest = get(
        &format!("{base}/{}", danso_ops::release::MANIFEST_FILE),
        MAX_MANIFEST_BYTES,
        MANIFEST_TIMEOUT,
    )?;
    let signature = get(
        &format!("{base}/{}", danso_ops::release::SIGNATURE_FILE),
        MAX_SIGNATURE_BYTES,
        MANIFEST_TIMEOUT,
    )?;
    let signature = String::from_utf8(signature).context("signature file is not text")?;
    Ok((manifest, signature))
}

/// Download one artifact named by an already-verified manifest.
///
/// `name` must come from [`danso_ops::release::Manifest`], never from a URL, a
/// header or an operator's argument: the manifest is the only place a name has
/// been signed, and `release` already refuses names that are paths, options or
/// anything but a plain file name.
pub fn artifact(base: &str, name: &str) -> Result<Vec<u8>> {
    let base = base.trim_end_matches('/');
    get(
        &format!("{base}/{name}"),
        MAX_ARTIFACT_BYTES,
        ARTIFACT_TIMEOUT,
    )
}

/// Write a downloaded release into a private directory `apply` can read.
///
/// Owner-only, and created fresh: `apply` is about to run `tar` and then a
/// binary out of whatever is here, so a directory another user can write to
/// would make the download the least interesting way in.
pub fn stage(
    dir: &Path,
    manifest: &[u8],
    signature: &str,
    name: &str,
    artifact: &[u8],
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| "create the staging directory".to_string())?;
    for (file, bytes) in [
        (danso_ops::release::MANIFEST_FILE, manifest),
        (danso_ops::release::SIGNATURE_FILE, signature.as_bytes()),
        (name, artifact),
    ] {
        let path = dir.join(file);
        let mut handle = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .context("write the staged release")?;
        handle
            .write_all(bytes)
            .context("write the staged release")?;
        handle.sync_all().context("write the staged release")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_is_allowed_and_plain_http_is_not() {
        assert!(is_allowed("https://example.invalid/releases/0.1.0"));
        assert!(is_allowed("https://example.invalid"));
        assert!(!is_allowed("http://example.invalid/releases"));
        assert!(
            !is_allowed("https://"),
            "a scheme with no host is not a URL"
        );
    }

    #[test]
    fn loopback_http_is_allowed_because_it_is_not_a_network_hop() {
        for url in [
            "http://127.0.0.1:8080/releases",
            "http://localhost/releases",
            "http://localhost:1/x",
            "http://[::1]:9/x",
        ] {
            assert!(is_allowed(url), "{url}");
        }
    }

    #[test]
    fn a_host_that_merely_looks_like_loopback_is_not() {
        // The interesting ones: userinfo and a prefix match. Both are how a
        // naive check lets an arbitrary host through.
        for url in [
            "http://localhost.example.invalid/releases",
            "http://127.0.0.1.example.invalid/releases",
            "http://evil.invalid/@127.0.0.1/x",
            "http://user@evil.invalid/x",
            "http://127.0.0.2/x",
        ] {
            assert!(!is_allowed(url), "{url}");
        }
        // Userinfo naming loopback still resolves to loopback, so it passes —
        // pinned so the behaviour is deliberate rather than discovered.
        assert!(is_allowed("http://user@127.0.0.1/x"));
    }

    #[test]
    fn other_schemes_are_refused() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.invalid/x",
            "ssh://example.invalid/x",
            "javascript:alert(1)",
            "/releases",
            "example.invalid/releases",
            "",
        ] {
            assert!(!is_allowed(url), "{url}");
        }
    }

    #[test]
    fn a_redirect_is_checked_again_at_every_hop() {
        // `https://…` in config.toml is otherwise a promise about the first
        // request only: a redirect is a second URL that never went through it.
        assert_eq!(
            redirect_decision("https://elsewhere.invalid/x", 0),
            Redirect::Follow
        );
        assert_eq!(
            redirect_decision("http://elsewhere.invalid/x", 0),
            Redirect::Refused,
            "a redirect off https is how a source downgrades the transport \
             without the configuration ever saying so"
        );
        assert_eq!(
            redirect_decision("file:///etc/passwd", 0),
            Redirect::Refused
        );
        assert_eq!(
            redirect_decision("http://127.0.0.1:9/x", 0),
            Redirect::Follow,
            "loopback stays allowed on a hop, as it is on the first request"
        );
    }

    #[test]
    fn a_redirect_chain_is_bounded() {
        assert_eq!(
            redirect_decision("https://ok.invalid/x", MAX_REDIRECTS - 1),
            Redirect::Follow
        );
        assert_eq!(
            redirect_decision("https://ok.invalid/x", MAX_REDIRECTS),
            Redirect::TooMany,
            "a source that redirects forever is a source that hangs a cron job"
        );
        // The limit outranks the scheme check only in the sense that it is
        // reached first; both refuse.
        assert_eq!(
            redirect_decision("http://bad.invalid/x", MAX_REDIRECTS),
            Redirect::TooMany
        );
    }

    #[test]
    fn staging_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("release");
        stage(
            &dir,
            b"manifest",
            "signature",
            "danso-0.0.0-x.tar.gz",
            b"tar",
        )
        .unwrap();

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for file in ["SHA256SUMS", "SHA256SUMS.minisig", "danso-0.0.0-x.tar.gz"] {
            let mode = std::fs::metadata(dir.join(file))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{file}");
        }
    }

    #[test]
    fn staging_refuses_to_write_over_an_existing_release() {
        // `create_new`: a staging directory that already holds a manifest is
        // one somebody else is using, or one a previous run left behind. Both
        // are reasons to stop rather than to mix two releases in one place.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("release");
        stage(&dir, b"a", "b", "danso-0.0.0-x.tar.gz", b"c").unwrap();
        assert!(stage(&dir, b"a", "b", "danso-0.0.0-x.tar.gz", b"c").is_err());
    }

    #[test]
    fn a_refused_url_is_not_fetched() {
        // The check happens before any connection, so this returns without a
        // network stack and without a timeout.
        let error = get("http://example.invalid/x", 10, Duration::from_secs(1)).unwrap_err();
        assert!(format!("{error}").contains("https"), "{error}");
    }

    #[test]
    fn a_failure_does_not_echo_the_url() {
        // A release source can carry a token in a query string; an error that
        // prints it puts it in a cron log.
        let error = get(
            "https://127.0.0.1:1/releases?token=SHOULD-NOT-APPEAR",
            10,
            Duration::from_millis(200),
        )
        .unwrap_err();
        let rendered = format!("{error:#}");
        assert!(!rendered.contains("SHOULD-NOT-APPEAR"), "{rendered}");
        assert!(!rendered.contains("127.0.0.1"), "{rendered}");
    }
}

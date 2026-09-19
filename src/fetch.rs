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
//! surprising, and not hanging a cron job until somebody notices.
//!
//! Errors name a reason and never the URL. A release source is operator
//! configuration — a private mirror's path, an internal host — and a cron log
//! is not where it belongs. (A source cannot carry a query string at all; see
//! [`is_allowed`].)

use anyhow::{Context, Result, bail, ensure};
use std::time::Duration;

/// A manifest is a few lines per artifact; this is room for hundreds.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// A minisign signature file is two lines.
pub const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Small files: a server that cannot produce two lines in this long is not
/// going to produce an archive either.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;

/// Whether a URL may be fetched from.
///
/// `https` anywhere; `http` only to loopback. A loopback URL is not a network
/// hop — it is a local mirror or a test — and refusing it would mean the only
/// way to exercise this code is to weaken it. Everything else is refused
/// including `file:` and `ftp:`, not because the signature would accept them
/// but because a release URL that is not a release URL is a configuration
/// mistake worth naming.
///
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
    // A base with a query or a fragment cannot work: the sub-paths are joined
    // onto the end, so `…/rel?token=X` becomes `…/rel?token=X/SHA256SUMS` —
    // the token is extended, not the path, and the failure arrives as a
    // signature error pointing at the wrong thing entirely. Refusing it at
    // config load says what is actually wrong.
    if url.query().is_some() || url.fragment().is_some() {
        return false;
    }
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
/// `previous` is what `reqwest` passes: it **includes the original URL**, so
/// the comparison is `>` and not `>=`. With `>=` the bound is one hop tighter
/// than `MAX_REDIRECTS` says — measured, five requests where six were meant.
/// reqwest's own `Policy::limited` compensates the same way.
pub fn redirect_decision(url: &str, previous: usize) -> Redirect {
    if previous > MAX_REDIRECTS {
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
    // Loopback http is intentional (tests + local mirrors). Non-loopback http
    // is refused by check_url. codeql[rust/cleartext-transmission]
    let response = client(timeout)?
        .get(url)
        .send()
        // Body-free: the error carries the failure, never the URL. A release
        // source is operator configuration and a cron log is not where it
        // belongs.
        .map_err(|_| anyhow::anyhow!("release source unreachable"))?;
    ensure!(
        response.status().is_success(),
        "release source answered {}",
        response.status().as_u16()
    );
    // A deadline this function owns, because the client's does not mean what
    // it looks like: `reqwest::blocking`'s `timeout` is re-armed on *every*
    // `read`, so it bounds a stall and not a transfer. Measured — a source
    // sending one byte every five seconds against a 30-second budget was
    // still being read at 150 seconds. At that rate the manifest cap alone is
    // three weeks, which is not a timeout, it is a wedged cron job.
    let deadline = std::time::Instant::now() + timeout;
    let mut reader = response.take(limit + 1);
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if std::time::Instant::now() >= deadline {
            bail!("release source took longer than this updater will wait");
        }
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            Err(_) => bail!("release source stopped mid-transfer"),
        }
    }
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
    fn a_source_with_a_query_or_fragment_is_refused() {
        // Not a style rule: `{base}/{file}` extends whatever comes last, so a
        // query base produces `…?token=X/SHA256SUMS` and the operator is told
        // the signature is malformed. Refused where it can be explained.
        for url in [
            "https://host.invalid/rel?token=X",
            "https://host.invalid/rel#frag",
            "https://host.invalid/?a=b",
        ] {
            assert!(!is_allowed(url), "{url}");
        }
        assert!(is_allowed("https://host.invalid/rel"));
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
        // The argument is `attempt.previous().len()`, which **includes the
        // original URL**: at `previous == MAX_REDIRECTS` exactly
        // `MAX_REDIRECTS - 1` hops have been followed, so this is the last one
        // that may be. Getting this wrong makes the bound one tighter than the
        // constant says, silently — measured, and the reason the comparison is
        // `>` rather than `>=`.
        assert_eq!(
            redirect_decision("https://ok.invalid/x", MAX_REDIRECTS),
            Redirect::Follow,
            "the original URL is one of the `previous` entries"
        );
        assert_eq!(
            redirect_decision("https://ok.invalid/x", MAX_REDIRECTS + 1),
            Redirect::TooMany,
            "a source that redirects forever is a source that hangs a cron job"
        );
        // The limit outranks the scheme check only in the sense that it is
        // reached first; both refuse.
        assert_eq!(
            redirect_decision("http://bad.invalid/x", MAX_REDIRECTS + 1),
            Redirect::TooMany
        );
    }

    #[test]
    fn a_refused_url_is_not_fetched() {
        // The check happens before any connection, so this returns without a
        // network stack and without a timeout.
        let error = get("http://example.invalid/x", 10, Duration::from_secs(1)).unwrap_err();
        assert!(format!("{error}").contains("https"), "{error}");
    }

    /// A server that answers, then sends one byte at a time, forever.
    ///
    /// `reqwest::blocking`'s own timeout re-arms on every `read`, so this is
    /// the shape that slips past it: never stalled, never finished.
    fn trickle() -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = format!("http://{}", listener.local_addr().expect("address"));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = [0u8; 1024];
                use std::io::Read;
                let _ = stream.read(&mut buffer);
                if stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 65536\r\n\
                          Connection: close\r\n\r\n",
                    )
                    .is_err()
                {
                    continue;
                }
                loop {
                    if stream.write_all(b"x").is_err() || stream.flush().is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
        address
    }

    #[test]
    fn a_source_that_trickles_forever_is_still_bounded() {
        // Measured before this bound existed: a source sending one byte every
        // five seconds against a 30-second budget was still being read at 150
        // seconds. The client's timeout bounds a stall, not a transfer.
        let address = trickle();
        let started = std::time::Instant::now();
        let error = get(
            &format!("{address}/x"),
            64 * 1024,
            Duration::from_millis(400),
        )
        .expect_err("a transfer that never ends must not be waited on forever");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "gave up after {elapsed:?}; the budget was 400ms"
        );
        assert!(
            format!("{error}").contains("longer than"),
            "and it says the wait ended, not that the source stalled: {error}"
        );
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

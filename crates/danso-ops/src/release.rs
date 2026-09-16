//! Release manifest signature verification (`docs/unified-design.md` §6.3).
//!
//! Danso ships a compiled binary, so self-update cannot reuse ccc-node's
//! "git fast-forward + `setup.sh`" model. What replaces it is a signed release:
//! `SHA256SUMS` lists every artifact, `SHA256SUMS.minisig` signs that list, and
//! the artifact is only ever trusted through the list.
//!
//! The order this module enforces, and the reason it exists:
//!
//! 1. Verify the signature over the **raw manifest bytes** with the release
//!    public key.
//! 2. Only then parse the manifest into entries.
//! 3. Compare an artifact's SHA-256 against the entry for its exact name.
//!
//! Parsing before verifying would hand attacker-controlled text to the parser,
//! which is the bug class signing was adopted to close. So `Manifest` has no
//! public constructor that skips step 1 — the only way to obtain one is
//! [`verify_manifest`].
//!
//! Two things this module deliberately does **not** do:
//!
//! * It does not trust the signature's *trusted comment*. The comment is
//!   attacker-visible metadata; nothing here reads a version, a channel or a
//!   filename out of it. Names come from the signed manifest body only.
//! * It has no bypass. There is no "signature missing, continue anyway" path;
//!   a caller that wants to run unsigned has to not call this module at all.
//!
//! Legacy-format signatures are rejected. The difference is prehashing: the
//! current format signs a BLAKE2b hash of the file, the legacy one signs the
//! file through pure EdDSA. Both cover the trusted comment. Rejecting legacy
//! is a one-format rule — if both are accepted, an attacker picks which one
//! the verifier applies, and the producing side can drift without anyone
//! noticing. `minisign -V` accepts *both* unless `-H` is passed, so the
//! signing self-test has to opt in explicitly to agree with this module.

use anyhow::{Context, Result, bail, ensure};
use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The signed list of artifacts published with every release.
pub const MANIFEST_FILE: &str = "SHA256SUMS";
/// The detached minisign signature over [`MANIFEST_FILE`].
pub const SIGNATURE_FILE: &str = "SHA256SUMS.minisig";

/// The release public key, embedded at compile time.
///
/// `docs/unified-design.md` §6.3 requires the key to ship *in the binary* and
/// to stay rotatable through `[update] public_key`. Embedding is what makes a
/// fresh install verifiable before it has any config at all; the config
/// override is what makes rotation possible without a flag day.
///
/// This is the whole key file, comment line included, so that the checked-in
/// file stays the single source of truth — there is no second copy of the key
/// material in Rust source to drift from it.
pub const EMBEDDED_PUBLIC_KEY_FILE: &str = include_str!("../../../keys/danso-release.pub");

/// The base64 key line of [`EMBEDDED_PUBLIC_KEY_FILE`], comment stripped.
pub fn embedded_public_key() -> Result<&'static str> {
    public_key_line(EMBEDDED_PUBLIC_KEY_FILE)
}

/// Extract the key line from a minisign public key file.
///
/// A minisign `.pub` is two lines: an untrusted comment and the key. Blank
/// lines and CRLF picked up on the way through CI are tolerated; a *second*
/// key line is not. "Take the first" and "take the last" disagree on such a
/// file, and a trust root must not depend on which one the reader picked.
pub fn public_key_line(file: &str) -> Result<&str> {
    let mut lines = file
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("untrusted comment:"));
    let line = lines.next().context("public key file has no key line")?;
    ensure!(
        lines.next().is_none(),
        "public key file has more than one key line"
    );
    Ok(line)
}

/// Reject anything that is not a usable minisign public key line.
///
/// Config validation calls this instead of shape-checking the string itself.
/// A hand-rolled length test cannot tell a minisign key from 56 characters of
/// base64, and a key that passes config check but fails at update time turns a
/// startup error into an outage during the one operation that must not surprise
/// anyone.
///
/// Note what a minisign key line carries that a bare ed25519 key does not: an
/// 8-byte key id. Verification compares it against the signature's id, so a
/// signature produced by a *different* key is refused by id before any
/// cryptography runs. Storing the bare key would drop that check.
pub fn check_public_key(key: &str) -> Result<()> {
    PublicKey::from_base64(key)
        .map_err(|e| anyhow::anyhow!("not a minisign public key line: {e}"))?;
    Ok(())
}

/// A verified release manifest: artifact name to lowercase SHA-256 hex.
///
/// Holding one of these is the proof that the signature checked out. It is
/// constructed only by [`verify_manifest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    entries: BTreeMap<String, String>,
}

impl Manifest {
    /// The signed digest for `name`, or `None` if the release does not carry it.
    pub fn digest(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(String::as_str)
    }

    /// Artifact names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Number of signed artifacts.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Always false: [`verify_manifest`] rejects an empty manifest.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Verify `signature` over `manifest` with `public_key`, then parse it.
///
/// `public_key` is the base64 key line (see [`public_key_line`]). Any failure —
/// unparseable key, unparseable signature, wrong signer, tampered bytes,
/// malformed manifest — is an `Err`. There is no partial success.
pub fn verify_manifest(manifest: &[u8], signature: &str, public_key: &str) -> Result<Manifest> {
    let key = PublicKey::from_base64(public_key)
        .map_err(|e| anyhow::anyhow!("release public key is not a minisign key: {e}"))?;
    let signature = Signature::decode(signature)
        .map_err(|e| anyhow::anyhow!("release signature is not a minisign signature: {e}"))?;
    // `false` = reject the legacy format; see the module comment.
    key.verify(manifest, &signature, false)
        .map_err(|e| anyhow::anyhow!("release signature verification failed: {e}"))?;
    parse_manifest(manifest)
}

/// Check `bytes` against the signed digest for `name`.
///
/// An artifact the manifest does not name is an error, not a pass. A release
/// that ships a file nobody signed is exactly the case this rejects.
pub fn verify_artifact(manifest: &Manifest, name: &str, bytes: &[u8]) -> Result<()> {
    let expected = manifest
        .digest(name)
        .with_context(|| format!("{name} is not listed in the signed {MANIFEST_FILE}"))?;
    let actual = hex_digest(bytes);
    ensure!(
        actual == expected,
        "{name} does not match the signed digest"
    );
    Ok(())
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Parse `sha256sum` output: 64 lowercase hex, two spaces, a plain file name.
///
/// Only called on bytes whose signature already verified.
fn parse_manifest(manifest: &[u8]) -> Result<Manifest> {
    let text = std::str::from_utf8(manifest).context("signed manifest is not UTF-8")?;
    let mut entries = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let number = index + 1;
        let (digest, name) = line
            .split_once("  ")
            .with_context(|| format!("manifest line {number} is not `<sha256>  <name>`"))?;
        ensure!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "manifest line {number} does not start with a sha256 digest"
        );
        ensure!(
            digest.bytes().all(|b| !b.is_ascii_uppercase()),
            "manifest line {number} digest must be lowercase hex"
        );
        check_artifact_name(name, number)?;
        if entries
            .insert(name.to_string(), digest.to_string())
            .is_some()
        {
            // A duplicate lets whoever built the manifest decide which digest a
            // reader sees. Both entries are signed, so neither is "the" one.
            bail!("manifest lists {name} more than once");
        }
    }
    ensure!(!entries.is_empty(), "signed manifest lists no artifacts");
    Ok(Manifest { entries })
}

/// A manifest name is a file in the release, never a path.
///
/// The caller joins these onto a download directory, so `../` or an absolute
/// path would write outside it. Rejecting at parse keeps every caller safe
/// rather than relying on each one to re-check.
fn check_artifact_name(name: &str, number: usize) -> Result<()> {
    ensure!(!name.is_empty(), "manifest line {number} has an empty name");
    // `sha256sum` separates with exactly two spaces, so a name that arrives
    // with leading or trailing whitespace came from a manifest that padded the
    // separator. Accepting it produces a name that looks identical in a log and
    // compares unequal, and two such names defeat the duplicate check below.
    ensure!(
        name.trim() == name,
        "manifest line {number} name must not be padded with whitespace"
    );
    // A caller joins these onto a download directory and may hand them to a
    // tool as arguments. `--checkpoint-action=exec=...` is the GNU tar
    // argument-injection primitive; a release artifact never starts with `-`.
    ensure!(
        !name.starts_with('-'),
        "manifest line {number} name must not start with a dash"
    );
    ensure!(
        !name.contains('/') && !name.contains('\\'),
        "manifest line {number} name must not contain a path separator"
    );
    ensure!(
        name != ".." && name != ".",
        "manifest line {number} name must not be a directory reference"
    );
    ensure!(
        name.chars().all(|c| !c.is_control()),
        "manifest line {number} name must not contain control characters"
    );
    ensure!(name.is_ascii(), "manifest line {number} name must be ASCII");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Throwaway fixture key, generated for these tests only. It is not the
    // release key and nothing signed by it is ever published.
    const FIXTURE_KEY: &str = "RWQbf5jrBubDWDWgYNOyi1nYm+uTycGKIGfh+oOVB09ocmmx8o4mAj8w";
    // A second, unrelated key: used to prove a valid signature from the wrong
    // signer is rejected, which a "does it parse" test would not catch.
    const OTHER_KEY: &str = "RWST0HTBrC+WCOk/vrCjfFlPdAlZW4wvbZSOGyEKhSXIl+oY0jX4mrCz";

    const MANIFEST: &[u8] = b"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  danso-0.1.0-x86_64-unknown-linux-gnu.tar.gz\n9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08  danso-0.1.0-aarch64-unknown-linux-gnu.tar.gz\n";

    const SIGNATURE: &str = concat!(
        "untrusted comment: signature from minisign secret key\n",
        "RUQbf5jrBubDWB0Acfb+iqBIv/hp+xyPWz5PuLZotoK2SglCziYMXvQKdt/z2Td6QW9xZPGB42PGXQf7LANk23kbZDv2XAAagQs=\n",
        "trusted comment: danso fixture 0.1.0\n",
        "OCNMVspMSVdR0ccGLSj6ELZQ30qajDek1Bomx2c38VDAFTu8qnZBuQvpGP6qW4WMO+6R7kKb4cdbx/+GAXhjAA==\n",
    );

    // The same key over the same bytes, in the pre-0.10 legacy format
    // (`minisign -S -l`). It is a *valid* signature — `minisign -V` without
    // `-H` accepts it — which is what makes it the right probe: only the
    // format rule can reject it.
    const LEGACY_SIGNATURE: &str = concat!(
        "untrusted comment: signature from minisign secret key\n",
        "RWQbf5jrBubDWNMZy3xKW342KVoILSJscFv49g1P4dZj7KYl3S/ynCfsLFl1tPN5zishXSN/3Z8xXgNnlDD9r3PaTjoamI1tfgU=\n",
        "trusted comment: danso fixture 0.1.0 legacy\n",
        "AQuNpGrKw82G1scPjiVvbJ1LUCM7cA6O8gVWiFSbN2bXnRSF6lbj4nO6lLOBK+QHEH+aY2/a22BbBswEio4PDw==\n",
    );

    const X86: &str = "danso-0.1.0-x86_64-unknown-linux-gnu.tar.gz";

    fn verified() -> Manifest {
        verify_manifest(MANIFEST, SIGNATURE, FIXTURE_KEY).unwrap()
    }

    #[test]
    fn embedded_release_key_is_a_usable_minisign_key() {
        let key = embedded_public_key().unwrap();
        assert!(!key.starts_with("untrusted comment:"));
        // The checked-in file has to be loadable, or every install ships a
        // binary that cannot verify anything.
        PublicKey::from_base64(key).unwrap();
    }

    #[test]
    fn valid_signature_yields_every_signed_entry() {
        let manifest = verified();
        assert_eq!(manifest.len(), 2);
        assert_eq!(
            manifest.digest(X86),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(manifest.names().count(), 2);
    }

    #[test]
    fn tampered_manifest_is_rejected() {
        let mut tampered = MANIFEST.to_vec();
        tampered.extend_from_slice(
            b"0000000000000000000000000000000000000000000000000000000000000000  evil.tar.gz\n",
        );
        let err = verify_manifest(&tampered, SIGNATURE, FIXTURE_KEY).unwrap_err();
        assert!(err.to_string().contains("verification failed"), "{err}");
    }

    #[test]
    fn a_single_flipped_byte_is_rejected() {
        let mut tampered = MANIFEST.to_vec();
        tampered[0] = b'f';
        assert!(verify_manifest(&tampered, SIGNATURE, FIXTURE_KEY).is_err());
    }

    #[test]
    fn signature_from_another_key_is_rejected() {
        // The signature is well formed and internally consistent; only the
        // signer is wrong. This is the substitution attack, not corruption.
        let err = verify_manifest(MANIFEST, SIGNATURE, OTHER_KEY).unwrap_err();
        assert!(err.to_string().contains("verification failed"), "{err}");
    }

    #[test]
    fn legacy_format_signature_is_rejected() {
        // Guards the `allow_legacy = false` argument. Flipping it to `true`
        // must turn this test red; every other test in this module stays green
        // either way, because they only ever see current-format signatures.
        let err = verify_manifest(MANIFEST, LEGACY_SIGNATURE, FIXTURE_KEY).unwrap_err();
        assert!(err.to_string().contains("verification failed"), "{err}");
    }

    #[test]
    fn malformed_key_and_signature_are_rejected() {
        assert!(verify_manifest(MANIFEST, SIGNATURE, "not-a-key").is_err());
        assert!(verify_manifest(MANIFEST, "not-a-signature", FIXTURE_KEY).is_err());
        assert!(verify_manifest(MANIFEST, "", FIXTURE_KEY).is_err());
    }

    #[test]
    fn artifact_must_match_its_signed_digest() {
        let manifest = verified();
        // sha256("") is the digest signed for the x86_64 artifact.
        verify_artifact(&manifest, X86, b"").unwrap();
        let err = verify_artifact(&manifest, X86, b"danso").unwrap_err();
        assert!(err.to_string().contains("signed digest"), "{err}");
    }

    #[test]
    fn unlisted_artifact_is_rejected_rather_than_skipped() {
        let manifest = verified();
        let err = verify_artifact(&manifest, "danso-0.1.0-surprise.tar.gz", b"").unwrap_err();
        assert!(err.to_string().contains("not listed"), "{err}");
    }

    #[test]
    fn manifest_shapes_that_must_not_parse() {
        for (body, reason) in [
            ("", "empty manifest"),
            ("\n\n", "blank lines only"),
            ("deadbeef  danso.tar.gz\n", "short digest"),
            (
                "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855  danso.tar.gz\n",
                "uppercase digest",
            ),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 danso.tar.gz\n",
                "single space",
            ),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  ../danso.tar.gz\n",
                "path traversal",
            ),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  sub/danso.tar.gz\n",
                "path separator",
            ),
            // `../x` above is caught by the `/` rule, so the bare directory
            // reference is the only input that exercises its own guard.
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  ..\n",
                "bare parent directory",
            ),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  .\n",
                "bare current directory",
            ),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  sub\\danso.tar.gz\n",
                "backslash separator",
            ),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  dan\u{7f}so.tar.gz\n",
                "control character in name",
            ),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  danso-\u{fb01}le.tar.gz\n",
                "non-ascii name",
            ),
            // GNU tar reads a leading dash as an option; `--checkpoint-action`
            // is the argument-injection primitive this rules out.
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  --checkpoint-action=exec=sh\n",
                "name starting with a dash",
            ),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855   danso.tar.gz\n",
                "padded separator leaving a leading space",
            ),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  danso.tar.gz \n",
                "trailing space in name",
            ),
            // Two names that differ only by trailing space would otherwise be
            // distinct keys, which is the duplicate check defeated by padding.
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  a.tar.gz\n9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08  a.tar.gz \n",
                "near-duplicate name differing only by whitespace",
            ),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  \n",
                "empty name",
            ),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  a.tar.gz\n9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08  a.tar.gz\n",
                "duplicate name",
            ),
        ] {
            assert!(
                parse_manifest(body.as_bytes()).is_err(),
                "{reason} must be rejected"
            );
        }
    }

    #[test]
    fn manifest_must_be_utf8() {
        assert!(parse_manifest(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn public_key_line_ignores_comment_and_trailing_whitespace() {
        let file = "untrusted comment: minisign public key ABC\n";
        assert!(public_key_line(file).is_err());
        assert_eq!(
            public_key_line(&format!("untrusted comment: x\n{FIXTURE_KEY}\n\n")).unwrap(),
            FIXTURE_KEY
        );
        assert_eq!(
            public_key_line(&format!("untrusted comment: x\r\n{FIXTURE_KEY}\r\n")).unwrap(),
            FIXTURE_KEY
        );
    }

    #[test]
    fn a_second_key_line_is_an_error_not_a_choice() {
        // Appending a key must not silently change which key is trusted,
        // whichever end of the file the reader starts from.
        let two = format!("untrusted comment: x\n{FIXTURE_KEY}\n{OTHER_KEY}\n");
        let err = public_key_line(&two).unwrap_err();
        assert!(err.to_string().contains("more than one key line"), "{err}");
    }

    #[test]
    fn parse_errors_name_the_offending_line() {
        // Blank lines are skipped but still counted, so the number in the
        // message is the line an operator sees in the file.
        let body = "\n\nnot-a-digest  danso.tar.gz\n";
        let err = parse_manifest(body.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("line 3"), "{err}");
    }

    #[test]
    fn check_public_key_takes_minisign_lines_and_nothing_else() {
        check_public_key(FIXTURE_KEY).unwrap();
        check_public_key(embedded_public_key().unwrap()).unwrap();
        for (bad, reason) in [
            ("", "empty"),
            ("short", "too short"),
            // A bare 32-byte ed25519 key in base64. This is what the config
            // shape check accepted before signing was wired up; it has no key
            // id, so it must not be accepted now.
            (
                "uhnlFLDCRGn9SMAfkZQRDrHU0C7iYZm8P42pccxVwyo=",
                "bare ed25519 key without a key id",
            ),
            // Right length, valid base64, wrong algorithm prefix.
            (
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "56 chars of base64 that is not a minisign key",
            ),
            (
                "RWSmkwMcAqlV5Gx4Zg0x2ldpwP6AgKNhnQSUVSfk6mnjKxZg5Px3yrs",
                "truncated key",
            ),
        ] {
            assert!(check_public_key(bad).is_err(), "{reason} must be rejected");
        }
    }

    #[test]
    fn hex_digest_is_lowercase_sha256() {
        assert_eq!(
            hex_digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}

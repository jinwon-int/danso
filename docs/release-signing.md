# Release signing

`danso update` is to install only what the release key signed
(`docs/unified-design.md` §6.3). This page records where that key lives and how
it is handled. It contains **no key material and no secret values** — only
locations and rules.

**Status.** The key exists and `crates/danso-ops/src/release.rs` verifies a
signed manifest, but nothing calls it yet: `danso update` still implements only
`status`, there is no `release.yml`, and no release has been published. Until
`apply` lands, this page describes custody, not an enforced install path.

## The key

| | |
|---|---|
| Format | minisign (ed25519) |
| Key id | `3F1414BCF1F7514C` |
| Public half | `keys/danso-release.pub`, checked in, embedded at compile time |
| Private half | `MINISIGN_SECRET_KEY`, an Actions secret on `jinwon-int/danso` |
| Passphrase | none (`minisign -G -W`) |

The private half exists only inside the GitHub secret store. It was generated
on a node, registered, and the local copy destroyed; there is no escrow copy.
GitHub secrets cannot be read back, so **the private key cannot be recovered** —
a lost or compromised key is replaced by rotation, not by restore.

The key carries no passphrase because CI signs non-interactively. Storing a
passphrase next to the key it protects, in the same secret store, would add
ceremony rather than protection. The real boundary is who can run a workflow
that reads the secret.

### What protects the key

Anyone who can change a workflow file can sign with it. That is why
`jinwon-int/danso` `main` requires a review, dismisses stale reviews on push,
requires approval of the last push, and applies all of it to admins (danso #125).
Those rules are part of the signing boundary, not general repository hygiene —
weakening them weakens the signature.

The self-test workflow therefore does **not** run on `pull_request`. A job that
holds the secret must not be editable by the same unreviewed branch that runs it.

## Verifying the stored secret

`.github/workflows/signing-selftest.yml` signs a fixture with the stored secret
and verifies it against `keys/danso-release.pub`, on every push to `main` and on
demand. It also checks that a tampered manifest is refused, so a green run means
signing works, not merely that `minisign` is installed.

Two details of that job are load-bearing:

* `runs-on: ubuntu-24.04`. `minisign` is not packaged before noble; on
  `ubuntu-22.04` the install step fails with "Unable to locate package".
* `minisign -V -H`. Without `-H` the CLI accepts legacy (non-prehashed)
  signatures, which `danso-ops` rejects — the job would then be green for a
  signature the shipped binary refuses. `scripts/test_release_signing.py`
  fails the build if either is changed.

This job exists because a GitHub secret is write-only. Without it, a truncated
or mis-pasted secret would first surface as a failed release, after the local
copy was already gone.

```
gh workflow run "Release signing self-test" --repo jinwon-int/danso
```

## Rotation

1. Generate a new key pair on a trusted node, in memory-backed storage.
2. Replace the `MINISIGN_SECRET_KEY` secret and destroy the local private half.
3. Update `keys/danso-release.pub` through the normal pull request flow.
4. Let the self-test run on `main` and confirm it is green.
5. Only then publish a release signed by the new key.

Order matters: a node that has not yet installed a binary carrying the new
public key can still be pointed at it through `[update] public_key` in
`config.toml`, which is why that override exists.

## Rules

* Key material is never printed, logged, committed or pasted into an issue,
  a pull request or a Wiki page. Locations and handling only.
* `keys/danso-release.pub` is public by design; treat a change to it as a
  security-relevant change and review it as one.
* No signature bypass. `crates/danso-ops/src/release.rs` has no "unsigned but
  continue" path, and none should be added; a caller that must run unsigned
  simply does not call it.

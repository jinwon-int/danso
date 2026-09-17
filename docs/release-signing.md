# Release signing

`danso update` is to install only what the release key signed
(`docs/unified-design.md` §6.3). This page records where that key lives and how
it is handled. It contains **no key material and no secret values** — only
locations and rules.

**Status.** `danso update apply --artifact-dir <dir> --artifact <name>`
verifies and installs a signed release that is already on disk, exiting 13 with
no bypass if it does not verify; `danso update activate` decides whether the
replacement is actually serving, and `danso update rollback` puts the previous
binary back. What is still missing is the other end: there is no `release.yml`,
so nothing **produces** a signed release yet, and `apply` has no fetch step —
it is pointed at a directory, not a URL.

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

## Archive layout

`release.yml` must produce, for each target, a gzip tar containing the binary
**exactly once, stored as `danso`** — not `./danso`, not `bin/danso`, and not
twice:

```
tar -czf danso-<ver>-<target>.tar.gz -C <staging-dir> danso
```

`danso update apply` lists the archive before unpacking it and refuses anything
else. This is not pedantry: `tar -xzO -- danso` does not match a stored
`./danso` (the ordinary `tar -czf x.tar.gz ./danso` idiom), and GNU tar
*concatenates* duplicate members onto stdout, which would install two binaries
glued together. Both are silent at build time, so the check is on the install
side where it can still refuse.

## The update state machine

```
apply  ->  pending        activate  ->  activated     (done)
                                    ->  failed        (rollback, or fix and
                                    ->  unverified        `activate --retry`)
```

`activate` compares the **serving** image's digest against the recorded target.
It refuses to decide on evidence that does not postdate the activation — the
health document the not-yet-restarted process wrote is fresh and says the old
digest, and reading that as failure condemns a generation that was merely not
restarted yet. So `apply; activate; restart; activate` is the ordinary
sequence and the first `activate` reports `unverified` (exit 3).

A failed activation keeps reporting exit 1, and `apply` refuses past it:
installing again would snapshot the binary that just failed on top of the
known-good `bin/danso.prev`, at the exact moment that file is the only way
back. The two ways forward are `update rollback`, which supersedes the record
and swaps the binaries back, and `update activate --retry`, for when the
failure was environmental.

## The idle gate

Before any of that, `apply` asks whether the service is in the middle of a
turn, and defers if it is — replacing the binary means restarting, and a
restart during a turn kills the work in flight. The gate reads
`workload.active_requests` and `workload.oldest_request_age_seconds` out of
`health.json`, with the document's top-level `updated_at` for freshness.

```
danso update apply --artifact-dir <dir> --artifact <name> --data-dir <state-root>
# exit 8: deferred, nothing was read, locked or written
```

Four rules, each of which exists because its absence breaks something:

- **Exit 8, not an error.** Nothing happened, so a cron wrapper should treat
  it as an ordinary tick (ccc registers its task `--success-exit-codes 0,8,11`).
- **Bounded.** The wait accumulates in `state/self-update.deferred-since`
  across runs and tops out at one hour, after which the update proceeds even
  though the service is busy. Continuous load must not stop updates forever.
- **A turn older than 30 minutes stops counting.** It is a wedged turn, not a
  healthy service doing work, and treating it as busy lets one stuck turn hold
  the deferral budget open.
- **Fail-open.** No health document, unreadable, invalid, missing fields,
  stale — all mean *proceed*. The gate is an optimisation for the common case,
  not a safety property, and an update lane that one corrupt file can stop
  silently is worse than a turn that occasionally dies.

`--force` skips the gate. It kills the turn in flight, which is why it is a
flag rather than the default.

A node with no service publishes no health document and is never gated.

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

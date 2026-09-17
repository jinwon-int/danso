# Release signing

`danso update` is to install only what the release key signed
(`docs/unified-design.md` §6.3). This page records where that key lives and how
it is handled. It contains **no key material and no secret values** — only
locations and rules.

**Status.** Both ends exist. `.github/workflows/release.yml` builds, signs and
verifies a release; `danso update apply --artifact-dir <dir> --artifact <name>`
verifies and installs one, exiting 13 with no bypass if it does not verify;
`danso update activate` decides whether the replacement is actually serving,
and `danso update rollback` puts the previous binary back. What is still
missing is the middle: `apply` has no fetch step, so the artifacts have to
reach the node some other way — it is pointed at a directory, not a URL.

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

## Cutting a release

`release.yml` is **manual only** — `gh workflow run Release --repo
jinwon-int/danso --ref main`. There is deliberately no tag trigger: a tag can
be put on any commit and the workflow that runs is the one *at that commit*, so
a tag trigger would let an unreviewed branch run a job holding the signing key.
The rules on `main` are what protect the key, and a trigger that routes around
them removes the protection. The job checks its own ref for the same reason,
because `workflow_dispatch` can also name one.

The version comes from `Cargo.toml`, never from an input: a release labelled
with a version its binary does not carry is a lie that then lives in the signed
manifest.

Two runners, for two reasons that pull in opposite directions:

| Job | Runner | Why |
|---|---|---|
| `build` | `ubuntu-22.04` | its glibc is the floor every node has to clear; noble's is higher than the fleet's |
| `sign` | `ubuntu-24.04` | `minisign` is not packaged before noble |

The build records the glibc it actually linked against into `BUILD-INFO`, so
the baseline is checkable rather than asserted.

The signing job verifies what it just produced, the way a node will: `minisign
-V -H` against the committed public key, `sha256sum -c` against the artifacts,
and a tampered copy that must be refused. A release our own installer would
reject must not leave the runner.

### The environment gate — not yet configured

The job declares `environment: release`. That is where the signing secret
*should* live, as an environment secret with a deployment-branch rule, so that
a job running from any other ref cannot start at all.

**It is not configured yet, and GitHub creates a referenced environment
implicitly and without protection rules** — so the declaration alone would look
exactly like a protected one while protecting nothing. The job therefore
refuses to run unless the environment supplies `RELEASE_GATE=configured`, a
variable only a deliberately configured environment has. Until an operator sets
it up, `release.yml` fails closed on its first step.

Configuring it is a repository-settings and secret change:

1. Create the `release` environment; set its deployment branch rule to `main`.
2. Add required reviewers.
3. Add `RELEASE_GATE=configured` as an environment **variable**.
4. Move `MINISIGN_SECRET_KEY` to an environment **secret** and remove the
   repository-level one — while a repository secret exists, any job can read it
   without declaring the environment at all.

`signing-selftest.yml` reads the repository secret today, so step 4 has to move
it too or that workflow stops proving anything.

## Archive layout

`release.yml` produces, for each target, a gzip tar containing the binary
**exactly once, stored as `danso`** — not `./danso`, not `bin/danso`, and not
twice:

```
tar -czf danso-<ver>-<target>.tar.gz -C <staging-dir> danso
```

`release.yml` checks this itself before signing — a layout that only fails at
install time fails on a node, days later, with the signature already published.
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

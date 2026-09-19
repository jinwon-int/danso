# Release signing

`danso update` is to install only what the release key signed
(`docs/unified-design.md` §6.3). This page records where that key lives and how
it is handled. It contains **no key material and no secret values** — only
locations and rules.

**Status.** Both ends exist and a node can now ask what is available.
`.github/workflows/release.yml` builds, signs and verifies a release;
`danso update check` asks a configured source what it offers for this target;
`danso update apply --artifact-dir <dir> --artifact <name>` verifies and
installs one, exiting 13 with no bypass if it does not verify; `danso update
activate` decides whether the replacement is actually serving, and `danso
update rollback` puts the previous binary back.

What is still missing is the hand-off between the two: `release.yml` uploads
workflow artifacts, which expire and need authentication, and `apply` takes a
directory rather than a URL. Somebody still moves the files.

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

## Asking what is available

```
$ danso update check
a different release is available: danso-0.2.0-x86_64-unknown-linux-gnu.tar.gz
```

`[update] source` in `config.toml` is the base URL. `check` fetches
`<source>/SHA256SUMS` and its signature, **verifies them**, and then reads the
artifact named for this build's target triple. It downloads no archive, writes
nothing and takes no lock — a `check` that installed something as a side effect
would be the worst surprise a cron job could hold.

| exit | meaning |
|---|---|
| 0 | the offered archive is the one this generation was installed from; the release carries nothing for this target; or nothing recorded which archive this node came from |
| 10 | a **different** archive is available, or nothing is installed at all |
| 2 | no source configured, unreachable, the manifest did not verify, the record is corrupt, or the release names more than one artifact for this target |

**"Cannot tell" exits 0, not 10.** `update rollback` writes a record without
the archive fields, so a wrapper that re-applies on 10 would immediately
reinstall the release the operator had just rolled away from. The next `apply`
records the archive and the comparison starts working again.

**Two signed artifacts for one target is exit 2, not a choice.** The manifest
is a sorted map, so "take the first" means lexicographic order — a release
carrying both `…-0.1.0-…` and `…-0.2.0-…` would read as "up to date" on the
former while the latter sat in the same signed manifest.

**It says nothing about newer or older.** An ordering would have to be parsed
out of a file name, and a source that has been rolled back would then read as
"up to date" while serving something else. Different is different; deciding
whether to take it is why `apply` is a separate command.

The comparison is archive digest against archive digest. `installed-generation.json`
records `artifact_sha256` for this — the binary's digest is not the archive's,
and comparing those two would never match. A record written before that field
existed reports exit 10 with `"result": "unknown"`, and the next `apply` fills
it in.

### What the transport is and is not

The signature is the boundary. Whoever controls the connection can serve
anything; `verify_manifest` refuses it, because the manifest is checked against
the release key before one name inside it is read. `https` is required (and
plain `http` allowed only to loopback, which is not a network hop and is how
the tests exercise this at all) for confidentiality and to stop a casual
tamper-and-DoS — **not** because it is what makes an update safe.

What the fetcher is responsible for is the part a signature cannot cover: a
bounded download (the manifest declares digests, never sizes), a bounded
redirect chain with every hop re-checked against the same scheme rule, and a
wall-clock deadline.

That deadline is this updater's own. `reqwest::blocking`'s `timeout` re-arms on
every read, so it bounds a *stall* and not a transfer — measured, a source
sending one byte every five seconds against a 30-second budget was still being
read at 150 seconds.

Failures name a reason and never the URL: a release source is operator
configuration and a cron log is not where it belongs. A source may not carry a
query string or fragment, because the sub-paths are joined onto the end —
`…/rel?token=X` would become `…/rel?token=X/SHA256SUMS`, and the failure would
arrive as a signature error pointing at the wrong thing.

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
  silently is worse than a turn that occasionally dies. This extends to the
  marker itself: a deferral that cannot be written down cannot be bounded, so
  it is not made. Without that rule an unwritable state directory — a
  root-created marker a lower-privileged run can neither read nor replace —
  would defer from zero on every run and block updates forever.

`--force` skips the gate. It kills the turn in flight, which is why it is a
flag rather than the default.

A node with no service publishes no health document and is never gated. If the
update runs in an environment that does not set `DANSO_TELEGRAM_DATA_DIR` while
the service does — a different unit, a different user, a cron job without
`HOME` — the gate resolves a path that does not exist and proceeds every time.
Pass `--data-dir` explicitly wherever the two environments can differ.

### What it does not protect

`apply` replaces a file. It does not restart anything, and the SIGTERM that
ends a turn comes from the restart that follows. An operator who runs `apply`,
sees exit 8, and restarts by hand anyway is not protected by any of this. The
gate's job is to make the *wrapper* stop early, which is why a deferral is an
exit code rather than a warning.

### Reading the log

Every attempt leaves a line in `state/self-update.log`, including one that
never started — otherwise a node several generations behind is
indistinguishable from one whose updater never ran:

| `event` | Meaning |
|---|---|
| `deferred` | busy; nothing was read, locked or written |
| `gate_budget_exhausted` | busy, the hour ran out, **the turn was ended** |
| `gate_untrackable` | busy, the deferral could not be recorded, **the turn was ended** |
| `gate_forced` | `--force`; `active_requests` present if a turn was ended |

The two that end a turn also print a warning to stderr. The counts
(`active_requests`, `oldest_request_age_seconds`, `waited_seconds`) are the
only thing carried over from the health document.

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

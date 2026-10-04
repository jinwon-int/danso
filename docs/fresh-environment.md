# Fresh-environment reproduction (stage C, #204)

Roadmap #33 closes stage C when install → service → restart → recover can be
reproduced **in a new environment from the documentation alone**
([unified-design.md](unified-design.md) §9, stage C row). This page records
how that is checked and what it measured. Part 1 of #204 covers steps 1–4;
steps 5–10 follow in part 2 and are listed below as pending.

## Method

- **Inside the container: only danso.** A digest-pinned `debian:stable-slim`
  (`sha256:5bc3287b25407c965a30f38e32603dc253a3869e1b12a21ac09bfc27fd8b13ce`,
  pushed 2026-09-19) is started with host networking and `--init`, and the
  release binary the CI job just built (`target/release/danso`) is the one
  file copied in. The image has no Python, Node or ccc-node, and step 1
  asserts that before anything else runs.
- **Outside the container: the harness and fakes.** `scripts/fresh_env.py`
  runs on the CI host. Its loopback fake provider reuses the Anthropic
  fixture reply and SSE framing of `scripts/test_e2e.py` and answers 401
  without the fixture key, so a passing turn proves the credential variable
  reached it. Its fake Bot API is the `tests/service_supervise.rs` stub
  (`{"ok":true,"result":[]}`) ported to Python; after the first `getUpdates`
  it holds each poll for the requested timeout (capped at 1 s) like an idle
  Bot API. Both listen on 127.0.0.1, which the container shares.
- **Commands come from the docs.** Each command run in the container is one
  that README.md or docs/* gives. Where a step needed something the docs did
  not say, it is listed under *Doc gaps*; documentation-only gaps are fixed
  in the same PR.
- **Measurements.** Exit code, wall time and time-to-ready are measured inside
  the container with `date +%s%N` around the command (no `docker exec`
  overhead), except step 1, which is timed on the host. Slim images have no
  `ps` or `/usr/bin/time`, so memory is read from `/proc/<pid>/status`: for
  one-shot commands the danso process's `VmHWM` (peak RSS) sampled every
  20 ms until it exits; for the resident service its `VmRSS` once `service
  status` first reports available, plus the supervisor's. A one-shot command
  that exits before the first sample has no RSS figure, not a zero.
- **Fail closed.** The first failed check stops the run and fails the job;
  later steps show as not run. The job prints this table and appends it to the
  run summary; the rows are also uploaded as the `fresh-env` artifact.

Run it locally (needs docker and a release build):

```sh
cargo build --release --locked
python3 scripts/fresh_env.py --bin target/release/danso
python3 scripts/fresh_env.py --self-test   # fakes and table only, no docker
```

CI: `.github/workflows/fresh-env.yml`, on every pull request, merge-queue
entry and push to `main`.

## Steps

| # | What | Commands (in the container) | Docs followed |
| --- | --- | --- | --- |
| 1 | preflight | `command -v python3 python node nodejs ccc-node` (each must fail); `ldd`; `uname -r`; `/bin/bash` present | README "Build and run" (runtime requirements) |
| 2 | install | `install -m 0755 … /usr/local/bin/danso`; owner-only token file and `$DANSO_HOME/config.toml` as in the example; `danso config check` | README, service-install.md "Installing", telegram.md |
| 3 | provider auth | `mkdir -p $HOME/.danso/sessions`; `danso --cwd … --trust-project --session … --model … -p …` with `ANTHROPIC_API_KEY` and `DANSO_ANTHROPIC_BASE_URL`; one `PIRI_USAGE=` line | README, providers.md |
| 4 | resident service | `danso service install` (no systemd: guidance only); `danso service run --data-dir <state-root> --supervise`; poll `danso service status --data-dir <state-root>` until available; `danso doctor`; `danso service stop --data-dir <state-root>` | service-install.md "Running without systemd", doctor.md, telegram.md |

Signature verification does not apply: a pull-request build is not a release
and has no signed manifest. `minisign -V -H` against `keys/danso-release.pub`
([release-signing.md](release-signing.md)) is exercised by the release and
signing self-test workflows, not here. Step 2 records this rather than
skipping it silently.

## Measured

Not yet measured: the PR that added the harness was written on a runner with
neither docker nor cargo, so the first figures come from the `Fresh
environment` CI run, which prints this table and attaches it to the run
summary. Replace the placeholders with that run's values (and its run link)
when it is green.

| Step | Check | Exit | Wall s | Ready s | RSS (danso) | Result | Notes |
| --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| 1 | preflight: no python/node/ccc-node, ldd | | | | | measured in CI | |
| 2 | install binary (signature: n/a, unreleased PR build) | | | | | measured in CI | |
| 2 | config.toml per docs + `danso config check` | | | | | measured in CI | |
| 3 | provider auth: print-mode turn on loopback fake | | | | | measured in CI | |
| 4 | `danso service install` on a host without systemd | | | | | measured in CI | |
| 4 | `service run --supervise` -> available | | | | | measured in CI | |
| 4 | `danso doctor` against the running service | | | | | measured in CI | |
| 4 | `danso service stop` -> supervisor exits | | | | | measured in CI | |
| 5 | Telegram module: single turn + token lock | | | | | pending part 2 (#204) | |
| 6 | memory: write, search, inject, survives restart | | | | | pending part 2 (#204) | |
| 7 | cron: one scheduled run + failure spool | | | | | pending part 2 (#204) | |
| 8 | backup, restore to a new target, start from it | | | | | pending part 2 (#204) | |
| 9 | kill -9, restart, orphan cleanup, no duplicate effects | | | | | pending part 2 (#204) | |
| 10 | update, then roll back | | | | | pending part 2 (#204) | |

For comparison, the systemd path measured on yukson (service-install.md):
ready in 1.07 s, resident set 8.8 MB.

## Doc gaps

| # | Step | Gap | Status |
| --- | --- | --- | --- |
| 1 | 2 | README showed only how to build and run `target/release/danso` from the build tree; nothing said how to put the binary on `PATH`, which every operator command (`service`, `doctor`, the Termux:Boot script) assumes. | Fixed: README "Build and run" gives the install command, the config location and where signature verification applies. |
| 2 | 2 | README did not say where configuration lives; only telegram.md and service-install.md named `$DANSO_HOME/config.toml`, and `danso config check` on a fresh home fails with `config file is missing`. | Fixed in the same README paragraph. |
| 3 | 4 | service-install.md named `service run --supervise` as the non-systemd path but gave no invocation, readiness check or stop command for it. | Fixed: new "Running without systemd (`--supervise`)" section. |
| 4 | 2 | No doc says whether `core.workspace` must exist before the service starts or danso creates it. The harness creates it (`mkdir -p`) before `config check`. | Open: needs the behaviour confirmed before documenting it. |

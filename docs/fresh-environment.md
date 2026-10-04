# Fresh-environment reproduction (stage C, #204)

Roadmap #33 closes stage C when install → service → restart → recover can be
reproduced **in a new environment from the documentation alone**
([unified-design.md](unified-design.md) §9, stage C row). This page records
how that is checked and what it measured. Part 1 of #204 (#205) covers steps
1–4; part 2 appends steps 5–10 (Telegram, memory, cron, backup/restore, crash
recovery, update) to the same ordered list.

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
- **Part 2 fakes.** The fake Bot API also offers the text updates the harness
  pushes (a private chat with allow-listed user `1`) until a later
  `getUpdates` offset confirms them, as the Bot API does, and answers
  `sendMessage`/`editMessageText` with a Message, recording every send. Every
  Telegram claim in steps 5–9 (one answer, the progress edit, the restart
  notice, no duplicate send) is checked against that record. The fake
  provider can hold one turn's request (matched on its newest prompt), so
  step 9 can kill the service with that turn in flight.
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
  that exits before the first sample has no RSS figure, not a zero. A
  Telegram turn has no process of its own: it is timed on the host, from
  the update being offered to the answer reaching the fake Bot API (no exit
  code), and its RSS is the service's `VmRSS` right after the turn.
- **Fail closed.** The first failed check stops the run and fails the job;
  later steps show as not run. A check tied to a known code defect (*Known
  gaps* below) is shown as `known gap K<n>`. It passes only while the defect
  reproduces exactly as documented. If the defect stops reproducing, the run
  fails so this page gets updated, and any other failure is still a failure. The job prints this table and appends it to the
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
| 5 | Telegram module | `DANSO_TELEGRAM_WORKSPACE=<absent dir> danso service run --data-dir <state-root>` (must refuse, not create it); `service run --supervise`; one text update → progress message, `✅ Turn complete.` edit, one answer; `danso telegram` while the service runs, against a second fake Bot API (must exit without polling) | telegram.md (configuration, token lock, "Turns and persistence"), service-install.md |
| 6 | memory | `danso memory init`; `danso memory add --kind constraint --text …`; `danso memory search <word> --json`; headless `danso … --memory read -p …` (managed block and fact in the provider request); a Telegram turn's provider request inspected; `service stop` + `--supervise`, then `memory search` and `memory check` again | memory.md (CLI, snapshot assembly, diagnostics), telegram.md (`memory.scope`) |
| 7 | cron | `danso cron add fresh-ok --schedule 'every 1m' --prompt … --argv /usr/bin/true`; `danso cron tick --json`; `danso cron add fresh-fail … --argv /usr/bin/false --notify telegram-owner-on-failure`; `danso cron run fresh-fail --json`; the spool entry read back | agent-cron.md (commands, schedule forms, payload kinds, notify modes) |
| 8 | backup, restore, start from it | `danso service stop`; `danso backup` (manifest components; token not in the snapshot); `danso restore --target /srv/danso-restored <backup>`; `DANSO_HOME=<target> DANSO_TELEGRAM_DATA_DIR=<target> danso config check`; `danso service run --data-dir <target> --supervise`; a turn on the restored chat; `/new` and a turn | backup.md (incl. "Starting from a restored target"), telegram.md |
| 9 | crash | a turn held in flight at the fake provider; `kill -9 <service pid>`; poll `danso service status` until a replacement pid is available; restart notice on the interrupted progress message, no replayed provider request, no answer for it, `danso memory check` unchanged; one more turn → exactly one answer; `danso service stop` | service-install.md "Running without systemd" (supervisor restarts a `SIGKILL`ed service), telegram.md "Turns and persistence" |
| 10 | update | `danso update check` with no `[update] source` (exit 2, nothing fetched); `update apply` → `update rollback` not run, see doc gap 10 | release-signing.md |

Signature verification does not apply: a pull-request build is not a release
and has no signed manifest. `minisign -V -H` against `keys/danso-release.pub`
([release-signing.md](release-signing.md)) is exercised by the release and
signing self-test workflows, not here. Step 2 records this rather than
skipping it silently.

## Measured

Steps 1–4 were first measured by the `Fresh environment` CI run
[37203423774](https://github.com/jinwon-int/danso/actions/runs/37203423774)
(GitHub-hosted `ubuntu-22.04`, release build of PR #205 head `afbb7ed`).
The harness was written on a runner with neither docker nor cargo, so these
are the first real figures; every run prints the same table to its log and
step summary, and later runs may differ slightly in timing. The step 5–10
rows below are placeholders that the first CI run of part 2 (#204) fills in.
Like the harness, they were written on a runner with neither docker nor
cargo.

| Step | Check | Exit | Wall s | Ready s | RSS (danso) | Result | Notes |
| --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| 1 | preflight: no python/node/ccc-node, ldd | 0 | 0.118 |  |  | pass | absent: python3, python, node, nodejs, ccc-node; 5 ldd entries |
| 2 | install binary (signature: n/a, unreleased PR build) | 0 | 0.024 |  |  | pass | no signed manifest exists for a PR build; minisign -V -H (docs/release-signing.md) applies to Release artifacts only |
| 2 | config.toml per docs + `danso config check` | 0 | 0.026 |  |  | pass | valid; set_keys=4 |
| 3 | provider auth: print-mode turn on loopback fake | 0 | 0.025 |  | 8.0 MiB | pass | usage line present: requests=1 totalTokens=18 |
| 4 | `danso service install` on a host without systemd | 0 | 0.025 |  |  | pass | prints the Termux:Boot equivalent, installs nothing |
| 4 | `service run --supervise` -> available | 0 | 0.108 | 0.108 | 10.8 MiB (supervisor 7.2 MiB) | pass | ready = `danso service status` exit 0 (Bot status: available) |
| 4 | `danso doctor` against the running service | 0 | 0.025 |  |  | pass | ok=8 warn=2 fail=0 (home.layout, memory.store) |
| 4 | `danso service stop` -> supervisor exits | 0 | 0.107 |  |  | pass | Bot stop: drained; supervisor and service gone |
| 5 | service with an absent `core.workspace` (doc gap 4) | | | | | measured in CI | |
| 5 | `service run --supervise` -> available | | | | | measured in CI | |
| 5 | one Telegram turn end to end (fake Bot API) | | | | | measured in CI | |
| 5 | token lock: second `danso telegram` refused | | | | | measured in CI | |
| 6 | `danso memory init` | | | | | measured in CI | |
| 6 | `danso memory add` (constraint fact) | | | | | measured in CI | |
| 6 | `danso memory search --json` finds it | | | | | measured in CI | |
| 6 | injection: `--memory read` print-mode turn | | | | | measured in CI | |
| 6 | injection: Telegram service turn (known gap K1) | | | | | measured in CI (expected: known gap K1) | |
| 6 | restart (stop, `--supervise`): fact still found | | | | | measured in CI | |
| 7 | `cron add` + `cron tick`: one scheduled run | | | | | measured in CI | |
| 7 | failing job -> alert spool entry | | | | | measured in CI | |
| 8 | stop the service, `danso backup` | | | | | measured in CI | |
| 8 | `danso restore --target <new dir>` | | | | | measured in CI | |
| 8 | service from the restored home | | | | | measured in CI | |
| 8 | restored chat: first turn (known gap K2) | | | | | measured in CI (expected: known gap K2) | |
| 8 | `/new`, then one turn on the restored home | | | | | measured in CI | |
| 9 | turn in flight, `kill -9` the service -> replacement available | | | | | measured in CI | |
| 9 | orphan cleanup: restart notice, no replay, no duplicate effects | | | | | measured in CI | |
| 9 | next turn after the crash: exactly one answer | | | | | measured in CI | |
| 9 | `danso service stop` -> supervisor exits | | | | | measured in CI | |
| 10 | `danso update check` without a release source | | | | | measured in CI | |
| 10 | `update apply` then `update rollback` | | | | | not run: doc gap 10 | offline by design, but needs a release-key-signed SHA256SUMS; a PR build has none and a stand-in key would be a fake release |

For comparison, the systemd path measured on yukson (service-install.md):
ready in 1.07 s, resident set 8.8 MB.

## Doc gaps

| # | Step | Gap | Status |
| --- | --- | --- | --- |
| 1 | 2 | README showed only how to build and run `target/release/danso` from the build tree; nothing said how to put the binary on `PATH`, which every operator command (`service`, `doctor`, the Termux:Boot script) assumes. | Fixed: README "Build and run" gives the install command, the config location and where signature verification applies. |
| 2 | 2 | README did not say where configuration lives; only telegram.md and service-install.md named `$DANSO_HOME/config.toml`, and `danso config check` on a fresh home fails with `config file is missing`. | Fixed in the same README paragraph. |
| 3 | 4 | service-install.md named `service run --supervise` as the non-systemd path but gave no invocation, readiness check or stop command for it. | Fixed: new "Running without systemd (`--supervise`)" section. |
| 4 | 2 | No doc says whether `core.workspace` must exist before the service starts or danso creates it. The harness creates it (`mkdir -p`) before `config check`. | Fixed: telegram.md says it must exist: danso does not create it and the service exits 1 with `Telegram workspace does not exist` before polling. The first row of step 5 asserts exactly that on every run, so the doc cannot drift from the binary. |
| 5 | 5 | telegram.md said "One consumer owns a bot token at a time", but the lock is `.telegram-token.lock` below the data directory. A consumer with another data directory (another `DANSO_HOME`, a restored copy) is not refused. | Fixed: telegram.md and backup.md say the lock is per data directory and the old consumer must be stopped first. Step 5 checks the same-directory refusal; step 8 stops the original before starting the restored copy. |
| 6 | 6 | memory.md documents `danso run --memory …`, but there is no `run` subcommand: `--memory` and its siblings are flags of the headless invocation (`danso --cwd … --session … -p …`). | Fixed: memory.md says so where `danso run` appears. |
| 7 | 6 | memory.md documents `danso memory check --json`, but `check` takes no `--json` flag (it always prints JSON), so the documented command is a usage error. | Fixed: memory.md gives `danso memory check`. |
| 8 | 7 | agent-cron.md says notifications go "under the spool directory" without saying where it is. | Fixed: agent-cron.md gives the order `DANSO_AGENT_CRON_PUSH_SPOOL` → `DANSO_PUSH_SPOOL` → `$DANSO_HOME/telegram/spool`, the file name and `notification.spoolPath`. |
| 9 | 8 | backup.md documented `restore --target` but not how to start a node from the target. The restored conversation records sit at `<target>/conversations/`, which is a data-directory layout, so the default `$DANSO_HOME/telegram` would not see them. | Fixed: backup.md "Starting from a restored target" (`DANSO_HOME` and `DANSO_TELEGRAM_DATA_DIR` both the target, `--data-dir <target>`). Its journal caveat is known gap K2. |
| 10 | 10 | `danso update apply --artifact-dir <dir> --artifact <name>` reads a local directory, so it is offline by design. But it installs only what the release key signed (release-signing.md), and a pull-request build has no signed `SHA256SUMS`. Signing one with a stand-in key through `[update] public_key` would be a fake release. The workflow-artifact → `--artifact-dir` hand-off is also not built yet ("What is still missing", release-signing.md). | Open: apply → rollback is not reproduced here. It needs a signed release artifact the job may fetch, or an offline signed fixture the docs bless. Step 10 runs only the offline `update check` and records this row as not run. |

## Known gaps (code defects)

Found by this reproduction and left failing-safe. The harness asserts each
one's exact signature and fails if it changes (see *Method*). Code is not
changed by #204; each is a follow-up.

| # | Step | Defect | What the harness asserts |
| --- | --- | --- | --- |
| K1 | 6 | Telegram service turns never inject memory. telegram.md says `memory.scope` "selects the memory route used by turns", but the service builds each turn's run configuration with memory mode `off` (the default), so no managed block reaches the provider. The CLI's `--memory read` does inject. | The Telegram turn is answered, and its provider request has neither the `ccc-node:codex-memory:begin` marker nor the stored fact, while the `--memory read` print-mode turn just before it has both. |
| K2 | 8 | `danso backup` captures the Telegram `conversations/` records but not the session journals (`<data-dir>/journals/`) they point at. A restored chat's first turn is therefore refused (`⚠️ Turn could not start`, journal missing) until the chat sends `/new`. | On the restored home the chat gets the progress message and the `Turn could not start` edit, with no answer and no provider request. After `/new` the next turn is answered once. |

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
| 2 | install | `install -m 0755 … /usr/local/bin/danso`; owner-only token file and `$DANSO_HOME/config.toml` as in the example (plus `[memory] mode = "read"`); `danso config check` | README, service-install.md "Installing", telegram.md |
| 3 | provider auth | `mkdir -p $HOME/.danso/sessions`; `danso --cwd … --trust-project --session … --model … -p …` with `ANTHROPIC_API_KEY` and `DANSO_ANTHROPIC_BASE_URL`; one `PIRI_USAGE=` line | README, providers.md |
| 4 | resident service | `danso service install` (no systemd: guidance only); `danso service run --data-dir <state-root> --supervise`; poll `danso service status --data-dir <state-root>` until available; `danso doctor`; `danso service stop --data-dir <state-root>` | service-install.md "Running without systemd", doctor.md, telegram.md |
| 5 | Telegram module | `DANSO_TELEGRAM_WORKSPACE=<absent dir> danso service run --data-dir <state-root>` (must refuse, not create it); `service run --supervise`; one text update → progress message, `✅ Turn complete.` edit, one answer; `danso telegram` while the service runs, against a second fake Bot API (must exit without polling) | telegram.md (configuration, token lock, "Turns and persistence"), service-install.md |
| 6 | memory | `danso memory init`; `danso memory add --kind constraint --text …`; `danso memory search <word> --json`; headless `danso … --memory read -p …` (managed block and fact in the provider request); a Telegram turn's provider request inspected (managed block and fact present); `service stop` + `--supervise`, then `memory search` and `memory check` again | memory.md (CLI, snapshot assembly, diagnostics), telegram.md (`memory.mode`, `memory.scope`) |
| 7 | cron | `danso cron add fresh-ok --schedule 'every 1m' --prompt … --argv /usr/bin/true`; `danso cron tick --json`; `danso cron add fresh-fail … --argv /usr/bin/false --notify telegram-owner-on-failure`; `danso cron run fresh-fail --json`; the spool entry read back | agent-cron.md (commands, schedule forms, payload kinds, notify modes) |
| 8 | backup, restore, start from it | `danso service stop`; `danso backup` (manifest components; token not in the snapshot); `danso restore --target /srv/danso-restored <backup>`; `DANSO_HOME=<target> DANSO_TELEGRAM_DATA_DIR=<target> danso config check`; `danso service run --data-dir <target> --supervise`; a turn on the restored chat (answered, carrying the pre-backup history); `/new` and a turn | backup.md (incl. "Starting from a restored target"), telegram.md |
| 9 | crash | a turn held in flight at the fake provider; `kill -9 <service pid>`; poll `danso service status` until a replacement pid is available; restart notice on the interrupted progress message, no replayed provider request, no answer for it, `danso memory check` unchanged; one more turn → exactly one answer; `danso service stop` | service-install.md "Running without systemd" (supervisor restarts a `SIGKILL`ed service), telegram.md "Turns and persistence" |
| 10 | update | `danso update check` with no `[update] source` (exit 2, nothing fetched); in a scratch `DANSO_HOME` seeded with the installed binary: `update apply` of the committed fixture (exit 13 under the release key), then with the fixture key in `[update] public_key`: `update apply`, `update activate`, `update rollback`, `update activate` | release-signing.md "Rehearsing apply and rollback offline" |

Signature verification does not apply: a pull-request build is not a release
and has no signed manifest. `minisign -V -H` against `keys/danso-release.pub`
([release-signing.md](release-signing.md)) is exercised by the release and
signing self-test workflows, not here. Step 2 records this rather than
skipping it silently.

## Measured

All ten steps measured by the `Fresh environment` CI run
[37300710609](https://github.com/jinwon-int/danso/actions/runs/37300710609)
(GitHub-hosted `ubuntu-22.04`, release build of PR #212 head `dc6b02d`):
33 rows, all pass, none a known gap or not run. That run is the first with
K1 (#209) and K2 (#210) fixed and step 10's offline apply → rollback rehearsal
(#211). The previous measurement, with K1/K2 still reproducing and step 10's
second row not run, was run
[37205989960](https://github.com/jinwon-int/danso/actions/runs/37205989960)
(PR #208); steps 1–4 were first measured in run
[37203423774](https://github.com/jinwon-int/danso/actions/runs/37203423774)
(PR #205). Every run prints the same table to its log and step summary, and
later runs may differ slightly in timing.

| Step | Check | Exit | Wall s | Ready s | RSS (danso) | Result | Notes |
| --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| 1 | preflight: no python/node/ccc-node, ldd | 0 | 0.108 |  |  | pass | absent: python3, python, node, nodejs, ccc-node; 5 ldd entries |
| 2 | install binary (signature: n/a, unreleased PR build) | 0 | 0.024 |  |  | pass | no signed manifest exists for a PR build; minisign -V -H (docs/release-signing.md) applies to Release artifacts only |
| 2 | config.toml per docs + `danso config check` | 0 | 0.025 |  |  | pass | valid; set_keys=5 |
| 3 | provider auth: print-mode turn on loopback fake | 0 | 0.025 |  | 8.2 MiB | pass | usage line present: requests=1 totalTokens=18 |
| 4 | `danso service install` on a host without systemd | 0 | 0.025 |  |  | pass | prints the Termux:Boot equivalent, installs nothing |
| 4 | `service run --supervise` -> available | 0 | 0.108 | 0.108 | 10.8 MiB (supervisor 7.1 MiB) | pass | ready = `danso service status` exit 0 (Bot status: available) |
| 4 | `danso doctor` against the running service | 0 | 0.025 |  |  | pass | ok=8 warn=2 fail=0 (home.layout, memory.store) |
| 4 | `danso service stop` -> supervisor exits | 0 | 0.106 |  |  | pass | Bot stop: drained; supervisor and service gone |
| 5 | service with an absent `core.workspace` (doc gap 4) | 1 | 0.025 |  | 7.6 MiB | pass | exit 1 before polling: `Telegram workspace does not exist`; not created |
| 5 | `service run --supervise` -> available | 0 | 0.108 | 0.108 | 10.4 MiB (supervisor 7.1 MiB) | pass | ready = `danso service status` exit 0 (Bot status: available) |
| 5 | one Telegram turn end to end (fake Bot API) |  | 0.050 |  | 11.7 MiB | pass | 1 progress message, 1 edit(s), 1 answer; 1 provider request |
| 5 | token lock: second `danso telegram` refused | 1 | 0.025 |  |  | pass | exit 1 (`telegram service failed`), 0 Bot API calls; lock is per data directory (doc gap 5) |
| 6 | `danso memory init` | 0 | 0.025 |  |  | pass | scope global |
| 6 | `danso memory add` (constraint fact) | 0 | 0.025 |  | 8.4 MiB | pass | added through the write gates |
| 6 | `danso memory search --json` finds it | 0 | 0.025 |  | 8.8 MiB | pass | 1 matching result(s) |
| 6 | injection: `--memory read` print-mode turn | 0 | 0.049 |  | 13.8 MiB | pass | managed block and the fact are in the provider request |
| 6 | injection: Telegram service turn (`memory.mode = "read"`) |  | 0.100 |  | 14.8 MiB | pass | turn answered; managed block and the fact are in its provider request |
| 6 | restart (stop, `--supervise`): fact still found | 0 | 0.108 | 0.108 | 10.8 MiB (supervisor 7.0 MiB) | pass | records=1 before and after; ready = status available |
| 7 | `cron add` + `cron tick`: one scheduled run | 0 | 0.072 |  | 9.4 MiB | pass | executed=1; status=success, payload exit 0 |
| 7 | failing job -> alert spool entry | 1 | 0.073 |  | 9.3 MiB | pass | exit 1, status=failed; /root/.danso/telegram/spool/2026-10-05T11-07-00Z-fresh-fail-fresh-fail-1791198420-380.json (event AgentCronRun, send=false) |
| 8 | stop the service, `danso backup` | 0 | 0.025 |  | 7.8 MiB | pass | components: config, memory, conversations, journals; token not in the snapshot |
| 8 | `danso restore --target <new dir>` | 0 | 0.025 |  | 8.1 MiB | pass | restored: config=1, memory=9, conversations=1, journals=1 |
| 8 | service from the restored home | 0 | 0.108 | 0.108 | 10.6 MiB (supervisor 7.0 MiB) | pass | config valid; memory fact found; ready = status available |
| 8 | restored chat: first turn continues the session |  | 0.050 |  | 14.6 MiB | pass | 1 answer, 1 provider request; the request carries the pre-backup history |
| 8 | `/new`, then one turn on the restored home |  | 0.050 |  | 14.9 MiB | pass | fresh session pointer; 1 answer, 1 provider request |
| 9 | turn in flight, `kill -9` the service -> replacement available | 0 | 0.110 | 0.110 | 10.8 MiB (supervisor 7.2 MiB) | pass | pid 465 -> 528 under the same supervisor |
| 9 | orphan cleanup: restart notice, no replay, no duplicate effects |  | 0.000 |  |  | pass | restart notice on the interrupted progress message; 1 provider request (no replay); 0 answers; memory records=1 unchanged |
| 9 | next turn after the crash: exactly one answer |  | 0.100 |  | 14.9 MiB | pass | 1 answer, 1 provider request, same chat and session |
| 9 | `danso service stop` -> supervisor exits | 0 | 0.106 |  |  | pass | Bot stop: drained; supervisor and service gone |
| 10 | `danso update check` without a release source | 2 | 0.025 |  |  | pass | exit 2, no source configured (docs/release-signing.md); nothing fetched |
| 10 | `update apply`: the release key refuses the fixture | 13 | 0.025 |  | 7.2 MiB | pass | exit 13, installed binary unchanged (danso 0.1.0) |
| 10 | `update apply` + `update activate` with the fixture key | 0 | 0.121 |  | 24.1 MiB | pass | exit 0; `bin/danso --version` = danso 9.9.9; activate: activated |
| 10 | `update rollback` -> the seeded binary is back | 0 | 0.097 |  | 23.9 MiB | pass | exit 0; byte-identical to the seeded binary (danso 0.1.0); activate exit 0 |

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
| 9 | 8 | backup.md documented `restore --target` but not how to start a node from the target. The restored conversation records sit at `<target>/conversations/`, which is a data-directory layout, so the default `$DANSO_HOME/telegram` would not see them. | Fixed: backup.md "Starting from a restored target" (`DANSO_HOME` and `DANSO_TELEGRAM_DATA_DIR` both the target, `--data-dir <target>`). Its journal caveat was known gap K2, fixed by #210. |
| 10 | 10 | `danso update apply --artifact-dir <dir> --artifact <name>` reads a local directory, so it is offline by design. But it installs only what the release key signed (release-signing.md), and a pull-request build has no signed `SHA256SUMS`. Signing one with a stand-in key through `[update] public_key` would be a fake release. The workflow-artifact → `--artifact-dir` hand-off is also not built yet ("What is still missing", release-signing.md). | Fixed (#211): release-signing.md "Rehearsing apply and rollback offline" blesses the committed `tests/fixtures/release` fixture (throwaway key, private half destroyed, a shell script for a binary) in a scratch home only. A real release artifact was not chosen: `release.yml` keeps its workflow artifacts 7 days, so a pull request opened after a quiet week would fail for want of one. Step 10 first asserts the release key refuses the fixture (exit 13), then measures apply → activate → rollback. The hand-off itself is still missing and is not claimed here. |

## Known gaps (code defects)

Found by this reproduction and left failing-safe. The harness asserts each
open one's exact signature and fails if it changes (see *Method*). Code is not
changed by #204; each is a follow-up.

None is open. K1 and K2 below were fixed; their rows in steps 6 and 8 are
now ordinary checks that assert the corrected behaviour.

| # | Step | Defect | Status |
| --- | --- | --- | --- |
| K1 | 6 | Telegram service turns never inject memory. telegram.md says `memory.scope` "selects the memory route used by turns", but the service builds each turn's run configuration with memory mode `off` (the default), so no managed block reaches the provider. The CLI's `--memory read` does inject. | Fixed (#209): the service reads `memory.mode` (`DANSO_TELEGRAM_MEMORY_MODE`; default `off`, so an unset node is unchanged). Step 2 sets `read`; step 6 asserts the Telegram turn's provider request carries the marker and the fact. |
| K2 | 8 | `danso backup` captures the Telegram `conversations/` records but not the session journals (`<data-dir>/journals/`) they point at. A restored chat's first turn is therefore refused (`⚠️ Turn could not start`, journal missing) until the chat sends `/new`. | Fixed (#210): the snapshot carries a `journals` component, restored to `<target>/journals/`. Step 8 asserts the restored chat's first turn is answered once, with no `Turn could not start`, and that its provider request carries the step 6 prompt from the restored journal. |

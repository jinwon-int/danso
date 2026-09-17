# Installing the resident service

Everything here was measured on a node, not inferred from the code
(yukson, 2026-09-17, #118 D-stage). Each item is something the acceptance run
tripped over in the order an operator would meet it.

## What the service needs to start

Two variables are required and there is no default for either:

| Variable | Notes |
|---|---|
| `DANSO_TELEGRAM_BOT_TOKEN` | the bot token; never put it in the unit file |
| `DANSO_TELEGRAM_MODEL` | or `DANSO_MODEL`, or the provider-specific name |

Three more have defaults that are usually wrong for a service:

| Variable | Default | Why you normally set it |
|---|---|---|
| `DANSO_TELEGRAM_WORKSPACE` | the process's working directory | see *the workspace trap* below |
| `DANSO_TELEGRAM_PROVIDER` | `anthropic` | set it when you are not on the default |
| `DANSO_TELEGRAM_ALLOWED_USER_IDS` | empty | an empty allowlist answers nobody |

`config.toml` declares most of these too, under `[telegram]` and `[provider]`.
**The service does not read it.** The Telegram loop is environment-only; the
config file is consumed by `doctor`, `backup` and `update`. Setting
`telegram.token_file` in `config.toml` will not start a service. This is a
known gap, tracked separately.

## The workspace trap

The service refuses to keep journals inside the workspace it runs commands in,
and the state root holds the journals. The rendered unit sets
`WorkingDirectory` to `$HOME`, and the default state root is
`$HOME/.danso/telegram` — inside it. So a unit that supplies the two required
variables and nothing else **still cannot start**:

```
service run failed: Telegram journals must be outside the workspace
```

Set `DANSO_TELEGRAM_WORKSPACE` to a directory that does not contain the state
root. `danso service install` refuses to install a unit with this problem and
says so.

## Do not create the state directory first

`danso` creates it with mode `0700`. If it already exists with any other mode,
the service refuses to start:

```
service run failed: Telegram data directory must have mode 0700
```

That is deliberate — tightening a directory somebody else created would be
worse than refusing — but `mkdir -p` is the natural first move and produces
`0755`. Let `danso` create it, or `chmod 700` it yourself.

## Installing

`danso service install` writes the unit, runs `daemon-reload` and `enable`, and
**does not start the service**. Combining install with start would make a
configuration change into a silent restart.

The unit carries no secrets. Supply the configuration through a systemd
drop-in, which is also how ccc-node pins its model and labels:

```
# /etc/systemd/system/danso.service.d/10-danso.conf
[Service]
Environment=DANSO_TELEGRAM_BOT_TOKEN=...
Environment=DANSO_TELEGRAM_MODEL=...
Environment=DANSO_TELEGRAM_WORKSPACE=/srv/danso/workspace
```

The drop-in file holds a token: `chmod 600` it. The unit file itself is
world-readable, which is why the token must not go in it.

`install` reads the unit it is about to write **and any drop-ins already
present**, and refuses when the result could not start — naming the variables
that are missing, never their values. `--dry-run` changes nothing, so it
reports the same problems as warnings on stderr and still prints the unit on
stdout.

Then:

```
systemctl start danso
danso service status --data-dir <state-root>
```

Measured on yukson: ready in **1.07 s**, `Bot status: available`, resident set
**8.8 MB**.

## `danso service stop` does not stop a systemd unit

```
$ danso service stop --data-dir <state-root>
Bot stop: drained            exit=0
$ systemctl is-active danso
active                       # NRestarts=1
```

The drain runs correctly and the CLI reports success — and then `Restart=always`
brings the service back three seconds later. On a systemd node use
`systemctl stop`. `danso service stop` is for the Termux/`--supervise` path and
for stopping a service nothing else is supervising.

## Uninstalling

`danso service uninstall` refuses while the service is still serving:

```
service uninstall failed: service is still available — stop it before removing its unit
```

Removing a unit from under a live process leaves something nothing supervises
and no unit explains. Stop first, then uninstall; it removes both the unit and
its `multi-user.target.wants` symlink.

## Restarting

`systemctl restart danso` is the right command **from a shell**. It is the
wrong one from inside the service: systemd stops the whole cgroup, and the
`systemctl` process issuing the restart is in it. So anything running as part
of the service — a `/restart` command, an update that wants to activate a new
binary — goes through the handoff instead:

```
$ danso service restart --data-dir <state-root>
Restart 953f9d3a scheduled in 5s
```

That returns immediately, because the process asking is usually the one about
to be replaced. `systemd-run` creates a one-shot transient unit — its own
cgroup — which restarts the service and writes the answer down:

```
$ danso service restart-status --data-dir <state-root>
restart 953f9d3a: completed, now pid 7777
```

A finished result blocks the next restart until somebody reads it:

```
$ danso service restart --data-dir <state-root>
service restart not scheduled: restart_result_pending (the service is still running)
```

`restart-status --acknowledge` files it away as `restart-handoff.last.json` and
unblocks the next one. Losing the answer to a restart is worse than refusing a
second one, which is why the block has no timeout. A request still *in flight*
blocks for five minutes, so a worker that died does not hold the door forever.

A restart is only `completed` when a **different** pid is serving and has
published `available` health that postdates the request. A unit that restarted
into the same image reports success to systemd and changes nothing; that is
ccc-node #1527, and it is why the unit's own exit status is not the evidence.

Failures are codes, never output: `restart_failed`, `health_timeout`,
`worker_error`. The receipt is designed to be delivered verbatim into a chat,
so nothing from `systemctl` goes into it.

On a host with no systemd the restart is simply not scheduled
(`systemd_run_unavailable`) and the service keeps running. There is no fallback
— Termux restarts through `service run --supervise`.

## Hosts without a user session

`systemctl --user` needs `$XDG_RUNTIME_DIR` and a session bus. On a headless
root host — which is most of the fleet — it simply fails, so `--user` is not an
option there and the system scope is the only one available.

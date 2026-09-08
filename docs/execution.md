# Execution modes

The CLI defaults to `--sandbox host`. It runs the four tools as the current
Linux user without an external sandbox executable. Linux 5.3+ pidfds, procfs,
child-subreaper support and Bash are required. Rust libraries are linked into
the binary; this mode adds no package dependency.

Host mode is not filesystem or network isolation. Bash can access everything
allowed to the user, including files outside the workspace and host services.
The read/write/edit tool checks are not a security boundary around Bash.
The worker environment is cleared and rebuilt from the explicit native HOME:
`HOME=$HOME` and `PATH=$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin`.
Provider credentials and arbitrary caller variables are not inherited, but
files and `/proc` may still expose current-user credentials. A separate working
folder is organizational separation, not a sandbox. Run under the intended
non-root user. Model-visible Bash metadata describes the selected mode and
limits. Host tools also receive the fixed `CARGO_BUILD_JOBS=2` development
setting so a local compile does not fan out across every host CPU.
When a host worker needs a toolchain from another absolute HOME, pass
`--tool-home /absolute/path`. This changes only the child tool's `HOME` and
Cargo-first `PATH`; provider authentication, context discovery and the caller's
native HOME remain unchanged. The option is rejected for bubblewrap and invalid
PATH components fail before provider construction.

`--sandbox bubblewrap` retains the original PID/mount/network isolation and
requires /usr/bin/bwrap plus working user namespaces. A selected backend must
pass preflight before model dispatch; it never falls back automatically.
`--unsafe-no-sandbox` remains a deprecated host alias and conflicts with an
explicit --sandbox flag. Existing CLI invocations now default to host mode;
callers needing the previous boundary must explicitly select bubblewrap.
The Python auxiliary ccc-node adapter and live-acceptance workflow explicitly
retain bubblewrap. External ccc-node Telegram launchers must choose/review their
policy before upgrading a native binary; this PR does not deploy a bridge.

## Process lifetime and limits

Each host tool starts a single-threaded supervisor in the Danso binary. It
becomes a Linux child subreaper before starting the worker, watches parent death,
and kills/reaps descendants after normal completion or interruption. Reparented
children are collected even after setsid/double-fork. A pidfd identifies the
supervisor during cancellation, avoiding signals to recycled process IDs.
A killed CLI triggers supervisor cleanup through PR_SET_PDEATHSIG. Cleanup
is asynchronous when the parent itself exits; tests wait for actual reaping.

This is lifecycle management for trusted host execution, not containment of
hostile same-user code: such code may kill the supervisor or use external
services to start work outside its descendant tree. SIGKILL cannot instantly
remove processes stuck in uninterruptible kernel sleep. Persistent background
services must not be started as tool-owned descendants.

Both modes retain the existing 64 KiB output bound and journal recovery rules.
Bubblewrap keeps its restrictive per-process limits: virtual memory 512 MiB,
file size 16 MiB, 128 descriptors and 30 CPU seconds; its tool wall timeout is
30 seconds by default and accepts 1..300 seconds. Host development execution
uses 32 GiB of virtual address space, 4 GiB per-file size, 4096 descriptors,
and a CPU limit matching the selected tool wall timeout. Host tools default to
900 seconds and accept 1..3600 seconds, while the enclosing short run still
defaults to 300 seconds unless its run timeout is raised. `RLIMIT_AS` is a
virtual-address-space cap, not an RSS or physical-memory reservation. These are
not aggregate descendant budgets. Explicit `--tool-timeout-seconds` values
remain effective within the selected backend's range, and a caller's tighter
inherited hard limit is never raised; the trusted execution context labels such
limits as configured maxima because the OS may tighten them.

## Verification

`python3 scripts/test_host_execution.py` runs real native host tools, all three
fake provider protocols, file changes/session resume, cleared environment,
explicit host permissions, and descendant cleanup after completion, timeout,
SIGTERM and CLI SIGKILL. No live credentials or model calls are used. Existing
bubblewrap suites remain explicit and run alongside it in host checks/CI.

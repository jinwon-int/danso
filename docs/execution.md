# Execution modes

The CLI defaults to `--sandbox host`. It runs the four tools as the current
Linux user without an external sandbox executable. Linux 5.3+ pidfds, procfs,
child-subreaper support and Bash are required. Rust libraries are linked into
the binary; this mode adds no package dependency.

Host mode is not filesystem or network isolation. Bash can access everything
allowed to the user, including files outside the workspace and host services.
The read/write/edit tool checks are not a security boundary around Bash.
The worker environment is cleared (PATH=/usr/bin:/bin, HOME=/tmp), but files
and /proc may still expose current-user credentials. A separate working folder
is organizational separation, not a sandbox. Run under the intended non-root
user. Model-visible Bash metadata describes the selected mode.

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

Both modes retain the existing 64 KiB output bound, tool/run timeouts and
per-process limits for virtual memory (512 MiB), file size (16 MiB), descriptors
(128) and CPU (30 seconds). These are not aggregate descendant budgets. Journal
started/result/settled ordering and uncertain-operation recovery are unchanged.

## Verification

`python3 scripts/test_host_execution.py` runs real native host tools, all three
fake provider protocols, file changes/session resume, cleared environment,
explicit host permissions, and descendant cleanup after completion, timeout,
SIGTERM and CLI SIGKILL. No live credentials or model calls are used. Existing
bubblewrap suites remain explicit and run alongside it in host checks/CI.

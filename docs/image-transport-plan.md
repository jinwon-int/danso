# Image transport foundation — test-only, not enabled

## Scope

This PR adds experiments, not photo/model support. Production OpenAI and
ChatGPT serialization remains text-only. Four OpenAI regressions pin rejection
of image history, including unsupported roles and attempted validation bypass.
No CLI, runtime, journal, bridge, deployment, or live-provider behavior changes.
All credentials and images in tests are synthetic/local.

`provider/openai_image.rs` and `provider/image_pixels.rs` are included only under
`cfg(test)`; the Linux integration test imports the pixel helper separately.
`image` 0.25.10 is a dev-only dependency with default features disabled and only
PNG/JPEG enabled. Do not promote these helpers into production admission.

## Experimental boundaries

- The envelope prototype permits one PNG/JPEG user image only with an explicitly
  selected capability. That selection is not a provider/endpoint/model registry.
  Canonical base64 and signatures are framing, not proof of decoded pixels or
  immutable normalized provenance. Some wire fixtures intentionally cannot decode.
- Synthetic final request serialization has a 512 KiB logical byte ceiling.
  Mutation requires resizing again; it does not bound ingress allocations or
  compaction. The user-content evidence projection emits a fixed omission marker
  for images. It is not general role-aware log/history scrubbing and does not
  remove arbitrary secrets embedded in ordinary text.
- Pixel normalization admits nonempty input up to 192 KiB, PNG/JPEG MIME/signature
  agreement, sides up to 2048 and at most 1,048,576 pixels. Decoder allocation
  limits are best-effort (32 MiB), not hard RSS or time limits. EXIF orientation
  is applied; pixels are re-encoded as metadata-free PNG. A sticky writer overflow
  flag enforces a logical output cap of min(caller budget, 192 KiB), not a Vec
  capacity or encoder allocation bound. This encoder is not wired to the envelope.

## Isolated decoder fixture

Linux integration requires `/usr/bin/bwrap`; there is no host fallback. The
launcher uses unshare-all, die-with-parent, new-session, cap-drop ALL and clearenv.
Runtime trees `/usr`, `/lib`, `/lib64` (where present), the test executable and
input are read-only. Only a precreated output file is writable; the containing
root is remounted read-only. Home/workspace are not mounted and a live host
loopback listener makes the network denial check non-vacuous. The runtime mounts
are broad, not a minimal audited runtime image.

Child setup uses null stdio, umask 077, no_new_privs, RLIMIT_AS 256 MiB,
RLIMIT_CPU 2 seconds, RLIMIT_FSIZE 192 KiB and RLIMIT_CORE 0. The last pre-exec
hook marks inherited FDs >=3 CLOEXEC with close_range; unsupported/denied kernels
fail closed. This retains Rust's exec-error reporting pipe until exec.

The parent retains a create-new 0600 O_NOFOLLOW/CLOEXEC output FD and verifies
regular-file type, effective UID, private mode, nlink 1, nonzero bounded length,
bounded read/length agreement and PNG framing. It never decodes output outside
the sandbox. These checks do not establish immutable normalized provenance.

ChildGuard cleanup has a one-second polling budget, distinct kill/reap/unresolved
errors, and best-effort Drop. Controlled detached descendants independently open
and flock the output, signal readiness and sleep without explicit unlock. Timeout
and independent supervisor SIGKILL tests observe a live holder/conflicting lock
before checking bounded release. This is not universal descendant reaping or
persistent crash ownership. CPU termination signal attribution is not established.

## Cancellation and assertion-unwind ownership

An async waiter owns only a cancel-on-drop flag and result receiver. The outer
scope owns the OS worker thread and private directory. Deterministic checkpoints
park the owner before spawn, after the detached holder is demonstrably ready,
and immediately before and after retained-FD output validation/read (using a
real successful decoder for the read checkpoints, after child reap).
Aborting and awaiting the waiter signals cancellation but does not transfer or
release the owner's resources. Resuming the owner verifies no launch before
spawn, stop/reap and lock release after readiness, or rejection of decoded bytes
at the read boundaries; cleanup errors propagate. These seams do not interrupt
a read syscall or prove atomic cancellation versus result delivery.

`CancellationOwner::Drop` signals cancellation, unblocks the checkpoint and joins
the OS thread **before** its directory is dropped. It suppresses secondary thread
panic propagation. A caught assertion-unwind regression at all four checkpoints
verifies cancellation, completion while the directory still exists, lock release,
and directory removal after Drop. This supersedes the earlier fixture's
assertion-unwind detach gap.

The join has deliberately no detach-on-timeout. A stuck fixture relies on the
external test-job timeout (CI: 15 minutes). This blocking destructor is unsuitable
for a production async executor; none of this is durable cancellation safety.
Waiter abort must never authorize retry or discard an unresolved owner.

## Required work before production activation

1. Expand cancellation coverage into in-flight output reads, kill/reap, launch races and
   injected cleanup errors. Audit broad mounts, other inherited FD types and
   unsupported/denied close_range. Cover replacement/growth/concurrent writers,
   unexpected files and pixel-level EXIF/progressive JPEG/metadata/truncated/
   high-entropy corpora; add explicit RSS/CPU attribution.
2. Bind immutable input digest, session entry, operation identity and cleanup
   state durably before launch. PID-only recovery is unsafe under reuse; pidfds
   do not survive owner death. Use separately supervised containment/identity,
   quarantine uncertain records and prohibit automatic decoder/provider replay.
3. Verify output through retained FDs, persist/sync an immutable blob before
   journal references, and test every crash/cancel edge, missing/replaced blobs,
   owned orphan quarantine and retention that excludes unresolved inputs.
4. Admit typed attachments for the exact provider/endpoint/model before durable
   user append or dispatch. Preserve runtime history validation before preflight,
   no-append resume, immediate long-task user-entry references, durable assistant
   append before pending responses, unresolved-call gates and settled-boundary
   pause semantics. No image base64 in prompts, control markers or text evidence.
5. Implement role-aware projections for logs/events/history/memory/import and
   Recorder persistence; compaction must retain the latest structured image or
   reject an impossible budget before summary dispatch. String-only app/CLI and
   bridge entry points are not attachment support.
6. Capture synthetic Responses/ChatGPT requests and complete real bridge-flow
   switch/overflow/cancel/retry/crash/cleanup/replay/compaction tests against the
   intended checkout before separately authorized activation. No live calls here.
7. Review dependency source/license/MSRV/transitive SIMD exposure. CI's existing
   supply-chain workflow audits the lockfile with denyWarnings; a green advisory
   scan is not a source audit or production decoder safety approval.

## Validation

Run as an unprivileged user on a configured Ubuntu host (not a restricted worker):

```sh
python3 scripts/dev_check.py --profile host
DANSO_BIN=target/release/danso python3 scripts/test_e2e.py
cargo test --locked --test image_decode_process
```

Use an external job deadline for cancellation-owner tests. Host validation and
GitHub checks are recorded in the PR; they do not validate Windows/WSL or prove
end-to-end photo support. Optional Pi interop and real-model acceptance are not
part of this change. Historical local operational notes are not published here.

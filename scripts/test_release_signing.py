"""Offline guard for the release signing key and its self-test workflow.

Two things are checked here that Rust tests cannot see:

* the checked-in public key is the specific minisign key this repository
  trusts, not merely a well-formed one;
* the workflow that proves the stored secret matches it cannot be made green
  by skipping, by neutering its failure case, or by printing the key.

The workflow assertions match **whole lines**, not substrings. A substring
guard passes for ``minisign -V ... || true`` and for ``if false; then echo
"self-test failed..."``, both of which leave the job green while proving
nothing; every mutation below was observed passing a substring-based version
of this file before it was rewritten.

Every ``validate_*`` helper is exercised against a mutated copy of the live
file as well as the live file itself. A guard nobody has watched fail is a
guard nobody knows works.
"""
import base64
import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
PUBLIC_KEY = ROOT / "keys/danso-release.pub"
WORKFLOW = ROOT / ".github/workflows/signing-selftest.yml"
RELEASE = ROOT / ".github/workflows/release.yml"
SECRET_NAME = "MINISIGN_SECRET_KEY"

# The trust root, pinned. Rotating the release key is supposed to be a visible
# edit here: without this, swapping the file for any other valid minisign key
# passes every structural check.
KEY_ID = "E59BCBCB0600807C"

# `minisign -V` accepts legacy (non-prehashed) signatures unless `-H` is given,
# while danso-ops rejects them. Without `-H` the job can be green for a
# signature the shipped verifier refuses.
VERIFY = 'minisign -V -H -p keys/danso-release.pub -m "${fixture}"'

# Exact lines, with their indentation, that carry the job's meaning.
REQUIRED_LINES = (
    "          set -euo pipefail",
    f"          {VERIFY}",
    f"          if {VERIFY}; then",
    '            echo "self-test failed: a tampered manifest verified"',
    "            exit 1",
    '          minisign -S -s "${key}" -m "${fixture}" -t "danso signing self-test ${GITHUB_SHA}"',
)

# The only lines allowed to *expand* the secret: the env binding, a presence
# check and the write to a file. Any other expansion is a potential echo into
# the log. A bare mention of the name in a message is harmless and ignored.
SECRET_LINES = (
    f"          MINISIGN_SECRET_KEY: ${{{{ secrets.{SECRET_NAME} }}}}",
    f'          if [ -z "${{{SECRET_NAME}}}" ]; then',
    f'          printf \'%s\\n\' "${{{SECRET_NAME}}}" > "${{key}}"',
)
SECRET_EXPANSION = re.compile(rf"\$\{{{SECRET_NAME}\}}|\$\{{\{{\s*secrets\.{SECRET_NAME}")

FORBIDDEN = (
    (r"\|\|\s*true", "|| true masks a failure"),
    (r"set\s+-[a-z]*x[a-z]*(\s|$)", "set -x traces commands"),
    (r"set\s+-o\s+xtrace", "set -o xtrace traces commands"),
    (r"^\s*shell:", "a shell: override can re-enable tracing"),
    (r"^\s*if:|continue-on-error", "a conditional step can be green while skipped"),
    (r"if\s+(false|true)\s*;", "a constant condition disables the branch"),
    (r"^\s*cat\s+\"\$\{key\}\"", "printing the key file"),
)


def validate_public_key(text):
    lines = [line for line in text.splitlines() if line.strip()]
    assert len(lines) == 2, "a minisign public key file is a comment and a key"
    assert lines[0].startswith("untrusted comment:")
    key = lines[1].strip()
    raw = base64.b64decode(key, validate=True)
    # 2-byte algorithm id, 8-byte key id, 32-byte ed25519 key.
    assert len(raw) == 42, "public key must decode to 42 bytes"
    assert raw[:2] == b"Ed", "signature algorithm must be ed25519"
    # minisign prints the key id in reverse byte order.
    key_id = raw[2:10][::-1].hex().upper()
    assert key_id == KEY_ID, f"public key is {key_id}, expected the pinned {KEY_ID}"
    # The comment is what a human reads; a mismatch means the file was swapped
    # without updating what it claims to be.
    assert KEY_ID in lines[0], "comment line must name the key id"


# The producing workflow's own load-bearing lines. Each one fails invisibly:
# without `-H` the release verifies here and is refused on every node; without
# the layout check a bad archive is signed and only fails at install time;
# without the ref and gate guards the signing key is reachable from an
# unreviewed branch.
RELEASE_VERIFY = 'minisign -V -H -p "${pubkey}" -m SHA256SUMS'
RELEASE_REQUIRED_LINES = (
    "          set -euo pipefail",
    f"          {RELEASE_VERIFY}",
    "          sha256sum -c SHA256SUMS",
    '          if [ "${GITHUB_REF}" != "refs/heads/main" ]; then',
    '          if [ "${GATE}" != "configured" ]; then',
    '            echo "release failed: a tampered manifest verified"',
    """            echo "archive must contain exactly one member named 'danso', found:\"""",
    '          sha256sum -- "${archives[@]}" > SHA256SUMS',
    '          minisign -S -s "${key}" -m SHA256SUMS -t "danso ${VERSION} ${GITHUB_SHA}"',
)


def validate_release_workflow(text):
    """The release workflow may not be reachable from an unreviewed ref."""
    events = text.split("\non:\n", 1)[1].split("\npermissions:\n", 1)[0]
    # A tag can be put on any commit and the workflow that runs is the one at
    # that commit, so a tag trigger routes around the review rules on `main`
    # that are part of the signing boundary.
    assert not re.search(r"^  push", events, re.M), "must not run on push, including tags"
    assert not re.search(r"^  pull_request", events, re.M), "must not run on pull_request"
    assert not re.search(r"^  release", events, re.M), "must not run on release events"
    assert re.search(r"^  workflow_dispatch:$", events, re.M), "manual dispatch only"

    # Scoped to the whole file, not just the signing job: `|| true` in the
    # build job would mask the archive-layout check, which is the one thing
    # standing between a malformed archive and a signature over it.
    for pattern, reason in FORBIDDEN:
        assert not re.search(pattern, text, re.M), reason

    sign = text.split("\n  sign:\n", 1)[1]
    assert re.search(r"^    runs-on: ubuntu-24\.04$", sign, re.M), "minisign needs noble"
    assert re.search(r"^    environment: release$", sign, re.M), "the secret belongs to an environment"
    assert re.search(r"shred -u", sign), "must destroy the key file it wrote"

    lines = text.splitlines()
    for line in RELEASE_REQUIRED_LINES:
        assert line in lines, f"missing exact line: {line.strip()}"

    for line in sign.splitlines():
        if SECRET_EXPANSION.search(line):
            assert line in SECRET_LINES, f"unexpected use of the secret: {line.strip()}"
    steps = re.split(r"^      - (?:name|uses):", sign, flags=re.M)
    signing = [step for step in steps if SECRET_EXPANSION.search(step)]
    assert len(signing) == 1, "exactly one step may read the signing secret"

    # The build job must not be on the signing runner: its glibc is the
    # floor every node has to clear, and noble's is higher than the fleet's.
    build = text.split("\n  build:\n", 1)[1].split("\n  sign:\n", 1)[0]
    assert re.search(r"^    runs-on: ubuntu-22\.04$", build, re.M), (
        "the build job stays on the oldest supported runner; releasing from "
        "noble raises the glibc floor for every node that installs it"
    )
    assert not SECRET_EXPANSION.search(build), "the build job must not see the secret"


def validate_workflow(text):
    events = text.split("\non:\n", 1)[1].split("\npermissions:\n", 1)[0]
    # The secret is readable by anything this workflow runs. A pull_request
    # trigger would let an unreviewed branch edit the job that holds it.
    assert not re.search(r"^  pull_request", events, re.M), "must not run on pull_request"
    assert re.search(r"^  push:\n    branches: \[main\]$", events, re.M)
    job = text.split("\n  signing-selftest:\n", 1)[1]
    # minisign is not packaged before Ubuntu 24.04; an older image can never
    # install it, so the job would fail for a reason unrelated to the key.
    assert re.search(r"^    runs-on: ubuntu-24\.04$", job, re.M), "needs a noble-or-newer runner"
    # The secret is an environment secret with a deployment-branch rule; a job
    # that does not declare the environment cannot see it at all.
    assert re.search(r"^    environment: release-selftest$", job, re.M), "the secret belongs to an environment"
    for pattern, reason in FORBIDDEN:
        assert not re.search(pattern, job, re.M), reason
    for line in REQUIRED_LINES:
        assert line in job.splitlines(), f"missing exact line: {line.strip()}"
    for line in job.splitlines():
        if SECRET_EXPANSION.search(line):
            assert line in SECRET_LINES, f"unexpected use of the secret: {line.strip()}"
    assert re.search(r"shred -u", job), "must destroy the key file it wrote"

    # Scope the shell-safety checks to the one step that holds the secret.
    # Asserting them against the whole job lets an identical line in some other
    # step satisfy a requirement this step has dropped.
    steps = re.split(r"^      - (?:name|uses):", job, flags=re.M)
    signing = [step for step in steps if SECRET_EXPANSION.search(step)]
    assert len(signing) == 1, "exactly one step may read the signing secret"
    body = signing[0].split("        run: |\n", 1)[1]
    first = next(
        line.strip()
        for line in body.splitlines()
        if line.strip() and not line.strip().startswith("#")
    )
    assert first == "set -euo pipefail", f"signing step must start with set -euo pipefail, got: {first}"


class PublicKeyFile(unittest.TestCase):
    def setUp(self):
        self.text = PUBLIC_KEY.read_text()

    def test_live_key(self):
        validate_public_key(self.text)

    def test_placeholder_key_rejected(self):
        placeholder = base64.b64encode(b"Ed" + bytes(40)).decode()
        with self.assertRaises(AssertionError):
            validate_public_key(f"untrusted comment: x\n{placeholder}\n")

    def test_different_valid_key_rejected(self):
        # A real, well-formed minisign key that is simply not ours. This is the
        # case a structure-only check cannot see.
        other = "RWST0HTBrC+WCOk/vrCjfFlPdAlZW4wvbZSOGyEKhSXIl+oY0jX4mrCz"
        with self.assertRaises(AssertionError):
            validate_public_key(f"untrusted comment: minisign public key {KEY_ID}\n{other}\n")

    def test_comment_naming_another_key_rejected(self):
        with self.assertRaises(AssertionError):
            validate_public_key(
                self.text.replace(KEY_ID, "0000000000000000", 1)
            )

    def test_wrong_algorithm_rejected(self):
        raw = base64.b64decode(self.text.splitlines()[1], validate=True)
        swapped = base64.b64encode(b"ED" + raw[2:]).decode()
        with self.assertRaises(AssertionError):
            validate_public_key(f"untrusted comment: {KEY_ID}\n{swapped}\n")

    def test_truncated_key_rejected(self):
        raw = base64.b64decode(self.text.splitlines()[1], validate=True)
        short = base64.b64encode(raw[:-1]).decode()
        with self.assertRaises(AssertionError):
            validate_public_key(f"untrusted comment: {KEY_ID}\n{short}\n")

    def test_comment_only_file_rejected(self):
        with self.assertRaises(AssertionError):
            validate_public_key("untrusted comment: minisign public key ABC\n")


class SelfTestWorkflow(unittest.TestCase):
    def setUp(self):
        self.text = WORKFLOW.read_text()

    def test_live_workflow(self):
        validate_workflow(self.text)

    def test_pull_request_trigger_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(self.text.replace("  push:\n", "  pull_request:\n  push:\n"))

    def test_conditional_job_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(
                self.text.replace(
                    "  signing-selftest:\n",
                    "  signing-selftest:\n    if: github.event_name == 'workflow_dispatch'\n",
                )
            )

    def test_runner_without_minisign_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(self.text.replace("ubuntu-24.04", "ubuntu-22.04"))

    def test_verification_without_prehash_flag_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(self.text.replace("minisign -V -H -p", "minisign -V -p"))

    def test_ignored_verification_failure_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(self.text.replace(f"          {VERIFY}\n", f"          {VERIFY} || true\n", 1))

    def test_disabled_negative_case_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(self.text.replace(f"if {VERIFY}; then", "if false; then"))

    def test_dropped_verification_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(self.text.replace(VERIFY, "true"))

    def test_dropped_signing_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(self.text.replace('minisign -S -s "${key}"', "true #"))

    def test_missing_errexit_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(self.text.replace("          set -euo pipefail\n", "", 1))

    def test_command_tracing_rejected(self):
        for mutation in ("set -euxo pipefail", "set -euo pipefail\n          set -o xtrace"):
            with self.assertRaises(AssertionError):
                validate_workflow(self.text.replace("          set -euo pipefail", f"          {mutation}", 1))

    def test_shell_override_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(
                self.text.replace(
                    "        env:\n", "        shell: bash -x {0}\n        env:\n", 1
                )
            )

    def test_echoing_the_secret_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(
                self.text.replace(
                    "          umask 077\n",
                    '          umask 077\n          echo "${MINISIGN_SECRET_KEY}"\n',
                    1,
                )
            )

    def test_printing_the_key_file_rejected(self):
        with self.assertRaises(AssertionError):
            validate_workflow(
                self.text.replace(
                    "          umask 077\n", '          umask 077\n          cat "${key}"\n', 1
                )
            )


class ReleaseWorkflow(unittest.TestCase):
    """The producing half. Every mutation here leaves a workflow that still
    runs and still produces something — which is the point: none of these
    failures announce themselves."""

    def setUp(self):
        self.text = RELEASE.read_text()

    def rejects(self, text, fragment=""):
        with self.assertRaises(AssertionError) as caught:
            validate_release_workflow(text)
        if fragment:
            self.assertIn(fragment, str(caught.exception))

    def test_live_workflow(self):
        validate_release_workflow(self.text)

    def test_tag_trigger_rejected(self):
        # The whole reason this workflow is manual: a tag can be placed on any
        # commit, and the workflow that runs is the one at that commit.
        self.rejects(
            self.text.replace(
                "on:\n  workflow_dispatch:",
                "on:\n  push:\n    tags: ['v*']\n  workflow_dispatch:",
            ),
            "must not run on push",
        )

    def test_pull_request_trigger_rejected(self):
        self.rejects(
            self.text.replace(
                "on:\n  workflow_dispatch:", "on:\n  pull_request:\n  workflow_dispatch:"
            ),
            "pull_request",
        )

    def test_release_event_trigger_rejected(self):
        self.rejects(
            self.text.replace(
                "on:\n  workflow_dispatch:",
                "on:\n  release:\n    types: [published]\n  workflow_dispatch:",
            ),
            "release events",
        )

    def test_dropped_ref_guard_rejected(self):
        self.rejects(
            self.text.replace('          if [ "${GITHUB_REF}" != "refs/heads/main" ]; then', "          if false; then"),
        )

    def test_dropped_environment_gate_rejected(self):
        self.rejects(
            self.text.replace('          if [ "${GATE}" != "configured" ]; then', "          if false; then"),
        )

    def test_dropped_environment_rejected(self):
        # Without it the secret is a repository secret any job can read.
        self.rejects(self.text.replace("    environment: release\n", ""), "environment")

    def test_verification_without_prehash_flag_rejected(self):
        # Green here, refused on every node: danso-ops rejects legacy
        # signatures and `minisign -V` accepts them without `-H`.
        self.rejects(self.text.replace(RELEASE_VERIFY, 'minisign -V -p "${pubkey}" -m SHA256SUMS'))

    def test_dropped_digest_check_rejected(self):
        # A signed manifest whose digests do not match the artifacts it names
        # is a correctly signed lie.
        self.rejects(self.text.replace("          sha256sum -c SHA256SUMS\n", ""))

    def test_dropped_layout_check_rejected(self):
        self.rejects(
            self.text.replace(
                """            echo "archive must contain exactly one member named 'danso', found:\"""",
                '            echo "unexpected layout"',
            )
        )

    def test_rewriting_the_manifest_with_sed_rejected(self):
        # `sha256sum ./x` writes `<digest>  ./x`; rewriting the prefix away
        # with sed eats one of the two spaces the format requires. Measured:
        # the resulting manifest is not the one the parser reads.
        self.rejects(
            self.text.replace(
                '          sha256sum -- "${archives[@]}" > SHA256SUMS',
                "          sha256sum ./*.tar.gz | sed 's| \\./| |' > SHA256SUMS",
            )
        )

    def test_disabled_negative_case_rejected(self):
        self.rejects(
            self.text.replace(
                '            echo "release failed: a tampered manifest verified"',
                '            echo "ok"',
            )
        )

    def test_building_on_the_signing_runner_rejected(self):
        # noble's glibc is higher than the fleet's; releasing from it would
        # produce binaries that cannot start on the nodes they are for.
        self.rejects(
            self.text.replace("    runs-on: ubuntu-22.04", "    runs-on: ubuntu-24.04", 1),
            "oldest supported runner",
        )

    def test_older_signing_runner_rejected(self):
        sign = self.text.split("\n  sign:\n", 1)
        self.rejects(
            sign[0] + "\n  sign:\n" + sign[1].replace("    runs-on: ubuntu-24.04", "    runs-on: ubuntu-22.04", 1),
            "noble",
        )

    def test_secret_in_the_build_job_rejected(self):
        self.rejects(
            self.text.replace(
                "      - name: Install the toolchain",
                "      - name: Leak\n        env:\n"
                "          MINISIGN_SECRET_KEY: ${{ secrets.MINISIGN_SECRET_KEY }}\n"
                "        run: true\n"
                "      - name: Install the toolchain",
            ),
            "build job must not see the secret",
        )

    def test_command_tracing_rejected(self):
        self.rejects(self.text.replace("          set -euo pipefail", "          set -euxo pipefail", 1))

    def test_ignored_verification_failure_rejected(self):
        self.rejects(self.text.replace(RELEASE_VERIFY, f"{RELEASE_VERIFY} || true"))

    def test_second_secret_reader_rejected(self):
        self.rejects(
            self.text.replace(
                "      - name: Verify what was just signed, as the installer will",
                "      - name: Also\n        env:\n"
                "          MINISIGN_SECRET_KEY: ${{ secrets.MINISIGN_SECRET_KEY }}\n"
                "        run: true\n"
                "      - name: Verify what was just signed, as the installer will",
            ),
            "exactly one step",
        )

    def test_dropped_shred_rejected(self):
        self.rejects(
            self.text.replace(
                """          trap 'shred -u -z "${key}" 2>/dev/null || rm -f "${key}"' EXIT""",
                """          trap 'rm -f "${key}"' EXIT""",
            )
        )



if __name__ == "__main__":
    unittest.main()

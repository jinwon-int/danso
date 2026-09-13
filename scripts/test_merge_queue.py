"""Offline guard for the always-required contracts workflow; no providers."""
import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]


def validate(text):
    # Deliberately guard the checked-in block form rather than parse arbitrary YAML.
    events = text.split("\non:\n", 1)[1].split("\npermissions:\n", 1)[0]
    assert re.search(r"^  pull_request:\s*$", events, re.M)
    assert "  merge_group:\n    types: [checks_requested]\n" in events
    assert not re.search(r"^\s+(paths|paths-ignore):", events, re.M)
    job = re.split(r"^  [\w-]+:\s*$", text.split("\n  contracts:\n", 1)[1], maxsplit=1, flags=re.M)[0]
    # Scope assertions to the required job, and reject step as well as job
    # conditions (including folded YAML); skipped steps can otherwise be green.
    assert not re.search(r"^\s*if:|continue-on-error", job, re.M)
    assert "run: python3 scripts/test_merge_queue.py" in job
    assert "cargo +1.98.1 test --locked" in job
    assert "DANSO_BIN=target/release/danso python3 scripts/test_e2e.py" in job


class MergeQueue(unittest.TestCase):
    def setUp(self):
        self.text = (ROOT / ".github/workflows/ci.yml").read_text()

    def test_live_workflow(self):
        validate(self.text)

    def test_missing_queue_event_rejected(self):
        with self.assertRaises(AssertionError):
            validate(self.text.replace("  merge_group:\n    types: [checks_requested]\n", ""))

    def test_paths_filter_rejected(self):
        with self.assertRaises(AssertionError):
            validate(self.text.replace("  pull_request:\n", "  pull_request:\n    paths: ['src/**']\n"))

    def test_pr_only_job_rejected(self):
        with self.assertRaises(AssertionError):
            validate(self.text.replace("  contracts:\n", "  contracts:\n    if: github.event_name == 'pull_request'\n"))

    def test_pr_only_contracts_step_rejected(self):
        with self.assertRaises(AssertionError):
            validate(self.text.replace(
                "      - name: Format, lint and test\n",
                "      - name: Format, lint and test\n        if: >-\n          github.event_name == 'pull_request'\n",
            ))

    def test_commands_moved_to_another_job_rejected(self):
        with self.assertRaises(AssertionError):
            validate(self.text.replace(
                "      - name: Format, lint and test\n",
                "  non-required:\n    runs-on: ubuntu-latest\n    steps:\n      - name: Format, lint and test\n",
            ))


if __name__ == "__main__":
    unittest.main()

"""Protocol statuses govern first-ledger creation and never replay CAS writes."""

import importlib.util
import json
import os

# Offline fixtures construct results; subprocess execution is mocked.
import subprocess  # nosec B404
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = next(
    parent / "scripts"
    for parent in Path(__file__).resolve().parents
    if (parent / "scripts/release_state.py").is_file()
)
SPEC = importlib.util.spec_from_file_location(
    "release_state_http_client", SCRIPTS / "release_state.py"
)
state = importlib.util.module_from_spec(SPEC)
sys.path.insert(0, str(SCRIPTS))
try:
    SPEC.loader.exec_module(state)
finally:
    sys.path.remove(str(SCRIPTS))


class LedgerHTTPTests(unittest.TestCase):
    """Use real transport methods with an endpoint-specific subprocess fake."""

    def setUp(self):
        environment = patch.dict(os.environ, {"RELEASE_REQUIRE_WORKFLOW_SCOPE": ""})
        environment.start()
        self.addCleanup(environment.stop)
        self.github = state.StateGitHub("example/project")

    def response(self, path, status, payload, method="GET", malformed=False):
        """Build wire headers independently from intentionally misleading stderr."""

        def run(command, **kwargs):
            self.assertEqual(command[:2], ["gh", "api"])
            self.assertIn("--include", command)
            self.assertEqual(command[command.index("--method") + 1], method)
            self.assertEqual(command[-2:], ["--", f"repos/example/project/{path}"])
            if method == "PUT":
                self.assertEqual(json.loads(kwargs["input"])["branch"], state.BRANCH)
            wire = (
                f"HTTP/2.0 {status} Response\nContent-Type: application/json\r\n\r\n".encode()
                + json.dumps(payload).encode()
            )
            return subprocess.CompletedProcess(
                command,
                int(status >= 400),
                b"" if malformed else wire,
                b"gh: HTTP 404: misleading upstream diagnostic\n",
            )

        return run

    def test_only_two_real_404_reads_initialize_an_absent_ledger(self):
        replies = [
            self.response(state.READ_PATH, 404, {"errors": "HTTP 403"}),
            self.response(state.REF_PATH, 404, {"errors": "HTTP 403"}),
        ]

        def dispatch(command, **kwargs):
            return replies.pop(0)(command, **kwargs)

        with patch.object(state.subprocess, "run", side_effect=dispatch) as run:
            ledger, previous = state.read_state(self.github)
        self.assertEqual(
            ledger, {"schema": 1, "counter": 0, "publication_floor": 0, "plans": {}}
        )
        self.assertIsNone(previous)
        self.assertEqual(run.call_count, 2)
        self.assertFalse(replies)

    def test_errors_cannot_initialize_or_replay_a_ledger_read(self):
        for status in (401, 403, 429, 503):
            with (
                self.subTest(status=status),
                patch.object(
                    state.subprocess,
                    "run",
                    side_effect=self.response(
                        state.READ_PATH, status, {"errors": "HTTP 404"}
                    ),
                ) as run,
                self.assertRaises(state.rc.GitHubError) as error,
            ):
                state.read_state(self.github)
            self.assertFalse(error.exception.not_found)
            run.assert_called_once()

    def test_absent_headers_do_not_initialize_a_ledger(self):
        with (
            patch.object(
                state.subprocess,
                "run",
                side_effect=self.response(state.READ_PATH, 404, {}, malformed=True),
            ) as run,
            self.assertRaises(state.rc.GitHubError) as error,
        ):
            state.read_state(self.github)
        self.assertFalse(error.exception.not_found)
        run.assert_called_once()

    def test_existing_branch_with_missing_file_is_not_reset(self):
        replies = [
            self.response(state.READ_PATH, 404, {}),
            self.response(state.REF_PATH, 200, {"ref": "refs/heads/" + state.BRANCH}),
        ]

        def dispatch(command, **kwargs):
            return replies.pop(0)(command, **kwargs)

        with (
            patch.object(state.subprocess, "run", side_effect=dispatch) as run,
            self.assertRaisesRegex(state.rc.ReleaseError, "restore it from history"),
        ):
            state.read_state(self.github)
        self.assertEqual(run.call_count, 2)

    def test_failed_cas_write_is_never_replayed(self):
        for status in (403, 409, 503):
            with (
                self.subTest(status=status),
                patch.object(
                    state.subprocess,
                    "run",
                    side_effect=self.response(
                        state.WRITE_PATH, status, {"errors": "HTTP 404"}, "PUT"
                    ),
                ) as run,
                self.assertRaises(state.rc.GitHubError) as error,
            ):
                self.github.api(
                    state.WRITE_PATH, "PUT", {"branch": state.BRANCH, "content": "e30="}
                )
            self.assertFalse(error.exception.not_found)
            run.assert_called_once()

    def test_successful_cas_write_strips_headers(self):
        with patch.object(
            state.subprocess,
            "run",
            side_effect=self.response(
                state.WRITE_PATH, 201, {"content": {"sha": "a" * 40}}, "PUT"
            ),
        ) as run:
            result = self.github.api(
                state.WRITE_PATH, "PUT", {"branch": state.BRANCH, "content": "e30="}
            )
        self.assertEqual(result, {"content": {"sha": "a" * 40}})
        run.assert_called_once()


if __name__ == "__main__":
    unittest.main()

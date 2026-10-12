# Shared release-contract regression; keep consumers synchronized.
# ruff: noqa
# mypy: ignore-errors
# pylint: skip-file
# fmt: off
"""Only exact GitHub 404 diagnostics may establish an absent release resource."""

import importlib.util
import os
import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "release_control_http_status",
    Path(__file__).parents[2] / "scripts/release_control.py",
)
rc = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(rc)


class MissingResourceStatusTests(unittest.TestCase):
    def setUp(self):
        environment = patch.dict(os.environ, {"RELEASE_REQUIRE_WORKFLOW_SCOPE": ""})
        environment.start()
        self.addCleanup(environment.stop)
        self.github = rc.GitHub("example/project")

    def test_exact_404_is_optional(self):
        for diagnostic in (
            b"gh: Not Found (HTTP 404)",
            b"gh: HTTP 404",
            b"gh: HTTP 404: Not Found",
            b"gh: HTTP 404 \t\r\n",
        ):
            with (
                self.subTest(diagnostic=diagnostic),
                patch.object(
                    rc.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess(
                        [],
                        1,
                        b"",
                        diagnostic,
                    ),
                ) as run,
            ):
                self.assertIsNone(self.github.optional("git/ref/tags/v1.2.3"))
            run.assert_called_once()

    def test_embedded_or_conflicting_404_cannot_authorize_publication(self):
        for diagnostic in (
            b"gh: Request failed: HTTP 404 from upstream (HTTP 403)",
            b"gh: HTTP 4040: invalid status",
            b"gh: HTTP 404suffix",
            b"other tool: HTTP 404",
            b"gh: HTTP 404: Gateway reported (HTTP 502)",
            b"gh: HTTP 502: Gateway reported (HTTP 404)",
            b"gh: HTTP 404\ngh: HTTP 403",
            b"gh: HTTP 502: upstream returned HTTP 404",
        ):
            for operation in (
                lambda: self.github.optional("git/ref/tags/v1.2.3"),
                lambda: rc.ensure_absent(self.github, "v1.2.3"),
            ):
                with self.subTest(diagnostic=diagnostic, operation=operation):
                    with (
                        patch.object(
                            rc.subprocess,
                            "run",
                            return_value=subprocess.CompletedProcess(
                                [],
                                1,
                                b"",
                                diagnostic,
                            ),
                        ) as run,
                        patch("time.sleep"),
                        self.assertRaises(rc.GitHubError) as error,
                    ):
                        operation()
                    self.assertFalse(error.exception.not_found)
                    self.assertTrue(run.called)
                    for call in run.call_args_list:
                        command = call.args[0]
                        method = (
                            command[command.index("--method") + 1]
                            if "--method" in command
                            else "GET"
                        )
                        self.assertEqual(method, "GET")

    def test_failed_write_is_not_missing_or_replayed(self):
        with (
            patch.object(
                rc.subprocess,
                "run",
                return_value=subprocess.CompletedProcess(
                    [],
                    1,
                    b"",
                    b"gh: Authorization error mentions HTTP 404 (HTTP 403)",
                ),
            ) as run,
            self.assertRaises(rc.GitHubError) as error,
        ):
            self.github.api("releases", "POST", {"tag_name": "v1.2.3-rc.1"})
        self.assertFalse(error.exception.not_found)
        run.assert_called_once()


if __name__ == "__main__":
    unittest.main()

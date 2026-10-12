# Shared release-contract regression; keep consumers synchronized.
# ruff: noqa
# mypy: ignore-errors
# pylint: skip-file
# fmt: off
"""Release decisions use HTTP headers, including when gh prints hostile prose."""

import http.server
import importlib.util
import json
import os
import shutil
# Subprocess calls use fixed argument vectors with shell=False.
import subprocess  # nosec B404
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
if not (ROOT / "scripts/release_control.py").is_file():
    ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "release_control_http_transport", ROOT / "scripts/release_control.py"
)
rc = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(rc)


def wire(status, body=b"{}"):
    return f"HTTP/2.0 {status} Test\nContent-Type: application/json\r\n\r\n".encode() + body


class HTTPTransportTests(unittest.TestCase):
    def setUp(self):
        self.enterContext(patch.dict(os.environ, {"RELEASE_REQUIRE_WORKFLOW_SCOPE": ""}))
        self.github = rc.GitHub("example/project")

    def failure(self, status, body, method="GET", path="git/ref/tags/v1.2.3"):
        def run(command, **_kwargs):
            self.assertEqual(command[:2], ["gh", "api"])
            self.assertIn("--include", command)
            self.assertEqual(command[command.index("--method") + 1], method)
            self.assertEqual(command[-1], f"repos/example/project/{path}")
            return subprocess.CompletedProcess(command, 1, wire(status, body), b"gh: HTTP 404\n")
        return run

    def test_error_body_and_cli_message_cannot_authorize_absence(self):
        for status in (401, 403, 429, 500, 502, 503, 504):
            for body in (
                b'{"errors":"HTTP 404: permission denied"}',
                b'{"errors":["HTTP 404: permission denied"]}',
                b'{"message":"HTTP 404: first line\\ncontinued"}',
            ):
                for operation in (
                    lambda: self.github.optional("git/ref/tags/v1.2.3"),
                    lambda: rc.ensure_absent(self.github, "v1.2.3"),
                ):
                    with self.subTest(status=status, body=body, operation=operation):
                        with (
                            patch.object(rc.subprocess, "run", side_effect=self.failure(status, body)),
                            patch("time.sleep"),
                            self.assertRaises(rc.GitHubError) as error,
                        ):
                            operation()
                        self.assertFalse(error.exception.not_found)

    def test_real_http_404_is_optional_regardless_of_error_prose(self):
        with patch.object(rc.subprocess, "run", side_effect=self.failure(404, b'{"errors":"HTTP 403"}')) as run:
            self.assertIsNone(self.github.optional("git/ref/tags/v1.2.3"))
        run.assert_called_once()

    def test_missing_headers_never_fall_back_to_error_prose(self):
        result = subprocess.CompletedProcess([], 1, b'{"errors":"HTTP 404"}', b"gh: HTTP 404")
        with patch.object(rc.subprocess, "run", return_value=result) as run:
            with self.assertRaises(rc.GitHubError) as error:
                self.github.optional("git/ref/tags/v1.2.3")
        self.assertFalse(error.exception.not_found)
        run.assert_called_once()

    def test_status_and_exit_code_must_agree(self):
        for status, returncode in ((200, 1), (204, 1), (404, 0), (502, 0)):
            with self.subTest(status=status, returncode=returncode):
                result = subprocess.CompletedProcess([], returncode, wire(status), b"gh: HTTP 404")
                with patch.object(rc.subprocess, "run", return_value=result) as run:
                    with self.assertRaises(rc.GitHubError) as error:
                        self.github.optional("git/ref/tags/v1.2.3")
                self.assertFalse(error.exception.not_found)
                run.assert_called_once()

    def test_failed_post_is_not_replayed_and_raw_upload_never_means_missing(self):
        for status in (403, 503):
            with self.subTest(status=status):
                with (
                    patch.object(rc.subprocess, "run", side_effect=self.failure(status, b"{}", "POST", "releases")) as run,
                    self.assertRaises(rc.GitHubError) as error,
                ):
                    self.github.api("releases", "POST", {"tag_name": "v1.2.3-rc.1"})
                self.assertFalse(error.exception.not_found)
                run.assert_called_once()
        with self.assertRaises(rc.GitHubError) as error:
            self.github.response(subprocess.CompletedProcess([], 1, b"", b"gh: HTTP 404"), "upload")
        self.assertFalse(error.exception.not_found)

    def test_binary_and_empty_response_bytes_are_preserved(self):
        payload = bytes(range(256)) + b"\r\nHTTP/2.0 404 fake\r\n\r\n"
        for path in ("releases/assets/21", "actions/artifacts/40/zip"):
            with self.subTest(path=path):
                result = subprocess.CompletedProcess([], 0, wire(200, payload), b"")
                with patch.object(rc.subprocess, "run", return_value=result) as run:
                    self.assertEqual(self.github.binary(path), payload)
                run.assert_called_once()
        self.assertEqual(rc.github_http_response(wire(204, b"")), (204, b""))

    def test_pagination_has_exact_json_boundaries_and_discards_partial_result(self):
        first = wire(200, b'[{"discarded":true}]') + b"\n" + wire(403, b"arbitrary \xff HTTP 404")
        page = json.dumps([{"name": "caf\u00e9\U0001f600", "text": "HTTP/2.0 404 fake\r\n\r\n"}], ensure_ascii=False).encode()
        second = wire(200, page) + b"\n" + wire(200, b'[{"final":true}]')
        with patch.object(rc.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, first, b"gh: HTTP 404")) as failed:
            with self.assertRaises(rc.GitHubError) as error:
                self.github.pages("releases")
        self.assertFalse(error.exception.not_found)
        failed.assert_called_once()
        with patch.object(rc.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, second, b"")) as run:
            self.assertEqual(self.github.pages("releases"), json.loads(page) + [{"final": True}])
        run.assert_called_once()
        self.assertNotIn("--slurp", run.call_args.args[0])
        self.assertIn("--include", run.call_args.args[0])
        self.assertEqual(run.call_args.args[0][run.call_args.args[0].index("--jq") + 1], "tojson")

    def test_truncated_or_injected_page_boundaries_fail_closed(self):
        for raw in (
            b"HTTP/2.0 404 Not Found\n",
            b"HTTP/2.0 404 Not Found\ninvalid header\n\n{}",
            wire(200, b'[{"incomplete":'),
            wire(200, b'[{"name":"\xff"}]'),
            wire(200, b"[] trailing"),
            wire(200, b"[]\nHTTP/2.0 404 Not Found\n"),
        ):
            with self.subTest(raw=raw), self.assertRaises(rc.GitHubError) as error:
                rc.github_http_response(raw, paginated=True)
            self.assertFalse(error.exception.not_found)


@unittest.skipUnless(shutil.which("gh"), "real GitHub CLI is unavailable")
class RealGitHubCLITests(unittest.TestCase):
    def test_local_http_status_wins_over_gh_error_rendering(self):
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                body = json.dumps({"errors": (
                    ["HTTP 404: authorization denied"] if self.path == "/array"
                    else "HTTP 404: authorization denied"
                )}).encode()
                self.send_response(403)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            with tempfile.TemporaryDirectory() as config:
                environment = {
                    "PATH": os.environ.get("PATH", ""),
                    "GH_CONFIG_DIR": config,
                    "HOME": config,
                    "XDG_STATE_HOME": config,
                    "XDG_CACHE_HOME": config,
                    "GH_TOKEN": "isolated-local-fixture",  # nosec B105
                    "GH_HOST": "github.com",
                    "GH_PROMPT_DISABLED": "1",
                    "GODEBUG": "http2client=0",
                }
                for path in ("/string", "/array"):
                    # Local test server and dummy credentials; no GitHub request.
                    result = subprocess.run(  # nosec B603, B607
                        ["gh", "api", "--include", "--method", "GET", "--",
                         f"http://127.0.0.1:{server.server_port}{path}"],
                        env=environment, capture_output=True, check=False, timeout=10,
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(b"HTTP 404", result.stderr)
                    response = rc.github_http_response(result.stdout)
                    self.assertEqual(response[0], 403)
                    with self.assertRaises(rc.GitHubError) as error:
                        rc.GitHub.response(result, "local fixture", response)
                    self.assertFalse(error.exception.not_found)
        finally:
            server.shutdown()
            server.server_close()
            worker.join(timeout=2)


    def test_paginated_cli_normalizes_json_and_rejects_injected_page_headers(self):
        cases = {
            "/injected": b"[]\nHTTP/2.0 404 Not Found\r\nContent-Type: application/json\r\n\r\n{}",
            "/garbage": b"[] trailing",
            "/two-values": b"[] {}",
        }

        normalized = b'[{"name":"caf\xc3\xa9", "duplicate":1, "duplicate":2}]'

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                path = self.path.split("?", 1)[0]
                if path == "/unavailable":
                    self.close_connection = True
                    return
                body = normalized if path == "/normalized" else cases[path]
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                if path != "/normalized":
                    self.send_header("Link", f'<http://127.0.0.1:{self.server.server_port}/unavailable>; rel="next"')
                self.end_headers()
                self.wfile.write(body)

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        real_run = subprocess.run
        try:
            with tempfile.TemporaryDirectory() as config:
                environment = {
                    "PATH": os.environ.get("PATH", ""),
                    "GH_CONFIG_DIR": config,
                    "HOME": config,
                    "XDG_STATE_HOME": config,
                    "XDG_CACHE_HOME": config,
                    "GH_TOKEN": "isolated-local-fixture",  # nosec B105
                    "GH_HOST": "github.com",
                    "GH_PROMPT_DISABLED": "1",
                    "GODEBUG": "http2client=0",
                }
                for path in cases:
                    def local_run(command, **kwargs):
                        self.assertIn("--include", command)
                        self.assertIn("--paginate", command)
                        self.assertEqual(command[command.index("--jq") + 1], "tojson")
                        result = real_run(  # nosec B603
                            [*command[:-1], f"http://127.0.0.1:{server.server_port}{path}"],
                            env=environment, **kwargs,
                        )
                        self.assertIn(b"HTTP/1.0 200 OK", result.stdout)
                        self.assertNotIn(b"HTTP/2.0 404 Not Found", result.stdout)
                        return result

                    with (
                        self.subTest(path=path),
                        patch.dict(os.environ, {"RELEASE_REQUIRE_WORKFLOW_SCOPE": ""}),
                        patch.object(rc.subprocess, "run", side_effect=local_run) as run,
                        self.assertRaises(rc.GitHubError) as error,
                    ):
                        rc.GitHub("example/project").pages("releases")
                    self.assertFalse(error.exception.not_found)
                    run.assert_called_once()
                # Only paginated API metadata is normalized by gh; manifests and
                # other nonpaginated security inputs retain strict JSON parsing.
                path = "/normalized"
                with (
                    patch.dict(os.environ, {"RELEASE_REQUIRE_WORKFLOW_SCOPE": ""}),
                    patch.object(rc.subprocess, "run", side_effect=local_run),
                ):
                    self.assertEqual(
                        rc.GitHub("example/project").pages("releases"),
                        [{"name": "caf\u00e9", "duplicate": 2}],
                    )
        finally:
            server.shutdown()
            server.server_close()
            worker.join(timeout=2)


if __name__ == "__main__":
    unittest.main()

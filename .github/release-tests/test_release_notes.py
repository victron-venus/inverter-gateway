"""Keep human release notes bound to the source used to build the package."""

import base64
import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

_SPEC = importlib.util.spec_from_file_location(
    "notes_release_control", Path(__file__).parents[2] / "scripts/release_control.py"
)
release = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(release)
SOURCE = "a" * 40


class StrictGitHub:
    """Permit only the expected source read and reject all remote mutation."""

    def __init__(self, response: dict | None = None):
        self.responses = [] if response is None else [response]
        self.calls = []
        self.writes = []

    def api(self, path: str, method: str = "GET", body: dict | None = None):
        self.calls.append((method, path, body))
        if method != "GET":
            self.writes.append((method, path, body))
            message = f"Unexpected remote write: {method} {path}"
            raise AssertionError(message)
        if path != f"contents/CHANGELOG.md?ref={SOURCE}" or not self.responses:
            message = f"Unexpected remote read: {path}"
            raise AssertionError(message)
        return self.responses.pop(0)

    def upload(self, tag: str, path: Path):
        self.writes.append(("upload", tag, path))
        message = f"Unexpected remote upload: {tag} {path}"
        raise AssertionError(message)


def contents(text):
    raw = text.encode()
    return {
        "type": "file",
        "path": "CHANGELOG.md",
        "encoding": "base64",
        "content": base64.b64encode(raw).decode(),
        "size": len(raw),
        "sha": release.hashlib.sha1(
            b"blob " + str(len(raw)).encode() + b"\0" + raw, usedforsecurity=False
        ).hexdigest(),
    }


NOTES = """# Changelog
## [1.2.4] - Unreleased
Do not publish this later change.
## [1.2.3] - 2026-10-07
### Fixed
Preserve unavailable telemetry instead of reporting zero.
### Upgrade
Review optional site settings before enabling the feature.
### Security
Reject malformed requests before issuing hardware commands.
## [1.2.2] - 2026-10-01
Do not copy older notes either.
"""


def render(text=NOTES, tag="v1.2.3-beta.8", response_change=None):
    response = contents(text)
    response.update(response_change or {})
    github = StrictGitHub(response)
    with patch.object(
        release,
        "source_policy_snapshot",
        return_value={"data": {"release_notes": "CHANGELOG.md"}},
    ):
        body = release.release_notes(
            github, tag, SOURCE, "Original source and validation links."
        )
    assert github.calls == [("GET", f"contents/CHANGELOG.md?ref={SOURCE}", None)]
    assert not github.responses
    assert not github.writes
    return body


class ReleaseNotesTests(unittest.TestCase):
    def test_fenced_guidance_cannot_satisfy_real_sections(self):
        for marker in ("```", "````", "~~~", "~~~~~"):
            for indentation in ("", " ", "  ", "   "):
                text = (
                    "## [1.2.3]\n"
                    + indentation
                    + marker
                    + "markdown\n"
                    + "### Upgrade\nExample upgrade.\n"
                    + "### Security\nExample security.\n"
                    + indentation
                    + marker
                    + "\n"
                )
                with (
                    self.subTest(marker=marker, indentation=indentation),
                    self.assertRaisesRegex(release.ReleaseError, "Upgrade guidance"),
                ):
                    render(text)

    def test_fenced_version_headings_do_not_select_or_split_sections(self):
        for marker in ("```", "~~~~"):
            example = marker + "markdown\n## [1.2.3]\n## [9.9.9]\n" + marker + "\n"
            text = example + NOTES.replace("### Fixed", example + "### Fixed")
            body = render(text)
            self.assertIn(example, body)
            self.assertIn("Preserve unavailable telemetry", body)
            self.assertNotIn("Do not publish", body)
            self.assertNotIn("Do not copy", body)

    def test_fence_closes_only_with_matching_marker_and_sufficient_length(self):
        for opening, false_closer in (
            ("````", "```"),
            ("```", "~~~"),
            ("~~~~", "~~~"),
            ("~~~", "```"),
            ("```", "``` trailing text"),
            ("```", "    ```"),
            ("```", "```\u00a0"),
        ):
            text = (
                "## [1.2.3]\n"
                + opening
                + "\n"
                + false_closer
                + "\n### Upgrade\nExample upgrade.\n### Security\nExample security.\n"
            )
            with (
                self.subTest(opening=opening, false_closer=false_closer),
                self.assertRaisesRegex(release.ReleaseError, "Upgrade guidance"),
            ):
                render(text)

    def test_longer_matching_fences_restore_heading_recognition(self):
        for marker in ("```", "~~~"):
            prefix = marker + "text\n## [1.2.3]\n   " + marker * 2 + " \t\n"
            self.assertEqual(render(prefix + NOTES), render(NOTES))

    def test_unclosed_fences_keep_following_headings_inside_example(self):
        for marker in ("```", "~~~"):
            with self.subTest(marker=marker):
                with self.assertRaisesRegex(release.ReleaseError, "changelog section"):
                    render(marker + "markdown\n" + NOTES)
                text = NOTES.replace("### Upgrade", marker + "\n### Upgrade")
                with self.assertRaisesRegex(release.ReleaseError, "Upgrade guidance"):
                    render(text)

    def test_real_guidance_can_contain_fenced_commands_verbatim(self):
        for marker in ("```", "~~~"):
            command = marker + "shell\nupgrade --check\n" + marker
            text = NOTES.replace(
                "Review optional site settings before enabling the feature.", command
            )
            self.assertIn(command, render(text))
            self.assertEqual(render(text.replace("\n", "\r\n")), render(text))

    def test_invalid_backtick_info_and_inline_markers_do_not_start_fences(self):
        for prefix in ("```invalid`info\n", "Text with ``` inline markers.\n"):
            self.assertEqual(render(prefix + NOTES), render(NOTES))

    def test_unicode_line_separator_does_not_introduce_a_heading(self):
        text = "## [1.2.3]\nExample\u2028### Upgrade\nExample upgrade.\n### Security\nSafe.\n"
        with self.assertRaisesRegex(release.ReleaseError, "Upgrade guidance"):
            render(text)

    def test_fenced_only_notes_fail_before_any_remote_mutation(self):
        text = "## [1.2.3]\n```\n### Upgrade\nExample.\n### Security\nExample.\n```\n"
        github = StrictGitHub(contents(text))
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(
                release,
                "source_policy_snapshot",
                return_value={"data": {"release_notes": "CHANGELOG.md"}},
            ),
            self.assertRaisesRegex(release.ReleaseError, "Upgrade guidance"),
        ):
            release.publish(
                github, "v1.2.3", SOURCE, Path(directory), False, "provenance"
            )
        self.assertEqual(
            github.calls, [("GET", f"contents/CHANGELOG.md?ref={SOURCE}", None)]
        )
        self.assertEqual(github.writes, [])

    def test_exact_base_and_provenance(self):
        for tag in ("v1.2.3", "v1.2.3-rc.1", "v1.2.3-beta.8"):
            with self.subTest(tag=tag):
                body = render(tag=tag)
                self.assertIn("Preserve unavailable telemetry", body)
                self.assertNotIn("Do not publish", body)
                self.assertNotIn("Do not copy", body)
                self.assertTrue(body.endswith("Original source and validation links."))
                self.assertIn("## Build provenance", body)

    def test_crlf_notes_preserve_source_byte_verification(self):
        crlf = NOTES.replace("\n", "\r\n")
        self.assertEqual(render(crlf), render(NOTES))
        lf_contents = contents(NOTES)
        with self.assertRaisesRegex(release.ReleaseError, "size mismatch"):
            render(crlf, response_change={"size": lf_contents["size"]})
        with self.assertRaisesRegex(release.ReleaseError, "blob identity"):
            render(crlf, response_change={"sha": lf_contents["sha"]})

    def test_missing_or_incomplete_sections(self):
        for text in (
            NOTES.replace("[1.2.3]", "[2.0.0]"),
            NOTES + "\n## [1.2.3]\nDuplicate version.\n",
            NOTES.replace("### Upgrade", "### Other"),
            NOTES.replace("### Security", "### Other"),
            "## [1.2.3]\n### Upgrade\n\n### Security\nNo vulnerabilities fixed.\n",
        ):
            with self.subTest(text=text), self.assertRaises(release.ReleaseError):
                render(text)

    def test_rejects_invalid_source_content(self):
        for changes in (
            {"type": "symlink"},
            {"path": "other.md"},
            {"size": 1},
            {"sha": "b" * 40},
            {"content": "!invalid!"},
            {"content": "a" * 400_001},
        ):
            with (
                self.subTest(changes=list(changes)),
                self.assertRaises(release.ReleaseError),
            ):
                render(response_change=changes)

    def test_policy_is_opt_in(self):
        github = StrictGitHub()
        with patch.object(release, "source_policy_snapshot", return_value={"data": {}}):
            self.assertEqual(
                release.release_notes(github, "v1.2.3", SOURCE, "provenance"),
                "provenance",
            )
        self.assertEqual(github.calls, [])
        self.assertEqual(github.writes, [])

    def test_missing_notes_abort_before_mutation(self):
        github = StrictGitHub()
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(
                release,
                "release_notes",
                side_effect=release.ReleaseError("missing notes"),
            ),
            self.assertRaisesRegex(release.ReleaseError, "missing notes"),
        ):
            release.publish(
                github, "v1.2.3", SOURCE, Path(directory), False, "provenance"
            )
        self.assertEqual(github.calls, [])
        self.assertEqual(github.writes, [])

    def test_api_requires_commit_pinned_source(self):
        import re

        allowed = release.API_PATHS["GET"]
        self.assertTrue(
            any(re.fullmatch(p, f"contents/CHANGELOG.md?ref={SOURCE}") for p in allowed)
        )
        for ref in ("main", "v1.2.3", "a" * 39, "../main"):
            self.assertFalse(
                any(
                    re.fullmatch(p, f"contents/CHANGELOG.md?ref={ref}") for p in allowed
                )
            )

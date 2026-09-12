# Vendored release toolkit; change the toolkit source, then render again.
# ruff: noqa
# mypy: ignore-errors
# pylint: skip-file
# fmt: off
"""Offline release-contract tests. No test invokes GitHub or mutates a remote."""

import argparse
import base64
import importlib.util
import io
import json
import os
import subprocess
import tempfile
import unittest
import zipfile
from copy import deepcopy
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "release_control", Path(__file__).parents[2] / "scripts/release_control.py"
)
rc = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(rc)

REPO = "example/project"
SHA = "a" * 40
RC_TAG = "v1.2.3-rc.2"


def policy():
    """Return a minimal release-eligible source policy."""
    return {
        "repository": REPO,
        "mode": "release",
        "default_branch": "main",
        "validation_workflows": ["ci.yml"],
    }


def policy_response(data):
    """Represent policy bytes as a hash-verified GitHub contents response."""
    raw = rc.json_bytes(data)
    blob_sha = rc.hashlib.sha1(
        b"blob " + str(len(raw)).encode() + b"\0" + raw, usedforsecurity=False
    ).hexdigest()
    return {
        "type": "file",
        "path": rc.POLICY,
        "encoding": "base64",
        "size": len(raw),
        "sha": blob_sha,
        "content": base64.b64encode(raw).decode(),
    }


def policy_snapshot(data=None):
    """Build the immutable policy snapshot stored in a release manifest."""
    data = policy() if data is None else data
    raw = rc.json_bytes(data)
    return {
        "schema": 1,
        "path": rc.POLICY,
        "git_blob_sha": policy_response(data)["sha"],
        "sha256": rc.digest(raw),
        "data": deepcopy(data),
    }


def manifest():
    """Return a valid RC manifest with one deterministic binary payload."""
    data = b"built-once-package\x00\xff"
    return {
        "schema": 1,
        "repository": REPO,
        "version": "1.2.3",
        "channel": "rc",
        "tag": RC_TAG,
        "source_sha": SHA,
        "source_policy": policy_snapshot(),
        "workflow_path": rc.WORKFLOW,
        "run_id": 17,
        "run_attempt": 1,
        "created_at": "2026-09-12T00:00:00+00:00",
        "assets": [
            {"name": "package.tar.gz", "size": len(data), "sha256": rc.digest(data)}
        ],
    }


def run(run_id=17, completed=True):
    """Return a trusted default-branch Actions run with configurable completion."""
    return {
        "id": run_id,
        "repository": {"full_name": REPO},
        "head_repository": {"full_name": REPO},
        "head_sha": SHA,
        "head_branch": "main",
        "path": rc.WORKFLOW,
        "event": "workflow_dispatch",
        "run_attempt": 1,
        "status": "completed" if completed else "in_progress",
        "conclusion": "success" if completed else None,
    }


# Separate fields intentionally model independently mutable GitHub resources.
# pylint: disable-next=too-many-instance-attributes
class FakeGitHub:
    """Stateful fake with draft uploads, publication history and mutable API data."""

    repo = REPO

    def __init__(self):
        self.info = {"full_name": REPO, "default_branch": "main"}
        self.runs = {17: run(), 99: run(99, False)}
        self.source_policies = {SHA: policy()}
        self.policy_reads = []
        self.jobs = [
            {
                "name": "Release gate",
                "head_sha": SHA,
                "status": "completed",
                "conclusion": "success",
            }
        ]
        self.environment = {
            "name": "release",
            "protection_rules": [
                {
                    "type": "required_reviewers",
                    "reviewers": [{"type": "User", "reviewer": {"id": 1}}],
                }
            ],
        }
        self.refs = {
            RC_TAG: {
                "ref": f"refs/tags/{RC_TAG}",
                "object": {"type": "commit", "sha": SHA},
            }
        }
        self.releases = {
            10: {
                "id": 10,
                "tag_name": RC_TAG,
                "target_commitish": SHA,
                "draft": False,
                "prerelease": True,
                "html_url": f"https://github.com/{REPO}/releases/tag/{RC_TAG}",
                "updated_at": "2026-09-12T01:00:00Z",
            }
        }
        self.files = {20: b"built-once-package\x00\xff", 21: rc.json_bytes(manifest())}
        self.assets = {
            10: [
                {
                    "id": asset_id,
                    "name": name,
                    "size": len(self.files[asset_id]),
                    "state": "uploaded",
                }
                for asset_id, name in [(20, "package.tar.gz"), (21, rc.MANIFEST)]
            ]
        }
        self.artifacts = []
        self.set_evidence(self.files[21])
        self.writes = []
        self.corrupt_upload = False
        self.mutate_snapshot = False
        self.snapshot_reads = 0

    def set_evidence(self, raw):
        """Package manifest bytes into an immutable artifact with matching metadata."""
        zipped = io.BytesIO()
        with zipfile.ZipFile(zipped, "w") as archive:
            archive.writestr(rc.MANIFEST, raw)
        self.archive = zipped.getvalue()
        self.artifacts = [
            {
                "id": 40,
                "name": "release-evidence",
                "expired": False,
                "digest": f"sha256:{rc.digest(self.archive)}",
                "workflow_run": {"id": 17, "head_sha": SHA},
            }
        ]

    # Endpoint branches mirror the REST API and make unexpected calls fail closed.
    # pylint: disable-next=too-many-return-statements,too-many-branches
    def api(self, path, method="GET", body=None):
        """Model repository reads and release writes while recording every mutation."""
        if method != "GET":
            self.writes.append((path, method, deepcopy(body)))
        if path == "":
            return deepcopy(self.info)
        if path.startswith(f"contents/{rc.POLICY}?ref="):
            sha = path.split("?ref=", 1)[1]
            self.policy_reads.append(sha)
            if sha not in self.source_policies:
                raise rc.GitHubError("HTTP 404", True)
            return policy_response(self.source_policies[sha])
        if path.startswith("actions/runs/"):
            return deepcopy(self.runs[int(path.split("/")[2])])
        if path == "environments/release":
            return deepcopy(self.environment)
        if path.startswith("compare/"):
            return {"status": "identical", "merge_base_commit": {"sha": SHA}}
        if path.startswith("git/ref/tags/"):
            tag = path.removeprefix("git/ref/tags/")
            if tag not in self.refs:
                raise rc.GitHubError("HTTP 404", True)
            self.snapshot_reads += 1
            if self.mutate_snapshot and self.snapshot_reads > 1:
                self.refs[tag]["object"]["sha"] = "b" * 40
            return deepcopy(self.refs[tag])
        if path.startswith("releases/tags/"):
            tag = path.removeprefix("releases/tags/")
            matches = [
                release
                for release in self.releases.values()
                if release["tag_name"] == tag and not release["draft"]
            ]
            if not matches:
                raise rc.GitHubError("HTTP 404", True)
            return deepcopy(matches[0])
        if path == "git/refs" and method == "POST":
            tag = body["ref"].removeprefix("refs/tags/")
            if tag in self.refs:
                raise rc.GitHubError("HTTP 422 already exists")
            self.refs[tag] = {
                "ref": body["ref"],
                "object": {"type": "commit", "sha": body["sha"]},
            }
            return deepcopy(self.refs[tag])
        if path == "releases" and method == "POST":
            release_id = max(self.releases) + 1
            self.releases[release_id] = {
                **body,
                "id": release_id,
                "html_url": f"https://github.com/{REPO}/releases/tag/{body['tag_name']}",
            }
            self.assets[release_id] = []
            return deepcopy(self.releases[release_id])
        if path.startswith("releases/") and method == "PATCH":
            release_id = int(path.split("/")[1])
            self.releases[release_id].update(body)
            return deepcopy(self.releases[release_id])
        raise AssertionError(f"Unexpected API call {method} {path}")

    optional = rc.GitHub.optional

    def pages(self, path, field=None):
        """Return complete in-memory endpoint collections for the release engine."""
        assert field in (None, "jobs", "artifacts")
        if path.endswith("/jobs"):
            return deepcopy(self.jobs)
        if path.endswith("/artifacts"):
            return deepcopy(self.artifacts)
        if path.startswith("releases/") and path.endswith("/assets"):
            return deepcopy(self.assets[int(path.split("/")[1])])
        if path == "releases":
            return deepcopy(list(self.releases.values()))
        if path == "tags":
            return [{"name": tag} for tag in self.refs]
        raise AssertionError(f"Unexpected pages call {path}")

    def binary(self, path):
        """Return the exact stored release asset or immutable artifact bytes."""
        if path.startswith("releases/assets/"):
            return self.files[int(path.split("/")[2])]
        if path == "actions/artifacts/40/zip":
            return self.archive
        raise AssertionError(f"Unexpected download {path}")

    def upload(self, tag, path):
        """Model asset upload and optionally corrupt its server-side bytes."""
        self.writes.append(("upload", tag, path.name))
        release_id = next(
            key for key, value in self.releases.items() if value["tag_name"] == tag
        )
        asset_id = max(self.files) + 1
        data = path.read_bytes()
        self.files[asset_id] = b"x" * len(data) if self.corrupt_upload else data
        self.assets[release_id].append(
            {"id": asset_id, "name": path.name, "size": len(data), "state": "uploaded"}
        )


# Each public method covers an independent adversarial release condition.
# pylint: disable-next=too-many-public-methods
class ReleaseControlTests(unittest.TestCase):
    """Exercise publication sequencing and adversarial promotion inputs."""

    def setUp(self):
        # enterContext also closes the directory when a test or setup fails.
        # pylint: disable-next=consider-using-with
        self.directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.event = self.directory / "event.json"
        self.event.write_text(json.dumps({"inputs": {"channel": "stable"}}))
        environment = {
            "GITHUB_ACTIONS": "true",
            "GITHUB_REPOSITORY": REPO,
            "GITHUB_RUN_ID": "99",
            "GITHUB_RUN_ATTEMPT": "1",
            "GITHUB_REF": "refs/heads/main",
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_WORKFLOW_REF": f"{REPO}/{rc.WORKFLOW}@refs/heads/main",
            "GITHUB_EVENT_PATH": str(self.event),
        }
        self.enterContext(patch.dict(os.environ, environment))
        self.enterContext(patch.object(rc, "checked_out_sha", return_value=SHA))
        self.gh = FakeGitHub()
        self.enterContext(patch.object(rc, "GitHub", return_value=self.gh))
        self.args = argparse.Namespace(repo=REPO, rc=RC_TAG, run_id="99")

    def reject_promotion(self):
        """Require invalid promotion to fail before making any remote writes."""
        with self.assertRaises(rc.ReleaseError):
            rc.promote(self.args)
        self.assertEqual(
            self.gh.writes, [], "Rejected promotion performed remote writes"
        )

    def test_promote_copies_exact_candidate_bytes_then_publishes_latest(self):
        """Promote copies exact candidate bytes then publishes latest."""
        result = rc.promote(self.args)
        self.assertEqual(result["tag"], "v1.2.3")
        stable = next(
            release
            for release in self.gh.releases.values()
            if release["tag_name"] == "v1.2.3"
        )
        self.assertFalse(stable["draft"])
        self.assertFalse(stable["prerelease"])
        self.assertEqual(stable["make_latest"], "true")
        source = {
            asset["name"]: self.gh.files[asset["id"]] for asset in self.gh.assets[10]
        }
        published = {
            asset["name"]: self.gh.files[asset["id"]]
            for asset in self.gh.assets[stable["id"]]
        }
        self.assertEqual(source, published)
        self.assertEqual(
            self.gh.writes[0],
            ("git/refs", "POST", {"ref": "refs/tags/v1.2.3", "sha": SHA}),
        )
        self.assertEqual(self.gh.writes[-1][1], "PATCH")

    def test_untrusted_source_runs_never_write(self):
        """Untrusted source runs never write."""
        cases = [
            {"conclusion": "failure"},
            {"conclusion": "skipped"},
            {"status": "in_progress"},
            {"head_sha": "b" * 40},
            {"head_branch": "feature"},
            {"event": "pull_request_target"},
            {"event": "push"},
            {"path": ".github/workflows/unrelated.yml"},
            {"run_attempt": 2},
            {"repository": {"full_name": "fork/project"}},
            {"head_repository": {"full_name": "fork/project"}},
        ]
        for change in cases:
            with self.subTest(change=change):
                self.gh.runs[17] = {**run(), **change}
                self.reject_promotion()

    def test_gate_must_explicitly_succeed_for_source_sha(self):
        """Gate must explicitly succeed for source sha."""
        valid = deepcopy(self.gh.jobs[0])
        for jobs in (
            [],
            [valid, valid],
            [{**valid, "conclusion": "skipped"}],
            [{**valid, "conclusion": "failure"}],
            [{**valid, "status": "queued"}],
            [{**valid, "head_sha": "b" * 40}],
            [{**valid, "name": "Almost release gate"}],
        ):
            with self.subTest(jobs=jobs):
                self.gh.jobs = jobs
                self.reject_promotion()

    def test_candidate_checksum_change_never_writes(self):
        """Candidate checksum change never writes."""
        self.gh.files[20] = b"different-bytes!!!!\x00"
        self.reject_promotion()

    def test_manifest_rewrite_cannot_forge_actions_evidence(self):
        """Manifest rewrite cannot forge actions evidence."""
        forged = manifest()
        forged["assets"][0]["sha256"] = "f" * 64
        self.gh.files[21] = rc.json_bytes(forged)
        self.reject_promotion()

    def test_missing_expired_duplicated_or_corrupt_evidence_never_writes(self):
        """Missing expired duplicated or corrupt evidence never writes."""
        valid = deepcopy(self.gh.artifacts[0])
        for artifacts in (
            [],
            [{**valid, "expired": True}],
            [valid, valid],
            [{**valid, "digest": None}],
            [{**valid, "digest": "sha256:" + "a" * 64}],
            [{**valid, "workflow_run": {"id": 5, "head_sha": SHA}}],
        ):
            with self.subTest(artifacts=artifacts):
                self.gh.artifacts = artifacts
                self.reject_promotion()

    def test_reviewers_are_required(self):
        """Reviewers are required."""
        for environment in (
            {"name": "release", "protection_rules": []},
            {
                "name": "release",
                "protection_rules": [{"type": "wait_timer", "wait_timer": 1}],
            },
            {
                "name": "release",
                "protection_rules": [{"type": "required_reviewers", "reviewers": []}],
            },
        ):
            with self.subTest(environment=environment):
                self.gh.environment = environment
                self.reject_promotion()

    def test_manual_stable_context_required(self):
        """Manual stable context required."""
        for key, value in (
            ("GITHUB_ACTIONS", "false"),
            ("GITHUB_RUN_ID", "17"),
            ("GITHUB_REF", "refs/heads/feature"),
            ("GITHUB_EVENT_NAME", "push"),
            ("GITHUB_WORKFLOW_REF", "wrong"),
            ("GITHUB_RUN_ATTEMPT", "2"),
        ):
            with self.subTest(key=key), patch.dict(os.environ, {key: value}):
                self.reject_promotion()
        self.event.write_text(json.dumps({"inputs": {"channel": "rc"}}))
        self.reject_promotion()

    def test_checkout_sha_must_match_current_run(self):
        """Checkout sha must match current run."""
        with patch.object(rc, "checked_out_sha", return_value="b" * 40):
            self.reject_promotion()

    def test_stable_tag_collision_never_overwrites(self):
        """Stable tag collision never overwrites."""
        self.gh.refs["v1.2.3"] = {
            "ref": "refs/tags/v1.2.3",
            "object": {"type": "commit", "sha": SHA},
        }
        self.reject_promotion()

    def test_draft_collision_never_overwrites(self):
        """Draft collision never overwrites."""
        self.gh.releases[11] = {"id": 11, "tag_name": "v1.2.3", "draft": True}
        self.reject_promotion()

    def test_candidate_snapshot_mutation_never_writes(self):
        """Candidate snapshot mutation never writes."""
        self.gh.mutate_snapshot = True
        self.reject_promotion()

    def test_extra_release_asset_never_writes(self):
        """Extra release asset never writes."""
        self.gh.assets[10].append(
            {"name": "surprise.txt", "id": 44, "state": "uploaded", "size": 0}
        )
        self.reject_promotion()

    def test_corrupt_upload_leaves_draft_and_does_not_publish(self):
        """Corrupt upload leaves draft and does not publish."""
        self.gh.corrupt_upload = True
        with self.assertRaisesRegex(rc.ReleaseError, "Uploaded bytes differ"):
            rc.promote(self.args)
        stable = next(
            release
            for release in self.gh.releases.values()
            if release["tag_name"] == "v1.2.3"
        )
        self.assertTrue(stable["draft"])
        self.assertFalse(any(write[1] == "PATCH" for write in self.gh.writes))

    def test_candidate_channels_publish_unique_prereleases(self):
        """Candidate channels publish unique prereleases."""
        for channel in ("nightly", "beta", "rc"):
            with self.subTest(channel=channel):
                self.gh.runs[99]["status"] = "in_progress"
                self.event.write_text(json.dumps({"inputs": {"channel": channel}}))
                assets = self.directory / channel
                assets.mkdir()
                (assets / "candidate.zip").write_bytes(b"candidate-build")
                args = argparse.Namespace(
                    repo=REPO,
                    channel=channel,
                    version="2.0.0",
                    sha=SHA,
                    run_id="99",
                    run_attempt="1",
                    sequence=None,
                    assets=str(assets),
                )
                with patch.object(
                    rc, "EVIDENCE", self.directory / "evidence" / rc.MANIFEST
                ):
                    result = rc.candidate(args)
                released = next(
                    value
                    for value in self.gh.releases.values()
                    if value["tag_name"] == result["tag"]
                )
                self.assertTrue(released["prerelease"])
                self.assertFalse(released["draft"])
                self.assertEqual(released["make_latest"], "false")
                self.assertTrue(result["tag"].startswith(f"v2.0.0-{channel}."))

    def test_candidate_base_cannot_already_be_stable(self):
        """Candidate base cannot already be stable."""
        self.gh.refs["v2.0.0"] = {
            "ref": "refs/tags/v2.0.0",
            "object": {"type": "commit", "sha": SHA},
        }
        for channel in ("beta", "rc"):
            with self.subTest(channel=channel):
                self.event.write_text(json.dumps({"inputs": {"channel": channel}}))
                args = argparse.Namespace(
                    repo=REPO,
                    channel=channel,
                    version="2.0.0",
                    sha=SHA,
                    run_id="99",
                    run_attempt="1",
                    sequence=None,
                    assets=str(self.directory),
                )
                with self.assertRaisesRegex(rc.ReleaseError, "Tag already exists"):
                    rc.candidate(args)
                self.assertEqual(self.gh.writes, [])

    def test_rc_creation_rejects_source_policy_blockers_before_writes(self):
        """Rc creation rejects source policy blockers before writes."""
        self.event.write_text(json.dumps({"inputs": {"channel": "rc"}}))
        args = argparse.Namespace(
            repo=REPO,
            channel="rc",
            version="2.0.0",
            sha=SHA,
            run_id="99",
            run_attempt="1",
            sequence=None,
            assets=str(self.directory),
        )
        for field in ("release_blockers", "stable_blockers"):
            with self.subTest(field=field):
                self.gh.source_policies[SHA] = {
                    **policy(),
                    field: ["Integration coverage is missing"],
                }
                with self.assertRaisesRegex(rc.ReleaseError, field):
                    rc.candidate(args)
                self.assertEqual(self.gh.writes, [])

    def test_exploratory_candidates_record_blockers_without_qualifying_for_stable(self):
        """Exploratory candidates record blockers without qualifying for stable."""
        blocked = {**policy(), "stable_blockers": ["Hardware validation is missing"]}
        self.gh.source_policies[SHA] = blocked
        for channel in ("nightly", "beta"):
            with self.subTest(channel=channel):
                self.event.write_text(json.dumps({"inputs": {"channel": channel}}))
                assets = self.directory / channel
                assets.mkdir()
                (assets / "package.zip").write_bytes(b"experimental-build")
                args = argparse.Namespace(
                    repo=REPO,
                    channel=channel,
                    version="2.0.0",
                    sha=SHA,
                    run_id="99",
                    run_attempt="1",
                    sequence=None,
                    assets=str(assets),
                )
                with patch.object(
                    rc, "EVIDENCE", self.directory / "evidence" / rc.MANIFEST
                ):
                    result = rc.candidate(args)
                release_id = next(
                    key
                    for key, release in self.gh.releases.items()
                    if release["tag_name"] == result["tag"]
                )
                manifest_id = next(
                    asset["id"]
                    for asset in self.gh.assets[release_id]
                    if asset["name"] == rc.MANIFEST
                )
                recorded = json.loads(self.gh.files[manifest_id])
                self.assertEqual(recorded["source_policy"], policy_snapshot(blocked))
                with self.assertRaises(rc.ReleaseError):
                    rc.validate_policy_snapshot(recorded["source_policy"], REPO)

    def test_old_source_blockers_cannot_be_cleared_by_current_policy(self):
        """Old source blockers cannot be cleared by current policy."""
        newer_sha = "b" * 40
        self.gh.runs[99]["head_sha"] = newer_sha
        self.gh.source_policies[newer_sha] = policy()
        self.gh.source_policies[SHA] = {
            **policy(),
            "stable_blockers": ["Original source had no integration tests"],
        }
        with patch.object(rc, "checked_out_sha", return_value=newer_sha):
            self.reject_promotion()
        self.assertEqual(self.gh.policy_reads, [SHA])

    def test_policy_snapshot_content_must_match_the_original_commit(self):
        """Policy snapshot content must match the original commit."""
        self.gh.source_policies[SHA] = {
            **policy(),
            "validation_workflows": ["different-ci.yml"],
        }
        with self.assertRaisesRegex(rc.ReleaseError, "snapshot differs"):
            rc.promote(self.args)
        self.assertEqual(self.gh.writes, [])

    def test_source_mode_must_be_release(self):
        """Source mode must be release."""
        for mode in ("validation-only", None):
            with self.subTest(mode=mode):
                self.gh.source_policies[SHA] = {**policy(), "mode": mode}
                self.reject_promotion()

    def test_legacy_rc_without_immutable_policy_snapshot_is_rejected(self):
        """Legacy rc without immutable policy snapshot is rejected."""
        legacy = manifest()
        legacy.pop("source_policy")
        raw = rc.json_bytes(legacy)
        self.gh.files[21] = raw
        self.gh.assets[10][1]["size"] = len(raw)
        self.gh.set_evidence(raw)
        self.reject_promotion()


class TransportTests(unittest.TestCase):
    """Reject CLI argument and path injection before a subprocess can execute."""

    def test_repository_cannot_supply_host_or_options(self):
        """An explicit github.com owner/repository identity is mandatory."""
        for repo in (
            "--hostname/evil",
            "evil.com/owner/repo",
            "owner/repo --help",
            "../repo",
        ):
            with self.subTest(repo=repo), self.assertRaises(rc.ReleaseError):
                rc.GitHub(repo)

    def test_api_routes_methods_and_traversal_fail_before_execution(self):
        """Neither flags, URLs, query fields nor encoded traversal become API routes."""
        gh = rc.GitHub(REPO)
        for path, method, body in (
            ("--hostname=evil.com", "GET", None),
            ("https://evil.com/repos/owner/repo", "GET", None),
            ("../../other/repo/releases", "GET", None),
            ("releases?hostname=evil.com", "GET", None),
            (f"compare/{SHA}...main%2F..%2F..%2Freleases", "GET", None),
            ("releases", "--hostname", None),
            ("releases", "DELETE", None),
            ("", "POST", {}),
            ("releases", "GET", {"draft": False}),
        ):
            with (
                self.subTest(path=path, method=method),
                patch.object(rc.subprocess, "run") as command,
                self.assertRaises(rc.ReleaseError),
            ):
                gh.api(path, method, body)
            command.assert_not_called()

    def test_default_endpoint_and_json_body_have_fixed_argument_boundaries(self):
        """A hostile body remains stdin data, never command-line options."""
        gh = rc.GitHub(REPO)
        body = {"body": "--hostname evil.com --clobber #label"}
        with patch.object(
            rc.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, b"{}")
        ) as command:
            gh.api("")
            gh.api("releases", "POST", body)
        root_call, create_call = command.call_args_list
        self.assertEqual(
            root_call.args[0],
            [
                "gh",
                "api",
                "--hostname",
                "github.com",
                "--method",
                "GET",
                "--",
                f"repos/{REPO}",
            ],
        )
        self.assertEqual(
            create_call.args[0][-4:], ["--input", "-", "--", f"repos/{REPO}/releases"]
        )
        self.assertEqual(create_call.kwargs["input"], rc.json_bytes(body))
        self.assertNotIn(body["body"], create_call.args[0])

    def test_expected_release_routes_remain_available(self):
        """Preserve every internal REST resource, including encoded default branches."""
        gh = rc.GitHub(REPO)
        paths = [
            "",
            "tags",
            "releases",
            "releases/10",
            "releases/10/assets",
            "releases/assets/21",
            f"releases/tags/{RC_TAG}",
            f"git/ref/tags/{RC_TAG}",
            "environments/release",
            "actions/runs/17",
            "actions/runs/17/artifacts",
            "actions/runs/17/attempts/2/jobs",
            "actions/artifacts/40/zip",
            f"contents/{rc.POLICY}?ref={SHA}",
            f"compare/{SHA}...feature%2Fbranch",
        ]
        with patch.object(
            rc.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, b"{}")
        ) as command:
            for path in paths:
                gh.api(path)
            gh.api("git/refs", "POST", {"ref": f"refs/tags/{RC_TAG}", "sha": SHA})
            gh.api("releases/10", "PATCH", {"draft": False})
        self.assertEqual(command.call_count, len(paths) + 2)

    def test_upload_rejects_tags_labels_symlinks_and_unstaged_paths(self):
        """Only a regular asset in the private staging directory can reach gh."""
        gh = rc.GitHub(REPO)
        with tempfile.TemporaryDirectory(prefix="release-candidate-") as temp:
            root = Path(temp)
            asset = root / "package.tar.gz"
            asset.write_bytes(b"package")
            link = root / "link.tar.gz"
            link.symlink_to(asset)
            nested = root / "nested"
            nested.mkdir()
            (nested / asset.name).write_bytes(b"package")
            invalid = [
                ("--clobber", asset),
                ("v1.2.3-rc.01", asset),
                ("v1.2.3#label", asset),
                ("v1.2.٣-rc.2", asset),
                (RC_TAG, link),
                (RC_TAG, nested / asset.name),
                (RC_TAG, Path(asset.name)),
                (RC_TAG, root),
            ]
            for name in ("package.tar.gz#label", "--clobber", "asset*.tar.gz"):
                path = root / name
                path.write_bytes(b"package")
                invalid.append((RC_TAG, path))
            for tag, path in invalid:
                with (
                    self.subTest(tag=tag, path=path),
                    patch.object(rc.subprocess, "run") as command,
                    self.assertRaises(rc.ReleaseError),
                ):
                    gh.upload(tag, path)
                command.assert_not_called()

    def test_upload_never_overwrites_or_retries_an_existing_asset(self):
        """Keep the gh no-clobber default even when GitHub rejects an existing asset."""
        gh = rc.GitHub(REPO)
        with tempfile.TemporaryDirectory(prefix="release-promote-") as temp:
            asset = Path(temp) / "package.tar.gz"
            asset.write_bytes(b"package")
            with (
                patch.object(
                    rc.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess(
                        [], 1, stderr=b"HTTP 422 already_exists"
                    ),
                ) as command,
                self.assertRaises(rc.GitHubError),
            ):
                gh.upload("v1.2.3", asset)
            command.assert_called_once_with(
                [
                    "gh",
                    "release",
                    "upload",
                    "--repo",
                    f"github.com/{REPO}",
                    "--",
                    "v1.2.3",
                    str(asset.resolve()),
                ],
                capture_output=True,
                check=False,
            )

    def test_canonical_numeric_fields_remain_ascii(self):
        """Reject Unicode digits after adopting concise regexes with ASCII flags."""
        for value in ("1.٢.3", "01.2.3", "1.2.3\n"):
            with self.subTest(version=value), self.assertRaises(rc.ReleaseError):
                rc.version(value)
        for value in ("1١", "１２", "01", True):
            with self.subTest(identifier=value), self.assertRaises(rc.ReleaseError):
                rc.positive(value, "run ID")
        for tag in (
            "v0.2.3",
            RC_TAG,
            "v1.2.3-beta.4",
            "v1.2.3-nightly.20260912000000.17.1",
        ):
            self.assertIsNotNone(rc.TAG_RE.fullmatch(tag))
        with self.assertRaises(rc.ReleaseError):
            rc.parse_json(b"\xff", "invalid UTF-8")


class OfflineValidationTests(unittest.TestCase):
    """Check parsing, staging and REST contracts without network access."""

    def test_actions_zip_uses_default_accept_and_release_assets_use_binary_accept(self):
        """Actions zip uses default accept and release assets use binary accept."""
        gh = rc.GitHub(REPO)

        def service(command, **_kwargs):
            if command[-1].endswith("actions/artifacts/40/zip"):
                if "-H" in command:
                    return subprocess.CompletedProcess(
                        command, 1, stderr=b"HTTP 415 Unsupported Accept header"
                    )
                data = b"zip archive"
            else:
                self.assertIn("Accept: application/octet-stream", command)
                data = b"release asset"
            return subprocess.CompletedProcess(command, 0, data)

        with patch.object(rc.subprocess, "run", side_effect=service):
            self.assertEqual(gh.binary("actions/artifacts/40/zip"), b"zip archive")
            self.assertEqual(gh.binary("releases/assets/21"), b"release asset")
            with self.assertRaises(rc.ReleaseError):
                gh.binary("unexpected/endpoint")

    def test_source_policy_blob_hash_and_encoding_are_checked(self):
        """Source policy blob hash and encoding are checked."""
        gh = rc.GitHub(REPO)
        valid = policy_response(policy())
        for changes in (
            {"type": "symlink"},
            {"encoding": "none"},
            {"content": "!invalid!"},
            {"sha": "b" * 40},
            {"size": 0},
        ):
            with (
                self.subTest(changes=changes),
                patch.object(gh, "api", return_value={**valid, **changes}),
                self.assertRaises(rc.ReleaseError),
            ):
                rc.source_policy_snapshot(gh, SHA)

    def test_version_rejects_injection_and_noncanonical_values(self):
        """Version rejects injection and noncanonical values."""
        for value in (
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "v1.2.3",
            "1.2",
            "1.2.3-rc.1",
            "1.2.3\n",
            "1.2.3; touch /tmp/owned",
            "$(id)",
        ):
            with self.subTest(value=value), self.assertRaises(rc.ReleaseError):
                rc.version(value)
        self.assertEqual(rc.version("0.0.0"), "0.0.0")

    def test_manifest_rejects_beta_nightly_bad_versions_and_unsafe_names(self):
        """Manifest rejects beta nightly bad versions and unsafe names."""
        for change in (
            {"channel": "beta"},
            {"channel": "nightly"},
            {"version": "01.2.3"},
            {"repository": "other/repo"},
            {"source_sha": "short"},
            {"workflow_path": "other.yml"},
            {"run_id": 0},
            {"run_attempt": True},
            {"tag": "v1.2.3-rc.01"},
            {"assets": []},
        ):
            raw = rc.json_bytes({**manifest(), **change})
            with self.subTest(change=change), self.assertRaises(rc.ReleaseError):
                rc.validate_manifest(raw, REPO, RC_TAG)
        for name in (
            "../escape",
            "/absolute",
            "nested/file",
            "release-manifest.json",
            "asset\n.txt",
        ):
            invalid = manifest()
            invalid["assets"][0]["name"] = name
            raw = rc.json_bytes(invalid)
            with self.subTest(name=name), self.assertRaises(rc.ReleaseError):
                rc.validate_manifest(raw, REPO, RC_TAG)

    def test_duplicate_manifest_fields_and_assets_rejected(self):
        """Duplicate manifest fields and assets rejected."""
        with self.assertRaises(rc.ReleaseError):
            rc.validate_manifest(b'{"schema":1,"schema":2}', REPO, RC_TAG)
        invalid = manifest()
        invalid["assets"].append({**invalid["assets"][0], "name": "PACKAGE.TAR.GZ"})
        raw = rc.json_bytes(invalid)
        with self.assertRaises(rc.ReleaseError):
            rc.validate_manifest(raw, REPO, RC_TAG)

    def test_empty_nested_symlink_reserved_and_colliding_assets_rejected(self):
        """Empty nested symlink reserved and colliding assets rejected."""
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, output = root / "source", root / "output"
            source.mkdir()
            output.mkdir()
            with self.assertRaises(rc.ReleaseError):
                rc.stage_assets(source, output)
            (source / "directory").mkdir()
            with self.assertRaises(rc.ReleaseError):
                rc.stage_assets(source, output)
            (source / "directory").rmdir()
            (source / "link").symlink_to(root / "missing")
            with self.assertRaises(rc.ReleaseError):
                rc.stage_assets(source, output)
            (source / "link").unlink()
            (source / rc.MANIFEST).write_bytes(b"reserved")
            with self.assertRaises(rc.ReleaseError):
                rc.stage_assets(source, output)

    def test_nightly_identity_and_monotonic_rc_sequences(self):
        """Nightly identity and monotonic rc sequences."""
        gh = FakeGitHub()
        now = datetime(2026, 9, 12, 1, 2, 3, tzinfo=timezone.utc)
        self.assertEqual(
            rc.candidate_tag(gh, "nightly", "1.2.3", 17, 2, None, now),
            "v1.2.3-nightly.20260912010203.17.2",
        )
        self.assertEqual(rc.next_sequence(gh, "1.2.3", "rc"), 3)
        self.assertEqual(rc.next_sequence(gh, "1.2.3", "beta"), 1)
        with self.assertRaises(rc.ReleaseError):
            rc.candidate_tag(gh, "nightly", "1.2.3", 17, 2, 1, now)

    def test_pagination_keeps_gate_on_later_pages(self):
        """Pagination keeps gate on later pages."""
        gh = rc.GitHub(REPO)
        with patch.object(
            rc.subprocess,
            "run",
            return_value=subprocess.CompletedProcess(
                [],
                0,
                json.dumps(
                    [
                        {"jobs": [{"name": "build"}]},
                        {"jobs": [{"name": "Release gate"}]},
                    ]
                ).encode(),
            ),
        ) as command:
            jobs = gh.pages("actions/runs/17/attempts/1/jobs", "jobs")
        self.assertEqual([job["name"] for job in jobs], ["build", "Release gate"])
        self.assertIn("--paginate", command.call_args.args[0])
        self.assertIn("--slurp", command.call_args.args[0])

    def test_permission_errors_do_not_masquerade_as_absent_tags(self):
        """Permission errors do not masquerade as absent tags."""
        gh = rc.GitHub(REPO)
        with (
            patch.object(gh, "api", side_effect=rc.GitHubError("HTTP 403 forbidden")),
            self.assertRaises(rc.GitHubError),
        ):
            gh.optional("git/ref/tags/v1.2.3")


if __name__ == "__main__":
    unittest.main()

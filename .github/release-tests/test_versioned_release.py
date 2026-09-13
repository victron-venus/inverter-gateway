# Vendored release toolkit; change the toolkit source, then render again.
# ruff: noqa
# mypy: ignore-errors
# pylint: skip-file
# fmt: off
"""Offline integration contracts for allocation, real file overlays and publication."""

# Fixture byte layouts and descriptive test names are intentional; cleanup uses unittest.
# pylint: disable=missing-function-docstring,missing-class-docstring,line-too-long,consider-using-with
# Imports follow the vendored script path setup.
# pylint: disable=wrong-import-position

import argparse
import base64
import copy
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
import zipfile
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
import prepare_version
import release_state as state
import release_versioned as lifecycle
import test_release_control as legacy_tests
import version_plan as versions
import version_receipt as receipt
from test_release_control import REPO, SHA, FakeGitHub

# The legacy contract suite loads the script through importlib, while lifecycle
# modules import it normally. Bind the fake to the same exception/module identity.
rc = legacy_tests.rc
state.rc = rc
lifecycle.rc = rc


def policy(profile="final-build"):
    return {
        "repository": REPO,
        "mode": "release",
        "default_branch": "main",
        "version_file": "version",
        "validation_workflows": ["ci.yml"],
        "versioning": {
            "schema": 1,
            "promotion": profile,
            "build_number_floor": 50,
            "files": [{"path": "version", "format": "text", "value": "package"}],
        },
    }


class LedgerGitHub(FakeGitHub):  # pylint: disable=too-many-instance-attributes
    """Model GitHub's atomic file update, independently from release mutations."""

    def __init__(self):
        super().__init__()
        self.ledger = None
        self.ledger_sha = None
        self.branch = False
        self.conflict = False
        self.ledger_writes = []
        self.evidence_by_run = {}
        self.evidence_archives = {}
        self.jobs_by_run = {}

    def api(self, path, method="GET", body=None):
        if path.startswith("compare/"):
            source = path.removeprefix("compare/").split("...", 1)[0]
            return {"status": "ahead", "merge_base_commit": {"sha": source}}
        if path == state.REF_PATH:
            if not self.branch:
                raise rc.GitHubError("HTTP 404", True)
            return {"ref": "refs/heads/" + state.BRANCH, "object": {"sha": SHA}}
        if path == state.READ_PATH:
            if self.ledger is None:
                raise rc.GitHubError("HTTP 404", True)
            return {
                "type": "file",
                "encoding": "base64",
                "sha": self.ledger_sha,
                "content": base64.b64encode(rc.json_bytes(self.ledger)).decode(),
            }
        if (
            path == "git/refs"
            and method == "POST"
            and body["ref"] == "refs/heads/" + state.BRANCH
        ):
            if self.branch:
                raise rc.GitHubError("HTTP 422")
            self.branch = True
            return {"ref": body["ref"]}
        if path == state.WRITE_PATH and method == "PUT":
            if self.conflict or body.get("sha") != self.ledger_sha:
                raise rc.GitHubError("HTTP 409 conflicting ledger write")
            self.ledger = json.loads(base64.b64decode(body["content"]))
            self.ledger_sha = rc.digest(rc.json_bytes(self.ledger))[:40]
            self.ledger_writes.append(copy.deepcopy(self.ledger))
            return {"content": {"sha": self.ledger_sha}}
        return super().api(path, method, body)

    def save_completed_evidence(self, raw):
        """Freeze real manifest bytes under the actual completed source run."""
        manifest = json.loads(raw)
        run_id = manifest["run_id"]
        archive_id = 1000 + run_id
        zipped = io.BytesIO()
        with zipfile.ZipFile(zipped, "w") as archive:
            archive.writestr(rc.MANIFEST, raw)
        content = zipped.getvalue()
        self.evidence_archives[archive_id] = content
        self.evidence_by_run[run_id] = [
            {
                "id": archive_id,
                "name": "release-evidence",
                "expired": False,
                "digest": "sha256:" + rc.digest(content),
                "workflow_run": {"id": run_id, "head_sha": manifest["source_sha"]},
            }
        ]
        self.runs[run_id].update(status="completed", conclusion="success")

    def pages(self, path, field=None):
        if path.startswith("actions/runs/") and path.endswith("/jobs"):
            run_id = int(path.split("/")[2])
            if run_id in self.jobs_by_run:
                return copy.deepcopy(self.jobs_by_run[run_id])
        if path.startswith("actions/runs/") and path.endswith("/artifacts"):
            run_id = int(path.split("/")[2])
            if run_id in self.evidence_by_run:
                return copy.deepcopy(self.evidence_by_run[run_id])
        return super().pages(path, field)

    def binary(self, path):
        if path.startswith("actions/artifacts/") and path.endswith("/zip"):
            archive_id = int(path.split("/")[2])
            if archive_id in self.evidence_archives:
                return self.evidence_archives[archive_id]
        return super().binary(path)


class AllocationTests(unittest.TestCase):
    def setUp(self):
        self.gh = LedgerGitHub()
        self.policy = policy()
        self.now = datetime(2026, 9, 13, tzinfo=timezone.utc)

    def reserve(self, run_id=99, channel="beta", attempt=1):
        return state.reserve_plan(
            self.gh, self.policy, "1.2.3", channel, SHA, run_id, attempt, self.now
        )

    def test_retry_uses_same_plan_without_another_write(self):
        first = self.reserve()
        self.assertEqual(first["build_number"], 51)
        self.assertEqual(first["tag"], "v1.2.3-beta.1")
        self.assertEqual(self.reserve(attempt=2), first)
        self.assertEqual(len(self.gh.ledger_writes), 1)
        self.assertEqual(self.reserve(100)["tag"], "v1.2.3-beta.2")

    def test_retry_cannot_change_policy_or_channel(self):
        self.reserve()
        with self.assertRaises(rc.ReleaseError):
            self.reserve(channel="rc")
        self.policy["versioning"]["build_number_floor"] = 100
        with self.assertRaises(ValueError):
            self.reserve()

    def test_cas_conflict_does_not_silently_reassign_or_publish(self):
        self.gh.conflict = True
        with self.assertRaisesRegex(rc.GitHubError, "409"):
            self.reserve()
        self.assertEqual(self.gh.ledger_writes, [])
        self.assertEqual(self.gh.writes, [])

    def test_abandoned_build_reservations_are_not_reused(self):
        first = self.reserve()
        second = self.reserve(100)
        self.assertGreater(second["build_number"], first["build_number"])
        self.assertNotEqual(second["tag"], first["tag"])

    def test_delayed_build_cannot_publish_after_newer_number(self):
        first = self.reserve()
        second = self.reserve(100)
        self.gh.releases[50] = {"tag_name": second["tag"], "draft": False}
        with self.assertRaisesRegex(rc.ReleaseError, "newer build number"):
            state.verify_reservation(self.gh, first, 99)

    def test_plan_tampering_fails_reservation(self):
        plan = self.reserve()
        plan["build_number"] += 1
        with self.assertRaisesRegex(rc.ReleaseError, "reservation differs"):
            state.verify_reservation(self.gh, plan, 99)

    def test_byte_promotion_ignores_selected_rc_but_rejects_newer_published_build(self):
        self.policy = policy("promote-bytes")
        candidate = self.reserve(channel="rc")
        self.gh.releases[50] = {"tag_name": candidate["tag"], "draft": False}
        state.verify_promotion_order(self.gh, candidate)
        later = self.reserve(run_id=100)
        self.gh.releases[51] = {"tag_name": later["tag"], "draft": True}
        state.verify_promotion_order(self.gh, candidate)
        self.gh.releases[51]["draft"] = False
        with self.assertRaisesRegex(rc.ReleaseError, "newer native build"):
            state.verify_promotion_order(self.gh, candidate)
        self.assertEqual(self.gh.writes, [])

    def test_byte_promotion_requires_unique_exact_reservation(self):
        self.policy = policy("promote-bytes")
        candidate = self.reserve(channel="rc")
        self.gh.releases[50] = {"tag_name": candidate["tag"], "draft": False}
        self.gh.ledger["plans"].clear()
        with self.assertRaisesRegex(rc.ReleaseError, "unique durable"):
            state.verify_promotion_order(self.gh, candidate)

    def test_branch_creation_and_cas_requests_use_real_narrow_client(self):
        client = state.StateGitHub(REPO)
        responses = [
            subprocess.CompletedProcess(
                [], 0, b'{"ref":"refs/heads/release-version-state"}', b""
            ),
            subprocess.CompletedProcess(
                [], 0, b'{"content":{"sha":"' + SHA.encode() + b'"}}', b""
            ),
        ]
        with patch.object(state.subprocess, "run", side_effect=responses) as run:
            client.api(
                "git/refs", "POST", {"ref": "refs/heads/" + state.BRANCH, "sha": SHA}
            )
            state.write_state(client, {"schema": 1, "counter": 0, "plans": {}}, None)
        self.assertEqual(run.call_args_list[0].args[0][-1], f"repos/{REPO}/git/refs")
        self.assertEqual(
            json.loads(run.call_args_list[1].kwargs["input"])["branch"], state.BRANCH
        )
        with (
            patch.object(state.subprocess, "run") as run,
            self.assertRaisesRegex(rc.ReleaseError, "branch mismatch"),
        ):
            client.api(state.WRITE_PATH, "PUT", {"branch": "main", "content": "bad"})
        run.assert_not_called()


class ToolchainTests(unittest.TestCase):
    def test_receipt_records_actual_available_tool_version_output(self):
        def available(name):
            return "/installed/rustc" if name == "rustc" else None

        response = subprocess.CompletedProcess(
            [], 0, "rustc 1.90.0 (recorded compiler)\n"
        )
        with (
            patch.object(receipt.shutil, "which", side_effect=available),
            patch.object(receipt.subprocess, "run", return_value=response) as execute,
        ):
            result = receipt.capture_toolchain()
        self.assertEqual(result["rustc"], "rustc 1.90.0 (recorded compiler)")
        self.assertEqual(result["python"], sys.version.split()[0])
        execute.assert_called_once_with(
            ["/installed/rustc", "--version"],
            capture_output=True,
            text=True,
            check=True,
            timeout=30,
        )

    def test_installed_tool_with_empty_version_output_cannot_be_attested(self):
        with (
            patch.object(receipt.shutil, "which", return_value="/installed/compiler"),
            patch.object(
                receipt.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0, ""),
            ),
            self.assertRaisesRegex(ValueError, "Invalid toolchain version output"),
        ):
            receipt.capture_toolchain()


class ReceiptTests(unittest.TestCase):
    def setUp(self):
        self.enterContext(
            patch.object(
                receipt,
                "capture_toolchain",
                return_value={
                    "python": "3.12.8",
                    "platform": "linux",
                    "rustc": "rustc 1.90.0",
                },
            )
        )
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "version").write_text("1.2.3\n")
        self.policy = policy()
        self.plan = versions.create_plan("1.2.3", "beta", 1, SHA, self.policy, 51)
        self.plan_path = self.root / ".release-plan.json"
        self.plan_path.write_bytes(rc.json_bytes(self.plan))
        evidence = versions.sync_versions(self.root, self.policy, self.plan)
        self.inputs_path = self.root / ".release-inputs.json"
        self.inputs_path.write_bytes(
            rc.json_bytes(
                {
                    "schema": 1,
                    "source_sha": SHA,
                    "plan_sha256": versions.plan_digest(self.plan),
                    "files": evidence,
                    "effective_inputs_sha256": versions.effective_inputs_digest(
                        evidence
                    ),
                }
            )
        )
        self.assets = self.root / "assets"
        self.assets.mkdir()
        (self.assets / "app.bin").write_bytes(
            b"real package " + (self.root / "version").read_bytes()
        )

    def create(self):
        return receipt.create_receipt(
            self.plan_path,
            self.inputs_path,
            self.assets,
            self.assets / "release-inputs-linux.json",
        )

    def staged(self):
        return [
            {
                "name": path.name,
                "size": path.stat().st_size,
                "sha256": rc.digest(path.read_bytes()),
            }
            for path in self.assets.iterdir()
        ]

    def test_overlay_receipt_binds_exact_payload_and_inputs(self):
        self.create()
        verified = receipt.verify_receipts(
            self.assets, self.plan, self.staged(), self.policy
        )
        self.assertEqual(len(verified), 1)
        self.assertEqual((self.root / "version").read_text(), "1.2.3-beta.1\n")

    def test_changed_or_extra_payload_is_rejected(self):
        self.create()
        (self.assets / "app.bin").write_bytes(b"different binary")
        with self.assertRaisesRegex(ValueError, "does not match"):
            receipt.verify_receipts(self.assets, self.plan, self.staged())
        (self.assets / "uncovered.bin").write_bytes(b"missing evidence")
        with self.assertRaises(ValueError):
            receipt.verify_receipts(self.assets, self.plan, self.staged())

    def test_other_plan_or_partial_input_inventory_is_rejected(self):
        self.create()
        other = versions.create_plan("1.2.3", "beta", 2, SHA, self.policy, 52)
        with self.assertRaisesRegex(ValueError, "different release plan"):
            receipt.verify_receipts(self.assets, other, self.staged())
        altered = copy.deepcopy(self.policy)
        altered["versioning"]["files"].append({"path": "missing", "format": "text"})
        with self.assertRaisesRegex(ValueError, "every declared"):
            receipt.verify_receipts(self.assets, self.plan, self.staged(), altered)

    def test_symlink_or_empty_assets_never_get_receipt(self):
        (self.assets / "app.bin").unlink()
        with self.assertRaisesRegex(ValueError, "empty"):
            self.create()
        (self.assets / "link").symlink_to(self.root / "version")
        with self.assertRaisesRegex(ValueError, "regular file"):
            self.create()


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        self.enterContext(
            patch.object(
                receipt,
                "capture_toolchain",
                return_value={
                    "python": "3.12.8",
                    "platform": "linux",
                    "rustc": "rustc 1.90.0",
                },
            )
        )
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        old = Path.cwd()
        os.chdir(self.root)
        self.addCleanup(os.chdir, old)
        self.policy = policy()
        (self.root / ".release-policy.json").write_bytes(rc.json_bytes(self.policy))
        (self.root / "version").write_text("1.2.3\n")
        event = self.root / "event.json"
        event.write_text(
            json.dumps({"inputs": {"channel": "beta", "expected_sha": SHA}})
        )
        self.gh = LedgerGitHub()
        self.gh.source_policies[SHA] = self.policy
        self.args = argparse.Namespace(repo=REPO, assets=".release-assets")
        patches = [
            patch.object(lifecycle, "StateGitHub", return_value=self.gh),
            patch.object(lifecycle.client, "ROOT", self.root),
            patch.object(lifecycle.rc, "checked_out_sha", return_value=SHA),
            patch.dict(
                os.environ,
                {
                    "GITHUB_ACTIONS": "true",
                    "GITHUB_REPOSITORY": REPO,
                    "GITHUB_RUN_ID": "99",
                    "GITHUB_RUN_ATTEMPT": "1",
                    "GITHUB_REF": "refs/heads/main",
                    "GITHUB_WORKFLOW_REF": f"{REPO}/{rc.WORKFLOW}@refs/heads/main",
                    "GITHUB_EVENT_NAME": "workflow_dispatch",
                    "GITHUB_EVENT_PATH": str(event),
                    "PUBLICATION_ENABLED": "true",
                },
            ),
        ]
        for item in patches:
            item.start()
            self.addCleanup(item.stop)

    def start_run(self, channel, run_id, rc_tag=None):
        """Model an isolated fresh checkout and one manual workflow request."""
        self.gh.runs[run_id] = legacy_tests.run(run_id, completed=False)
        os.environ["GITHUB_RUN_ID"] = str(run_id)
        inputs = {"channel": channel, "expected_sha": SHA}
        if rc_tag:
            inputs["rc_tag"] = rc_tag
        Path(os.environ["GITHUB_EVENT_PATH"]).write_text(
            json.dumps({"inputs": inputs}), encoding="utf-8"
        )
        (self.root / "version").write_text("1.2.3\n")
        self.args.assets = str(self.root / f"assets-{run_id}")

    def build_current(self):
        plan = json.loads(lifecycle.PLAN.read_text(encoding="utf-8"))
        evidence = versions.sync_versions(self.root, self.policy, plan)
        inputs = self.root / ".release-inputs.json"
        inputs.write_bytes(
            rc.json_bytes(
                {
                    "schema": 1,
                    "source_sha": plan["source_sha"],
                    "plan_sha256": versions.plan_digest(plan),
                    "files": evidence,
                    "effective_inputs_sha256": versions.effective_inputs_digest(
                        evidence
                    ),
                }
            )
        )
        assets = Path(self.args.assets)
        assets.mkdir()
        package = {
            "version": (self.root / "version").read_text().strip(),
            "build_number": plan["build_number"],
        }
        (assets / "app.json").write_bytes(rc.json_bytes(package))
        receipt.create_receipt(
            lifecycle.PLAN, inputs, assets, assets / "release-inputs-linux.json"
        )
        return plan

    def release_run(self, channel, run_id, rc_tag=None):
        self.start_run(channel, run_id, rc_tag)
        lifecycle.prepare(self.args)
        plan = self.build_current()
        result = lifecycle.publish_versioned(self.args)
        raw = rc.EVIDENCE.read_bytes()
        self.gh.save_completed_evidence(raw)
        return result, plan, json.loads(raw)

    def declare_json_artifact(self):
        self.policy["versioning"]["artifacts"] = [
            {
                "path": "app.json",
                "format": "json",
                "field": "version",
                "value": "package",
            }
        ]
        self.gh.source_policies[SHA] = copy.deepcopy(self.policy)
        (self.root / ".release-policy.json").write_bytes(rc.json_bytes(self.policy))

    def test_prepare_build_receipt_publish_keeps_exact_identity(self):
        result = lifecycle.prepare(self.args)
        self.assertEqual(result["build"], "true")
        self.assertEqual(self.gh.writes, [])
        plan = json.loads(lifecycle.PLAN.read_text(encoding="utf-8"))
        evidence = versions.sync_versions(self.root, self.policy, plan)
        inputs = self.root / ".release-inputs.json"
        inputs.write_bytes(
            rc.json_bytes(
                {
                    "schema": 1,
                    "source_sha": SHA,
                    "plan_sha256": versions.plan_digest(plan),
                    "files": evidence,
                    "effective_inputs_sha256": versions.effective_inputs_digest(
                        evidence
                    ),
                }
            )
        )
        assets = self.root / self.args.assets
        assets.mkdir()
        (assets / "app.bin").write_bytes((self.root / "version").read_bytes())
        receipt.create_receipt(
            lifecycle.PLAN, inputs, assets, assets / "release-inputs-linux.json"
        )
        published = lifecycle.publish_versioned(self.args)
        self.assertEqual(published["tag"], plan["tag"])
        recorded = json.loads(rc.EVIDENCE.read_text())
        self.assertEqual(recorded["version_plan"], plan)
        self.assertEqual(recorded["version"], "1.2.3")
        self.assertEqual(len(recorded["assets"]), 2)

    def test_stale_request_rejects_before_reserving(self):
        Path(os.environ["GITHUB_EVENT_PATH"]).write_text(
            json.dumps({"inputs": {"channel": "beta", "expected_sha": "b" * 40}}),
            encoding="utf-8",
        )
        with self.assertRaisesRegex(rc.ReleaseError, "changed since dispatch"):
            lifecycle.prepare(self.args)
        self.assertEqual(self.gh.ledger_writes, [])

    def test_real_beta_rc_final_cycle_has_exact_versions_and_fresh_final_hashes(self):
        self.declare_json_artifact()
        beta, beta_plan, beta_manifest = self.release_run("beta", 100)
        candidate, rc_plan, rc_manifest = self.release_run("rc", 101)
        stable, final_plan, final_manifest = self.release_run(
            "stable", 102, candidate["tag"]
        )
        self.assertEqual(beta["tag"], "v1.2.3-beta.1")
        self.assertTrue(candidate["tag"].startswith("v1.2.3-rc."))
        self.assertEqual(stable["tag"], "v1.2.3")
        self.assertLess(beta_plan["build_number"], rc_plan["build_number"])
        self.assertLess(rc_plan["build_number"], final_plan["build_number"])
        self.assertEqual(final_plan["source_sha"], rc_plan["source_sha"])
        self.assertEqual(final_manifest["derived_from_rc"]["tag"], candidate["tag"])
        self.assertEqual(
            final_manifest["derived_from_rc"]["manifest_sha256"],
            rc.digest(rc.json_bytes(rc_manifest)),
        )
        self.assertEqual(final_manifest["derived_from_rc"]["run_id"], 101)

        def package_hash(manifest):
            return next(
                item["sha256"]
                for item in manifest["assets"]
                if item["name"] == "app.json"
            )

        self.assertEqual(
            len(
                {
                    package_hash(beta_manifest),
                    package_hash(rc_manifest),
                    package_hash(final_manifest),
                }
            ),
            3,
        )
        stable_release = next(
            item for item in self.gh.releases.values() if item["tag_name"] == "v1.2.3"
        )
        self.assertFalse(stable_release["prerelease"])
        self.assertFalse(stable_release["draft"])
        self.assertEqual(stable_release["make_latest"], "true")
        rc.validate_manifest(
            rc.json_bytes(final_manifest), REPO, "v1.2.3", allow_final=True
        )
        with self.assertRaisesRegex(rc.ReleaseError, "Only release candidates"):
            rc.validate_manifest(rc.json_bytes(final_manifest), REPO, "v1.2.3")

    def test_final_build_rejects_legacy_rc_before_allocating(self):
        self.start_run("stable", 100, legacy_tests.RC_TAG)
        # Model the real migration: legacy RC exists at old source SHA, while the
        # versioning policy and workflow are introduced by a later commit.
        new_sha = "b" * 40
        self.gh.source_policies[SHA] = legacy_tests.policy()
        self.gh.source_policies[new_sha] = self.policy
        self.gh.runs[100]["head_sha"] = new_sha
        self.gh.jobs_by_run[100] = [{**self.gh.jobs[0], "head_sha": new_sha}]
        event = {
            "inputs": {
                "channel": "stable",
                "expected_sha": new_sha,
                "rc_tag": legacy_tests.RC_TAG,
            }
        }
        Path(os.environ["GITHUB_EVENT_PATH"]).write_text(
            json.dumps(event), encoding="utf-8"
        )
        with (
            patch.object(rc, "checked_out_sha", return_value=new_sha),
            self.assertRaisesRegex(rc.ReleaseError, "accepted RC at current HEAD"),
        ):
            lifecycle.prepare(self.args)
        self.assertEqual(self.gh.ledger_writes, [])
        self.assertEqual(self.gh.writes, [])

    def test_final_build_rejects_later_source_or_recipe_even_with_same_policy(self):
        candidate, _, _ = self.release_run("rc", 100)
        self.start_run("stable", 101, candidate["tag"])
        new_sha = "b" * 40
        self.gh.source_policies[new_sha] = copy.deepcopy(self.policy)
        self.gh.runs[101]["head_sha"] = new_sha
        self.gh.jobs_by_run[101] = [{**self.gh.jobs[0], "head_sha": new_sha}]
        Path(os.environ["GITHUB_EVENT_PATH"]).write_text(
            json.dumps(
                {
                    "inputs": {
                        "channel": "stable",
                        "expected_sha": new_sha,
                        "rc_tag": candidate["tag"],
                    }
                }
            ),
            encoding="utf-8",
        )
        writes = copy.deepcopy(self.gh.writes)
        allocations = copy.deepcopy(self.gh.ledger_writes)
        with (
            patch.object(rc, "checked_out_sha", return_value=new_sha),
            self.assertRaisesRegex(rc.ReleaseError, "accepted RC at current HEAD"),
        ):
            lifecycle.prepare(self.args)
        self.assertEqual(self.gh.writes, writes)
        self.assertEqual(self.gh.ledger_writes, allocations)

    def test_final_rechecks_gate_and_required_reviewers_before_any_publication(self):
        candidate, _, _ = self.release_run("rc", 100)
        self.start_run("stable", 101, candidate["tag"])
        lifecycle.prepare(self.args)
        self.build_current()
        previous = copy.deepcopy(self.gh.writes)
        self.gh.jobs[0]["conclusion"] = "failure"
        with self.assertRaisesRegex(rc.ReleaseError, "Release gate"):
            lifecycle.publish_versioned(self.args)
        self.assertEqual(self.gh.writes, previous)
        self.gh.jobs[0]["conclusion"] = "success"
        self.gh.environment["protection_rules"] = []
        with self.assertRaisesRegex(rc.ReleaseError, "reviewer"):
            lifecycle.publish_versioned(self.args)
        self.assertEqual(self.gh.writes, previous)

    def test_final_toolchain_drift_requires_new_rc_before_publication(self):
        candidate, _, _ = self.release_run("rc", 100)
        self.start_run("stable", 101, candidate["tag"])
        lifecycle.prepare(self.args)
        self.build_current()
        input_path = Path(self.args.assets) / "release-inputs-linux.json"
        inputs = json.loads(input_path.read_bytes())
        inputs["toolchain"]["rustc"] = "rustc unexpected different compiler"
        input_path.write_bytes(rc.json_bytes(inputs))
        previous = copy.deepcopy(self.gh.writes)
        with self.assertRaisesRegex(
            rc.ReleaseError, "toolchain differs from accepted RC"
        ):
            lifecycle.publish_versioned(self.args)
        self.assertEqual(self.gh.writes, previous)

    def test_changed_rc_bytes_or_evidence_block_final_before_reservation(self):
        candidate, _, _ = self.release_run("rc", 100)
        self.start_run("stable", 101, candidate["tag"])
        release_id = next(
            key
            for key, value in self.gh.releases.items()
            if value["tag_name"] == candidate["tag"]
        )
        payload_id = next(
            item["id"]
            for item in self.gh.assets[release_id]
            if item["name"] == "app.json"
        )
        original = self.gh.files[payload_id]
        self.gh.files[payload_id] = b"x" * len(original)
        writes = copy.deepcopy(self.gh.writes)
        reservations = copy.deepcopy(self.gh.ledger_writes)
        with self.assertRaisesRegex(rc.ReleaseError, "checksum mismatch"):
            lifecycle.prepare(self.args)
        self.assertEqual(self.gh.writes, writes)
        self.assertEqual(self.gh.ledger_writes, reservations)
        self.gh.files[payload_id] = original
        self.gh.evidence_by_run[100][0]["expired"] = True
        with self.assertRaisesRegex(rc.ReleaseError, "evidence"):
            lifecycle.prepare(self.args)
        self.assertEqual(self.gh.writes, writes)

    def test_metadata_mismatch_fails_even_with_valid_payload_receipt(self):
        self.declare_json_artifact()
        self.start_run("beta", 100)
        lifecycle.prepare(self.args)
        self.build_current()
        assets = Path(self.args.assets)
        (assets / "release-inputs-linux.json").unlink()
        (assets / "app.json").write_text('{"version":"wrong-version"}')
        receipt.create_receipt(
            lifecycle.PLAN,
            self.root / ".release-inputs.json",
            assets,
            assets / "release-inputs-linux.json",
        )
        with self.assertRaisesRegex(ValueError, "Artifact field mismatch"):
            lifecycle.publish_versioned(self.args)
        self.assertEqual(self.gh.writes, [])

    def test_publish_tag_collision_never_relabels_already_built_payloads(self):
        self.start_run("beta", 100)
        lifecycle.prepare(self.args)
        plan = self.build_current()
        self.gh.refs[plan["tag"]] = {"object": {"type": "commit", "sha": SHA}}
        with self.assertRaisesRegex(rc.ReleaseError, "already exists"):
            lifecycle.publish_versioned(self.args)
        self.assertEqual(self.gh.writes, [])
        self.assertFalse(any(tag.endswith("beta.2") for tag in self.gh.refs))

    def test_plan_receipt_source_and_effective_digest_tampering_never_publish(self):
        self.start_run("beta", 100)
        lifecycle.prepare(self.args)
        self.build_current()
        receipt_path = Path(self.args.assets) / "release-inputs-linux.json"
        raw = receipt_path.read_bytes()
        for field, value in (
            ("plan_sha256", "0" * 64),
            ("source_sha", "b" * 40),
            ("effective_inputs_sha256", "0" * 64),
        ):
            body = json.loads(raw)
            body[field] = value
            receipt_path.write_bytes(rc.json_bytes(body))
            with self.subTest(field=field), self.assertRaises(ValueError):
                lifecycle.publish_versioned(self.args)
            self.assertEqual(self.gh.writes, [])
        receipt_path.write_bytes(raw)

    def test_old_allocator_cannot_publish_a_versioned_policy(self):
        arguments = argparse.Namespace(
            repo=REPO,
            channel="beta",
            version="1.2.3",
            sha=SHA,
            run_id=99,
            run_attempt=1,
            sequence=None,
            assets=self.args.assets,
        )
        with (
            patch.object(rc, "GitHub", return_value=self.gh),
            self.assertRaisesRegex(rc.ReleaseError, "frozen-plan publisher"),
        ):
            rc.candidate(arguments)
        self.assertEqual(self.gh.writes, [])

    def test_byte_promotion_rejects_final_build_rc(self):
        candidate, _, _ = self.release_run("rc", 100)
        self.start_run("stable", 101, candidate["tag"])
        before = copy.deepcopy(self.gh.writes)
        arguments = argparse.Namespace(repo=REPO, rc=candidate["tag"], run_id="101")
        with (
            patch.object(rc, "GitHub", return_value=self.gh),
            self.assertRaisesRegex(rc.ReleaseError, "separately validated final build"),
        ):
            rc.promote(arguments)
        self.assertEqual(self.gh.writes, before)

    def test_real_upload_boundary_accepts_versioned_private_staging_only(self):
        with tempfile.TemporaryDirectory(prefix="release-versioned-") as temp:
            path = Path(temp).resolve() / "app.bin"
            path.write_bytes(b"package")
            with patch.object(
                rc.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0, b"", b""),
            ) as run:
                rc.GitHub(REPO).upload("v1.2.3-beta.1", path)
            self.assertEqual(run.call_args.args[0][1:3], ["release", "upload"])


class PreparationTests(unittest.TestCase):
    def test_version_order_and_explicit_breaking_intent(self):
        self.assertEqual(
            prepare_version.choose_version("1.2.9", ["v1.2.9", "v1.2.10"]), "1.2.11"
        )
        self.assertEqual(
            prepare_version.choose_version("1.3.0", ["v1.2.9", "v1.3.0-beta.8"]),
            "1.3.0",
        )
        self.assertEqual(
            prepare_version.choose_version("1.3.0", [], bump="major"), "2.0.0"
        )
        with self.assertRaisesRegex(ValueError, "already has a stable"):
            prepare_version.choose_version("1.2.3", ["v1.2.3"], requested="1.2.3")


if __name__ == "__main__":
    unittest.main()

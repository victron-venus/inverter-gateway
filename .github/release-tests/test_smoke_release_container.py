"""Exact OCI smoke contract tests; Docker/network execution is always mocked."""

import copy
import gzip
import io
import json
import sys
import tarfile
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import smoke_release_container as g

SOURCE = "1" * 40
VERSION = "0.4.4"
ELF = b"\x7fELF\x02\x01\x01" + b"\x00" * 11 + b"\x3e\x00" + b"\x00" * 80


def pack(entries):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as t:
        for entry in entries:
            name, data, *extra = entry
            h = tarfile.TarInfo(name)
            h.size = len(data)
            h.mode = 0o755 if name.endswith("inverter-gateway") else 0o644
            if extra:
                h.type = extra[0]
                h.linkname = extra[1] if len(extra) > 1 else ""
                h.size = 0
            t.addfile(h, io.BytesIO(data))
    return stream.getvalue()


def oci(
    path,
    layer_entries=None,
    cfg_edit=None,
    extra_outer=None,
    duplicate_image=False,
    attestations=True,
):
    m = g.load_vendor()
    blobs = {}

    def desc(raw, media):
        d = "sha256:" + g.sha(raw)
        blobs["blobs/sha256/" + d[7:]] = raw
        return {"mediaType": media, "size": len(raw), "digest": d}

    layers = []
    diff = []
    for entries in layer_entries or [[(g.TARGET, ELF)]]:
        raw = pack(entries)
        diff.append("sha256:" + g.sha(raw))
        layers.append(
            desc(gzip.compress(raw), "application/vnd.oci.image.layer.v1.tar+gzip")
        )
    cfg = {
        "architecture": "amd64",
        "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": diff},
        "config": {
            "Entrypoint": g.ENTRYPOINT,
            "User": "appuser",
            "Env": ["PATH=/usr/local/bin:/usr/bin:/bin"],
            "Labels": {
                "org.opencontainers.image.version": VERSION,
                "org.opencontainers.image.revision": SOURCE,
            },
        },
    }
    if cfg_edit:
        cfg_edit(cfg)
    c = desc(g.canonical(cfg), m.CONFIG)
    image = {"schemaVersion": 2, "mediaType": m.MANIFEST, "config": c, "layers": layers}
    d = desc(g.canonical(image), m.MANIFEST)
    d["platform"] = {"os": "linux", "architecture": "amd64"}
    manifests = [d]
    if duplicate_image:
        manifests.append(copy.deepcopy(d))
    if attestations:
        ats = []
        for pred in ["https://spdx.dev/Document", "https://slsa.dev/provenance/v0.2"]:
            s = {
                "_type": "https://in-toto.io/Statement/v0.1",
                "predicateType": pred,
                "subject": [{"name": "image", "digest": {"sha256": d["digest"][7:]}}],
                "predicate": {},
            }
            ats.append(desc(g.canonical(s), m.IN_TOTO))
        ac = desc(g.canonical({"architecture": "unknown", "os": "unknown"}), m.CONFIG)
        ad = desc(
            g.canonical(
                {
                    "schemaVersion": 2,
                    "mediaType": m.MANIFEST,
                    "config": ac,
                    "layers": ats,
                }
            ),
            m.MANIFEST,
        )
        ad.update(
            platform={"architecture": "unknown", "os": "unknown"},
            annotations={
                m.REFERENCE_TYPE: "attestation-manifest",
                "vnd.docker.reference.digest": d["digest"],
            },
        )
        manifests.append(ad)
    outer = [
        ("oci-layout", b'{"imageLayoutVersion":"1.0.0"}'),
        ("index.json", g.canonical({"schemaVersion": 2, "manifests": manifests})),
        *blobs.items(),
    ]
    if extra_outer:
        outer.extend(extra_outer)
    path.write_bytes(pack(outer))


def plan():
    return {
        "schema_version": 1,
        "base_version": VERSION,
        "version": VERSION + "-rc.1",
        "channel": "rc",
        "sequence": 1,
        "tag": "v" + VERSION + "-rc.1",
        "source_sha": SOURCE,
        "build_number": None,
        "policy_sha256": "a" * 64,
        "promotion": "promote-bytes",
    }


class SmokeTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.archive = self.root / "inverter-gateway-container.oci.tar"
        self.output = self.root / g.RECEIPT
        self.diagnostics = self.root / "diagnostics"
        self.diagnostics.mkdir()
        oci(self.archive)
        self.report = g.inspect_oci(self.archive, VERSION, SOURCE)
        self.identity = {
            "repository": g.REPO,
            "source_sha": SOURCE,
            "run_id": 123,
            "run_attempt": 1,
        }

    def test_rewrap_preserves_config_and_layers(self):
        target = self.root / "docker.tar"
        before = self.archive.read_bytes()
        result = g.docker_archive(
            self.archive,
            target,
            VERSION,
            SOURCE,
            "inverter-gateway-release-smoke:" + "b" * 32,
        )
        self.assertEqual(result, self.report)
        self.assertEqual(self.archive.read_bytes(), before)
        with tarfile.open(target) as t:
            m = json.load(t.extractfile("manifest.json"))[0]
            config = t.extractfile(m["Config"]).read()
            self.assertEqual("sha256:" + g.sha(config), result["config_digest"])
            self.assertEqual(
                ["sha256:" + g.sha(t.extractfile(n).read()) for n in m["Layers"]],
                result["rootfs_diff_ids"],
            )
            self.assertEqual(len(t.getmembers()), len(m["Layers"]) + 2)

    def test_rewrap_rejects_changed_version_source_platform_and_diffids(self):
        edits = [
            lambda c: c["config"]["Labels"].update(
                {"org.opencontainers.image.version": "9.9.9"}
            ),
            lambda c: c["config"]["Labels"].update(
                {"org.opencontainers.image.revision": "2" * 40}
            ),
            lambda c: c.update(architecture="arm64"),
            lambda c: c["rootfs"].update(diff_ids=["sha256:" + "0" * 64]),
        ]
        for edit in edits:
            with self.subTest(edit=edit):
                oci(self.archive, cfg_edit=edit)
                with self.assertRaises(ValueError):
                    g.docker_archive(
                        self.archive,
                        self.root / "new.tar",
                        VERSION,
                        SOURCE,
                        "inverter-gateway-release-smoke:" + "b" * 32,
                    )

    def test_expanded_bound(self):
        with patch.object(g, "MAX_ROOTFS", 100), self.assertRaises(ValueError):
            g.docker_archive(
                self.archive,
                self.root / "new.tar",
                VERSION,
                SOURCE,
                "inverter-gateway-release-smoke:" + "b" * 32,
            )

    def test_second_archive_open_keeps_expansion_and_outer_bounds(self):
        old = (
            g.oci_validator.MAX_LAYER_BYTES,
            g.oci_validator.MAX_ROOTFS_BYTES,
            g.oci_validator.identity,
        )
        for limit_name in ("MAX_ROOTFS", "MAX_OUTER"):
            with (
                self.subTest(limit=limit_name),
                patch.object(g, "inspect_oci", return_value=self.report),
                patch.object(g, limit_name, 1),
                self.assertRaises(ValueError),
            ):
                g.docker_archive(
                    self.archive,
                    self.root / "second.tar",
                    VERSION,
                    SOURCE,
                    "inverter-gateway-release-smoke:" + "b" * 32,
                )
            self.assertEqual(
                (
                    g.oci_validator.MAX_LAYER_BYTES,
                    g.oci_validator.MAX_ROOTFS_BYTES,
                    g.oci_validator.identity,
                ),
                old,
            )
        self.assertFalse((self.root / "second.tar").exists())

    def test_missing_attestations_and_unsafe_members(self):
        for options in [
            {"attestations": False},
            {"extra_outer": [("../evil", b"")]},
            {"duplicate_image": True},
            {"layer_entries": [[(g.TARGET, b"", tarfile.SYMTYPE, "/elsewhere")]]},
        ]:
            with self.subTest(options=options):
                oci(self.archive, **options)
                with self.assertRaises(ValueError):
                    g.inspect_oci(self.archive, VERSION, SOURCE)

    def fake_runtime(
        self,
        *,
        preexisting=False,
        state_edit=None,
        identity_edit=None,
        image_edit=None,
        output=None,
        fail_load=False,
        fail_create=False,
        late_create=False,
        cleanup_tamper=False,
        wrong_tag=False,
    ):
        image = {
            "Id": self.report["config_digest"],
            "Os": "linux",
            "Architecture": "amd64",
            "RootFS": {"Type": "layers", "Layers": self.report["rootfs_diff_ids"]},
            "Config": self.report["runtime_config"],
        }
        if image_edit:
            image_edit(image)
        calls = []
        exists = loaded = False
        name = tag = None

        def fake(command, **_):
            nonlocal exists, loaded, name, tag
            calls.append(command)
            c = command[3:]
            if c[:2] == ["image", "ls"]:
                if "--filter" in c:
                    return (
                        (
                            ("sha256:" + "f" * 64 if wrong_tag else image["Id"]) + "\n"
                        ).encode()
                        if loaded
                        else b""
                    )
                return (image["Id"] + "\n").encode() if preexisting or loaded else b""
            if c[:2] == ["image", "load"]:
                intent = g.read_json(self.diagnostics / "intent.json")
                tag = intent["temporary_tag"]
                self.assertEqual(
                    intent["image_config_digest"], self.report["config_digest"]
                )
                loaded = True
                if fail_load:
                    raise ValueError("load timeout")
                return b"loaded\n"
            if c[:2] == ["image", "inspect"]:
                return g.canonical({**image, "RepoTags": [tag] if loaded else []})
            if c[0] == "create":
                intent = g.read_json(self.diagnostics / "intent.json")
                name = c[c.index("--name") + 1]
                self.assertEqual(name, intent["name"])
                self.assertTrue("--entrypoint" not in c)
                self.assertEqual(c[-2:], [self.report["config_digest"], "--version"])
                for required in [
                    "--network=none",
                    "--read-only",
                    "--cap-drop=ALL",
                    "--pull=never",
                ]:
                    self.assertIn(required, c)
                exists = not fail_create or late_create
                if fail_create:
                    raise ValueError("create timeout")
                return b"c" * 64 + b"\n"
            if c[0] == "start":
                return (
                    output
                    if output is not None
                    else ("inverter-gateway " + VERSION + "\n").encode()
                )
            if c[:2] == ["container", "ls"]:
                return b"c" * 64 + b"\n" if exists else b""
            if c[:2] == ["container", "inspect"]:
                item = {
                    "Id": "c" * 64,
                    "Name": "/" + name,
                    "Image": self.report["config_digest"],
                    "Config": {
                        "Labels": {
                            "org.victron.release-probe": g.version_plan.plan_digest(
                                plan()
                            )
                        }
                    },
                    "State": {
                        "Status": "exited",
                        "Running": False,
                        "ExitCode": 0,
                        "OOMKilled": False,
                        "Error": "",
                    },
                }
                if state_edit:
                    item["State"].update(state_edit)
                if identity_edit:
                    item.update(identity_edit)
                if cleanup_tamper and c[2] == name:
                    item["Config"]["Labels"] = {}
                return g.canonical(item)
            if c[:2] == ["container", "rm"]:
                exists = False
                return b"removed\n"
            if c[:2] == ["image", "rm"]:
                self.assertEqual(c, ["image", "rm", "--no-prune", tag])
                loaded = False
                return b"removed tag\n"
            raise AssertionError(command)

        with patch.object(g, "bounded_command", side_effect=fake):
            value = g.runtime(
                self.archive,
                self.report,
                plan(),
                self.identity,
                self.output,
                self.diagnostics,
                ["docker", "--host", "unix:///test.sock"],
            )
        return value, calls

    def test_new_image_full_flow(self):
        value, calls = self.fake_runtime()
        self.assertEqual(value["status"], "PASSED")
        self.assertEqual(value["oci"], self.report)
        self.assertEqual(value["version"], VERSION)
        self.assertEqual(value["tag"], "v0.4.4-rc.1")
        self.assertEqual(value["container_cleanup"]["status"], "removed")
        self.assertEqual(value["image_cleanup"]["status"], "removed_owned_tag")
        self.assertTrue(self.output.exists())
        self.assertTrue(any(c[3:5] == ["image", "load"] for c in calls))

    def test_preexisting_image_is_never_loaded_tagged_or_removed(self):
        value, calls = self.fake_runtime(preexisting=True)
        self.assertEqual(value["image_origin"], "preexisting_exact_image")
        self.assertEqual(value["image_cleanup"]["status"], "preexisting_preserved")
        self.assertFalse(
            any(
                c[3:5] in (["image", "load"], ["image", "rm"], ["image", "tag"])
                for c in calls
            )
        )

    def test_bad_container_result_never_writes_success(self):
        for edit in [
            {"ExitCode": 1},
            {"ExitCode": False},
            {"Running": True},
            {"OOMKilled": True},
            {"Error": "error"},
            {"Status": "created"},
        ]:
            with self.subTest(edit=edit):
                for p in self.diagnostics.iterdir():
                    p.unlink()
                with self.assertRaises(ValueError):
                    self.fake_runtime(state_edit=edit)
                self.assertFalse(self.output.exists())
                self.assertEqual(
                    g.read_json(self.diagnostics / "outcome.json")["status"],
                    "FAILED_OR_UNKNOWN",
                )

    def test_wrong_container_id_or_image(self):
        for edit in [{"Id": "d" * 64}, {"Image": "sha256:" + "e" * 64}]:
            with self.subTest(edit=edit):
                for p in self.diagnostics.iterdir():
                    p.unlink()
                with self.assertRaises(ValueError):
                    self.fake_runtime(identity_edit=edit)
                self.assertFalse(self.output.exists())

    def test_wrong_version_output(self):
        with self.assertRaises(ValueError):
            self.fake_runtime(output=b"inverter-gateway 0.0.0\n")
        self.assertFalse(self.output.exists())

    def test_wrong_imported_rootfs(self):
        with self.assertRaises(ValueError):
            self.fake_runtime(
                image_edit=lambda image: image["RootFS"].update(Layers=[])
            )
        self.assertFalse(self.output.exists())

    def test_load_timeout_retains_intent_and_fails_even_after_cleanup(self):
        with self.assertRaisesRegex(ValueError, "load timeout"):
            self.fake_runtime(fail_load=True)
        self.assertTrue((self.diagnostics / "intent.json").exists())
        self.assertTrue(
            g.read_json(self.diagnostics / "outcome.json")["image_cleanup"][
                "unknown_import_outcome"
            ]
        )
        self.assertFalse(self.output.exists())

    def test_create_timeout_retains_intent(self):
        with self.assertRaisesRegex(ValueError, "create timeout"):
            self.fake_runtime(fail_create=True, late_create=True)
        self.assertEqual(
            g.read_json(self.diagnostics / "outcome.json")["container_cleanup"][
                "status"
            ],
            "removed",
        )
        self.assertFalse(self.output.exists())

    def test_foreign_container_is_not_removed(self):
        with self.assertRaisesRegex(ValueError, "owner label"):
            self.fake_runtime(cleanup_tamper=True)
        self.assertEqual(
            g.read_json(self.diagnostics / "outcome.json")["container_cleanup"][
                "status"
            ],
            "failed",
        )
        self.assertFalse(self.output.exists())

    def test_foreign_tag_is_not_removed(self):
        with self.assertRaises(ValueError):
            self.fake_runtime(wrong_tag=True)
        self.assertFalse(self.output.exists())

    def test_non_native_host_stops_before_commands(self):
        for host, arch in [("Darwin", "arm64"), ("Linux", "aarch64")]:
            with (
                patch.object(g.platform, "system", return_value=host),
                patch.object(g.platform, "machine", return_value=arch),
                patch.object(
                    g,
                    "bounded_command",
                    side_effect=AssertionError("no command allowed"),
                ),
                self.assertRaisesRegex(ValueError, "Native Linux"),
            ):
                g.smoke(Namespace(root=self.root))

    def test_existing_durable_intent_blocks_new_mutations(self):
        g.write_new(self.diagnostics / "intent.json", {"old": True})
        with self.assertRaises(FileExistsError):
            self.fake_runtime()

    def test_frozen_source_plan_and_native_daemon_before_mutation(self):
        import os
        import shutil

        policy = json.loads((ROOT / ".release-policy.json").read_text())
        (self.root / ".release-policy.json").write_text(json.dumps(policy))
        for declaration in policy["versioning"]["files"]:
            target = self.root / declaration["path"]
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / declaration["path"], target)
        frozen = g.version_plan.create_plan(VERSION, "rc", 1, SOURCE, policy)
        plan_path = self.root / ".release-plan.json"
        plan_path.write_text(json.dumps(frozen))
        env = {
            "GITHUB_SHA": SOURCE,
            "GITHUB_REPOSITORY": g.REPO,
            "GITHUB_RUN_ID": "123",
            "GITHUB_RUN_ATTEMPT": "1",
        }

        daemon_arch = "arm64"

        def fake(command, **_):
            if command[0] == "git":
                return (SOURCE + "\n").encode()
            if command[3] == "version":
                return g.canonical({"Os": "linux", "Arch": daemon_arch})
            raise AssertionError("Docker mutation must not run")

        original_stat = g.Path.stat

        def socket_stat(path, *args, **kwargs):
            if str(path) == "/var/run/docker.sock":
                return type("S", (), {"st_mode": 0o140000})()
            return original_stat(path, *args, **kwargs)

        with (
            patch.dict(os.environ, env),
            patch.object(g.platform, "system", return_value="Linux"),
            patch.object(g.platform, "machine", return_value="x86_64"),
            patch.object(g.Path, "stat", socket_stat),
            patch.object(g, "bounded_command", side_effect=fake),
            self.assertRaisesRegex(ValueError, "Docker daemon"),
        ):
            g.smoke(Namespace(root=self.root))
        self.assertFalse(self.output.exists())
        self.assertFalse(
            (self.root / "container-smoke-diagnostics/intent.json").exists()
        )
        daemon_arch = "amd64"
        frozen["source_sha"] = "2" * 40
        plan_path.write_text(json.dumps(frozen))
        with (
            patch.dict(os.environ, env),
            patch.object(g.platform, "system", return_value="Linux"),
            patch.object(g.platform, "machine", return_value="x86_64"),
            patch.object(g, "bounded_command", side_effect=fake),
            patch.object(g.Path, "stat", socket_stat),
            self.assertRaisesRegex(ValueError, "source SHA"),
        ):
            g.smoke(Namespace(root=self.root))

    def test_workflow_keeps_smoke_before_staging_and_diagnostics_separate(self):
        import yaml

        workflow = yaml.safe_load(
            (ROOT / ".github/workflows/release-build.yml").read_text()
        )
        steps = workflow["jobs"]["container"]["steps"]
        names = [s.get("name") for s in steps]
        buildx = next(
            i
            for i, step in enumerate(steps)
            if step.get("uses", "").startswith("docker/setup-buildx-action@")
        )
        self.assertLess(names.index("Require native Linux amd64 Docker"), buildx)
        self.assertLess(
            names.index("Build OCI image archive"),
            names.index("Smoke exact OCI archive natively"),
        )
        self.assertLess(
            names.index("Smoke exact OCI archive natively"),
            names.index("Verify and stage versioned release assets"),
        )
        self.assertFalse(any("setup-qemu" in s.get("uses", "") for s in steps))
        self.assertFalse(
            steps[names.index("Smoke exact OCI archive natively")].get(
                "continue-on-error", False
            )
        )
        stage = steps[names.index("Verify and stage versioned release assets")]
        self.assertEqual(stage["env"]["RELEASE_ASSET_PATTERN_1"], g.RECEIPT)
        diagnostics = steps[names.index("Preserve OCI smoke diagnostics")]
        self.assertEqual(diagnostics["if"], "${{ always() }}")
        self.assertFalse(diagnostics["with"]["name"].startswith("release-assets"))
        policy = json.loads((ROOT / ".release-policy.json").read_text())
        declaration = [
            x for x in policy["versioning"]["artifacts"] if x["path"] == g.RECEIPT
        ]
        self.assertEqual(
            declaration,
            [
                {
                    "path": g.RECEIPT,
                    "format": "json",
                    "value": "package",
                    "field": "version",
                }
            ],
        )


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Smoke the exact staged OCI image natively, without rebuilding or publishing.

This project-owned adapter consumes the frozen version plan and maintained OCI
validator. Only its unique temporary image tag and container may be removed.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import platform
import re
import selectors
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid
from contextlib import ExitStack, contextmanager
from pathlib import Path

import merge_oci_archives as oci_validator
import version_plan

REPO = "victron-venus/inverter-gateway"
RECEIPT = "inverter-gateway-container-smoke.json"
MAX_JSON = 4 * 1024**2
MAX_NATIVE = 128 * 1024**2
MAX_OUTER = 512 * 1024**2
MAX_LAYER = 512 * 1024**2
MAX_ROOTFS = 1024**3
ENTRYPOINT = ["/sbin/tini", "--", "/usr/local/bin/inverter-gateway"]
TARGET = "usr/local/bin/inverter-gateway"
ANCESTORS = {"usr", "usr/local", "usr/local/bin"}


def load_vendor():
    return oci_validator


@contextmanager
def validated_archive(path, version, source):
    """Tighten process-local limits without changing other validator consumers."""
    old = (
        oci_validator.MAX_LAYER_BYTES,
        oci_validator.MAX_ROOTFS_BYTES,
        oci_validator.identity,
    )
    try:
        oci_validator.MAX_LAYER_BYTES = MAX_LAYER
        oci_validator.MAX_ROOTFS_BYTES = MAX_ROOTFS
        oci_validator.identity = bounded_oci_identity
        with ExitStack() as stack:
            yield oci_validator.Archive(path, "linux/amd64", version, source, stack)
    finally:
        (
            oci_validator.MAX_LAYER_BYTES,
            oci_validator.MAX_ROOTFS_BYTES,
            oci_validator.identity,
        ) = old


def need(ok, message):
    if not ok:
        raise ValueError(message)


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def canonical(value):
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), allow_nan=False
    ).encode()


def object_json(raw):
    need(len(raw) <= MAX_JSON, "JSON too large")

    def pairs(items):
        out = {}
        for key, val in items:
            need(key not in out, "Duplicate JSON key")
            out[key] = val
        return out

    def bad(value):
        raise ValueError("Non-finite JSON number: " + value)

    data = json.loads(raw, object_pairs_hook=pairs, parse_constant=bad)
    need(isinstance(data, dict), "JSON object required")
    return data


def regular(path, limit):
    """Open without following the final symlink and check before allocating."""
    path = Path(path)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        initial = os.fstat(stream.fileno())
        need(
            stat.S_ISREG(initial.st_mode) and initial.st_size <= limit,
            "Invalid/oversized regular file",
        )
        raw = stream.read(limit + 1)
        end = os.fstat(stream.fileno())
        need(
            (initial.st_size, initial.st_mtime_ns, initial.st_ctime_ns)
            == (end.st_size, end.st_mtime_ns, end.st_ctime_ns),
            "Input changed during read",
        )
        need(len(raw) == initial.st_size, "Truncated/oversized file")
        return raw


def bounded_oci_identity(stream):
    """Bound the exact outer descriptor and tar-member streams the reader hashes."""
    try:
        descriptor = stream.fileno()
    except (AttributeError, OSError):
        descriptor = None  # The validator also hashes bounded ExFileObject blobs.
    initial = os.fstat(descriptor) if descriptor is not None else None
    if initial is not None:
        need(
            stat.S_ISREG(initial.st_mode) and initial.st_size <= MAX_OUTER,
            "OCI outer archive exceeds bound",
        )
    digest, size = hashlib.sha256(), 0
    for chunk in iter(lambda: stream.read(1024**2), b""):
        size += len(chunk)
        need(size <= MAX_OUTER, "OCI archive/blob grew beyond bound")
        digest.update(chunk)
    if initial is not None:
        end = os.fstat(descriptor)
        need(
            (initial.st_size, initial.st_mtime_ns, initial.st_ctime_ns)
            == (end.st_size, end.st_mtime_ns, end.st_ctime_ns)
            and size == initial.st_size,
            "OCI outer archive changed",
        )
    return {"sha256": digest.hexdigest(), "size": size}


def file_identity(path, limit=MAX_OUTER):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        s = os.fstat(stream.fileno())
        need(
            stat.S_ISREG(s.st_mode) and s.st_size <= limit, "Invalid/oversized archive"
        )
        h = hashlib.sha256()
        size = 0
        for block in iter(lambda stream=stream: stream.read(1024**2), b""):
            size += len(block)
            need(size <= limit, "Archive grew beyond limit")
            h.update(block)
        e = os.fstat(stream.fileno())
        need(
            (s.st_size, s.st_mtime_ns, s.st_ctime_ns)
            == (e.st_size, e.st_mtime_ns, e.st_ctime_ns),
            "Archive changed",
        )
        need(size == s.st_size, "Archive size changed")
        return {"size": size, "sha256": h.hexdigest()}


def read_json(path):
    return object_json(regular(path, MAX_JSON))


def write_new(path, data):
    raw = json.dumps(data, indent=2, allow_nan=False).encode() + b"\n"
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as out:
        out.write(raw)
        out.flush()
        os.fsync(out.fileno())
    directory_fd = os.open(
        Path(path).parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
    )
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
    return sha(raw)


def elf_header(raw):
    need(
        len(raw) >= 64
        and raw[:7] == b"\x7fELF\x02\x01\x01"
        and raw[18:20] == b"\x3e\x00",
        "Expected Linux amd64 ELF64",
    )


def safe_member(entry):
    name = entry.name
    while name.startswith("./"):
        name = name[2:]
    name = name.rstrip("/") if entry.isdir() else name
    need(
        name
        and not name.startswith("/")
        and "\\" not in name
        and "\0" not in name
        and all(p not in ("", ".", "..") for p in name.split("/")),
        "Unsafe tar member",
    )
    need(not entry.issparse(), "Sparse tar entry rejected")
    return name


def final_binary(archive, image_manifest):
    """Inspect target only, without extracting. Reject target/ancestor aliases."""
    binary = None
    total = 0
    for layer in image_manifest["layers"]:
        replacement = None
        removes_lower = False
        with ExitStack() as stack:
            stream = stack.enter_context(
                archive.tar.extractfile(
                    archive.blobs["blobs/sha256/" + layer["digest"][7:]]
                )
            )
            if layer["mediaType"].endswith("+gzip"):
                stream = stack.enter_context(gzip.GzipFile(fileobj=stream))
            elif layer["mediaType"].endswith("+zstd"):
                mod = load_vendor()
                need(mod.zstd is not None, "zstd requires Python 3.14+")
                stream = stack.enter_context(
                    mod.zstd.ZstdFile(
                        stream,
                        options={mod.zstd.DecompressionParameter.window_log_max: 27},
                    )
                )
            tar = stack.enter_context(tarfile.open(fileobj=stream, mode="r|"))
            seen = set()
            for count, e in enumerate(tar):
                need(count < 100000, "Too many layer entries")
                if e.isdir() and e.name in (".", "./"):
                    continue
                n = safe_member(e)
                need(n not in seen, "Duplicate layer member")
                seen.add(n)
                total += max(e.size, 0)
                need(total <= MAX_ROOTFS, "Layer member bytes exceed bound")
                parent, _, leaf = n.rpartition("/")
                if leaf.startswith(".wh."):
                    need(e.isreg() and e.size == 0, "Invalid whiteout entry")
                    removed = (parent + "/" if parent else "") + leaf[4:]
                    if leaf == ".wh..wh..opq":
                        removed = parent
                    if removed == TARGET or removed in ANCESTORS or removed == "":
                        removes_lower = True
                    continue
                if n in ANCESTORS and not e.isdir():
                    raise ValueError("Target ancestor is not a real directory")
                if n == TARGET:
                    need(
                        e.isreg() and 64 <= e.size <= MAX_NATIVE,
                        "Target binary must be bounded regular ELF",
                    )
                    data = tar.extractfile(e).read(MAX_NATIVE + 1)
                    need(len(data) == e.size, "Truncated target binary")
                    elf_header(data)
                    replacement = {"size": e.size, "sha256": sha(data), "mode": e.mode}
        # Whiteouts remove lower layers, independent of ordering within this layer.
        binary = (
            replacement
            if replacement is not None
            else (None if removes_lower else binary)
        )
    need(
        binary is not None and binary["mode"] & 0o111,
        "Executable target missing after layer whiteouts",
    )
    return binary


def inspect_oci(path, version, source):
    before = file_identity(path)
    mod = load_vendor()
    with validated_archive(path, version, source) as archive:
        need(archive.archive_identity == before, "OCI archive identity mismatch")
        image = archive.images[0]
        descriptor = image["descriptor"]
        manifest = archive.read_json("blobs/sha256/" + descriptor["digest"][7:])
        config = archive.read_json("blobs/sha256/" + image["config_sha256"])
        runtime = config["config"]
        need(
            runtime.get("Entrypoint") == ENTRYPOINT
            and runtime.get("User") == "appuser",
            "Unexpected OCI runtime entrypoint/user",
        )
        need(
            runtime.get("Cmd") in (None, [])
            and not runtime.get("Volumes")
            and not runtime.get("Healthcheck"),
            "Unexpected OCI command/volume/healthcheck",
        )
        predicates = {p for a in archive.attestations for p in a["predicate_types"]}
        need(
            "https://spdx.dev/Document" in predicates
            and bool(predicates & mod.PROVENANCE_TYPES),
            "Missing release SBOM/provenance",
        )
        binary = final_binary(archive, manifest)
        archive.unchanged()
        result = {
            "archive": Path(path).name,
            **before,
            "manifest_digest": descriptor["digest"],
            "config_digest": "sha256:" + image["config_sha256"],
            "platform": "linux/amd64",
            "rootfs_diff_ids": config["rootfs"]["diff_ids"],
            "layers": manifest["layers"],
            "runtime_config": runtime,
            "binary": binary,
            "attestation_predicates": sorted(predicates),
        }
    need(file_identity(path) == before, "OCI archive changed during inspection")
    return result


def bounded_command(argv, timeout=20, limit=1024**2):
    """Bound wall time and output. Never invoke a shell."""
    env = {
        k: v
        for k, v in os.environ.items()
        if k.lower() not in {"http_proxy", "https_proxy", "all_proxy", "no_proxy"}
        and not k.startswith("DOCKER_")
    }
    env.pop("PYTHONOPTIMIZE", None)
    p = subprocess.Popen(
        argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env
    )
    output = bytearray()
    deadline = time.monotonic() + timeout
    try:
        with selectors.DefaultSelector() as sel:
            sel.register(p.stdout, selectors.EVENT_READ)
            while sel.get_map():
                remaining = deadline - time.monotonic()
                need(remaining > 0, "Probe command timeout")
                for key, _ in sel.select(min(remaining, 0.2)):
                    chunk = os.read(key.fileobj.fileno(), 65536)
                    if not chunk:
                        sel.unregister(key.fileobj)
                        continue
                    output.extend(chunk)
                    need(len(output) <= limit, "Probe output limit exceeded")
            p.wait(timeout=max(0.01, deadline - time.monotonic()))
        need(p.returncode == 0, "Probe command failed: " + str(p.returncode))
        return bytes(output)
    finally:
        if p.poll() is None:
            p.kill()
            p.wait()
        p.stdout.close()


def validate_runtime_image(image, oci):
    need(
        image.get("Id") == oci["config_digest"]
        and image.get("Os") == "linux"
        and image.get("Architecture") == "amd64",
        "Imported image identity/platform mismatch",
    )
    need(
        image.get("RootFS", {}).get("Type") == "layers"
        and image["RootFS"].get("Layers") == oci["rootfs_diff_ids"],
        "Imported root filesystem mismatch",
    )
    config = image.get("Config", {})
    # Config ID binds all config bytes; explicit checks guard accidental field assumptions.
    for key in (
        "Entrypoint",
        "User",
        "Env",
        "Labels",
        "Cmd",
        "WorkingDir",
        "Volumes",
        "Healthcheck",
    ):
        need(
            config.get(key) == oci["runtime_config"].get(key),
            "Imported runtime config mismatch: " + key,
        )


def container_identity(item, name, container_id, image_id, plan_hash):
    observed = item.get("Id")
    need(
        isinstance(observed, str) and re.fullmatch(r"[0-9a-f]{64}", observed),
        "Invalid observed container ID",
    )
    need(
        container_id is None or observed == container_id,
        "Container ID differs from create result",
    )
    need(item.get("Name") == "/" + name, "Container name mismatch")
    need(
        item.get("Image") == image_id,
        "Container Image differs from reviewed config digest",
    )
    need(
        item.get("Config", {}).get("Labels", {}).get("org.victron.release-probe")
        == plan_hash,
        "Container owner label mismatch",
    )
    return observed


def container_result(item, name, container_id, image_id, plan_hash):
    container_identity(item, name, container_id, image_id, plan_hash)
    state = item.get("State", {})
    need(
        state.get("Status") == "exited" and state.get("Running") is False,
        "Container has not exited",
    )
    need(
        type(state.get("ExitCode")) is int and state["ExitCode"] == 0,
        "Container exit code is not integer zero",
    )
    need(state.get("OOMKilled") is False, "Container was OOM killed or outcome unknown")
    need(state.get("Error") == "", "Container runtime error or unknown outcome")
    return {
        "Id": item["Id"],
        "Image": item["Image"],
        "Name": item["Name"],
        "State": {
            key: state[key]
            for key in ("Status", "Running", "ExitCode", "OOMKilled", "Error")
        },
    }


def cleanup_container(cli, name, known_id, image_id, plan_hash):
    lookup = cli + [
        "container",
        "ls",
        "--all",
        "--quiet",
        "--no-trunc",
        "--filter",
        "name=^/" + name + "$",
    ]
    found = bounded_command(lookup).decode("ascii").split()
    need(len(found) <= 1, "Ambiguous probe cleanup lookup")
    if not found:
        return {
            "status": "absent_at_check",
            "name": name,
            "container_id": known_id,
            "verified_absent_at_check": True,
            "unknown_create_outcome": known_id is None,
        }
    item = object_json(
        bounded_command(cli + ["container", "inspect", name, "--format", "{{json .}}"])
    )
    observed = container_identity(item, name, known_id, image_id, plan_hash)
    need(found == [observed], "Cleanup lookup/inspection ID mismatch")
    bounded_command(cli + ["container", "rm", "-f", observed])
    need(not bounded_command(lookup).strip(), "Probe container remains after removal")
    return {
        "status": "removed",
        "name": name,
        "container_id": observed,
        "verified_absent_at_check": True,
        "unknown_create_outcome": False,
    }


def docker_archive(archive_path, destination, version, source, tag):
    """Rewrap identical config/layer bytes for docker load; never extract paths."""
    need(
        re.fullmatch(r"inverter-gateway-release-smoke:[0-9a-f]{32}", tag),
        "Invalid temporary tag",
    )
    report = inspect_oci(archive_path, version, source)
    with validated_archive(archive_path, version, source) as archive:
        need(
            archive.archive_identity == {k: report[k] for k in ("size", "sha256")},
            "OCI changed before rewrap",
        )
        image = archive.images[0]
        manifest = archive.read_json(
            "blobs/sha256/" + image["descriptor"]["digest"][7:]
        )
        config_name = image["config_sha256"] + ".json"
        layer_names = []
        expanded = 0
        with (
            open(destination, "xb") as output,
            tarfile.open(fileobj=output, mode="w|") as tar,
        ):
            config_member = archive.blobs["blobs/sha256/" + image["config_sha256"]]
            with archive.tar.extractfile(config_member) as stream:
                oci_validator.add_file(tar, config_name, config_member.size, stream)
            for index, layer in enumerate(manifest["layers"]):
                # A bounded anonymous file permits tar headers without retaining
                # whole layers in memory. No archive member path reaches the host.
                with ExitStack() as layer_stack:
                    stream = layer_stack.enter_context(
                        archive.tar.extractfile(
                            archive.blobs["blobs/sha256/" + layer["digest"][7:]]
                        )
                    )
                    if layer["mediaType"].endswith("+gzip"):
                        stream = layer_stack.enter_context(
                            gzip.GzipFile(fileobj=stream)
                        )
                    elif layer["mediaType"].endswith("+zstd"):
                        need(
                            oci_validator.zstd is not None,
                            "Zstandard requires Python 3.14+",
                        )
                        stream = layer_stack.enter_context(
                            oci_validator.zstd.ZstdFile(
                                stream,
                                options={
                                    oci_validator.zstd.DecompressionParameter.window_log_max: 27
                                },
                            )
                        )
                    temporary = layer_stack.enter_context(
                        tempfile.TemporaryFile(dir=Path(destination).parent)
                    )
                    digest = hashlib.sha256()
                    size = 0
                    for block in iter(lambda stream=stream: stream.read(1024**2), b""):
                        size += len(block)
                        expanded += len(block)
                        need(
                            size <= MAX_LAYER and expanded <= MAX_ROOTFS,
                            "Expanded layer limit",
                        )
                        digest.update(block)
                        temporary.write(block)
                    need(
                        "sha256:" + digest.hexdigest()
                        == report["rootfs_diff_ids"][index],
                        "Rewrapped layer differs from OCI diffID",
                    )
                    name = f"{index}-{digest.hexdigest()}/layer.tar"
                    layer_names.append(name)
                    temporary.seek(0)
                    oci_validator.add_file(tar, name, size, temporary)
            metadata = canonical(
                [{"Config": config_name, "RepoTags": [tag], "Layers": layer_names}]
            )
            oci_validator.add_file(
                tar, "manifest.json", len(metadata), io.BytesIO(metadata)
            )
        archive.unchanged()
    need(
        file_identity(archive_path) == {k: report[k] for k in ("size", "sha256")},
        "OCI changed after rewrap",
    )
    return report


def image_ids(cli, tag=None):
    command = cli + ["image", "ls", "--all", "--quiet", "--no-trunc"]
    if tag is not None:
        command += ["--filter", "reference=" + tag]
    ids = bounded_command(command).decode("ascii").split()
    need(
        all(re.fullmatch(r"sha256:[0-9a-f]{64}", item) for item in ids),
        "Invalid image inventory",
    )
    return set(ids)


def cleanup_image(cli, tag, image_id, preexisting, imported):
    if preexisting:
        need(image_id in image_ids(cli), "Pre-existing image disappeared")
        need(not image_ids(cli, tag), "Unexpected temporary tag on pre-existing image")
        return {
            "status": "preexisting_preserved",
            "temporary_tag": tag,
            "image_id": image_id,
        }
    found = image_ids(cli, tag)
    if not found:
        return {
            "status": "absent_at_check",
            "temporary_tag": tag,
            "image_id": image_id,
            "verified_absent_at_check": True,
            "unknown_import_outcome": not imported,
        }
    need(found == {image_id}, "Temporary tag points to a different image")
    item = object_json(
        bounded_command(cli + ["image", "inspect", tag, "--format", "{{json .}}"])
    )
    need(
        item.get("Id") == image_id and tag in item.get("RepoTags", []),
        "Temporary image ownership mismatch",
    )
    # Delete this unique tag only. Never force, prune, or delete a shared ID.
    bounded_command(cli + ["image", "rm", "--no-prune", tag])
    need(not image_ids(cli, tag), "Temporary image tag remains")
    return {
        "status": "removed_owned_tag",
        "temporary_tag": tag,
        "image_id": image_id,
        "verified_absent_at_check": True,
        "unknown_import_outcome": not imported,
    }


def runtime(archive_path, oci, plan, identity, output, diagnostics, cli):
    plan_hash = version_plan.plan_digest(plan)
    suffix = uuid.uuid4().hex
    tag = "inverter-gateway-release-smoke:" + suffix
    name = "inverter-gateway-release-smoke-" + suffix
    image_id = oci["config_digest"]
    preexisting = image_id in image_ids(cli)
    need(not image_ids(cli, tag), "Temporary image tag already exists")
    intent = {
        "schema": 1,
        "status": "INTENT_BEFORE_IMPORT_OR_CREATE",
        **identity,
        "plan_sha256": plan_hash,
        "name": name,
        "temporary_tag": tag,
        "owner_label": {"org.victron.release-probe": plan_hash},
        "image_config_digest": image_id,
        "preexisting_image": preexisting,
        "archive": {k: oci[k] for k in ("archive", "size", "sha256")},
    }
    intent_hash = write_new(diagnostics / "intent.json", intent)
    container_id = None
    imported = False
    failure = None
    container_cleanup = image_cleanup = checked_state = result = None
    phase = "import"
    try:
        if not preexisting:
            with tempfile.TemporaryDirectory(prefix="gateway-oci-rewrap-") as scratch:
                wrapped = Path(scratch) / "image.tar"
                need(
                    docker_archive(
                        archive_path,
                        wrapped,
                        oci["runtime_config"]["Labels"][
                            "org.opencontainers.image.version"
                        ],
                        plan["source_sha"],
                        tag,
                    )
                    == oci,
                    "OCI changed at import",
                )
                bounded_command(
                    cli + ["image", "load", "--input", str(wrapped)], timeout=120
                )
                imported = True
                need(
                    image_ids(cli, tag) == {image_id}, "Imported temporary tag mismatch"
                )
        image = object_json(
            bounded_command(
                cli + ["image", "inspect", image_id, "--format", "{{json .}}"]
            )
        )
        validate_runtime_image(image, oci)
        phase = "create"
        value = (
            bounded_command(
                cli
                + [
                    "create",
                    "--name",
                    name,
                    "--label",
                    "org.victron.release-probe=" + plan_hash,
                    "--pull=never",
                    "--network=none",
                    "--read-only",
                    "--cap-drop=ALL",
                    "--security-opt=no-new-privileges",
                    "--pids-limit=64",
                    "--memory=256m",
                    "--cpus=1",
                    image_id,
                    "--version",
                ]
            )
            .strip()
            .decode("ascii")
        )
        need(re.fullmatch(r"[0-9a-f]{64}", value), "Invalid created container ID")
        container_id = value
        phase = "start"
        result = bounded_command(cli + ["start", "--attach", container_id])
        phase = "container-result"
        item = object_json(
            bounded_command(
                cli + ["container", "inspect", container_id, "--format", "{{json .}}"]
            )
        )
        checked_state = container_result(item, name, container_id, image_id, plan_hash)
        version = version_plan.projections(plan)["package"]
        need(
            result == ("inverter-gateway " + version + "\n").encode(),
            "Unexpected --version output",
        )
        validate_runtime_image(
            object_json(
                bounded_command(
                    cli + ["image", "inspect", image_id, "--format", "{{json .}}"]
                )
            ),
            oci,
        )
        need(
            file_identity(archive_path) == {k: oci[k] for k in ("size", "sha256")},
            "Original OCI changed during smoke",
        )
        phase = "verified"
    except BaseException as error:  # noqa: BLE001 - cleanup and persist even on interruption; re-raise below
        failure = error
    finally:
        try:
            container_cleanup = cleanup_container(
                cli, name, container_id, image_id, plan_hash
            )
        except BaseException as error:  # noqa: BLE001 - cleanup and persist even on interruption; re-raise below
            container_cleanup = {"status": "failed", "error": str(error)[:1000]}
            if failure is None:
                failure = error
        try:
            image_cleanup = cleanup_image(cli, tag, image_id, preexisting, imported)
        except BaseException as error:  # noqa: BLE001 - cleanup and persist even on interruption; re-raise below
            image_cleanup = {"status": "failed", "error": str(error)[:1000]}
            if failure is None:
                failure = error
        outcome = {
            "schema": 1,
            "status": "FAILED_OR_UNKNOWN" if failure else "RUNTIME_AND_CLEANUP_PASSED",
            "phase": phase,
            "intent_sha256": intent_hash,
            "container_result": checked_state,
            "container_cleanup": container_cleanup,
            "image_cleanup": image_cleanup,
            "error": None
            if failure is None
            else type(failure).__name__ + ": " + str(failure)[:1000],
        }
        outcome_hash = write_new(diagnostics / "outcome.json", outcome)
    if failure is not None:
        raise failure
    need(
        container_cleanup.get("verified_absent_at_check") is True
        and container_cleanup.get("unknown_create_outcome") is False,
        "Container cleanup not confirmed",
    )
    need(
        preexisting or image_cleanup.get("unknown_import_outcome") is False,
        "Image import cleanup not confirmed",
    )
    receipt = {
        "schema": 1,
        "status": "PASSED",
        **identity,
        "version": version_plan.projections(plan)["package"],
        "tag": plan["tag"],
        "channel": plan["channel"],
        "plan_sha256": plan_hash,
        "native_platform": "linux/amd64",
        "image_origin": "preexisting_exact_image"
        if preexisting
        else "imported_exact_archive",
        "oci": oci,
        "stdout": result.decode(),
        "container_result": checked_state,
        "container_cleanup": container_cleanup,
        "image_cleanup": image_cleanup,
        "intent_sha256": intent_hash,
        "outcome_sha256": outcome_hash,
        "limits": "Exact native loader/entrypoint/version smoke. No MQTT, TLS, equipment or deployment acceptance.",
    }
    write_new(output, receipt)
    return receipt


def native_docker():
    """Read-only prerequisite, also run before Buildx touches the daemon."""
    need(
        platform.system() == "Linux" and platform.machine() in ("x86_64", "amd64"),
        "Native Linux amd64 host required",
    )
    socket = Path("/var/run/docker.sock")
    need(
        not socket.is_symlink() and stat.S_ISSOCK(socket.stat().st_mode),
        "Local Unix Docker socket required",
    )
    cli = ["docker", "--host", "unix:///var/run/docker.sock"]
    server = object_json(
        bounded_command(cli + ["version", "--format", "{{json .Server}}"])
    )
    need(
        server.get("Os") == "linux" and server.get("Arch") == "amd64",
        "Native Linux amd64 Docker daemon required",
    )
    return cli


def smoke(args):
    cli = native_docker()
    root = Path(args.root).resolve()
    source = os.environ.get("GITHUB_SHA", "")
    need(re.fullmatch(r"[0-9a-f]{40}", source), "Missing exact GitHub source")
    need(os.environ.get("GITHUB_REPOSITORY") == REPO, "Unexpected GitHub repository")
    run, attempt = (
        os.environ.get("GITHUB_RUN_ID", ""),
        os.environ.get("GITHUB_RUN_ATTEMPT", ""),
    )
    need(
        re.fullmatch(r"[1-9][0-9]*", run) and re.fullmatch(r"[1-9][0-9]*", attempt),
        "Invalid run/attempt",
    )
    need(
        bounded_command(["git", "-C", str(root), "rev-parse", "HEAD"]).decode().strip()
        == source,
        "Checkout source mismatch",
    )
    policy = read_json(root / ".release-policy.json")
    need(policy.get("repository") == REPO, "Policy repository mismatch")
    plan = version_plan.validate_plan(
        read_json(root / ".release-plan.json"), policy, source
    )
    version_plan.sync_versions(root, policy, plan, check=True)
    need(
        plan["channel"] in ("nightly", "beta", "rc"),
        "Stable must promote without rebuilding",
    )
    archive = root / "inverter-gateway-container.oci.tar"
    output = root / RECEIPT
    diagnostics = root / "container-smoke-diagnostics"
    need(
        not output.exists() and not output.is_symlink(), "Smoke receipt already exists"
    )
    diagnostics.mkdir(mode=0o700)
    oci = inspect_oci(archive, version_plan.projections(plan)["package"], source)
    identity = {
        "repository": REPO,
        "source_sha": source,
        "run_id": int(run),
        "run_attempt": int(attempt),
    }
    return runtime(archive, oci, plan, identity, output, diagnostics, cli)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path("."))
    parser.add_argument(
        "--check-native",
        action="store_true",
        help="Read-only host/daemon prerequisite before Buildx",
    )
    args = parser.parse_args()
    try:
        native_docker() if args.check_native else smoke(args)
    except (
        ValueError,
        OSError,
        KeyError,
        TypeError,
        subprocess.SubprocessError,
        tarfile.TarError,
    ) as error:
        print("Exact OCI smoke rejected: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

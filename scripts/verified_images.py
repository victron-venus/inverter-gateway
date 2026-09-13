#!/usr/bin/env python3
# Vendored release toolkit; change the toolkit source, then render again.
# ruff: noqa
# mypy: ignore-errors
# pylint: skip-file
# fmt: off
"""Resolve stable container payloads to verified registry digests; never deploy."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

from publish_verified import verified_assets, verified_publication_config
from release_control import GitHub, ReleaseError, digest, require

ROOT = Path(__file__).resolve().parents[1]


def resolve(policy: dict, tag: str, directory: Path) -> dict:
    """Match registry manifests to approved OCI archives and return digest references."""
    manifest = verified_assets(GitHub(policy["repository"]), tag, directory)
    images = {}
    mapping = verified_publication_config(policy, manifest, "container_assets")
    require(mapping, "No declared container archives")
    for name, image in mapping.items():
        require(
            Path(name).name == name and (directory / name).is_file(),
            "Approved container archive is missing",
        )
        local = subprocess.run(
            ["skopeo", "inspect", "--raw", "oci-archive:" + str(directory / name)],
            capture_output=True,
            check=True,
        ).stdout
        remote = subprocess.run(
            [
                "skopeo",
                "inspect",
                "--raw",
                "docker://" + image + ":" + manifest["version"],
            ],
            capture_output=True,
            check=True,
        ).stdout
        require(
            local and digest(local) == digest(remote),
            f"Registry bytes differ from approved RC: {image}",
        )
        images[image] = image + "@sha256:" + digest(local)
    return {
        "repository": policy["repository"],
        "tag": tag,
        "source_sha": manifest["source_sha"],
        "images": images,
    }


def main():
    """Write verified image references for a later explicit deployment."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        policy = json.loads((ROOT / ".release-policy.json").read_text())
        with tempfile.TemporaryDirectory(prefix="verified-image-digests-") as temporary:
            result = resolve(policy, args.tag, Path(temporary))
        args.output.write_text(json.dumps(result, indent=2) + "\n")
        print(
            f"Verified {len(result['images'])} immutable image digests in {args.output}"
        )
        return 0
    except (ReleaseError, OSError, ValueError, subprocess.CalledProcessError) as exc:
        print(f"verified-images: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

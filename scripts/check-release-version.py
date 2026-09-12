#!/usr/bin/env python3
"""Verify that release metadata matches committed application version sources."""

import json
import re
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[1]
version, channel = sys.argv[1:]
if not re.fullmatch(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", version
):
    raise SystemExit("Release version must be a base X.Y.Z version")
if channel not in {"nightly", "beta", "rc"}:
    raise SystemExit(
        "Build only nightly/beta/rc; stable must promote verified RC artifacts"
    )
policy = json.loads((root / ".release-policy.json").read_text())
paths = [policy["version_file"], *policy.get("version_companions", [])]
for name in paths:
    path = root / name
    if path.suffix == ".json":
        actual = json.loads(path.read_text())["version"]
    elif path.suffix == ".toml":
        match = re.search(r'^version\s*=\s*"([^\"]+)"', path.read_text(), re.MULTILINE)
        if not match:
            raise SystemExit(f"No explicit version found in {name}")
        actual = match.group(1)
    elif path.suffix in {".gradle", ".pbxproj"}:
        pattern = (
            r'versionName\s+"([^\"]+)"'
            if path.suffix == ".gradle"
            else r"MARKETING_VERSION\s*=\s*([^;]+);"
        )
        values = [
            value.strip().strip('"') for value in re.findall(pattern, path.read_text())
        ]
        if not values or any(value != version for value in values):
            raise SystemExit(
                f"{name}: mobile version values {values} must all match {version}"
            )
        actual = version
    else:
        actual = path.read_text().strip().removeprefix("v")
    if actual != version:
        raise SystemExit(
            f"{name}: committed {actual} does not match requested {version}; bump all version files in a PR"
        )
print(f"Validated {version} for {channel}: {', '.join(paths)}")

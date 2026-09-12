#!/usr/bin/env python3
# Vendored release toolkit; change the toolkit source, then render again.
# ruff: noqa
# mypy: ignore-errors
# pylint: skip-file
# fmt: off
"""Publish candidates and promote checked RCs inside the guarded Actions workflow."""

# Keep the audited engine self-contained when vendored into application repos.
# pylint: disable=too-many-lines

from __future__ import annotations

import argparse
import base64
import hashlib
import io
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
import zipfile
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import quote, unquote

WORKFLOW = ".github/workflows/release-pipeline.yml"
MANIFEST = "release-manifest.json"
POLICY = ".release-policy.json"
EVIDENCE = Path(".release-evidence") / MANIFEST
VERSION_PATTERN = r"(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)"
VERSION_RE = re.compile(VERSION_PATTERN, re.ASCII)
TAG_RE = re.compile(
    "v"
    + VERSION_PATTERN
    + r"(?:-(?:beta|rc)\.[1-9]\d*|-nightly\.\d{14}\.[1-9]\d*\.[1-9]\d*)?",
    re.ASCII,
)
API_PATHS = {
    "GET": (
        r"(?:|tags|releases|environments/release)",
        r"releases/(?:[1-9]\d*(?:/assets)?|assets/[1-9]\d*)",
        "releases/tags/" + TAG_RE.pattern,
        "git/ref/tags/" + TAG_RE.pattern,
        r"actions/runs/[1-9]\d*(?:/artifacts|/attempts/[1-9]\d*/jobs)?",
        r"actions/artifacts/[1-9]\d*/zip",
        r"contents/\.release-policy\.json\?ref=[0-9a-f]{40}",
        r"compare/[0-9a-f]{40}\.\.\.(?:[A-Za-z0-9_.~-]|%[0-9A-F]{2})+",
    ),
    "POST": (r"(?:git/refs|releases)",),
    "PATCH": (r"releases/[1-9]\d*",),
}
SHA_RE = re.compile(r"[0-9a-f]{40}\Z")
NAME_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+-]{0,199}\Z")
REPO_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*\Z")


class ReleaseError(RuntimeError):
    """A release precondition failed. No retry should overwrite existing data."""


class GitHubError(ReleaseError):
    """Report a GitHub failure while distinguishing a genuine missing resource."""

    def __init__(self, message: str, not_found: bool = False):
        super().__init__(message)
        self.not_found = not_found


def require(condition: object, message: str) -> None:
    """Abort before the next operation when a release precondition fails."""
    if not condition:
        raise ReleaseError(message)


def version(value: str) -> str:
    """Validate and return a canonical numeric X.Y.Z base version."""
    require(
        bool(VERSION_RE.fullmatch(value)),
        "Version must be strict X.Y.Z without leading zeros",
    )
    return value


def positive(value: object, name: str) -> int:
    """Parse a positive integer identifier without accepting booleans."""
    require(
        isinstance(value, (str, int)) and not isinstance(value, bool), f"Invalid {name}"
    )
    require(bool(re.fullmatch(r"[1-9]\d*", str(value), re.ASCII)), f"Invalid {name}")
    return int(value)


def digest(data: bytes) -> str:
    """Return the SHA-256 hex digest of the exact supplied bytes."""
    return hashlib.sha256(data).hexdigest()


def json_bytes(value: object) -> bytes:
    """Encode deterministic UTF-8 JSON for manifests and request bodies."""
    return (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()


def parse_json(data: bytes, label: str) -> object:
    """Decode JSON while rejecting duplicate fields and invalid encoding."""

    def no_duplicates(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, f"Duplicate JSON field in {label}: {key}")
            result[key] = value
        return result

    try:
        return json.loads(data, object_pairs_hook=no_duplicates)
    except ValueError as exc:
        raise ReleaseError(f"Invalid JSON in {label}") from exc


class GitHub:
    """Access one repository through argument-safe gh subprocess calls."""

    def __init__(self, repository: str):
        require(bool(REPO_RE.fullmatch(repository)), "Repository must be OWNER/REPO")
        self.repo = repository
        self.base = f"repos/{repository}"

    @staticmethod
    def response(result: subprocess.CompletedProcess) -> bytes:
        """Translate a completed fixed-form command without retrying failed writes."""
        if result.returncode:
            message = result.stderr.decode(errors="replace").strip()
            raise GitHubError(message, "HTTP 404" in message)
        return result.stdout

    def request(self, path: str, method="GET", body=None, mode="json") -> bytes:
        """Call only allowed REST routes with fixed flags and one positional endpoint."""
        require(method in API_PATHS, "Unsupported API method")
        require(
            any(re.fullmatch(pattern, path, re.ASCII) for pattern in API_PATHS[method]),
            "Unsupported API endpoint",
        )
        require(
            all(part not in {".", ".."} for part in unquote(path).split("/")),
            "API endpoint cannot traverse repository paths",
        )
        require(
            (method == "GET" and body is None)
            or (method in {"POST", "PATCH"} and isinstance(body, dict)),
            "API body does not match its method",
        )
        modes = {
            "json": [],
            "pages": ["--paginate", "--slurp"],
            "asset": ["-H", "Accept: application/octet-stream"],
        }
        require(
            mode in modes and (mode == "json" or method == "GET"), "Invalid API mode"
        )
        endpoint = f"{self.base}/{path}".rstrip("/")
        if mode == "pages":
            endpoint += ("&" if "?" in endpoint else "?") + "per_page=100"
        method_args = {
            "GET": ["--method", "GET"],
            "POST": ["--method", "POST"],
            "PATCH": ["--method", "PATCH"],
        }[method]
        return self.response(
            subprocess.run(
                [
                    "gh",
                    "api",
                    "--hostname",
                    "github.com",
                    *method_args,
                    *modes[mode],
                    *(["--input", "-"] if body is not None else []),
                    "--",
                    endpoint,
                ],
                input=json_bytes(body) if body is not None else None,
                capture_output=True,
                check=False,
            )
        )

    def api(self, path: str, method: str = "GET", body: dict | None = None):
        """Read or mutate a permitted repository endpoint with optional JSON input."""
        raw = self.request(path, method, body)
        return parse_json(raw, path) if raw.strip() else None

    def optional(self, path: str):
        """Return None only when GitHub explicitly reports a missing resource."""
        try:
            return self.api(path)
        except GitHubError as exc:
            if exc.not_found:
                return None
            raise

    def pages(self, path: str, field: str | None = None) -> list:
        """Fetch and flatten every page so later jobs or artifacts are not skipped."""
        raw = self.request(path, mode="pages")
        pages = parse_json(raw, path)
        require(isinstance(pages, list), f"Invalid pagination response: {path}")
        records = []
        for page in pages:
            values = page.get(field) if field and isinstance(page, dict) else page
            require(isinstance(values, list), f"Invalid paginated records: {path}")
            records.extend(values)
        return records

    def binary(self, path: str) -> bytes:
        """Download release bytes or an Actions ZIP using its required media type."""
        # The Actions redirect rejects octet-stream with HTTP 415; gh follows it.
        if re.fullmatch(r"actions/artifacts/[1-9]\d*/zip", path, re.ASCII):
            return self.request(path)
        require(
            re.fullmatch(r"releases/assets/[1-9]\d*", path, re.ASCII),
            "Unsupported binary download endpoint",
        )
        return self.request(path, mode="asset")

    def upload(self, tag: str, path: Path) -> None:
        """Upload a canonical tag's staged regular file, with no overwrite option."""
        require(TAG_RE.fullmatch(tag), "Unsupported release upload tag")
        require(
            path.is_absolute() and NAME_RE.fullmatch(path.name), "Unsafe upload path"
        )
        require(
            path.is_file() and not path.is_symlink(), "Upload must be a regular file"
        )
        parent = path.parent.resolve(strict=True)
        require(
            parent.parent == Path(tempfile.gettempdir()).resolve()
            and parent.name.startswith(("release-candidate-", "release-promote-"))
            and "#" not in str(path),
            "Upload must come from the private release staging directory",
        )
        self.response(
            subprocess.run(
                [
                    "gh",
                    "release",
                    "upload",
                    "--repo",
                    f"github.com/{self.repo}",
                    "--",
                    tag,
                    str(parent / path.name),
                ],
                capture_output=True,
                check=False,
            )
        )


def repository_info(gh: GitHub) -> dict:
    """Read the repository identity and authoritative default branch."""
    info = gh.api("")
    require(isinstance(info, dict), "Invalid repository response")
    require(
        info.get("full_name", "").lower() == gh.repo.lower(),
        "Repository identity mismatch",
    )
    require(bool(info.get("default_branch")), "Repository has no default branch")
    return info


def require_release_policy(policy: object, repo: str, qualified: bool) -> None:
    """Check policy identity, release mode, and required eligibility blockers."""
    require(isinstance(policy, dict), "Source release policy must be an object")
    require(
        isinstance(policy.get("repository"), str)
        and policy["repository"].lower() == repo.lower(),
        "Source policy repository mismatch",
    )
    require(policy.get("mode") == "release", "Source policy mode must be release")
    for field in ("release_blockers", "stable_blockers"):
        blockers = policy.get(field, [])
        require(
            isinstance(blockers, list)
            and all(isinstance(item, str) and item.strip() for item in blockers),
            f"Source policy {field} must be a list of nonempty strings",
        )
        if qualified:
            require(
                not blockers,
                f"Source policy has unresolved {field}; create a new RC after resolving them",
            )


def source_policy_snapshot(gh: GitHub, sha: str) -> dict:
    """Bind eligibility to the policy at the exact candidate source commit."""
    require(SHA_RE.fullmatch(sha), "Invalid source policy commit SHA")
    response = gh.api(f"contents/{POLICY}?ref={sha}")
    require(
        isinstance(response, dict)
        and response.get("type") == "file"
        and response.get("path") == POLICY
        and response.get("encoding") == "base64",
        "Source commit must contain a regular release policy file",
    )
    encoded = response.get("content")
    require(
        isinstance(encoded, str) and len(encoded) <= 400_000,
        "Invalid or oversized source policy content",
    )
    try:
        raw = base64.b64decode("".join(encoded.split()), validate=True)
    except ValueError as exc:
        raise ReleaseError("Invalid source policy base64 content") from exc
    require(
        len(raw) <= 250_000 and response.get("size") == len(raw),
        "Source policy size mismatch",
    )
    # Git's object identifier is SHA-1; payload security uses SHA-256 separately.
    blob_sha = hashlib.sha1(
        b"blob " + str(len(raw)).encode() + b"\0" + raw, usedforsecurity=False
    ).hexdigest()
    require(response.get("sha") == blob_sha, "Source policy Git blob identity mismatch")
    data = parse_json(raw, "source release policy")
    require_release_policy(data, gh.repo, qualified=False)
    return {
        "schema": 1,
        "path": POLICY,
        "git_blob_sha": blob_sha,
        "sha256": digest(raw),
        "data": data,
    }


def validate_policy_snapshot(snapshot: object, repo: str) -> None:
    """Check the recorded policy schema, hashes, and stable eligibility."""
    require(
        isinstance(snapshot, dict)
        # JSON booleans must not qualify as integer schema versions.
        and type(snapshot.get("schema")) is int  # pylint: disable=unidiomatic-typecheck
        and snapshot["schema"] == 1
        and snapshot.get("path") == POLICY,
        "Manifest requires a versioned source policy snapshot",
    )
    require(
        isinstance(snapshot.get("git_blob_sha"), str)
        and SHA_RE.fullmatch(snapshot["git_blob_sha"])
        and isinstance(snapshot.get("sha256"), str)
        and re.fullmatch(r"[0-9a-f]{64}", snapshot["sha256"]),
        "Invalid source policy snapshot hashes",
    )
    require_release_policy(snapshot.get("data"), repo, qualified=True)


def check_ancestry(gh: GitHub, sha: str, default_branch: str) -> None:
    """Require the source commit to remain on the current default branch."""
    comparison = gh.api(f"compare/{sha}...{quote(default_branch, safe='')}")
    require(
        comparison.get("status") in ("ahead", "identical"),
        "Source commit is not an ancestor of the current default branch",
    )
    require(
        comparison.get("merge_base_commit", {}).get("sha") == sha,
        "Source/default branch ancestry could not be verified",
    )


def checked_out_sha() -> str:
    """Return the exact local commit used by the publication process."""
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"], text=True, capture_output=True, check=False
    )
    require(result.returncode == 0, "Must run from the checked-out release repository")
    return result.stdout.strip()


def check_execution(
    gh: GitHub, run_id: int, channel: str, info: dict, run: dict
) -> None:
    """Bind publication to the expected Actions repository, ref, run and event."""
    require(
        os.environ.get("GITHUB_ACTIONS") == "true",
        "Publication must run inside GitHub Actions; dispatch the release workflow locally",
    )
    require(
        os.environ.get("GITHUB_REPOSITORY", "").lower() == gh.repo.lower(),
        "Execution repository mismatch",
    )
    require(os.environ.get("GITHUB_RUN_ID") == str(run_id), "Execution run ID mismatch")
    require(
        os.environ.get("GITHUB_RUN_ATTEMPT") == str(run.get("run_attempt")),
        "Execution run attempt mismatch",
    )
    require(
        os.environ.get("GITHUB_REF") == f"refs/heads/{info['default_branch']}",
        "Dispatch must target the default branch",
    )
    require(
        os.environ.get("GITHUB_WORKFLOW_REF")
        == f"{info['full_name']}/{WORKFLOW}@refs/heads/{info['default_branch']}",
        "Execution workflow mismatch",
    )
    require(
        checked_out_sha() == run.get("head_sha"),
        "Checkout does not match the source run SHA",
    )
    event = os.environ.get("GITHUB_EVENT_NAME")
    require(event == run.get("event"), "Execution event mismatch")
    allowed = {
        "nightly": {"schedule", "workflow_dispatch"},
        "beta": {"push", "workflow_dispatch"},
        "rc": {"workflow_dispatch"},
        "stable": {"workflow_dispatch"},
    }
    require(event in allowed[channel], f"Event cannot publish channel {channel}")
    if event == "workflow_dispatch":
        event_file = os.environ.get("GITHUB_EVENT_PATH")
        require(bool(event_file), "Missing workflow dispatch event")
        payload = parse_json(Path(event_file).read_bytes(), "workflow dispatch event")
        require(
            isinstance(payload, dict)
            and payload.get("inputs", {}).get("channel") == channel,
            f"Workflow dispatch input channel must be {channel}",
        )


# Keep the independently verified provenance fields explicit at each call site.
# pylint: disable-next=too-many-arguments
def validate_run(
    gh: GitHub,
    run: dict,
    info: dict,
    sha: str,
    attempt: int,
    completed: bool,
    gate: bool = True,
) -> None:
    """Verify run provenance and require one explicitly successful Release gate."""
    require(
        run.get("repository", {}).get("full_name", "").lower() == gh.repo.lower(),
        "Source run belongs to another repository",
    )
    require(
        run.get("head_repository", {}).get("full_name", "").lower() == gh.repo.lower(),
        "Source run comes from another repository",
    )
    require(
        run.get("head_sha") == sha and SHA_RE.fullmatch(sha), "Source run SHA mismatch"
    )
    require(run.get("path") == WORKFLOW, "Source run uses an unexpected workflow path")
    require(
        run.get("head_branch") == info["default_branch"],
        "Source run does not target the default branch",
    )
    require(
        run.get("event") in ("push", "schedule", "workflow_dispatch"),
        "Untrusted source run event",
    )
    require(
        run.get("run_attempt") == attempt,
        "Source run was rerun; evidence is not from its latest attempt",
    )
    if completed:
        require(
            run.get("status") == "completed" and run.get("conclusion") == "success",
            "Source run has not completed successfully",
        )
    else:
        require(
            run.get("status") == "in_progress", "Publication run is not in progress"
        )
    if gate:
        jobs = gh.pages(
            f"actions/runs/{positive(run.get('id'), 'run ID')}/attempts/{attempt}/jobs",
            "jobs",
        )
        gates = [job for job in jobs if job.get("name") == "Release gate"]
        require(
            len(gates) == 1
            and gates[0].get("status") == "completed"
            and gates[0].get("conclusion") == "success",
            "Exactly one successful, completed Release gate is required; skipped is not a pass",
        )
        require(gates[0].get("head_sha") == sha, "Release gate SHA mismatch")


def ensure_absent(gh: GitHub, tag: str) -> None:
    """Refuse existing tags, published releases and hidden release drafts."""
    require(
        gh.optional(f"git/ref/tags/{quote(tag, safe='')}") is None,
        f"Tag already exists: {tag}",
    )
    require(
        gh.optional(f"releases/tags/{quote(tag, safe='')}") is None,
        f"Release already exists: {tag}",
    )
    # GitHub's releases-by-tag endpoint can hide drafts. Explicitly check them.
    require(
        not any(release.get("tag_name") == tag for release in gh.pages("releases")),
        f"Draft release already exists: {tag}",
    )


def next_sequence(gh: GitHub, base_version: str, channel: str) -> int:
    """Find an unused beta or RC sequence across all tags and releases."""
    require(channel in ("beta", "rc"), "Sequence only applies to beta and rc")
    prefix = f"v{version(base_version)}-{channel}."
    numbers = []
    tags = [item.get("name", "") for item in gh.pages("tags")]
    tags += [item.get("tag_name", "") for item in gh.pages("releases")]
    for tag in tags:
        if tag.startswith(prefix) and re.fullmatch(
            r"[1-9]\d*", tag[len(prefix) :], re.ASCII
        ):
            numbers.append(int(tag[len(prefix) :]))
    return max(numbers, default=0) + 1


# pylint: disable-next=too-many-arguments
def candidate_tag(
    gh: GitHub,
    channel: str,
    base_version: str,
    run_id: int,
    attempt: int,
    sequence: int | None,
    now: datetime,
) -> str:
    """Keep each validated channel, sequence and run identity component explicit."""
    prefix = f"v{version(base_version)}-{channel}."
    if channel == "nightly":
        require(sequence is None, "Nightly tags do not accept a sequence")
        return prefix + now.strftime("%Y%m%d%H%M%S") + f".{run_id}.{attempt}"
    require(channel in ("beta", "rc"), "Invalid candidate channel")
    return prefix + str(
        positive(sequence, "sequence")
        if sequence is not None
        else next_sequence(gh, base_version, channel)
    )


def stage_assets(source: Path, destination: Path) -> list[dict]:
    """Snapshot flat regular payload files and hash the private staged bytes."""
    require(
        source.is_dir() and not source.is_symlink(),
        "Assets must be a regular directory",
    )
    assets = []
    folded_names = set()
    for entry in sorted(source.iterdir()):
        require(
            not entry.is_symlink() and entry.is_file(),
            f"Assets must be flat regular files: {entry.name}",
        )
        require(
            NAME_RE.fullmatch(entry.name)
            and entry.name.casefold() != MANIFEST.casefold(),
            f"Unsafe or reserved asset name: {entry.name}",
        )
        require(
            entry.name.casefold() not in folded_names,
            f"Asset name collision: {entry.name}",
        )
        folded_names.add(entry.name.casefold())
        descriptor = os.open(entry, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        with os.fdopen(descriptor, "rb") as stream:
            require(
                stat.S_ISREG(os.fstat(stream.fileno()).st_mode),
                f"Asset is not a regular file: {entry.name}",
            )
            data = stream.read()
        (destination / entry.name).write_bytes(data)
        assets.append({"name": entry.name, "size": len(data), "sha256": digest(data)})
    require(assets, "Cannot publish an empty assets directory")
    return assets


# pylint: disable-next=too-many-arguments
def publish(
    gh: GitHub, tag: str, sha: str, directory: Path, prerelease: bool, body: str
) -> dict:
    """Keep draft creation, exact-byte upload checks and publication in one transaction."""
    ensure_absent(gh, tag)
    gh.api("git/refs", "POST", {"ref": f"refs/tags/{tag}", "sha": sha})
    release = gh.api(
        "releases",
        "POST",
        {
            "tag_name": tag,
            "target_commitish": sha,
            "name": tag,
            "body": body,
            "draft": True,
            "prerelease": prerelease,
            "make_latest": "false",
        },
    )
    require(
        release.get("draft") is True and release.get("tag_name") == tag,
        "Unexpected draft release response",
    )
    release_id = positive(release.get("id"), "release ID")
    for path in sorted(directory.iterdir()):
        gh.upload(tag, path)
    uploaded = gh.pages(f"releases/{release_id}/assets")
    expected = {path.name: path for path in directory.iterdir()}
    require(
        len(uploaded) == len(expected)
        and {item.get("name") for item in uploaded} == set(expected),
        "Uploaded asset inventory mismatch; draft left unpublished",
    )
    for item in uploaded:
        local = expected[item["name"]].read_bytes()
        require(
            item.get("state") == "uploaded" and item.get("size") == len(local),
            "Incomplete asset upload; draft left unpublished",
        )
        remote = gh.binary(f"releases/assets/{positive(item.get('id'), 'asset ID')}")
        require(
            digest(remote) == digest(local),
            "Uploaded bytes differ; draft left unpublished",
        )
    result = gh.api(
        f"releases/{release_id}",
        "PATCH",
        {
            "draft": False,
            "prerelease": prerelease,
            "make_latest": "false" if prerelease else "true",
        },
    )
    require(
        result.get("draft") is False
        and result.get("tag_name") == tag
        and result.get("prerelease") is prerelease,
        "Publication response did not confirm the requested state",
    )
    return result


def emit_result(result: dict) -> None:
    """Print the result and write validated single-line Actions outputs."""
    print(json.dumps(result, sort_keys=True))
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            for key, value in result.items():
                require(
                    "\n" not in str(value) and "\r" not in str(value),
                    "Unsafe workflow output",
                )
                handle.write(f"{key}={value}\n")


# Keep validation, immutable staging and publication in one auditable sequence.
# pylint: disable-next=too-many-locals
def candidate(args) -> dict:
    """Validate the active run and source policy before publishing a prerelease."""
    gh = GitHub(args.repo)
    base_version = version(args.version)
    require(
        SHA_RE.fullmatch(args.sha),
        "Source SHA must be a full lowercase 40-character commit SHA",
    )
    run_id, attempt = (
        positive(args.run_id, "run ID"),
        positive(args.run_attempt, "run attempt"),
    )
    info = repository_info(gh)
    run = gh.api(f"actions/runs/{run_id}")
    require(run.get("id") == run_id, "Run identity mismatch")
    check_execution(gh, run_id, args.channel, info, run)
    validate_run(gh, run, info, args.sha, attempt, completed=False)
    policy_snapshot = source_policy_snapshot(gh, args.sha)
    require_release_policy(
        policy_snapshot["data"], gh.repo, qualified=args.channel == "rc"
    )
    check_ancestry(gh, args.sha, info["default_branch"])
    if args.channel in ("beta", "rc"):
        # Once a base version is final, further candidates would mislabel new
        # code as an already released version. Nightlies retain run identities.
        ensure_absent(gh, f"v{base_version}")
    now = datetime.now(timezone.utc)
    tag = candidate_tag(
        gh, args.channel, base_version, run_id, attempt, args.sequence, now
    )
    with tempfile.TemporaryDirectory(prefix="release-candidate-") as temp:
        stage = Path(temp)
        assets = stage_assets(Path(args.assets), stage)
        manifest = {
            "schema": 1,
            "repository": info["full_name"],
            "version": base_version,
            "channel": args.channel,
            "tag": tag,
            "source_sha": args.sha,
            "source_policy": policy_snapshot,
            "workflow_path": WORKFLOW,
            "run_id": run_id,
            "run_attempt": attempt,
            "created_at": now.isoformat(),
            "assets": assets,
        }
        content = json_bytes(manifest)
        (stage / MANIFEST).write_bytes(content)
        EVIDENCE.parent.mkdir(parents=True, exist_ok=True)
        EVIDENCE.write_bytes(content)
        release = publish(
            gh,
            tag,
            args.sha,
            stage,
            True,
            f"{args.channel} candidate from `{args.sha}`.\n\n"
            f"Validation: https://github.com/{gh.repo}/actions/runs/{run_id}\n\n"
            f"See `{MANIFEST}` for checksums and immutable Actions evidence provenance.",
        )
    return {
        "tag": tag,
        "release_url": release["html_url"],
        "manifest_path": str(EVIDENCE),
    }


def validate_manifest(raw: bytes, repo: str, rc_tag: str) -> dict:
    """Reject malformed or ineligible RC manifests before trusting their assets."""
    require(len(raw) <= 2_000_000, "Manifest is unreasonably large")
    manifest = parse_json(raw, MANIFEST)
    require(
        isinstance(manifest, dict)
        # JSON booleans must not qualify as integer schema versions.
        and type(manifest.get("schema")) is int  # pylint: disable=unidiomatic-typecheck
        and manifest["schema"] == 1,
        "Unsupported manifest schema",
    )
    require(
        isinstance(manifest.get("repository"), str)
        and manifest["repository"].lower() == repo.lower(),
        "Manifest repository mismatch",
    )
    base_version = manifest.get("version")
    require(isinstance(base_version, str), "Missing manifest version")
    version(base_version)
    require(manifest.get("channel") == "rc", "Only release candidates can be promoted")
    require(
        manifest.get("tag") == rc_tag
        and re.fullmatch(rf"v{re.escape(base_version)}-rc\.[1-9]\d*", rc_tag, re.ASCII),
        "Manifest RC tag/version mismatch",
    )
    require(
        isinstance(manifest.get("source_sha"), str)
        and SHA_RE.fullmatch(manifest["source_sha"]),
        "Invalid manifest source SHA",
    )
    require(
        manifest.get("workflow_path") == WORKFLOW, "Manifest workflow path mismatch"
    )
    positive(manifest.get("run_id"), "manifest run ID")
    positive(manifest.get("run_attempt"), "manifest run attempt")
    validate_policy_snapshot(manifest.get("source_policy"), repo)
    assets = manifest.get("assets")
    require(isinstance(assets, list) and assets, "Manifest contains no assets")
    names = set()
    for item in assets:
        require(isinstance(item, dict), "Invalid manifest asset")
        name = item.get("name")
        require(
            isinstance(name, str)
            and NAME_RE.fullmatch(name)
            and name.casefold() != MANIFEST.casefold(),
            "Unsafe manifest asset name",
        )
        require(name.casefold() not in names, "Duplicate manifest asset name")
        names.add(name.casefold())
        require(
            # Reject JSON booleans, which isinstance(value, int) would accept.
            type(item.get("size")) is int  # pylint: disable=unidiomatic-typecheck
            and item["size"] >= 0,
            "Invalid manifest asset size",
        )
        require(
            isinstance(item.get("sha256"), str)
            and re.fullmatch(r"[0-9a-f]{64}", item["sha256"]),
            "Invalid asset checksum",
        )
    return manifest


def release_snapshot(gh: GitHub, tag: str) -> tuple[dict, dict, list[dict]]:
    """Read a candidate tag, release metadata and its complete asset inventory."""
    ref = gh.api(f"git/ref/tags/{quote(tag, safe='')}")
    require(
        ref.get("ref") == f"refs/tags/{tag}"
        and ref.get("object", {}).get("type") == "commit",
        "Candidate must have a lightweight commit tag",
    )
    release = gh.api(f"releases/tags/{quote(tag, safe='')}")
    require(
        release.get("tag_name") == tag
        and release.get("draft") is False
        and release.get("prerelease") is True,
        "Candidate is not a published prerelease",
    )
    assets = gh.pages(f"releases/{positive(release.get('id'), 'release ID')}/assets")
    require(
        all(asset.get("state") == "uploaded" for asset in assets),
        "Candidate has incomplete uploads",
    )
    names = [asset.get("name") for asset in assets]
    require(
        all(isinstance(name, str) and NAME_RE.fullmatch(name) for name in names),
        "Unsafe release asset name",
    )
    require(
        len(names) == len({name.casefold() for name in names}),
        "Duplicate release assets",
    )
    return ref, release, assets


def snapshot_identity(snapshot: tuple) -> bytes:
    """Encode the immutable identity fields used to detect candidate changes."""
    ref, release, assets = snapshot
    return json_bytes(
        {
            "ref": ref,
            "release": {
                key: release.get(key)
                for key in (
                    "id",
                    "tag_name",
                    "target_commitish",
                    "draft",
                    "prerelease",
                    "updated_at",
                )
            },
            "assets": sorted(assets, key=lambda asset: asset["id"]),
        }
    )


def verify_evidence(gh: GitHub, manifest: dict, raw: bytes) -> None:
    """Match the manifest to the digest-verified immutable Actions evidence ZIP."""
    artifacts = gh.pages(f"actions/runs/{manifest['run_id']}/artifacts", "artifacts")
    matching = [
        item
        for item in artifacts
        if item.get("name") == "release-evidence" and not item.get("expired")
    ]
    require(
        len(matching) == 1,
        "Exactly one unexpired immutable release-evidence artifact is required",
    )
    artifact = matching[0]
    require(
        artifact.get("workflow_run", {}).get("id") == manifest["run_id"]
        and artifact.get("workflow_run", {}).get("head_sha") == manifest["source_sha"],
        "Evidence artifact provenance mismatch",
    )
    archive = gh.binary(
        f"actions/artifacts/{positive(artifact.get('id'), 'artifact ID')}/zip"
    )
    require(
        artifact.get("digest") == f"sha256:{digest(archive)}",
        "Evidence must have a matching immutable v4 artifact digest",
    )
    try:
        with zipfile.ZipFile(io.BytesIO(archive)) as zipped:
            entries = [entry for entry in zipped.infolist() if not entry.is_dir()]
            require(
                len(entries) == 1 and entries[0].filename in (MANIFEST, str(EVIDENCE)),
                "Evidence artifact has unexpected contents",
            )
            require(
                entries[0].file_size <= 2_000_000,
                "Evidence artifact manifest is too large",
            )
            require(
                zipped.read(entries[0]) == raw,
                "Release manifest differs from immutable Actions evidence",
            )
    except zipfile.BadZipFile as exc:
        raise ReleaseError("Cannot read release evidence archive") from exc


def require_reviewers(gh: GitHub) -> None:
    """Require the release environment to configure at least one reviewer."""
    environment = gh.api("environments/release")
    require(
        environment.get("name") == "release", "Protected release environment is missing"
    )
    rules = environment.get("protection_rules", [])
    require(
        any(
            rule.get("type") == "required_reviewers" and rule.get("reviewers")
            for rule in rules
        ),
        "The release environment must have required reviewers configured",
    )


# Preserve the ordered security checks and staged bytes within one transaction.
# pylint: disable-next=too-many-locals
def promote(args) -> dict:
    """Verify an approved RC and publish the same bytes under a new stable tag."""
    gh = GitHub(args.repo)
    require(
        re.fullmatch(
            "v" + VERSION_PATTERN + r"-rc\.[1-9]\d*",
            args.rc,
            re.ASCII,
        ),
        "Promotion requires a strict vX.Y.Z-rc.N tag",
    )
    current_id = positive(args.run_id, "run ID")
    info = repository_info(gh)
    current = gh.api(f"actions/runs/{current_id}")
    require(current.get("id") == current_id, "Current run identity mismatch")
    check_execution(gh, current_id, "stable", info, current)
    validate_run(
        gh,
        current,
        info,
        current.get("head_sha", ""),
        positive(current.get("run_attempt"), "current run attempt"),
        completed=False,
        gate=False,
    )
    require_reviewers(gh)
    initial = release_snapshot(gh, args.rc)
    ref, candidate_release, release_assets = initial
    manifest_assets = [
        asset for asset in release_assets if asset.get("name") == MANIFEST
    ]
    require(len(manifest_assets) == 1, "Candidate manifest is missing")
    raw = gh.binary(
        f"releases/assets/{positive(manifest_assets[0].get('id'), 'manifest asset ID')}"
    )
    manifest = validate_manifest(raw, gh.repo, args.rc)
    policy_snapshot = source_policy_snapshot(gh, manifest["source_sha"])
    require_release_policy(policy_snapshot["data"], gh.repo, qualified=True)
    require(
        policy_snapshot == manifest["source_policy"],
        "Manifest policy snapshot differs from the policy at the candidate source commit",
    )
    require(
        ref["object"].get("sha") == manifest["source_sha"],
        "Candidate tag SHA differs from manifest",
    )
    require(
        manifest["run_id"] != current_id,
        "Candidate and promotion must use separate runs",
    )
    source_run = gh.api(f"actions/runs/{manifest['run_id']}")
    require(source_run.get("id") == manifest["run_id"], "Source run identity mismatch")
    validate_run(
        gh,
        source_run,
        info,
        manifest["source_sha"],
        manifest["run_attempt"],
        completed=True,
    )
    require(
        source_run.get("event") == "workflow_dispatch",
        "RC must originate from a manual workflow dispatch",
    )
    check_ancestry(gh, manifest["source_sha"], info["default_branch"])
    verify_evidence(gh, manifest, raw)
    tag = f"v{manifest['version']}"
    ensure_absent(gh, tag)
    expected = {item["name"]: item for item in manifest["assets"]}
    require(
        {item["name"] for item in release_assets} == set(expected) | {MANIFEST},
        "Candidate assets differ from manifest inventory",
    )
    with tempfile.TemporaryDirectory(prefix="release-promote-") as temp:
        stage = Path(temp)
        for asset in release_assets:
            name = asset["name"]
            data = (
                raw
                if name == MANIFEST
                else gh.binary(
                    f"releases/assets/{positive(asset.get('id'), 'asset ID')}"
                )
            )
            require(
                asset.get("size") == len(data), f"Release asset size mismatch: {name}"
            )
            if name != MANIFEST:
                require(
                    len(data) == expected[name]["size"]
                    and digest(data) == expected[name]["sha256"],
                    f"Candidate checksum mismatch: {name}",
                )
            (stage / name).write_bytes(data)
        require(
            snapshot_identity(release_snapshot(gh, args.rc))
            == snapshot_identity(initial),
            "Candidate changed during verification",
        )
        # Recheck mutable authorization/provenance just before the first write.
        latest_run = gh.api(f"actions/runs/{manifest['run_id']}")
        validate_run(
            gh,
            latest_run,
            info,
            manifest["source_sha"],
            manifest["run_attempt"],
            completed=True,
        )
        require_reviewers(gh)
        release = publish(
            gh,
            tag,
            manifest["source_sha"],
            stage,
            False,
            f"Promoted unchanged from [{args.rc}]({candidate_release['html_url']}).\n\n"
            f"Source: `{manifest['source_sha']}`\n\n"
            f"Validation: https://github.com/{gh.repo}/actions/runs/{manifest['run_id']}\n\n"
            f"Promotion: https://github.com/{gh.repo}/actions/runs/{current_id}\n\n"
            f"Assets and `{MANIFEST}` are byte-for-byte copies of the verified release candidate.",
        )
    return {"tag": tag, "release_url": release["html_url"]}


def parser() -> argparse.ArgumentParser:
    """Build the CLI for guarded publication and read-only release helpers."""
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    new = commands.add_parser(
        "candidate", help="Publish a checked prerelease from the current Actions run"
    )
    new.add_argument("--repo", required=True)
    new.add_argument("--channel", choices=("nightly", "beta", "rc"), required=True)
    new.add_argument("--version", required=True)
    new.add_argument("--sha", required=True)
    new.add_argument("--run-id", required=True)
    new.add_argument("--run-attempt", required=True)
    new.add_argument("--assets", required=True)
    new.add_argument("--sequence", type=int)
    stable = commands.add_parser(
        "promote", help="Promote an existing verified RC without rebuilding"
    )
    stable.add_argument("--repo", required=True)
    stable.add_argument("--rc", required=True)
    stable.add_argument("--run-id", required=True)
    sequence = commands.add_parser(
        "next", help="Read-only next beta/RC sequence; publication still detects races"
    )
    sequence.add_argument("--repo", required=True)
    sequence.add_argument("--channel", choices=("beta", "rc"), required=True)
    sequence.add_argument("--version", required=True)
    verify = commands.add_parser(
        "verify-manifest", help="Validate a local RC manifest's structure"
    )
    verify.add_argument("--repo", required=True)
    verify.add_argument("--rc", required=True)
    verify.add_argument("--manifest", type=Path, required=True)
    return result


def main(argv: list[str] | None = None) -> int:
    """Execute one release command and report failures without retrying writes."""
    args = parser().parse_args(argv)
    try:
        if args.command == "candidate":
            result = candidate(args)
        elif args.command == "promote":
            result = promote(args)
        elif args.command == "next":
            result = {
                "sequence": next_sequence(GitHub(args.repo), args.version, args.channel)
            }
        else:
            result = validate_manifest(args.manifest.read_bytes(), args.repo, args.rc)
        emit_result(result)
        return 0
    except (ReleaseError, OSError, KeyError, TypeError) as exc:
        print(f"release-control: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

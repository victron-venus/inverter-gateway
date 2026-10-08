"""Run the real release entrypoint without building or publishing artifacts."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BUILD_PREFIX = [
    "python:scripts/check-release-version.py",
    "cargo:build --locked --release",
    "python:scripts/release_version_adapter.py",
    "binary:--version",
]


class ReleaseBuildTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        (self.root / "scripts").mkdir()
        (self.root / "target/release").mkdir(parents=True)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.trace = self.root / "trace"
        shutil.copy2(
            ROOT / "scripts/release-build.sh", self.root / "scripts/release-build.sh"
        )
        self.write_command(
            self.bin / "python3",
            'printf "python:%s\\n" "$1" >> "$TRACE"\n'
            'case "$1" in\n'
            '  scripts/check-release-version.py) exit "${CHECK_STATUS:-0}" ;;\n'
            '  scripts/release_version_adapter.py) printf "%s\\n" "$PACKAGE_VERSION" ;;\n'
            '  scripts/write_binary_build_metadata.py) printf metadata > "$3" ;;\n'
            "  *) exit 91 ;;\n"
            "esac\n",
        )
        self.write_command(
            self.bin / "cargo",
            'printf "cargo:%s\\n" "$*" >> "$TRACE"\nexit "${CARGO_STATUS:-0}"\n',
        )
        self.write_command(
            self.root / "target/release/inverter-gateway",
            'printf "binary:%s\\n" "$*" >> "$TRACE"\n'
            'printf "%s\\n" "$BINARY_VERSION"\nexit "${BINARY_STATUS:-0}"\n',
        )
        self.write_command(self.bin / "tar", 'printf "tar\\n" >> "$TRACE"\n')
        candidates = ["/bin/bash", shutil.which("bash"), "/opt/homebrew/bin/bash"]
        self.shells = sorted(
            {str(Path(p).resolve()) for p in candidates if p and Path(p).is_file()}
        )
        self.assertTrue(self.shells, "Bash is required by the release entrypoint")

    @staticmethod
    def write_command(path, body):
        path.write_text("#!/bin/sh\n" + body)
        path.chmod(0o755)

    def run_build(self, shell, **overrides):
        self.trace.unlink(missing_ok=True)
        output = self.root / "release-output"
        if output.exists():
            shutil.rmtree(output)
        env = {
            **os.environ,
            "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
            "TRACE": str(self.trace),
            "PACKAGE_VERSION": "1.2.3-rc.1",
            "BINARY_VERSION": "inverter-gateway 1.2.3-rc.1",
            "CHECK_STATUS": "0",
            "CARGO_STATUS": "0",
            "BINARY_STATUS": "0",
            **overrides,
        }
        result = subprocess.run(
            [shell, str(self.root / "scripts/release-build.sh"), "1.2.3", "rc"],
            cwd=self.root,
            env=env,
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
        return result, self.trace.read_text().splitlines()

    def test_matching_binary_reaches_metadata_and_archive(self):
        for shell in self.shells:
            with self.subTest(shell=shell):
                result, trace = self.run_build(shell)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(
                    trace,
                    BUILD_PREFIX
                    + ["python:scripts/write_binary_build_metadata.py", "tar"],
                )

    def test_wrong_or_missing_binary_version_stops_before_packaging(self):
        for shell in self.shells:
            for version in ("inverter-gateway 9.9.9", "", "inverter-gateway *"):
                with self.subTest(shell=shell, version=version):
                    result, trace = self.run_build(shell, BINARY_VERSION=version)
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertEqual(
                        trace,
                        BUILD_PREFIX,
                    )
                    self.assertFalse(
                        (self.root / "release-output/build-info.json").exists()
                    )

    def test_version_command_failure_stops_even_when_output_matches(self):
        for shell in self.shells:
            with self.subTest(shell=shell):
                result, trace = self.run_build(shell, BINARY_STATUS="42")
                self.assertEqual(result.returncode, 42, result.stderr)
                self.assertEqual(trace, BUILD_PREFIX)
                self.assertFalse(
                    (self.root / "release-output/build-info.json").exists()
                )

    def test_build_failure_preserves_status_and_stops_later_steps(self):
        for shell in self.shells:
            with self.subTest(shell=shell):
                result, trace = self.run_build(shell, CARGO_STATUS="42")
                self.assertEqual(result.returncode, 42, result.stderr)
                self.assertEqual(
                    trace,
                    [
                        "python:scripts/check-release-version.py",
                        "cargo:build --locked --release",
                    ],
                )


if __name__ == "__main__":
    unittest.main()

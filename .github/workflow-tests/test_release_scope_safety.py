"""Preserve full release qualification independently of documentation-only PR scope."""

import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]


class ReleaseScopeSafetyTests(unittest.TestCase):
    """An automatic successor must qualify before superseding an older release."""

    def test_release_entry_point_and_checks_remain_full(self):
        """Reject regenerated workflows that let documentation bypass a release."""
        workflow = yaml.safe_load(
            (ROOT / ".github/workflows/release-pipeline.yml").read_text()
        )
        jobs = workflow["jobs"]
        steps = [step for step in jobs["scope"]["steps"] if step.get("id") == "scope"]
        self.assertEqual(len(steps), 1)
        self.assertEqual(str(steps[0]["env"]["FORCE_FULL"]).lower(), "true")
        self.assertIs(jobs["checks"]["with"]["force-full"], True)
        quality = yaml.safe_load(
            (ROOT / ".github/workflows/quality-gate.yml").read_text()
        )
        self.assertNotIn("if", quality["jobs"]["workflow-contracts"])

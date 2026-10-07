"""Behavioral tests for the offline Open-versus-original reader diff."""

from __future__ import annotations

import json
import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("compare_reffect_read_graph.py")
SPEC = importlib.util.spec_from_file_location("compare_reffect_read_graph", SCRIPT)
assert SPEC and SPEC.loader
compare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compare)


def graph_with_equipped_effects() -> dict[str, object]:
    return {
        "format": "reffect-read-graph/v1",
        "identity": {},
        "resources": {},
        "skillbar": {"skills": []},
        "gear": {
            "weapon_entries": [
                {
                    "raw_slot": 29,
                    "effects": [
                        {"direct_id": {"value": 21_824}},
                        {"wrapped_id": {"value": 9_452}},
                    ],
                }
            ],
            "relic_direct_id": {"value": 74_494},
        },
        "buffs": {"nodes": []},
    }


class CompareReadGraphTests(unittest.TestCase):
    def test_cli_normalizes_direct_and_wrapped_equipped_effects(self) -> None:
        """Catch a decoder that drops either of the live Foreign effect variants."""
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            reference = directory / "reference.json"
            open_graph = directory / "open.json"
            report = directory / "diff.json"
            fixture = graph_with_equipped_effects()
            reference.write_text(json.dumps(fixture), encoding="utf-8")
            open_graph.write_text(json.dumps(fixture), encoding="utf-8")

            completed = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--reference",
                    str(reference),
                    "--open",
                    str(open_graph),
                    "--out",
                    str(report),
                ],
                text=True,
                capture_output=True,
                check=False,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr)
            rendered = json.loads(report.read_text(encoding="utf-8"))
            self.assertEqual(
                rendered["open"]["gear"],
                {"sigils": [21_824, 9_452, 0, 0], "relic": 74_494},
            )
            self.assertEqual(rendered["differences"], [])

    def test_diff_filters_inactive_buffs_and_tolerates_short_duration_drift(self) -> None:
        """Catch a reader diff that mistakes stale nodes or a polling race for a mismatch."""
        reference = {
            "skillbar": {
                "weapon_swap": {
                    "kind": "weapon_swap",
                    "last_update": {"value": 1_000},
                    "cooldown": {"value": 8_000},
                },
                "skills": [],
            },
            "gear": {"weapon_entries": [], "relic_direct_id": {"value": 70_047}},
            "buffs": {
                "nodes": [
                    {
                        "state": {"value": 1},
                        "effect_id": {"value": 33_613},
                        "duration": {"value": 20_000},
                    },
                    {
                        "state": {"value": 1},
                        "effect_id": {"value": 740},
                        "duration": {"value": 0},
                    },
                    {
                        "state": {"value": 0},
                        "effect_id": {"value": 999_999},
                        "duration": {"value": 99_999},
                    },
                ]
            },
        }
        open_graph = {
            "skillbar": {"weapon_swap": None, "skills": []},
            "gear": {"weapon_entries": [], "relic_direct_id": {"value": 0}},
            "buffs": {
                "nodes": [
                    {
                        "state": {"value": 1},
                        "effect_id": {"value": 33_613},
                        "duration": {"value": 19_850},
                    },
                    {
                        "state": {"value": 0},
                        "effect_id": {"value": 999_999},
                        "duration": {"value": 99_999},
                    },
                ]
            },
        }

        differences = compare.diff_summaries(
            compare.summarize_read_graph(reference),
            compare.summarize_read_graph(open_graph),
        )

        self.assertEqual(
            {difference["path"] for difference in differences},
            {"buffs.active", "gear.relic", "skillbar.weapon_swap"},
        )


if __name__ == "__main__":
    unittest.main()

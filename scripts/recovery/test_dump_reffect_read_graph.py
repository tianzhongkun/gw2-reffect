"""Regression tests for the one-shot Reffect runtime read-graph dump."""

from __future__ import annotations

import importlib.util
import struct
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("dump_reffect_read_graph.py")
SPEC = importlib.util.spec_from_file_location("dump_reffect_read_graph", SCRIPT)
assert SPEC and SPEC.loader
dump = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(dump)


class FakeMemory:
    def __init__(self, values: dict[int, bytes]) -> None:
        self.values = values

    def read_exact(self, address: int, size: int) -> bytes:
        data = self.values[address]
        if len(data) != size:
            raise RuntimeError(f"short fixture at 0x{address:X}: {len(data)}/{size}")
        return data


class DumpReadGraphTests(unittest.TestCase):
    def test_thief_primary_resource_uses_profession_current_and_max(self) -> None:
        """Catch a regression that treats an empty generic field as initiative."""
        character = 0x10000
        health = 0x20000
        profession = 0x30000
        memory = FakeMemory(
            {
                character + 0x3E8: struct.pack("<Q", health),
                health + 0x0C: struct.pack("<f", 12_345.0),
                health + 0x10: struct.pack("<f", 12_345.0),
                health + 0x28: struct.pack("<f", 0.0),
                character + 0x510: struct.pack("<Q", profession),
                profession + 0x88: struct.pack("<f", 12.0),
                profession + 0x8C: struct.pack("<f", 15.0),
            }
        )

        try:
            resources = dump.dump_player_resources(
                dump.Trace(memory), character, profession_id=5, now_tick=1_500
            )
        except TypeError as error:
            self.fail(f"primary-resource snapshot must accept its capture tick: {error}")

        self.assertIn("primary_current", resources)
        self.assertEqual(resources["primary_current"]["value"], 12.0)
        self.assertEqual(resources["primary_max"]["value"], 15.0)
        self.assertNotIn("primary_rate", resources)
        self.assertNotIn("primary_interpolated_current", resources)

    def test_full_graph_keeps_thief_primary_reads_in_the_shared_trace(self) -> None:
        character = 0x10000
        core = 0x18000
        profession = 0x20000
        memory = FakeMemory(
            {
                character + 0x98: struct.pack("<Q", 0x30000),
                character + 0x388: struct.pack("<Q", core),
                core + 0x33: struct.pack("<B", 1),
                core + 0x12C: struct.pack("<I", 5),
                character + 0x510: struct.pack("<Q", profession),
                profession + 0x88: struct.pack("<f", 12.0),
                profession + 0x8C: struct.pack("<f", 15.0),
            }
        )

        graph = dump.dump_character(memory, character)

        paths = {entry["path"] for entry in graph["reads"]}
        self.assertIn("player.thief_primary.current", paths)
        self.assertIn("player.thief_primary.max", paths)

    def test_gear_trace_follows_only_the_direct_effect_variant(self) -> None:
        character = 0x10000
        skillbar = 0x20000
        entry = 0x30000
        foreign = 0x40000
        payload = 0x50000
        effect = 0x60000
        memory = FakeMemory(
            {
                character + 0x3F0: struct.pack("<Q", skillbar),
                skillbar + 0x160 + 29 * 8: struct.pack("<Q", entry),
                entry + 0xA0 + 0x28: struct.pack("<Q", foreign),
                foreign + 0x2C: struct.pack("<I", 0x12),
                foreign + 0x30: struct.pack("<Q", payload),
                payload: struct.pack("<Q", effect),
                effect + 0x28: struct.pack("<I", 21824),
            }
        )

        graph = dump.dump_gear(dump.Trace(memory), character)

        direct = graph["weapon_entries"][0]["effects"][0]
        self.assertEqual(direct["direct_id"]["value"], 21824)
        paths = {entry["path"] for entry in direct.values() if isinstance(entry, dict) and "path" in entry}
        self.assertNotIn("gear.raw[29].effect_28.wrapped_owner", paths)

    def test_skillbar_records_the_separate_weapon_swap_cooldown_state(self) -> None:
        """Catch a dump that only walks the ordinary skill-definition slots."""
        character = 0x10000
        combatant = 0x20000
        memory = FakeMemory(
            {
                character + 0x520: struct.pack("<Q", combatant),
                character + 0xD8: struct.pack("<I", 0),
                combatant + 0x28C: struct.pack("<I", 1_000),
                combatant + 0x290: struct.pack("<I", 8_000),
            }
        )

        skillbar = dump.dump_skillbar(dump.Trace(memory), character)

        self.assertIn("weapon_swap", skillbar)
        weapon_swap = skillbar["weapon_swap"]
        self.assertEqual(weapon_swap["kind"], "weapon_swap")
        self.assertEqual(weapon_swap["last_update"]["value"], 1_000)
        self.assertEqual(weapon_swap["cooldown"]["value"], 8_000)


if __name__ == "__main__":
    unittest.main()

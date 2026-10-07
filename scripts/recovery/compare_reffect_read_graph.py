#!/usr/bin/env python3
"""Compare two read-only Reffect runtime graph snapshots offline.

The raw graph format contains volatile addresses, bytes and timestamps.  This
tool compares only reader results that should be meaningful across two addon
implementations running against the same character state.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


FORMAT = "reffect-read-graph-diff/v1"


def entry_value(value: object) -> object:
    """Return a scalar from a read-graph trace entry when one was captured."""
    if isinstance(value, dict):
        return value.get("value")
    return value


def non_negative_int(value: object) -> int | None:
    scalar = entry_value(value)
    return scalar if isinstance(scalar, int) and scalar >= 0 else None


def effect_id(effect: object) -> int:
    if not isinstance(effect, dict):
        return 0
    for key in ("direct_id", "wrapped_id"):
        value = non_negative_int(effect.get(key))
        if value:
            return value
    return 0


def summarize_gear(raw_gear: object) -> dict[str, object]:
    gear = raw_gear if isinstance(raw_gear, dict) else {}
    equipped: list[int] = []
    entries = gear.get("weapon_entries")
    if isinstance(entries, list):
        ordered_entries = sorted(
            (entry for entry in entries if isinstance(entry, dict)),
            key=lambda entry: int(entry.get("raw_slot", 0)),
        )
        for entry in ordered_entries:
            effects = entry.get("effects")
            if isinstance(effects, list):
                equipped.extend(effect_id(effect) for effect in effects)
    sigils = (equipped + [0, 0, 0, 0])[:4]
    relic = non_negative_int(gear.get("relic_direct_id"))
    if not relic:
        relic = non_negative_int(gear.get("relic_wrapped_id"))
    return {"sigils": sigils, "relic": relic or 0}


def semantic_id(value: object) -> int | None:
    if isinstance(value, dict):
        for key in ("id", "skill_id", "effect_id"):
            if key in value:
                return non_negative_int(value[key])
    return non_negative_int(value)


def summarize_skillbar(raw_skillbar: object) -> dict[str, object]:
    skillbar = raw_skillbar if isinstance(raw_skillbar, dict) else {}
    skills: dict[str, int] = {}
    entries = skillbar.get("skills")
    if isinstance(entries, list):
        for entry in entries:
            if not isinstance(entry, dict):
                continue
            slot = non_negative_int(entry.get("raw_slot"))
            identifier = semantic_id(entry)
            if slot is not None and identifier:
                skills[str(slot)] = identifier
    raw_weapon_swap = skillbar.get("weapon_swap")
    if isinstance(raw_weapon_swap, dict) and raw_weapon_swap.get("kind") == "weapon_swap":
        weapon_swap: object = {
            "kind": "weapon_swap",
            "cooldown": non_negative_int(raw_weapon_swap.get("cooldown")) or 0,
        }
    else:
        weapon_swap = semantic_id(raw_weapon_swap)
    return {"skills": skills, "weapon_swap": weapon_swap}


def summarize_buffs(raw_buffs: object) -> dict[str, object]:
    buffs = raw_buffs if isinstance(raw_buffs, dict) else {}
    active: list[dict[str, int]] = []
    nodes = buffs.get("nodes")
    if isinstance(nodes, list):
        for node in nodes:
            if not isinstance(node, dict) or non_negative_int(node.get("state")) != 1:
                continue
            identifier = semantic_id(node)
            if not identifier:
                continue
            duration = non_negative_int(node.get("duration"))
            active.append({"effect_id": identifier, "duration": duration or 0})
    return {"active": sorted(active, key=lambda buff: (buff["effect_id"], buff["duration"]))}


def summarize_read_graph(graph: dict[str, object]) -> dict[str, object]:
    """Reduce a raw reader graph to stable, user-visible reader results."""
    return {
        "gear": summarize_gear(graph.get("gear")),
        "skillbar": summarize_skillbar(graph.get("skillbar")),
        "buffs": summarize_buffs(graph.get("buffs")),
    }


def diff_summaries(
    reference: dict[str, object], open_graph: dict[str, object], *, duration_tolerance_ms: int = 250
) -> list[dict[str, object]]:
    """Return deterministic semantic differences between two summaries."""
    differences: list[dict[str, object]] = []

    def add_difference(path: str, left: object, right: object) -> None:
        differences.append({"path": path, "reference": left, "open": right})

    def different_active_buffs(left: object, right: object) -> bool:
        if not isinstance(left, list) or not isinstance(right, list) or len(left) != len(right):
            return left != right
        for reference_buff, open_buff in zip(left, right, strict=True):
            if not isinstance(reference_buff, dict) or not isinstance(open_buff, dict):
                return reference_buff != open_buff
            if reference_buff.get("effect_id") != open_buff.get("effect_id"):
                return True
            reference_duration = reference_buff.get("duration")
            open_duration = open_buff.get("duration")
            if not isinstance(reference_duration, int) or not isinstance(open_duration, int):
                return reference_duration != open_duration
            if abs(reference_duration - open_duration) > duration_tolerance_ms:
                return True
        return False

    def compare(path: str, left: object, right: object) -> None:
        if path == "buffs.active":
            if different_active_buffs(left, right):
                add_difference(path, left, right)
            return
        if isinstance(left, dict) and isinstance(right, dict):
            for key in sorted(set(left) | set(right)):
                compare(f"{path}.{key}" if path else str(key), left.get(key), right.get(key))
            return
        if isinstance(left, list) and isinstance(right, list):
            for index in range(max(len(left), len(right))):
                left_value = left[index] if index < len(left) else None
                right_value = right[index] if index < len(right) else None
                compare(f"{path}[{index}]", left_value, right_value)
            return
        if left != right:
            add_difference(path, left, right)

    compare("", reference, open_graph)
    return differences


def load_graph(path: Path) -> dict[str, object]:
    with path.open(encoding="utf-8") as handle:
        graph = json.load(handle)
    if not isinstance(graph, dict):
        raise ValueError(f"snapshot must be a JSON object: {path}")
    return graph


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, required=True, help="original addon graph JSON")
    parser.add_argument("--open", dest="open_graph", type=Path, required=True, help="Open addon graph JSON")
    parser.add_argument("--out", type=Path, help="write report JSON here; otherwise print it")
    args = parser.parse_args(argv)

    reference = summarize_read_graph(load_graph(args.reference))
    open_graph = summarize_read_graph(load_graph(args.open_graph))
    report = {
        "format": FORMAT,
        "reference": reference,
        "open": open_graph,
        "differences": diff_summaries(reference, open_graph),
    }
    rendered = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.out:
        args.out.write_text(rendered, encoding="utf-8")
    else:
        print(rendered, end="")
    return 1 if report["differences"] else 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Capture one read-only snapshot of the Open Reffect runtime object graph.

This intentionally follows the reader's pointer chains rather than dumping the
whole Guild Wars 2 address space.  Each attempted read contains the exact
address, raw bytes, decoded value, and any read error, so a snapshot can be
compared directly with the original addon's output and static disassembly.
"""

from __future__ import annotations

import argparse
import ctypes
import ctypes.wintypes as wintypes
import json
import math
import re
import struct
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Protocol


PROCESS_QUERY_INFORMATION = 0x0400
PROCESS_VM_READ = 0x0010
MAX_NODES = 512


class Memory(Protocol):
    def read_exact(self, address: int, size: int) -> bytes: ...


class ProcessMemory:
    def __init__(self, process_id: int) -> None:
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        self._close_handle = kernel32.CloseHandle
        self._close_handle.argtypes = [wintypes.HANDLE]
        self._close_handle.restype = wintypes.BOOL
        self._read_process_memory = kernel32.ReadProcessMemory
        self._read_process_memory.argtypes = [
            wintypes.HANDLE,
            wintypes.LPCVOID,
            wintypes.LPVOID,
            ctypes.c_size_t,
            ctypes.POINTER(ctypes.c_size_t),
        ]
        self._read_process_memory.restype = wintypes.BOOL
        open_process = kernel32.OpenProcess
        open_process.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        open_process.restype = wintypes.HANDLE
        self._handle = open_process(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, False, process_id)
        if not self._handle:
            raise OSError(ctypes.get_last_error(), f"OpenProcess({process_id}) failed")

    def close(self) -> None:
        if self._handle:
            self._close_handle(self._handle)
            self._handle = None

    def read_exact(self, address: int, size: int) -> bytes:
        buffer = (ctypes.c_ubyte * size)()
        read = ctypes.c_size_t()
        if not self._read_process_memory(
            self._handle,
            ctypes.c_void_p(address),
            buffer,
            size,
            ctypes.byref(read),
        ):
            raise OSError(ctypes.get_last_error(), f"ReadProcessMemory(0x{address:X}) failed")
        if read.value != size:
            raise RuntimeError(f"ReadProcessMemory(0x{address:X}) short read: {read.value}/{size}")
        return bytes(buffer)

    def __enter__(self) -> "ProcessMemory":
        return self

    def __exit__(self, _type: Any, _value: Any, _traceback: Any) -> None:
        self.close()


def hex_address(value: int) -> str:
    return f"0x{value:016X}"


def valid_pointer(value: int) -> bool:
    return 0x1_0000 <= value <= 0x0000_7FFF_FFFF_FFFF


def current_tick_count() -> int:
    """Return the same wrapping 32-bit tick source used by the runtime reader."""
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    get_tick_count = kernel32.GetTickCount
    get_tick_count.argtypes = []
    get_tick_count.restype = wintypes.DWORD
    return int(get_tick_count())


class Trace:
    def __init__(self, memory: Memory) -> None:
        self.memory = memory
        self.reads: list[dict[str, Any]] = []

    def scalar(self, path: str, address: int, kind: str) -> tuple[Any | None, dict[str, Any]]:
        formats = {"u8": "<B", "u32": "<I", "u64": "<Q", "f32": "<f"}
        size = struct.calcsize(formats[kind])
        entry: dict[str, Any] = {"path": path, "address": hex_address(address), "type": kind}
        try:
            raw = self.memory.read_exact(address, size)
            value = struct.unpack(formats[kind], raw)[0]
            entry.update(raw=raw.hex(), value=value)
            if kind == "u64":
                entry["value"] = hex_address(value)
                entry["valid_pointer"] = valid_pointer(value)
                self.reads.append(entry)
                return value if valid_pointer(value) else None, entry
            self.reads.append(entry)
            return value, entry
        except Exception as error:  # A failed read is evidence, not a fatal dump error.
            entry["error"] = str(error)
            self.reads.append(entry)
            return None, entry

    def ptr(self, path: str, base: int, offset: int) -> tuple[int | None, dict[str, Any]]:
        return self.scalar(path, base + offset, "u64")

    def u32(self, path: str, base: int, offset: int) -> tuple[int | None, dict[str, Any]]:
        return self.scalar(path, base + offset, "u32")

    def u8(self, path: str, base: int, offset: int) -> tuple[int | None, dict[str, Any]]:
        return self.scalar(path, base + offset, "u8")

    def f32(self, path: str, base: int, offset: int) -> tuple[float | None, dict[str, Any]]:
        return self.scalar(path, base + offset, "f32")


def dump_combatant_resources(trace: Trace, prefix: str, character: int) -> dict[str, Any]:
    resources: dict[str, Any] = {}
    health, resources["health_object"] = trace.ptr(f"{prefix}.health_object", character, 0x3E8)
    if health is not None:
        _value, resources["health_current"] = trace.f32(f"{prefix}.health.current", health, 0x0C)
        _value, resources["health_max"] = trace.f32(f"{prefix}.health.max", health, 0x10)
        _value, resources["health_barrier"] = trace.f32(f"{prefix}.health.barrier", health, 0x28)
    defiance, resources["defiance_object"] = trace.ptr(f"{prefix}.defiance_object", character, 0xC8)
    if defiance is not None:
        _value, resources["defiance_state"] = trace.u32(f"{prefix}.defiance.state", defiance, 0x40)
        _value, resources["defiance_value"] = trace.f32(f"{prefix}.defiance.value", defiance, 0x44)
    return resources


def dump_player_resources(
    trace: Trace, character: int, profession_id: int | None, now_tick: int
) -> dict[str, Any]:
    resources = dump_combatant_resources(trace, "player", character)
    endurance, resources["endurance_object"] = trace.ptr("player.endurance_object", character, 0x3D0)
    if endurance is not None:
        _value, resources["endurance_current"] = trace.u32("player.endurance.current", endurance, 0x10)
        _value, resources["endurance_max"] = trace.u32("player.endurance.max", endurance, 0x14)
    if profession_id == 5:
        thief, resources["thief_profession_object"] = trace.ptr(
            "player.thief_profession_object", character, 0x510
        )
        if thief is not None:
            _value, resources["primary_current"] = trace.f32("player.thief_primary.current", thief, 0x88)
            _value, resources["primary_max"] = trace.f32("player.thief_primary.max", thief, 0x8C)
    else:
        energy, resources["profession_energy_object"] = trace.ptr(
            "player.profession_energy_object", character, 0x3D8
        )
        if energy is not None:
            sample_tick, resources["primary_sample_tick"] = trace.u32(
                "player.primary.sample_tick", energy, 0x08
            )
            base_current, resources["primary_current"] = trace.f32("player.primary.current", energy, 0x0C)
            maximum, resources["primary_max"] = trace.f32("player.primary.max", energy, 0x10)
            rate, resources["primary_rate"] = trace.f32("player.primary.rate", energy, 0x14)
            if (
                sample_tick is not None
                and base_current is not None
                and maximum is not None
                and rate is not None
                and all(math.isfinite(value) for value in (base_current, maximum, rate))
                and maximum > 0.0
            ):
                elapsed_ms = max(0, (now_tick & 0xFFFF_FFFF) - sample_tick)
                current = base_current + elapsed_ms / 1_000.0 * rate
                resources["primary_interpolated_current"] = max(0.0, min(current, maximum))
    kennel, resources["kennel_object"] = trace.ptr("player.kennel_object", character, 0x3F8)
    if kennel is not None:
        _value, resources["primary_pet"] = trace.ptr("player.primary_pet", kennel, 0xF8)
    return resources


def dump_cooldowns(trace: Trace, prefix: str, combatant: int) -> dict[str, Any]:
    result: dict[str, Any] = {"nodes": []}
    container, result["container"] = trace.ptr(f"{prefix}.cooldowns.container", combatant, 0x150)
    if container is None:
        return result
    stride, result["stride"] = trace.u32(f"{prefix}.cooldowns.stride", container, 0x38)
    _rate, result["rate"] = trace.f32(f"{prefix}.cooldowns.rate", container, 0x30)
    stride = stride if stride is not None and stride <= 0x100 else 0x38
    seen: set[int] = set()
    for root_offset in (0x40, 0x48, 0x60, 0x68):
        node, root = trace.ptr(f"{prefix}.cooldowns.root_{root_offset:X}", container, root_offset)
        result.setdefault("roots", []).append(root)
        node = (node or 0) & ~1
        for index in range(64):
            if not valid_pointer(node) or node in seen or container <= node < container + 0x140:
                break
            seen.add(node)
            item: dict[str, Any] = {"address": hex_address(node)}
            definition, item["definition"] = trace.ptr(f"{prefix}.cooldowns.node[{index}].definition", node, 0x18)
            if definition is not None:
                _value, item["skill_id"] = trace.u32(f"{prefix}.cooldowns.node[{index}].skill_id", definition, 0x28)
            _value, item["remaining"] = trace.u32(f"{prefix}.cooldowns.node[{index}].remaining", node, 0x10)
            _value, item["total"] = trace.u32(f"{prefix}.cooldowns.node[{index}].total", node, 0x20)
            result["nodes"].append(item)
            next_nodes = []
            for offset, name in ((stride + 8, "stride"), (0, "fallback"), (8, "next")):
                candidate, entry = trace.ptr(f"{prefix}.cooldowns.node[{index}].{name}", node, offset)
                next_nodes.append((candidate or 0) & ~1)
                item.setdefault("links", []).append(entry)
            node = next(
                (
                    candidate
                    for candidate in next_nodes
                    if valid_pointer(candidate)
                    and candidate not in seen
                    and not (container <= candidate < container + 0x140)
                ),
                0,
            )
    return result


def dump_skillbar(trace: Trace, character: int) -> dict[str, Any]:
    result: dict[str, Any] = {"skills": []}
    combatant, result["combatant"] = trace.ptr("skillbar.combatant", character, 0x520)
    if combatant is None:
        return result
    result["cooldowns"] = dump_cooldowns(trace, "player", combatant)
    weapon_swap_state, weapon_swap_state_read = trace.u32(
        "skillbar.weapon_swap.state", character, 0xD8
    )
    if weapon_swap_state is not None and weapon_swap_state != 2:
        last_update, last_update_read = trace.u32(
            "skillbar.weapon_swap.last_update", combatant, 0x28C
        )
        cooldown, cooldown_read = trace.u32("skillbar.weapon_swap.cooldown", combatant, 0x290)
        if last_update is not None and cooldown is not None:
            result["weapon_swap"] = {
                "kind": "weapon_swap",
                "state": weapon_swap_state_read,
                "last_update": last_update_read,
                "cooldown": cooldown_read,
            }
    for raw in (5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 12, 13, 14, 15, 16, 20, 21):
        item: dict[str, Any] = {"raw_slot": raw}
        definition, item["definition"] = trace.ptr(f"skillbar.raw[{raw}].definition", combatant, 0x1D0 + raw * 8)
        if definition is not None:
            _value, item["id"] = trace.u32(f"skillbar.raw[{raw}].id", definition, 0x28)
            cooldown, item["cooldown_object"] = trace.ptr(f"skillbar.raw[{raw}].cooldown", definition, 0x60)
            if cooldown is not None:
                _value, item["static_cooldown"] = trace.u32(
                    f"skillbar.raw[{raw}].static_cooldown", cooldown, 0x90
                )
        result["skills"].append(item)
    return result


def dump_build(trace: Trace, character: int) -> dict[str, Any]:
    result: dict[str, Any] = {}
    holder, result["holder"] = trace.ptr("build.holder", character, 0x1E8)
    if holder is None:
        return result
    view, result["view"] = trace.ptr("build.view", holder, 0x90)
    if view is None or view < 0x50:
        return result
    seed = view - 0x50
    result["seed_slot"] = hex_address(seed)
    active_index, result["active_index"] = trace.u32("build.seed.active_index", seed, 0xC58)
    if active_index is None or active_index > 8:
        return result
    active = seed + active_index * 0x150
    result["active_slot"] = hex_address(active)
    result["specializations"] = []
    for index, offset in enumerate((0x120, 0x128, 0x130)):
        definition, entry = trace.ptr(f"build.spec[{index}].definition", active, offset)
        item: dict[str, Any] = {"definition": entry}
        if definition is not None:
            _value, item["id"] = trace.u32(f"build.spec[{index}].id", definition, 0x28)
        result["specializations"].append(item)
    result["traits"] = []
    for index in range(9):
        definition, entry = trace.ptr(f"build.trait[{index}].definition", active, 0x138 + index * 8)
        item = {"definition": entry}
        if definition is not None:
            _value, item["id"] = trace.u32(f"build.trait[{index}].id", definition, 0x28)
        result["traits"].append(item)
    return result


def dump_gear(trace: Trace, character: int) -> dict[str, Any]:
    result: dict[str, Any] = {"weapon_entries": []}
    skillbar, result["skillbar"] = trace.ptr("gear.skillbar", character, 0x3F0)
    if skillbar is None:
        return result
    for raw in (29, 30, 31, 32):
        item: dict[str, Any] = {"raw_slot": raw, "effects": []}
        entry, item["entry"] = trace.ptr(f"gear.raw[{raw}].entry", skillbar, 0x160 + raw * 8)
        if entry is not None:
            meta, item["meta"] = trace.ptr(f"gear.raw[{raw}].meta", entry, 0x40)
            if meta is not None:
                _value, item["meta_kind"] = trace.u32(f"gear.raw[{raw}].meta_kind", meta, 0x2C)
                type_object, item["type_object"] = trace.ptr(f"gear.raw[{raw}].type_object", meta, 0x30)
                if type_object is not None:
                    _value, item["weapon_type"] = trace.u32(f"gear.raw[{raw}].weapon_type", type_object, 0x0C)
            for effect_offset in (0x28, 0x30):
                foreign, effect = trace.ptr(f"gear.raw[{raw}].effect_{effect_offset:X}", entry + 0xA0, effect_offset)
                effect["slot_offset"] = effect_offset
                if foreign is not None:
                    kind, effect["kind"] = trace.u32(f"gear.raw[{raw}].effect_{effect_offset:X}.kind", foreign, 0x2C)
                    payload, effect["payload"] = trace.ptr(
                        f"gear.raw[{raw}].effect_{effect_offset:X}.payload", foreign, 0x30
                    )
                    if kind == 0x12 and payload is not None:
                        direct, effect["direct_effect"] = trace.ptr(
                            f"gear.raw[{raw}].effect_{effect_offset:X}.direct_effect", payload, 0
                        )
                        if direct is not None:
                            _value, effect["direct_id"] = trace.u32(
                                f"gear.raw[{raw}].effect_{effect_offset:X}.direct_id", direct, 0x28
                            )
                    elif kind == 0x17 and payload is not None:
                        owner, effect["wrapped_owner"] = trace.ptr(
                            f"gear.raw[{raw}].effect_{effect_offset:X}.wrapped_owner", payload, 0x10
                        )
                        if owner is not None:
                            wrapped, effect["wrapped_effect"] = trace.ptr(
                                f"gear.raw[{raw}].effect_{effect_offset:X}.wrapped_effect", owner, 0x50
                            )
                            if wrapped is not None:
                                _value, effect["wrapped_id"] = trace.u32(
                                    f"gear.raw[{raw}].effect_{effect_offset:X}.wrapped_id", wrapped, 0x28
                                )
                item["effects"].append(effect)
        result["weapon_entries"].append(item)
    relic, result["relic_entry"] = trace.ptr("gear.relic_entry", skillbar, 0x378)
    if relic is not None:
        foreign, result["relic_foreign"] = trace.ptr("gear.relic_foreign", relic, 0x40)
        if foreign is not None:
            kind, result["relic_kind"] = trace.u32("gear.relic.kind", foreign, 0x2C)
            payload, result["relic_payload"] = trace.ptr("gear.relic.payload", foreign, 0x30)
            if kind == 0x12 and payload is not None:
                effect, result["relic_direct_effect"] = trace.ptr("gear.relic.direct_effect", payload, 0)
                if effect is not None:
                    _value, result["relic_direct_id"] = trace.u32("gear.relic.direct_id", effect, 0x28)
            elif kind == 0x17 and payload is not None:
                owner, result["relic_wrapped_owner"] = trace.ptr("gear.relic.wrapped_owner", payload, 0x10)
                if owner is not None:
                    effect, result["relic_wrapped_effect"] = trace.ptr("gear.relic.wrapped_effect", owner, 0x50)
                    if effect is not None:
                        _value, result["relic_wrapped_id"] = trace.u32("gear.relic.wrapped_id", effect, 0x28)
    return result


def dump_buffs(trace: Trace, prefix: str, character: int) -> dict[str, Any]:
    result: dict[str, Any] = {"nodes": []}
    container, result["container"] = trace.ptr(f"{prefix}.buffs.container", character, 0xD0)
    if container is None:
        return result
    node_offset, result["node_offset"] = trace.u32(f"{prefix}.buffs.node_offset", container, 0x38)
    raw_root, result["root"] = trace.ptr(f"{prefix}.buffs.root", container, 0x48)
    if node_offset is None or node_offset > 0x100:
        return result
    node = raw_root or 0
    seen: set[int] = set()
    for index in range(MAX_NODES):
        if not valid_pointer(node) or node in seen:
            break
        seen.add(node)
        item: dict[str, Any] = {"address": hex_address(node)}
        wrapper, item["wrapper"] = trace.ptr(f"{prefix}.buffs.node[{index}].wrapper", node, 0x10)
        _value, item["duration"] = trace.u32(f"{prefix}.buffs.node[{index}].duration", node, 0x40)
        _value, item["applied_at"] = trace.u32(f"{prefix}.buffs.node[{index}].applied_at", node, 0x44)
        if wrapper is not None:
            _value, item["state"] = trace.u32(f"{prefix}.buffs.node[{index}].state", wrapper, 0x58)
            _value, item["effect_id"] = trace.u32(f"{prefix}.buffs.node[{index}].effect_id", wrapper, 0x28)
            _value, item["metadata"] = trace.ptr(f"{prefix}.buffs.node[{index}].metadata", wrapper, 0x60)
        next_node, item["next"] = trace.ptr(f"{prefix}.buffs.node[{index}].next", node, node_offset + 8)
        result["nodes"].append(item)
        node = next_node or 0
    return result


def dump_character(memory: Memory, character: int, now_tick: int | None = None) -> dict[str, Any]:
    trace = Trace(memory)
    now_tick = current_tick_count() if now_tick is None else now_tick
    result: dict[str, Any] = {
        "format": "reffect-read-graph/v1",
        "captured_at_utc": datetime.now(timezone.utc).isoformat(),
        "capture_tick": now_tick,
        "character": hex_address(character),
    }
    _value, result["agent"] = trace.ptr("character.agent", character, 0x98)
    core, result["core_stats"] = trace.ptr("character.core_stats", character, 0x388)
    profession_id: int | None = None
    result["identity"] = {}
    if core is not None:
        _value, result["identity"]["race"] = trace.u8("identity.race", core, 0x33)
        profession_id, result["identity"]["profession"] = trace.u32("identity.profession", core, 0x12C)
    result["resources"] = dump_player_resources(trace, character, profession_id, now_tick)
    result["skillbar"] = dump_skillbar(trace, character)
    result["build"] = dump_build(trace, character)
    result["gear"] = dump_gear(trace, character)
    result["buffs"] = dump_buffs(trace, "player", character)
    combatant, result["target_combatant"] = trace.ptr("target.combatant", character, 0x520)
    if combatant is not None:
        bridge, result["target_bridge"] = trace.ptr("target.bridge", combatant, 0x80)
        if bridge is not None:
            selected, result["target_selected"] = trace.ptr("target.selected", bridge, 0x218)
            if selected is not None:
                character_plus_48, result["target_character_plus_48"] = trace.ptr(
                    "target.character_plus_48", selected, 0x10
                )
                if character_plus_48 is not None and character_plus_48 >= 0x48:
                    target = character_plus_48 - 0x48
                    result["target_character"] = hex_address(target)
                    result["target_resources"] = dump_combatant_resources(trace, "target", target)
                    result["target_buffs"] = dump_buffs(trace, "target", target)
    result["reads"] = trace.reads
    return result


def character_from_log(path: Path) -> int:
    if not path.is_file():
        raise FileNotFoundError(f"runtime log not found: {path}")
    pattern = re.compile(r"(?:raw_local|local)=0x([0-9a-fA-F]+)")
    for line in reversed(path.read_text(encoding="utf-8", errors="replace").splitlines()):
        match = pattern.search(line)
        if match:
            return int(match.group(1), 16)
    raise RuntimeError(f"no local character address found in {path}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, required=True, help="Gw2-64.exe process id")
    parser.add_argument("--character", type=lambda value: int(value, 0), help="character address")
    parser.add_argument(
        "--log",
        type=Path,
        default=Path.home() / "AppData/Local/Temp/gw2_omnireader.log",
        help="fallback log used to obtain the latest local character address",
    )
    parser.add_argument("--out", type=Path, help="write JSON here; otherwise print it")
    args = parser.parse_args(argv)
    character = args.character if args.character is not None else character_from_log(args.log)
    with ProcessMemory(args.pid) as memory:
        snapshot = dump_character(memory, character)
    rendered = json.dumps(snapshot, ensure_ascii=False, indent=2)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(rendered + "\n", encoding="utf-8")
        print(args.out)
    else:
        print(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

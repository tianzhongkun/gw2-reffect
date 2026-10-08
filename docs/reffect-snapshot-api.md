# Reffect snapshot C ABI

The `reffect_open.dll` addon exports a C-ABI snapshot of the current Guild Wars 2
character state so other **in-process** modules (another addon DLL, a companion
tool, C / C++ / C# / Python code loaded into the game) can read it directly —
no marshalling, no IPC.

The data is captured continuously by the internal reader and published into
`reffect_core::context::Context`; the exported functions expose that snapshot as
a fixed-layout, `#[repr(C)]` struct.

## Exported symbols

| Symbol | Description |
|---|---|
| `const RfSnapshot* reffect_snapshot(void)` | Returns a pointer to this thread's snapshot of the current state. Refreshed each call; valid on the calling thread until the next call. Never `NULL`. |
| `size_t reffect_snapshot_size(void)` | Size of `RfSnapshot` in bytes, for consumers that allocate their own buffer. |

## Layout

Struct definitions are in `reffect_snapshot.h` (C/C++/C#) and mirrored in
`scripts/read_snapshot.py` (ctypes). Field order and capacity constants must stay
in sync with `reffect_core::snapshot`. All arrays carry an explicit `count` /
`*_valid` / `has_*` flag instead of relying on sentinel values.

Structures (sizes as of the current build, little-endian x64):

| Struct | Size |
|---|---|
| `RfResource` | 8 |
| `RfCombatant` | 24 |
| `RfBuff` | 16 |
| `RfAbility` | 36 |
| `RfCooldown` | 32 |
| `RfResources` | 92 |
| `RfBuild` | 128 |
| `RfGear` | 24 |
| `RfGroupMember` | 1568 |
| `RfSnapshot` | 12404 |

`RfCombatant::health_state` is `0` alive, `1` downed, `2` dead, `255` unknown.

## Target classification and distance

The snapshot carries everything needed to gate an action on the current target
without any further memory reading:

| Field | Meaning |
| --- | --- |
| `target_valid` | A target is selected |
| `target_kind` | `RF_TARGET_*`: player, NPC, monster, attack target, gatherable, chest, gadget, pet, or unknown |
| `target_affinity` | `1` friendly, `2` hostile, `4` neutral, `0` unknown |
| `target_category` | `RF_CAT_*` bits: entity kind, disposition, NPC rank and gathering discipline |
| `target_position[3]` | Target world position |
| `player_position[3]` | Player world position |
| `target_distance` | Distance between the two, `-1.0` when either is unknown |
| `target_gadget_type` | Raw gadget type for gadget targets, `0` otherwise |

`target_kind` is the kind axis of `target_category` reduced to one byte, so a
consumer that only needs to decide whether a target is worth attacking can
switch on it:

```c
const RfSnapshot* snap = reffect_snapshot();

int attackable = snap->target_kind == RF_TARGET_MONSTER ||
                 snap->target_kind == RF_TARGET_ATTACK_TARGET;

if (attackable && snap->target_distance >= 0.0f &&
    snap->target_distance <= skill_range) {
    use_skill();
}
```

`target_category` adds the detail: `RF_CAT_VETERAN` … `RF_CAT_LEGENDARY` for the
NPC rank, `RF_CAT_ORE` / `RF_CAT_WOOD` / `RF_CAT_PLANT` for the gathering
discipline, and `RF_CAT_HOSTILE` / `RF_CAT_NEUTRAL` / `RF_CAT_FRIENDLY` for the
disposition. A target the reader could not classify reports
`RF_TARGET_UNKNOWN`, `target_category == 0` and `target_distance == -1.0`, so a
consumer that requires a classification stays inactive rather than guessing.

Many objects that the client files as plain gadgets are still worth attacking: a
supply depot, a gate or a PvP control point all report `RF_TARGET_GADGET`, while
a gathering node never does. `target_gadget_type` carries the raw type so the
consumer can decide, using the `RF_GADGET_*` constants. Only the attack target
and destructible types are promoted to `RF_TARGET_ATTACK_TARGET` automatically:

```c
int attackable = snap->target_kind == RF_TARGET_MONSTER ||
                 snap->target_kind == RF_TARGET_ATTACK_TARGET ||
                 (snap->target_kind == RF_TARGET_GADGET &&
                  snap->target_gadget_type == RF_GADGET_PROP);
```

The distance is in the game's own units, the same ones skill ranges use: a
skill with a 900 range reaches a target whose `target_distance` is 900 or less.
The client stores positions in units of 1/32 inch, so the reader scales them by
32; the Mumble link reports metres with the Y and Z axes swapped, which is
`game = (mumble.x, mumble.z, mumble.y) * 39.37008` for comparison.

```c
#include "reffect_snapshot.h"

const RfSnapshot* snap = reffect_snapshot();
if (snap->resources_valid) {
    printf("HP %.0f/%.0f  Spec %u\n",
           snap->resources.combatant.health_current,
           snap->resources.combatant.health_max,
           snap->spec);
}
for (uint32_t i = 0; i < snap->cooldown_count; ++i) {
    const RfCooldown* cd = &snap->cooldowns[i];
    printf("cd %u rem %.1fs (off-bar=%d)\n",
           cd->skill_id, cd->recharge_remaining_ms / 1000.0f,
           cd->slot_index < 0);
}
```

## Stability

- The struct is POD (no heap pointers, no drop glue), so foreign code can read
  it directly.
- `reffect_snapshot()` is cheap (locks the context, copies bounded arrays).
- Because it is an **in-process** interface, it only returns real data when the
  caller runs inside the game process alongside `reffect_open.dll`.

## Checks

`cargo test -p reffect_core --lib snapshot_ -- --nocapture` asserts the struct
sizes and the offsets of the target classification fields. Compare against
`python -c "import ctypes; ..."` on `scripts/read_snapshot.py`.

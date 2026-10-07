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
| `RfSnapshot` | 12368 |

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

`cargo test -p reffect_core --lib snapshot_sizes -- --nocapture` prints the Rust
sizes; compare against `python -c "import ctypes; ..."` on `scripts/read_snapshot.py`.

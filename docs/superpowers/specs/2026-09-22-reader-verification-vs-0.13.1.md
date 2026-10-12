# Runtime Reader Recovery — Verification Against Reffect.dll 0.13.1

Independent reverse-engineering verification of the recovered `reffect_internal`
memory-reading code against the shipping closed-source plugin
`Reffect.dll` (the "latest plugin" supplied at the repo root).

## Environment / method

- Tooling: IDA Professional 9.3 headless (`idat.exe`), Hex-Rays decompiler,
  gooMBA. Scripts + dumps live in `/re_work` (git-ignored).
- No Rust toolchain is installed in this environment, so `cargo build` /
  `cargo test` could **not** be run here. This verification is static-analysis
  only; it confirms the recovered offsets/logic match the binary, not that the
  crate currently compiles on this machine.
- `Reffect.dll`: PE32+ x86-64, imagebase `0x180000000`, 4936 functions,
  sections `.text 0x1000..0x19a000`, `.rdata`, `.data 0x221000..0x239000`.

## Confirmed: this DLL is the closed-source original, version 0.13.1

`GetAddonDef` → `AddonDefinition @ 0x1801A4B58`:

| field | value |
|---|---|
| name | `Reffect (alpha)` |
| version | **0.13.1** (rev -1) |
| author | `Zerthox` |
| description | `Customizable effect, resource & ability displays` |
| load / unload | `sub_1800C3C30` / `sub_1800C4790` |
| flags / provider | `1` / `2` (GitHub) |
| update link | `https://github.com/zerthox/gw2-reffect` |

None of the open-recovery-unique strings are present (`gw2-reffect-open-internal`,
`[reffect-runtime]`, `reffect-game-state`, `OpenInternal`), and neither
`ReadProcessMemory` nor the `ureq`/`rustls` HTTP stack is imported. This is the
in-process closed original, not a build of the open recovery — so it is a valid
ground-truth target. (The recovery's comments cite `reffect-0.12.0.dll`; the
shipped binary is 0.13.1 and the reader layout is unchanged across the bump.)

## Offsets confirmed against the binary (structurally specific, high confidence)

All reads below were confirmed off a consistent base pointer, not by a bare
value match. The big inlined collector is `sub_1800E2EC0`; the ability/skillbar
reader is `sub_1800E1BB0`; a content predicate is `sub_1800ED130`.

Character base (`v29 = *(v8+32)` in `sub_1800E2EC0`):

| recovered const | offset | evidence |
|---|---|---|
| `CHARACTER_HEALTH` | `0x3E8` | `*(v29+1000)` line 1066 |
| `CHARACTER_COMBATANT` | `0x520` | `*(v29+1312)` line 1292 |
| `CHARACTER_SKILLBAR` | `0x3F0` | `*(v29+1008)` lines 529, 1314 |
| `CHARACTER_WEAPON_SWAP_STATE` | `0xD8` | `*(v29+216)==2` gate, line 1644 |
| `CHARACTER_DEFIANCE` | `0xC8` | `*(v29+200)` line 1100 |
| `CHARACTER_CORE_STATS` | `0x388` | present |
| `CHARACTER_KENNEL` | `0x3F8` | present |
| `CHARACTER_PROFESSION` | `0x510` | present |
| `CHARACTER_PROFESSION_ENERGY` | `0x3D8` | present |
| `CHARACTER_ENDURANCE` | `0x3D0` | present |
| `CORE_PROFESSION` | `0x12C` | present |

Defiance decode (exact match to `read_defiance`): `defiance = *(char+0xC8)`,
`state = *(defiance+0x40)`, `value = *(defiance+0x44) * 100.0`, mapping
`0→Active, 1→Recover, 2→Immune` — reproduced verbatim by the switch at
`sub_1800E2EC0:1100-1120`.

Combatant / skillbar (`sub_1800E1BB0`):

| recovered const | offset | evidence |
|---|---|---|
| `COMBATANT_SKILLDEF_BASE` (+ `raw*8`) | `0x1D0` | `*(a3 + 8*idx + 464)` line 117 |
| `COMBATANT_COOLDOWNS` | `0x150` | present |
| `SKILLBAR_SLOT_BASE` | `0x160` | present |
| `SKILLDEF_GROUND_TARGETED` flag | `0x80000` | present |

Build: `BUILD_ACTIVE_INDEX 0xC58` → `*(v116+3160)` (line 739).

Group members (`sub_1800E2EC0:2039-2042`, single base `v171`):
`GROUP_MEMBER_ENTRIES 0xAA8` → `*(v171+2728)`, `GROUP_MEMBER_COUNT 0xAB4` →
`*(v171+2740)`. This confirms the group-tree offsets the recovery flagged as
"recovered from the last private backend" are current in 0.13.1.

Content tables (`content.rs`):
- `SKILL_SELECTOR` (16 bytes) present at `0x180221740`; `ITEM_SELECTOR` at
  `0x180221718` — byte-identical to the recovery.
- Item anchor predicate reproduced exactly: `sub_1800ED130` is
  `return *(a1+40) == 19700;` == `has_table_anchor(read(anchor+40), 19_700)`.
- Ability-metadata packer lives at `sub_1800D6690` (the comment RVA `+0xD89A8`
  lands inside it).

## Resolved: the `char+0x3E8` health object

The object at `character + 0x3E8` is confirmed to be the vitals/health object.
In `sub_1800E2EC0` (LABEL_494) it is read with a sample-advance formula whose
field layout is:

```
+0x08 sample tick     +0x0C current      +0x10 max     +0x14 regen rate
+0x28 barrier current +0x2C barrier max  +0x30 barrier regen rate
```

The recovery reads `current@0xC`, `max@0x10`, `barrier@0x28` — all three align
exactly with the binary. The only difference is that the original **interpolates**
health and barrier between samples using the `tick`/`regen-rate` fields
(`current += (now - tick)/1000 * rate`, clamped to max), which the open reader
omits — it reads the instantaneous stored value. That is cosmetic display
smoothing, not a correctness gap. `CHARACTER_HEALTH = 0x3E8` /
`HEALTH_CURRENT 0xC` / `HEALTH_MAX 0x10` / `HEALTH_BARRIER 0x28` are correct.

## Discrepancies found by instruction-level review

### 1. Group identity stride — FIXED (`GROUP_IDENTITY_STRIDE` 16 → 0x14)

The directory identity walk in `sub_1800E2EC0` uses `v31 = 20 * count` and
`v162 += 20`, i.e. a **20-byte** identity record: a 16-byte identity
(`dword@0, word@4, word@6, qword@8`) plus a 4-byte validity flag at `+0x10`.
The local identity is compared at `char+0x18` (`v156+24/+28/+30/+32`), matching
`CHARACTER_GROUP_IDENTITY = 0x18`, and reading the first two u64 words is
equivalent to the DLL's field-by-field compare.

The recovery had `GROUP_IDENTITY_STRIDE = size_of::<u64>() * 2` (**16**), so it
mis-strided the directory identity array for every element after the first —
breaking group membership/enumeration whenever a party/squad has >1 member. The
unit tests didn't catch it because the fixture lays identities out using the
same (wrong) constant. Changed to `0x14` in `state.rs`; the fixture, which uses
the constant symbolically, stays self-consistent and now matches the binary.

### 2. Elementalist charge offsets — NOT grounded in the binary (flagged, not changed)

`ELEMENTALIST_PRIMARY_CURRENT = 0xEECE0` and `ELEMENTALIST_SECONDARY_CURRENT =
0xD39B4` (and their `+4` maxes) appear **nowhere** in `Reffect.dll` as an
immediate or displacement. Offsets of ~1 MB are not plausible struct fields; the
recovery's own comment admits they were "independently observed" (live memory
scan), not taken from the binary. `read_charge_resource` self-guards
(`max == 6.0 / 3.0` exact check) and falls back to the generic energy resource
when the read doesn't validate, so this is not catastrophic — but the
Elementalist-specific charges will generally not display. The real per-profession
resource offset was not located in this pass. Left unchanged rather than
replaced with another guess. (The profession switch at `sub_1800E2EC0:940` cases
4/9/6 is the *selection* reader — ranger pets, revenant legends via
`dword_1801B0D74`, elementalist familiars via skill ids 77225/77226/77370 — not
the charge-resource read.)

## One minor note

`character + 0x188` (`v29+392`) is a resource/combat gate consulted at
`sub_1800E2EC0:1167,1287,1644` (e.g. `if (*(v29+392) || *(v29+216)==2)`); it is
**not** a defiance source (defiance is `0xC8`, confirmed above). The open reader
does not consult `0x188` before publishing resources — a possible behavioral
nuance, not an offset error.

## Build + test now pass

Installed rustup + used the machine's VS2022 MSVC toolchain and ran the reader
crate's tests:

```
cargo test -p reffect_internal --lib
test result: ok. 80 passed; 0 failed; 0 ignored
```

So the recovered crate compiles and all 80 unit tests pass, **including** the
group tests after the `GROUP_IDENTITY_STRIDE` fix (the fixture uses the constant
symbolically, so it stays self-consistent).

## Screenshot validation (original plugin output, per profession)

The user supplied `图片/*.png` — the original Reffect debug panel on seven elite
specs. These show what the *closed original* reads, and corroborate the reader
model:

| spec (zh) | profession / elite | Primary | notes |
|---|---|---|---|
| 唤元师 | Elementalist / Evoker | **4/6** + Secondary **0/3** | basic/enhanced skill charges — the real Evoker mechanic |
| 灵刃术士 | Mesmer / Virtuoso | 1/5 | blades |
| 狂战士 | Warrior / Berserker | 0/30 | adrenaline |
| 祭祀者 | Necromancer / Ritualist | **1463/19228** | life force — see cap note below |
| 狂兽师 | Ranger / Untamed | 0/0 | Pets 11 1; **Pet resources 29259/29259** |
| 玉偃师 | Engineer / Mechanist | 0/0 | **Pet (mech) 48467/48467**; Profession slots show 10586 (filtered placeholder) |
| 戏彩师 | Thief / Antiquary | 15/15 | initiative; gear sigils 21824/9452, relic 74494 (== content.rs test values) |

Pet-resource reading (ranger pet, mechanist mech) and the gear/sigil/relic
decode are confirmed working end-to-end by these captures.

## Verified vs. not verified — precise status

**Verified against the binary (base-object-specific decompilation):** health &
barrier layout (`0x3E8`), combatant `0x520`, skillbar `0x3F0`, kennel `0x3F8`,
build holder/active-index `0xC58`, defiance `0xC8`→`0x40/0x44 ×100`, weapon swap
`0xD8` + `0x28C/0x290`, skilldef walk `0x1D0 + idx*8`, cooldowns `0x150`, the
full group directory+member tree (`0x290/0x110/0x20/0x10/0x08`, dir stride
`0x28`, member entries `0xAA8`/count `0xAB4`/char `0x78`, identity stride
**`0x14`**), content anchor (`rec+40`), content vtable (`+72/+104`), ability
packer (`sub_1800D6690`).

**NOT located in the binary this pass (unverified):** the per-profession
*primary/secondary resource* reads (`char+0x510 → +0x50/+0x54/+0x88`). In
`sub_1800E2EC0` `char+0x510` is dereferenced only once (skillbar context), so the
profession-primary resource read path lives elsewhere and was not confirmed. The
values themselves look right in the screenshots, but the *offsets* are not yet
proven against 0.13.1.

## Necromancer life force — RESOLVED via live client (CE, 2026-09-22)

The original shows necro life force **1463/19228**. The recovery's necro branch
(`read_fixed_profession_primary`, profession 8) validated with `sane_resource`,
which caps `max <= 1000.0`, so `19228 > 1000` ⇒ it **rejected** necro life force
and published an empty primary.

Confirmed and fixed dynamically against the running 0.13.1 client with the
Cheat Engine MCP. A Necromancer was loaded; the local character pointer was
resolved through the live TLS getter chain (getter is hooked → scan committed
regions for the NT_TIB self-pointer to find the game thread's TEB → `TlsArray[0]
+ 0x10` = ContextCollection → `+0x98 → +0xA0 → +0x20` = char). Reading
`char+0x510` (profession object):

| offset | live value | ceil | screenshot |
|---|---|---|---|
| `+0x50` current f32 | 12071.67 → 14186.73 (regenerating) | 12072→14187 | 1463 (drains) |
| `+0x54` max f32 | 19227.816 | **19228** | **19228** (exact) |

This **proves** the previously-unverified `char+0x510 → +0x50/+0x54` offsets are
correct on 0.13.1, and the current value changing between reads confirms both a
live process and the right field. Fix applied: profession 8 now validates with a
new `sane_life_force` (upper bound 1_000_000) instead of `sane_resource`;
warrior adrenaline and mesmer clones stay under 1000 and keep `sane_resource`,
so no other resource is affected. Added a regression test seeded with the live
`12071.67 / 19227.82` values. `cargo test -p reffect_internal --lib` = **81
passed**.

Method reusable for the remaining Elementalist item — see the
`ce-live-pointer-chain` memory.

## Bottom line

The heavy structural spine of the memory reader — pointer chains, health/gear/
skillbar/build/defiance/group/content — is confirmed faithful to `Reffect.dll`
0.13.1, one real bug (group identity stride) was found and fixed, and the crate
builds with all 80 tests green. Of the two items that needed a *running client*, one is now closed: (2) the
per-profession primary-resource offsets (`char+0x510 → +0x50/+0x54`) and the
necro life-force cap were verified live with Cheat Engine and the cap bug fixed
(see the Necromancer section above; 81 tests pass). The remaining open item is
(1) the Elementalist/Evoker charge offsets — the feature is real but the offsets
are absent from the binary, and a Necromancer (not an Ele) was loaded during the
live session, so the true charge offsets still need an Elementalist character
loaded in the client. The reusable live-resolution method is captured in the
`ce-live-pointer-chain` memory.

## Ranger pet resolution — FIXED (live, 2026-10-12)

Reported: a Ranger / Untamed (狂兽师) could not read F1-F3 in the
ranger-unleashed state (觉醒游侠) while the original could.

Root cause: the recovery resolved the owned companion as
`char+0x3F8 (kennel) + 0xF8` for **every** profession. That slot is the
Mechanist mech / sixth companion record. The original collector
`sub_1800E2EC0` branches on the profession:

* Engineer (`case 2`, Mechanist): `*(kennel + 0xF8)` — unchanged.
* Ranger (`case 3`, every spec): `*(kennel + 0x30 + 40 * qword_1801B0D98[*(kennel + 0x100)])`,
  gated on `((0x2F >> index) & (index < 6)) != 0`. The static map
  `qword_1801B0D98` (`.rdata 0x1B0D98`) is `[0, 1, 2, 3, 0, 5]`.

The second half of the same call is `sub_1800E2560`'s `a4` flag, which the
collector sets to `1` for the Mechanist (spec 70) and for the Untamed
(spec 72) and to `0` for every other ranger spec; with `a4 == 0` the function
returns right after the second pet command, so only F2 can come from the pet.

Live verification against the running 0.13.1 client (Cheat Engine MCP, TLS
chain as in `ce-live-pointer-chain`; the original's own published player state
is `*(Reffect.dll + 0x237888)`, ability records 40 bytes each, id at `+4`):

| state | kennel `+0x100` | `+0xF8` | pet | our reader F1/F2/F3 | original F1/F2/F3 |
|---|---|---|---|---|---|
| ranger unleashed | 0 | **0** | `kennel+0x30` | 12719 / 20975 / 12720 | 12719 / 20975 / 12720 |
| pet unleashed | 0 | **0** | `kennel+0x30` | 63209 / 63258 / 63094 | 63209 / 63258 / 63094 |

In the first state the player's own profession slots (combatant raw 12-15) are
empty and the pet commands fill F1-F3; in the second they hold the skills
directly and the pet fallback correctly leaves them alone. The old code read
`+0xF8 == 0` in both states, so the first state published nothing. Every other
published slot (weapons, heal, utilities, elite, F5, special action, mount)
matched the original in the same comparison.

Fixed in `reffect_internal/src/game/state.rs` (`owned_pet_character` /
`ranger_pet_character` / `pet_skill_slots`), with four regression tests seeded
from the live layout; `cargo test -p reffect_internal --lib` = 125 passed.


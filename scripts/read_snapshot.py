"""
ctypes reader for the reffect_open DLL snapshot.

The DLL is an IN-PROCESS game addon: `reffect_snapshot()` reads the state the
reader publishes inside the game process. This example therefore only returns
real data when it runs as code that is itself loaded into the game process
(e.g. via a manual load / DllMain hook, or a script host running in-process).
Run it outside the game and every validity flag is 0.

Usage (in-process):
    from read_snapshot import reffect  # see load below

The struct definitions must stay byte-for-byte in sync with reffect_snapshot.h.
"""
import ctypes as C

# --- struct layout (must match reffect_snapshot.h) ---------------------------
class RfResource(C.Structure):
    _fields_ = [("current", C.c_float), ("max", C.c_float)]

class RfCombatant(C.Structure):
    _fields_ = [
        ("health_current", C.c_float),
        ("health_max", C.c_float),
        ("barrier_current", C.c_float),
        ("barrier_max", C.c_float),
        ("health_state", C.c_uint8),
        ("defiance", C.c_uint8),
        ("defiance_percent", C.c_float),
    ]

class RfBuff(C.Structure):
    _fields_ = [
        ("id", C.c_uint32),
        ("stacks", C.c_uint32),
        ("duration_ms", C.c_uint32),
        ("remaining_ms", C.c_uint32),
    ]

class RfAbility(C.Structure):
    _fields_ = [
        ("id", C.c_uint32),
        ("has_ability", C.c_uint8),
        ("ammo", C.c_uint32),
        ("recharge_ms", C.c_uint32),
        ("recharge_remaining_ms", C.c_uint32),
        ("recharge_rate", C.c_float),
        ("ammo_recharge_ms", C.c_uint32),
        ("ammo_recharge_remaining_ms", C.c_uint32),
        ("info", C.c_uint16),
    ]

class RfCooldown(C.Structure):
    _fields_ = [
        ("skill_id", C.c_uint32),
        ("slot_index", C.c_int16),
        ("recharge_ms", C.c_uint32),
        ("recharge_remaining_ms", C.c_uint32),
        ("recharge_rate", C.c_float),
        ("ammo", C.c_uint32),
        ("ammo_recharge_ms", C.c_uint32),
        ("ammo_recharge_remaining_ms", C.c_uint32),
    ]

class RfResources(C.Structure):
    _fields_ = [
        ("combatant", RfCombatant),
        ("health_reduction", RfResource),
        ("endurance", RfResource),
        ("primary", RfResource),
        ("secondary", RfResource),
        ("rate", RfResource),
        ("has_pet", C.c_uint8),
        ("pet", RfCombatant),
    ]

class RfBuild(C.Structure):
    _fields_ = [
        ("specs", C.c_uint32 * 3),
        ("traits", C.c_uint32 * 9),
        ("skill_selection_count", C.c_uint32),
        ("skill_selections", C.c_uint32 * 16),
        ("prof_selections", C.c_uint32),
        ("pets", C.c_uint32 * 2),
    ]

class RfGear(C.Structure):
    _fields_ = [
        ("weapons", C.c_uint32),
        ("sigils", C.c_uint32 * 4),
        ("relic", C.c_uint32),
    ]

class RfGroupMember(C.Structure):
    _fields_ = [
        ("has_account", C.c_uint8),
        ("has_resources", C.c_uint8),
        ("resources", RfCombatant),
        ("buff_count", C.c_uint32),
        ("buffs", RfBuff * 96),
    ]

class RfSnapshot(C.Structure):
    _fields_ = [
        ("now", C.c_uint32),
        ("race", C.c_uint8),
        ("profession", C.c_uint8),
        ("spec", C.c_uint32),
        ("mount", C.c_uint8),
        ("player_position", C.c_float * 3),
        ("resources_valid", C.c_uint8),
        ("resources", RfResources),
        ("buff_count", C.c_uint32),
        ("buffs", RfBuff * 96),
        ("last_screen_border", C.c_uint32),
        ("last_squad_highlight", C.c_uint32),
        ("skill_count", C.c_uint32),
        ("skills", RfAbility * 18),
        ("cooldown_count", C.c_uint32),
        ("cooldowns", RfCooldown * 64),
        ("build_valid", C.c_uint8),
        ("build", RfBuild),
        ("gear_valid", C.c_uint8),
        ("gear", RfGear),
        ("target_valid", C.c_uint8),
        ("target_kind", C.c_uint8),
        ("target_affinity", C.c_uint8),
        ("target_category", C.c_uint32),
        ("target_position", C.c_float * 3),
        ("target_distance", C.c_float),
        ("target", RfCombatant),
        ("target_buff_count", C.c_uint32),
        ("target_buffs", RfBuff * 96),
        ("group_type", C.c_uint8),
        ("group_member_count", C.c_uint32),
        ("group_members", RfGroupMember * 4),
    ]


def load(path):
    """Load the DLL and bind reffect_snapshot()."""
    dll = C.CDLL(path)
    dll.reffect_snapshot.restype = C.POINTER(RfSnapshot)
    dll.reffect_snapshot.argtypes = []
    dll.reffect_snapshot_size.restype = C.c_size_t
    return dll


def read(dll):
    """Return a (possibly empty) RfSnapshot for the current game state."""
    ptr = dll.reffect_snapshot()
    if not ptr:
        return None
    return ptr.contents


if __name__ == "__main__":
    # Path as loaded in the game process. Adjust to wherever the DLL is.
    dll = load(r"C:\path\to\reffect_open.dll")
    snap = read(dll)
    if snap is None or not snap.resources_valid:
        print("no state (expected when running outside the game process)")
    else:
        print(f"race={snap.race} prof={snap.profession} spec={snap.spec}")
        print(
            f"HP {snap.resources.combatant.health_current:.0f}/"
            f"{snap.resources.combatant.health_max:.0f}"
        )
        for i in range(snap.cooldown_count):
            cd = snap.cooldowns[i]
            off = "off-bar" if cd.slot_index < 0 else f"slot {cd.slot_index}"
            print(f"  cd {cd.skill_id} rem {cd.recharge_remaining_ms/1000:.1f}s ({off})")
        print(f"group members: {snap.group_member_count}")

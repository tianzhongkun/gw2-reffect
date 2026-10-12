r"""
loong_reader.dll 快照的 ctypes 读取器。

该 DLL 是进程内游戏插件：loong_snapshot() 读取读取器发布在游戏进程里的状态。
因此只有在游戏进程内运行（例如手动加载 / DllMain 钩子 / 进程内脚本宿主）
才会返回真实数据；在游戏外运行所有有效位都是 0。

用法（进程内）：
    from read_snapshot import load, read
    snap = read(load(r"C:\path\to\loong_reader.dll"))

结构体定义必须与 loong_snapshot.h 逐字节保持一致。
"""
import ctypes as C

# --- struct layout (must match loong_snapshot.h) -----------------------------
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
        ("target_gadget_type", C.c_uint32),
        ("target", RfCombatant),
        ("target_buff_count", C.c_uint32),
        ("target_buffs", RfBuff * 96),
        ("group_type", C.c_uint8),
        ("group_member_count", C.c_uint32),
        ("group_members", RfGroupMember * 4),
    ]


def _bind(dll, names):
    """Bind the first symbol present in the DLL, preferring the new names."""
    for name in names:
        fn = getattr(dll, name, None)
        if fn is not None:
            return fn
    raise AttributeError(f"none of {names} found in DLL")


def load(path):
    """Load the DLL and bind the snapshot entry points."""
    dll = C.CDLL(path)
    snapshot = _bind(dll, ("loong_snapshot", "reffect_snapshot"))
    snapshot.restype = C.POINTER(RfSnapshot)
    snapshot.argtypes = []
    size = _bind(dll, ("loong_snapshot_size", "reffect_snapshot_size"))
    size.restype = C.c_size_t
    dll.snapshot = snapshot
    dll.snapshot_size = size
    return dll


def read(dll):
    """Return a (possibly empty) RfSnapshot for the current game state."""
    ptr = dll.snapshot()
    if not ptr:
        return None
    return ptr.contents


if __name__ == "__main__":
    # Path as loaded in the game process. Adjust to wherever the DLL is.
    dll = load(r"C:\path\to\loong_reader.dll")
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

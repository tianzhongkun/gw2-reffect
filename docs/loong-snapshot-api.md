# Loong Reader 快照 C ABI

`loong_reader.dll` 把当前《激战2》角色状态以 C ABI 快照的形式导出，供**进程内**
的其它模块直接读取（另一个插件 DLL、配套工具，或加载进游戏的 C / C++ / C# / Python
代码），无需序列化，也无需 IPC。

数据由内部读取器持续采集并写入 `loong_core::context::Context`，下面导出的函数
把它展平成固定布局的 `#[repr(C)]` 结构体。

## 导出符号

| 符号 | 说明 |
|---|---|
| `const RfSnapshot* loong_snapshot(void)` | 返回本线程当前状态的快照指针。每次调用都会刷新；在下次调用前于调用线程有效。不会返回 `NULL`。 |
| `size_t loong_snapshot_size(void)` | `RfSnapshot` 的字节大小，供调用方自行分配缓冲区。 |
| `const RfSnapshot* reffect_snapshot(void)` | 旧名别名，等价于 `loong_snapshot()`。 |
| `size_t reffect_snapshot_size(void)` | 旧名别名，等价于 `loong_snapshot_size()`。 |

## 布局

结构体定义在 `loong_snapshot.h`（C/C++/C#），并在 `scripts/read_snapshot.py`
（ctypes）中镜像。字段顺序与容量常量必须与 `loong_core::snapshot` 保持一致。
所有数组都带显式的 `count` / `*_valid` / `has_*` 标志，不依赖哨兵值。

结构体大小（当前构建，x64 小端）：

| 结构体 | 大小 |
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
| `RfSnapshot` | 12408 |

`RfCombatant::health_state`：`0` 存活，`1` 倒地，`2` 死亡，`255` 未知。

## 目标分类与距离

快照自带判断“当前目标是否值得出手”所需的全部信息，无需再做内存读取：

| 字段 | 含义 |
| --- | --- |
| `target_valid` | 是否选中了目标 |
| `target_kind` | `RF_TARGET_*`：玩家、NPC、怪物、攻击目标、采集点、宝箱、机关、宠物或未知 |
| `target_affinity` | `1` 友善，`2` 敌对，`4` 中立，`0` 未知 |
| `target_category` | `RF_CAT_*` 位：实体种类、立场、NPC 等级、采集类型 |
| `target_position[3]` | 目标世界坐标 |
| `player_position[3]` | 玩家世界坐标 |
| `target_distance` | 两者距离，任一未知时为 `-1.0` |
| `target_gadget_type` | 机关目标的原始类型，否则为 `0` |

`target_kind` 是 `target_category` 种类轴压成一个字节的结果，只需要判断目标
是否值得攻击的调用方可以直接 switch：

```c
const RfSnapshot* snap = loong_snapshot();

int attackable = snap->target_kind == RF_TARGET_MONSTER ||
                 snap->target_kind == RF_TARGET_ATTACK_TARGET;

if (attackable && snap->target_distance >= 0.0f &&
    snap->target_distance <= skill_range) {
    use_skill();
}
```

`target_category` 提供细节：NPC 等级用 `RF_CAT_VETERAN` … `RF_CAT_LEGENDARY`，
采集类型用 `RF_CAT_ORE` / `RF_CAT_WOOD` / `RF_CAT_PLANT`，立场用
`RF_CAT_HOSTILE` / `RF_CAT_NEUTRAL` / `RF_CAT_FRIENDLY`。读取器无法分类的目标
会报告 `RF_TARGET_UNKNOWN`、`target_category == 0`、`target_distance == -1.0`，
因此需要分类的调用方会保持不动作，而不是猜测。

客户端把很多可攻击对象归为普通机关：补给站、城门、PvP 控制点都报告
`RF_TARGET_GADGET`，而采集点不会。`target_gadget_type` 提供原始类型，由调用方
用 `RF_GADGET_*` 常量自行判断。只有攻击目标与可破坏类型会被自动提升为
`RF_TARGET_ATTACK_TARGET`：

```c
int attackable = snap->target_kind == RF_TARGET_MONSTER ||
                 snap->target_kind == RF_TARGET_ATTACK_TARGET ||
                 (snap->target_kind == RF_TARGET_GADGET &&
                  snap->target_gadget_type == RF_GADGET_PROP);
```

距离使用游戏自身的单位，与技能射程一致：900 射程的技能能打到
`target_distance` 不大于 900 的目标。客户端坐标以 1/32 英寸为单位，读取器
乘以 32；Mumble link 报告的是米且 Y/Z 轴互换，换算关系为
`game = (mumble.x, mumble.z, mumble.y) * 39.37008`。

```c
#include "loong_snapshot.h"

const RfSnapshot* snap = loong_snapshot();
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

## 稳定性

- 结构体是 POD（无堆指针、无 drop），外部代码可直接读取。
- `loong_snapshot()` 开销很低（加锁上下文，拷贝有界数组）。
- 这是**进程内**接口：只有调用方与 `loong_reader.dll` 一同运行在游戏进程里时
  才会返回真实数据。

## 校验

`cargo test -p loong_core --lib snapshot_ -- --nocapture` 会断言结构体大小与
目标分类字段的偏移。可与 `scripts/read_snapshot.py` 上的 ctypes 结果对照。

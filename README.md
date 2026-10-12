# Loong Reader

Loong Reader 是一个独立的《激战2》(Guild Wars 2) 内存读取插件。
它读取游戏客户端里的角色状态与官方 API 元数据，并通过 C ABI 快照
把数据交给**进程内**的其它模块（例如 ACR 战斗循环）使用。

本仓库只保留**读取**能力：内存读取器、API 元数据，以及一个调试窗口。
不包含任何显示元素、编辑器或触发器。

> **警告**：本插件会读取游戏内存。使用风险自负，并请遵守
> [《激战2》第三方程序政策](https://help.guildwars2.com/hc/en-us/articles/360013625034-Policy-Third-Party-Programs)。
> 不保证在 PvP / WvW 等竞技模式下可用。

## 功能

- **角色状态读取**：种族、职业、精英特长、坐骑、世界坐标
- **资源**：生命、护盾、耐力、职业资源、宠物状态
- **增益/症状**：层数与剩余时间
- **技能栏与冷却**：含不在当前技能栏上的技能（另一套武器）
- **配置**：特性、技能选择、装备武器、法印、古物
- **目标**：分类（种类 / 立场 / 等级 / 采集类型）、原始机关类型、世界坐标与距离
- **队伍**：队伍类型与成员生命、增益
- **官方 API 元数据**：技能与物品信息（名称、图标、类别等）
- **C ABI 快照**：`loong_snapshot()`，供 C / C++ / C# / Python / 其它插件直接读取
- **调试窗口**：在 Nexus 选项里勾选“调试窗口”即可查看各数据域当前状态

## 安装

1. 安装 [Nexus](https://github.com/RaidcoreGG/Nexus) 插件管理器。
2. 把编译出的 `loong_reader.dll` 放进游戏的 `addons` 目录
   （例如 `C:\Program Files\Guild Wars 2\addons`）。
3. 打开 Nexus 插件窗口，必要时点刷新，然后加载 **Loong Reader**。

## C ABI 快照

导出的函数与结构体布局见 [docs/loong-snapshot-api.md](./docs/loong-snapshot-api.md)。
头文件为 [loong_snapshot.h](./loong_snapshot.h)，Python 示例为
[scripts/read_snapshot.py](./scripts/read_snapshot.py)。

```c
#include "loong_snapshot.h"

const RfSnapshot* snap = loong_snapshot();
if (snap->target_valid && snap->target_distance >= 0.0f) {
    /* 依据目标分类与距离决定是否出手 */
}
```

旧的 `reffect_snapshot()` / `reffect_snapshot_size()` 以及
`reffect_snapshot.h` 作为兼容别名保留，既有代码无需修改。

## 从源码构建

需要 Rust 1.95 或更高版本：

```
cargo build --release
```

产物为 `target/release/loong_reader.dll`。

版本号即构建日期（例如 `2026.10.13`），作者为 loong。

## 仓库结构

| crate | 说明 |
|---|---|
| [loong_reader](./loong_reader) | 插件外壳：生命周期、调试窗口、C ABI 导出 |
| [loong_core](./loong_core) | 数据模型、上下文、快照布局（`snapshot.rs`） |
| [loong_internal](./loong_internal) | 内存读取器与官方 API 元数据 |

读取器会校验每一条恢复出来的指针链；当游戏客户端更新导致布局不兼容时，
会按数据域发布具体错误，可在调试窗口中查看不可用的数据域。

## 许可与致谢

本项目以 **LGPL-3.0-or-later** 许可发布，派生自
[gw2-reffect](https://github.com/zerthox/gw2-reffect)（作者 Zerthox），
并受 [WeakAuras](https://github.com/WeakAuras/WeakAuras2) 与
[GW2Clarity](https://github.com/Friendly0Fire/GW2Clarity) 启发。
原始版权与许可声明予以保留，详见 [LICENSE](./LICENSE)。

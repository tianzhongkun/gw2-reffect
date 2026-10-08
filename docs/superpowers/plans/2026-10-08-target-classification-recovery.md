# 目标分类偏移定位

## 目标

把「目标分类」从目前只能判定两种情况的实现，扩展到完整覆盖客户端实际提供的分类信息：

- **阵营**（Disposition）：友方 / 中立 / 敌方
- **实体类型**：玩家 / 宠物 / NPC / 怪物 / 采集点 / 宝箱 / 物件
- **怪物等级**（Rank）：Ambient / Normal / Veteran / Elite / Champion / Legendary / Epic
- **采集专业**：矿（Mining）/ 树（Logging）/ 草（Harvesting）

分类的**类型与触发器已经完成**（`reffect_core::context::TargetCategory` + `ProgressSource::TargetCategory`）。
本文件只描述剩下的数据层工作：定位当前客户端里承载这些字段的偏移。

## 已经确定的事实

以下偏移已在恢复出的 0.13.1 读取图中验证过，可直接使用：

| 路径 | 含义 |
| --- | --- |
| `bridge + 0x298` / `+0x368` | 选中目标描述符（descriptor），前者优先 |
| `descriptor + 0x08` | 目标 kind（`u32`）：0 = 角色布局，10 / 11 = 大型 carrier 物件 |
| `descriptor + 0x10` | 目标对象指针（object） |
| `object - 0x48` | 角色指针（character），仅 kind 0 有意义 |
| `character + 0x98` | agent 指针（仅作形状校验，内容未解析） |
| `character + 0x388` | core stats，其中 `+0x12C` 是 profession |
| `character + 0x3E8` | health 对象 |
| `character + 0xC8` | defiance 对象 |
| `character + 0x18` | 队伍身份（两个 `u64`，用于队伍成员匹配） |

**尚未确定**：agent 类型、阵营、怪物等级、采集专业、目标名称。这些不在 0.13.1 的读取图里，
必须用下面的探针从运行中的客户端采样定位。

## 探针

### 1. 采集日志

`reffect_internal` 里已经加了一个探针：**每选中一个不同的目标对象，就向 Nexus 日志写一次
候选结构的 hexdump**（同一目标只写一次，最高 2 Hz）。输出格式：

```
[reffect-target-classify] seq=1 kind=0 descriptor=0x... object=0x... character=0x... agent=0x... core=None
[reffect-target-classify] descriptor[0x000..0x080]=00000000 ...
[reffect-target-classify] object[0x000..0x080]=...
[reffect-target-classify] object[0x080..0x100]=...
[reffect-target-classify] object[0x100..0x180]=...
[reffect-target-classify] character[0x000..0x080]=...     (仅 kind=0)
[reffect-target-classify] core[0x000..0x080]=...          (仅 kind=0 且可读)
[reffect-target-classify] agent[0x000..0x080]=...         (仅 kind=0 且可读)
```

- 每个区域按 0x80 字节一行分块输出；`character` 覆盖 `0x000..0x540`（含名称、core stats
  指针、health 指针、profession 等），`object` / `core` / `agent` 各覆盖 `0x000..0x180`。
- `character` / `agent` / `core` **只在 `kind=0` 时输出**：读取器一律用 `object - 0x48` 求
  character，对其它 kind 这个地址只是堆上的随机位置，dump 出来全是噪声。
- `seq=N` 是本次会话内的采样序号，事后标注就按它来。
- `???????` 表示该地址不可读，同时也标出了结构实际结束的位置。

日志由 Nexus 写在 `<GW2 安装目录>/addons/Nexus/Nexus.log`（注意是 `Nexus` 子目录，不是
`addons` 根目录），每行前面带 `[Reffect Open]` 标签。把日志复制出来，只保留
`[reffect-target-classify]` 行即可。

探针必须用**当前分支构建的 DLL**才会输出，上游发布的 0.11.3 / 0.13.1 都没有这些行：

```bash
cargo build --release
cp target/release/reffect_open.dll "<GW2 安装目录>/addons/reffect_open.dll"
```

用 `[Reffect Open] [INFO] Reffect v0.13.2 load` 这一行确认装上去的是新构建。

> 本机现状（2026-10-08 核对）：客户端在 `E:\Games\GuildWars2`，`addons\reffect_open.dll`
> 就是本分支的 release 构建。第一次采样（13 个目标）已经跑出来了，见下面「第一次采样」。

### 第一次采样（2026-10-08，13 个目标，无标注）

结论只有一条，但很重要：**`kind` 标签不能直接映射成分类**。

- 13 个目标里 **10 个是 kind 10/11，只有 3 个是 kind 0**。如果按早先的假设把 kind 10/11
  当成「大型地图物件」，绝大多数普通目标都会被错标成 `Gadget`。所以现在 `kind != 0`
  一律返回「未知」，不再猜测。
- 个别 kind=10 样本能读出形似合法的 `agent` / `core` 指针，但堆上指针密集，`object - 0x48`
  又总是落在已映射的堆里，所以这**不足以**证明 kind 10 是角色。需要下一批带标注的数据才能判断。
- descriptor 的 `0x00` 处 vtable 恰好按 kind 分成两组（kind 0 一组，kind 10/11 另一组），
  证实 `kind` 是真实的类型标签，但语义仍未确定。
- 这一批没有标注，也没有覆盖采集点 / 宝箱 / 各级怪物，所以无法用来定位 rank 或 agent type。

下一批采样请严格按下面的清单和顺序来。

### 2. 采样清单

**每个目标至少采样两次**（选中 → 等约 1 秒 → 切走 → 再选回来），并在 `--labels` 里重复它的标签。
只采样一次的话，任何「随采样递增」的字段（计时器、id、地址）都会看起来像完美分类字段。

**按顺序选，并把顺序记下来**：`seq` 从 1 开始，`--labels` 必须与 `seq` 一一对应。中途误选到
别的目标会让后面全部错位，所以宁可重开一次会话重新采。

建议的采样顺序（每项两次）：

| seq | 目标 | 期望分类 |
| --- | --- | --- |
| 1–2 | 队伍里的队友玩家 | Player + Friendly |
| 3–4 | 普通红名怪 | Monster + Hostile + Normal |
| 5–6 | Veteran（"Veteran ..." 铜边） | Monster + Hostile + Veteran |
| 7–8 | Elite（银边，副本内较多） | Monster + Hostile + Elite |
| 9–10 | Champion（金边 + 交叉剑） | Monster + Hostile + Champion |
| 11–12 | Legendary（紫边 + 交叉剑） | Monster + Hostile + Legendary |
| 13–14 | Epic 世界 boss | Monster + Hostile + Epic |
| 15–16 | 中立黄名生物 | Monster + Neutral + Ambient |
| 17–18 | 矿点（如 Mithril Ore） | Gatherable + Ore |
| 19–20 | 树（如 Ancient Sapling） | Gatherable + Wood |
| 21–22 | 草/植物（如 Herb Sprouts） | Gatherable + Plant |
| 23–24 | 宝箱 | Chest |
| 25–26 | 友好 NPC（商人 / 服务） | NPC + Friendly |

一个会话里凑不齐也没关系：**同一个 `--labels` 批次里的样本必须是连续采的**，分几次采就分几批
跑 diff，最后把各批的候选并起来看。

### 3. 比对候选偏移

```bash
python scripts/recovery/diff_target_classification.py \
    --log Nexus.log \
    --labels player,player,normal,normal,veteran,veteran,champion,champion,ore,ore,chest,chest \
    --out target-classification.json
```

输出示例：

```
candidates (constant per label, different across labels):
  region       offset  sep stable  values
  character   0x00304    4    yes  player=0x1 normal=0x3 veteran=0x4 champion=0x6
  core        0x0012c    4    yes  player=0x2 normal=0x0 veteran=0x0 champion=0x0
```

- `stable=yes` 表示该偏移在每个标签内取值一致，`sep` 是它在各标签间取到的不同值个数。
- 工具会警告「只采样一次」的标签。
- 默认只考虑小于 `0x1000` 的值（枚举都很小）；找指针型字段（比如目标名称）时用
  `--max-value 0x8000000000000000`，但那样噪声会大很多。

### 4. 判定方法

- **怪物等级**：值域很小且单调，能对上 7 档（Ambient…Epic），并且在 kind=0 的样本里都稳定。
  注意 Champion 与 Epic 的值要能和 Legendary 区分开。
- **agent 类型**：出现在 `agent` 区域，玩家 / 采集点 / 宝箱 / 怪物取值不同。
  找到后就能把「玩家 vs NPC vs 采集点 vs 物件」一次拆开。
- **阵营**：三类（友方 / 中立 / 敌方）在友好 NPC、中立生物、红名怪之间分开。
- **采集专业**：三类（矿 / 树 / 草）在三个采集样本之间分开，通常是 `agent` 或 `object` 上的小整数。
- **目标名称**：指针型，需要在样本间不同；定位后必须验证它指向的是一段可读字符串
  （UTF-8 或 UTF-16），而不是其它结构。

## 回写

定位到偏移后：

1. 在 `reffect_internal/src/game/state.rs` 顶部按现有风格加常量，并写清楚它是**实测**得到的
   （附日期与验证方式），例如：

   ```rust
   /// 目标怪物等级，实测于 2026-10-xx：Ambient=0 … Epic=6。
   const TARGET_RANK: usize = 0x??;
   ```

2. 扩展 `read_target_classification`：每个轴单独解析，**任一轴读不到就只丢这一轴**，
   不要把整组分类一起丢掉。指针与取值都要过现有的 `valid_pointer` / 范围校验。
3. 补充单元测试：沿用 `LegacyMixedTreeMemory` 的写法，构造带这些字段的假内存，
   断言每个轴映射到正确的 `TargetCategory`，并断言越界值会被丢弃（fail-closed）。
4. 删除或降级 `log_target_classification_probe`（它只是临时的定位工具）。
5. 更新 `docs/triggers.md` 里 `## Target Category` 段落末尾关于「尚未恢复」的说明。

## 验收标准

- 在游戏内逐个选中上表中的目标，Reffect 调试窗口的 **Target category** 与期望分类一致。
- 选不中的目标（例如未加载的物件）显示 `TargetState` 错误而不是错误的分类。
- `cargo test --workspace` 与 `python -m unittest discover -s scripts/recovery` 全绿。

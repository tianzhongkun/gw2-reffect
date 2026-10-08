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

## 已确认的发现

**第二次采样（2026-10-08，29 个已标注样本）确认了 `kind` 标签的语义：**

| kind | 已标注的目标 | 含义 |
| --- | --- | --- |
| 0 | neutral / normal / veteran / champion / elite / npc | **角色**：怪物与 NPC |
| 10 | plant / wood / ore / chest | **采集点与宝箱**（环境物件） |
| 11 | epic | **世界 boss 的 prop 物件**（wiki 里 Epic 内部就叫 "prop boss"） |

也就是说 `descriptor + 0x08` 已经把「角色 / 采集点·宝箱 / 世界 boss」三类分开了。
**但 `kind` 本身还不足以发布分类**：它区分不了玩家和怪物，也区分不了矿 / 树 / 草 / 宝箱，
所以 `read_target_classification` 目前仍然只在「目标是当前队伍成员」时发布结果。

另外两个观察（还只是候选，未验证）：

- `object + 0x78`：neutral=2、normal/veteran/champion/elite=1，看起来是**阵营**
  （中立 / 敌对），但 npc 三个样本取值不一致，需要更多数据。
- `descriptor + 0x40`：kind=10 时 **chest=0xe、ore/wood/plant=0x13**（8 个样本，标签内一致），
  像是物件类型枚举。**只有 2 个宝箱样本，所以还没写进读取器**，下一批多标几个宝箱再确认。
  kind=0 时这个位置放的是 `0x2000xxxx` 形式的实体 id，同一偏移在不同 kind 下含义不同，
  说明 descriptor 是按 kind 多态的（`0x00` 处 vtable 也按 kind 分成两组）。

**第三次采样（2026-10-08）确认的链路事实：**

- **`character + 0x98`（agent）在 kind=0 时就是目标 descriptor 本身**（3 个样本逐字节相同，
  两次会话一致）。所以「目标描述符」其实就是目标的 agent 条目，`agent + 0x08` 就是
  **agent 类型**（0 角色 / 10 采集点·宝箱 / 11 prop）。探针因此不再重复 dump 相同的 agent 区域。
- **RTAPI 不一定存在**：这次会话只加载了 7 个 addon，没有 `RTAPI` 数据链接，
  `RealTimeApi::get()` 返回 `None`。名称标定已改用 **Mumble 链接**（由 Nexus 自己创建，
  日志里能看到 `Created public shared resource: "DL_MUMBLE_LINK"`）。

**还没找到的**：怪物等级（Veteran/Elite/Champion/Legendary/Epic）、玩家 vs NPC、
采集专业（矿/树/草）。角色区域的 `0x000..0x540` 里没有任何内联字符串（已扫描确认），
`string` 扫描也只捞到随机堆字节——**目标名称在更深的地方**。

所以改成用**已知答案去标定**：读取器从 Mumble 拿到自己角色的名字，在自己的 character /
agent / core 里搜这个名字，命中的偏移就是名称字段（见下面 `local` / `name pointer` /
`name inline` 行）。名称很关键：游戏就是用名字前缀（"Veteran …"、"Champion …"）把等级
告诉玩家的，拿到名称就能同时解决等级、采集专业和宝箱，不必再找客户端内部的 rank 字段。

## 探针

### 1. 采集日志

`reffect_internal` 里已经加了一个探针：**每选中一个目标，就向 Nexus 日志写一次候选结构的
hexdump**（每次选择恰好一份）。输出格式：

```
[reffect-target-classify] seq=1 kind=0 descriptor=0x... object=0x... character=0x... agent=0x... core=None
[reffect-target-classify] local character=0x... agent=0x... core=0x... name="My Character"
[reffect-target-classify] region local readable 0x4000/0x4000
[reffect-target-classify] name pointer local-agent+0x0a0 -> 0x... (ascii)
[reffect-target-classify] name inline local+0x2c0 (utf16)
[reffect-target-classify] heap name 0x144e50adfc0 (utf16) page=0x144e50ad000
[reffect-target-classify] heap name referenced by local+0x0d0 (anchor 0x14595ba6830)
[reffect-target-classify] heap name scan done: 64 hit(s) over 0x... bytes
[reffect-target-classify] character[0x000..0x080]=...     (仅 kind=0，覆盖 0x000..0x1000)
[reffect-target-classify] agent[0x000..0x080]=...         (仅 kind=0，覆盖 0x000..0x1000)
[reffect-target-classify] core[0x000..0x080]=...          (仅 kind=0，覆盖 0x000..0x400)
[reffect-target-classify] descriptor[0x000..0x080]=...    (仅 kind≠0，覆盖 0x000..0x400)
[reffect-target-classify] object[0x000..0x080]=...        (仅 kind≠0，覆盖 0x000..0x400)
[reffect-target-classify] string character+0x0d0 -> 0x2a5... = "..."
```

- **kind=0 只 dump `character` / `agent` / `core`**：descriptor 就是 `character + 0x98`（即 agent），
  object 就是 `character + 0x48`，两者都是上面范围内的切片，重复 dump 没有意义。
- **kind≠0 只 dump `descriptor` / `object`**：这时 `object - 0x48` 不是角色，character/agent/core
  全是堆上的随机位置。
- 每个区域按 0x80 字节一行分块输出。
- **名称标定（`local` / `name pointer` / `name inline` / `heap name`）**：每次会话只跑一次。
  读取器从 **Mumble 链接**（Nexus 自己创建，一定存在）拿到自己角色的名字，然后：
  1. 在自己的 character（`0x000..0x4000`）、agent（`0x000..0x1000`）、core（`0x000..0x400`）
     里搜这个名字——内联、指针目标、以及「指针指向的结构的前 0x200 字节」三种形态都搜，
     单字节和 UTF-16 两种编码都搜；
  2. 如果都没命中，就**扫整个可写私有内存（堆）**找这个名字的每一份拷贝（最多 64 份、
     最多 4 GB），并对每个命中反查上面三个区域里有没有指向它的指针
     （`heap name referenced by ...`）。

  因为名字是**已知**的，命中就是确定的，不像「找长得像字符串的东西」那样被随机字节淹没。
  注意：**不要用 RTAPI 拿名字**——它是独立 addon，不一定装（2026-10-08 实测没装，
  `RealTimeApi::get()` 返回 `None`，标定静默不跑）。
- 每个区域还会打一行 `region <名字> readable <实际>/<请求>`，用来判断结构真实大小。
- `string <区域>+<偏移> -> <指针> = "..."`：该偏移上的指针指向一段可打印**单字节**字符串
  （4 字符以上、含字母、以 NUL 结束），每个区域最多 12 条。**不接受 UTF-16** —— 随机堆字节
  解码出来几乎都是合法 CJK 码位，会把真正的命中埋掉（实测 40 行全是垃圾）。
- `seq=N` 是本次会话内的采样序号，调试窗口会显示当前选中目标的序号，用它来标注。
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
> 就是本分支的 release 构建。

### 2. 在游戏里标注（不用记顺序）

打开 Reffect 的调试窗口（`Reffect Open 调试`），里面有一段 **Target probe**：

```
Target probe:  seq 7   not tagged
Tag the selected target, then select the next one:
[player] [npc] [normal] [veteran] [elite] [champion] [legendary]
[epic] [neutral] [ore] [wood] [plant] [chest]
```

流程：**选中目标 → 看 seq → 点一下对应的按钮 → 换下一个目标**。

点按钮会往日志里写一行：

```
[reffect-target-classify] label=champion seq=7
```

于是**日志自己带标注**，`seq` 只是给你核对用的（显示 `tagged "champion"` 说明这次点击生效了）。
选目标的顺序完全无所谓，中途误选也不会污染数据——只要那个目标没点按钮，它就是未标注样本，
分析时会被直接丢掉。

**每个目标至少标注两次**：选中 → 点按钮 → 切到别的目标 → 再选回来 → 再点一次。
只采一次的话，任何「随采样递增」的字段（计时器、id、地址）都会看起来像完美分类字段；
两次就能让它们通不过「标签内一致」检查。

建议覆盖的目标：

| 目标 | 点哪个按钮 |
| --- | --- |
| 队伍里的队友玩家 | `player` |
| 普通红名怪 | `normal` |
| Veteran（"Veteran ..." 铜边） | `veteran` |
| Elite（银边，副本内较多） | `elite` |
| Champion（金边 + 交叉剑） | `champion` |
| Legendary（紫边 + 交叉剑） | `legendary` |
| Epic 世界 boss | `epic` |
| 中立黄名生物 | `neutral` |
| 矿点（如 Mithril Ore） | `ore` |
| 树（如 Ancient Sapling） | `wood` |
| 草/植物（如 Herb Sprouts） | `plant` |
| 宝箱 | `chest` |
| 友好 NPC（商人 / 服务） | `npc` |

一个会话里凑不齐也没关系，分几次采、每次跑一遍 diff，最后把候选并起来看。探针在一次会话里
最多输出 256 个目标，到上限会写一行提示并停止。

### 3. 比对候选偏移

```bash
python scripts/recovery/diff_target_classification.py --log Nexus.log --out target-classification.json
```

不需要 `--labels`：标注直接从日志里读。如果确实想按位置手动指定（比如用旧版探针采的日志），
再传 `--labels player,player,normal,normal,...`，它会覆盖日志里的标注。

输出示例：

```
parsed 26 probe dump(s)
  #1 seq=1 label=player kind=0 object=0x1d0a43098b8 regions: ...
  ...
warning: ignoring 3 untagged sample(s): seq 4, seq 11, seq 12

candidates (constant per label, different across labels):
  region       offset  sep stable  values
  character   0x00304    4    yes  player=0x1 normal=0x3 veteran=0x4 champion=0x6
  core        0x0012c    4    yes  player=0x2 normal=0x0 veteran=0x0 champion=0x0
```

- `stable=yes` 表示该偏移在每个标签内取值一致，`sep` 是它在各标签间取到的不同值个数。
- 未标注的样本会被丢弃并列出 `seq`；只标注一次仍然会给出「只采样一次」的警告。
- 默认只考虑小于 `0x1000` 的值（枚举都很小）；找指针型字段（比如目标名称）时用
  `--max-value 0x8000000000000000`，但那样噪声会大很多。
- **不同 kind 要分开跑**：`--kind 0` 只看角色，`--kind 10` 只看采集点/宝箱。工具只比较
  「在所有样本里都可读」的偏移，而 `character` / `agent` / `core` 只有 kind=0 才有，
  混在一起会把所有角色偏移全部排除掉（这个坑踩过一次）。

### 4. 判定方法

**优先找目标名称**（探针的 `string` 行）：

```
[reffect-target-classify] string character+0x0d0 -> 0x2a5... = "Veteran Flame Legion Charr"
```

同一个标签的两个样本应该给出同一个名字；不同标签给出不同名字。找到名称后，等级就能用
前缀判断（`Veteran ` / `Champion ` / `Legendary ` / `Elite `），采集点用名字里的
`Ore` / `Sapling` / `Herb` 之类关键词判断，宝箱用 `Chest` 判断——这条路线不需要再找客户端
内部的 rank 字段，而且和游戏实际告诉玩家的信息一致。**注意名称可能被本地化**（中文客户端
会是中文名），所以关键词表要按语言分别维护，或者退回到下面的偏移方案。

如果确实要读客户端内部的字段：

- **怪物等级**：值域很小且单调，能对上 7 档（Ambient…Epic），并且在 kind=0 的样本里都稳定。
  注意 Champion 与 Epic 的值要能和 Legendary 区分开。
- **阵营**：三类（友方 / 中立 / 敌方）在友好 NPC、中立生物、红名怪之间分开。目前候选是
  `object + 0x78`（neutral=2、hostile=1）。
- **采集专业**：三类（矿 / 树 / 草）在三个采集样本之间分开，通常在 `object` 或 `agent` 上。
- **玩家 vs NPC**：玩家有队伍身份（`character + 0x18`）、profession 是 1–9；NPC 不是。

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

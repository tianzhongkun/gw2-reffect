# Reffect 当前客户端运行时读取恢复设计

## 目标

恢复原私有 `reffect_internal` 的运行时数据路径，使当前 GW2 客户端中的内存字段能够发布到 `reffect_core::context::Context`，同时禁止 Nexus 覆盖本地构建。

## 已验证事实

- `reffect.dll` 已由 Nexus 成功加载，但 `Nexus.log` 没有状态读取成功记录。
- 现有 `GameThreadFinder` 从后台线程枚举 TLS 并暂停线程；当前客户端的 `ContextCollection` getter 必须在 `GameThreadUpdate` 所在线程调用。
- 当前客户端中 `ContextCollection + 0x98 -> ChCliContext + 0x98 -> local character`、角色生命、技能、Build 和装备链均已由同进程的 OmniReader 验证。
- `character + 0xD0` 是混合 inventory/effect 树，不能作为旧版本 Buff 表直接使用。
- 当前 `decode_snapshot` 把 `resources` 和 `buffs` 设为必需值，任一失败会阻止所有内存域发布。

## 方案比较

1. 保留后台 TLS 枚举，仅把 Buff 改成可选：改动小，但根本无法保证获取 Context，淘汰。
2. 在 Rust 中直接 hook `CharClientContext` TLS 访问器，在游戏线程捕获其返回的 Context 指针；worker 读取已发布指针并按域发布：使用已验证的当前客户端链，推荐。
3. 把 OmniReader C++ DLL 作为桥接：可复用现有实现，但引入双 DLL 生命周期、ABI 和发布耦合，淘汰。

## 架构

`game::hook` 负责扫描并直接 detour 当前客户端的 Context getter。对于干净入口，它复制完整的六字节 RIP-relative TLS 索引加载到跳板并重定位；已有 `E9` 或绝对 detour 时则链式调用。后置回调使用 getter 的返回值验证 `ContextCollection -> ChCliContext -> local` 链，并以原子值发布最后一次有效指针；worker 不调用游戏 TLS 访问器。

`game::state` 的 worker 不再暂停游戏线程或枚举 TLS。它从捕获器取得本地角色地址后，用 bounded `ReadProcessMemory` 构建分域快照。每一域以 `Option<Result<...>>` 表达独立可用性；发布时只更新成功域，并对不可用域设置对应错误，绝不因 Buff、目标或职业资源失败抹掉生命与技能栏。

## 读取范围与失败语义

- 立即恢复：身份、生命/屏障、耐力、已验证职业资源、技能栏/冷却、Build、装备、目标生命。
- Buff：仅在得到可验证的 effect 根链与节点形状后发布；否则只设置 `Error::Buffs`，其他域继续显示。
- Group：在未恢复稳定遍历链时保持 `Error::Group`，不伪造空队伍。
- 每个边界写带固定前缀的诊断日志：签名、hook 安装、捕获次数、Context 验证、各域第一次失败。正常轮询不重复刷日志。

## 验证

单元测试必须覆盖签名解析、Context 发布、过期捕获拒绝、以及“Buff 失败不阻止资源/技能发布”。运行时验收通过 Nexus 日志中的 hook/capture 诊断和 Reffect 调试界面完成；同一会话至少看到有效生命与技能栏，并在 Buff 不可用时其余域保持可用。

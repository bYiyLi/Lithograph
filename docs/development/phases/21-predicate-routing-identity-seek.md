# Phase 21：MATCH 谓词下推、Element ID Seek 与 OOM 回归

**状态：done（本地实现与验收）。** P21-01～P21-05、A21-01～A21-08 已通过；本地验收完成，目标进入 v0.3.5；Commit、Push、Tag、Release 和远端验收另行核对。

## 目标、依赖和范围

按 [Query Engine 正式合同](../../design/query-engine.md#predicate-routing-and-identity-seek) 修复多 Pattern Part 的过滤时机和 Node Element ID 访问效率，在真实读/写 Cypher、历史 State、Graph View 与 staged transaction 中保持结果、内存、取消和回滚语义。外部算法证据见 [研究记录](../../research/query-optimization-evidence.md)。依赖已完成 Phase 04–07、15、19–20；实施前以当时实际 HEAD 和 v0.3.4 依赖/源码核对，不用本计划推断未来发行状态。

本阶段是一个开发及验收单位，不修改 KG OS，不添加新 application-facing API/选项、Cypher 方言、Index 对象、存储格式或新的常驻日志系统；Hash Join/通用 Join Reordering/SIP/全局 CBO 不在本阶段。

## 实施顺序与交付

| 工作 | 需要实现或留下的证据 | 对应验收 |
| --- | --- | --- |
| P21-01 安全复现与基线 | 隔离数据库复现两节点一关系 write MATCH + CREATE；记录真实旧 plan、读写访问、RSS/TEMP 和提交行为；大型旧笛卡尔积必须有预算限制 | A21-01 |
| P21-02 安全谓词路由 | 从 AST 依赖识别可早执行的确定性 AND 分量；Read 与 Write MATCH 在 Part 完成后立即过滤、保留 residual 与非法移动边界 | A21-02、03 |
| P21-03 Element ID Seek | 在适用 equality 谓词下生成真实 Snapshot Node ID 访问路径；解析 ID 与权限/Label/Graph View/历史/staged 语义完整 | A21-04 |
| P21-04 内存与取消 | 对 MATCH→CREATE 中间行、已有 spill / memoryLimitBytes / fail-closed/cancel 检查并修复真实漏洞，不引入重复资源框架 | A21-05、06 |
| P21-05 真实集成与 Review | SQL scalar/rows、Cypher TCK、版本控制、压力/性能、现有 quality/CI、最终 diff；按发现修复并重跑影响的测试 | A21-07、08 |

## Acceptance

- **A21-01**：固定 fixture seed、Node/Relationship 数、语句/参数形状、schema、pinned Commit、实际 SQLite/扩展构建、执行适配器、主机与预算。旧版的小规模执行说明计划与工作量；事故证据只能确认 OOM，**不假定具体算子唯一致因**。避免直接执行 100k×100k 的无界旧 Cartesian。
- **A21-02**：同一 MATCH 两个独立 Part 上的纯单变量 AND 谓词实际在下一个 Part 迭代之前求值，EXPLAIN/PROFILE 和可观察的访问计数与物理执行一致。不能仅替换 EXPLAIN 字符串。
- **A21-03**：跨 Part、OR、NULL、OPTIONAL、子查询/变量作用域、非确定性、失败时序、重复行、已绑定变量、路径和写入边界均不改变正确 Cypher 行为；不可安全下推时保守使用原计划，验证正反用例与 TCK。
- **A21-04**：elementId(n) = $id 的左右形式、参数、非法值、缺失/删除、Graph View 隐藏、不同历史 Commit 和 explicit transaction 内 CREATE/DELETE 均返回与原执行相同结果/错误；只有通过独立成员语义测试后才支持 IN。真实 Node ID 查找不随无关 Node 数近似线性增长。
- **A21-05**：新/已有端点、同一节点自环、一次 CREATE 的正确边数；单个 explicit transaction 无部分 durable Commit；错误/取消/预算拒绝后按既有 fail-closed 合同回滚，旧 Commit 与 Branch Head 可复核。
- **A21-06**：在 1 GiB/no-swap 的隔离 Linux amd64 容器中测 cgroup memory.peak/oom_kill、RSS、SQLite/TEMP、资源拒绝、cancel 与重启；成功无 OOM，不以 memoryLimitBytes（非 RSS 硬上限）替代容器实测。诊断不输出原始查询/业务参数或创建默认日志服务。
- **A21-07**：固定目标及返回规模，1k/10k/100k Node 合成图，10 倍无关数据增长，记录真实访问/VM 工作和 wall time；新计划不会生成随无关 Node 数平方扩张的候选，也不触发不必要的全图扫描。冷/热、read/write 分开测，构建与 fixture 加载单独计时。
- **A21-08**：真实 SQLite 3.45+ 的 lithograph()/lithograph_rows() 与原有历史/Graph View/Schema/Index/Mutation/Streaming/Cancel 及适用 Cypher 25 测试、现行仓库质量门禁与 CI、文档链接和逐项 Review 均有本次源码对应的实际通过记录；不能改 Profile/覆盖分母或弱化阈值。Release/Push/Tag 需要各自授权和验收，不凭 Phase 计划代表已交付。

## 2026-11-10 实现与最终验收

**验收基线**：本地 `main` HEAD `e245555` 之上的未提交工作区；macOS arm64 / Rust 1.98.1 / SQLite 3.51.0，以及隔离的 Linux amd64（QEMU）/ SQLite 3.54.1。全部采用真实 SQLite loadable extension 和合成测试数据。2026-11-09 KG OS 生产事故只确认容器 OOM，并不证明旧 Cartesian 是其唯一原因。

### 实现

- `query/plan/predicate.rs` 仅对可安全路由的 AND 前缀做单变量、按原求值顺序的提前拒绝；错误、NULL、跨 Part、OR、OPTIONAL、子查询、重复绑定和不安全条件保留 residual，且保留原 WHERE。
- `elementId(node)=$id` 的左右等式选择真实版本化 Snapshot 的 `NodeIdSeek`，命中后仍验证 Node 存在性、Label、Graph View、访问控制和原 WHERE。非 String/非法/NULL 等情况遵守原有类型及过滤合同；`IN` 暂不扩展。
- Read、Write MATCH 与 program cursor 共用执行路径。EXPLAIN/PROFILE 反映 NodeIdSeek、提前 Filter 和实际算子访问。只有仍需在多个 Label 中选择扫描起点的 Node 才收集统计，不为单 Label 或已选择 ID/Property Index Seek 的 Node 无谓读全局统计。
- 复用原有流式执行、取消、资源预算、显式事务和回滚；没有新增 application-facing API、Cypher 方言、存储格式、常驻日志或 KG OS 专用语义。

### Acceptance 矩阵

| 验收 | 状态 | 真实执行证据 |
| --- | --- | --- |
| **A21-01 基线与受控复现** | PASS | 80 Node 双 Pattern Part，v0.3.4 旧构建计划含 `LabelIndexScan → Cartesian → LabelIndexScan → Filter`、PROFILE `dbHits=12,960`；本工作区相同形状的单点查询为 6。80×80=6,400 是理论组合而非内存驻留；不执行无界 100k×100k 旧计划，也不宣称已查明生产 OOM 唯一致因。 |
| **A21-02 提前过滤** | PASS | 真实 Read / MATCH→CREATE 的 Filter 发生在下一 Pattern Part 的 Cartesian 扩展之前；EXPLAIN、PROFILE、实际 `dbHits` 和 SQLite VM 工作量相符，不只是计划字符串变化。 |
| **A21-03 Cypher 保守正确性** | PASS | OR、NULL、OPTIONAL、跨 Part、类型与错误时序、重复绑定、路径、历史 Graph View、写入边界、适用 Cypher 25/TCK；不安全场景仍走 residual/旧计划。 |
| **A21-04 Snapshot ID Seek** | PASS | 左右等式、参数与非法类型、缺失/删除/隐藏节点、Label、历史 Commit、staged CREATE/DELETE/abort、重启后身份读；两个 NodeIdSeek 各 3 `dbHits`，合计 6。 |
| **A21-05 Write 与失败原子性** | PASS | 新旧端点、自环、单条关系创建、一个 explicit transaction 中多次写入只形成一次 durable Commit；预算拒绝、取消、错误后回滚及 Branch Head/历史复验。 |
| **A21-06 Linux amd64 1 GiB/no-swap** | PASS | Linux x86_64 GNU extension、SQLite 3.54.1、容器 `memory.max=1073741824` / `memory.swap.max=0`，100k Node 无 Checkpoint cgroup `memory.peak=346,009,600` bytes，有 Checkpoint `208,941,056` bytes，均 `oom=0`、`oom_kill=0`；取消后恢复、单边写入、TEMP page count=5 均通过。退出容器并重新打开两份 DB 后，Relationship、Node identity 与 `PRAGMA quick_check` 通过。使用 QEMU 运行，不与原生平台做绝对吞吐横向比较。 |
| **A21-07 固定规模性能与 VM** | PASS | macOS 独立进程下 1k/10k/100k Node，`NodeIdSeek dbHits=6` 均不变；两侧属性过滤访问 4k/40k/400k，线性而非平方。Checkpoint 后 read P50 分别 **3.54/3.02/3.11 ms**，未 Checkpoint 为 **9.79/78.71/916.27 ms**；fixture/Checkpoint 构建另计。宿主 `sqlite3_trace_v2` + `sqlite3_stmt_status` 对 1k/10k Checkpoint fixture 分别测得 **2,149 VM steps、97 fullscan steps、36 statements**，10 倍无关 Node 增长不增加 VM 工作量。 |
| **A21-08 整合与 Review** | PASS | `cargo make quality` exit 0（Regions **84.93%**、Functions **86.56%**、Lines **88.13%**，门槛未变；`snapshot_stats.rs` 文件 Line **95.86%**）；`./scripts/ci.sh` exit 0，包含 SQLite 3.45.0 / 3.53.4 的真实加载、Cypher TCK、历史/Schema/Index/Mutation/Streaming/Cancel、SQL scalar/rows、Developer Docs、Phase 21 专项和 SQLite VM 回归。文档链接、untracked whitespace 与最终 diff 检查通过。 |

### 资源、安全与局限

- `RUSTSEC-2026-0459` 针对 `libsqlite3-sys` 使用 bundled SQLite 的情形；当前 all-features Cargo graph 未启用 bundled，扩展依赖宿主 SQLite。`deny.toml` 为该 advisory 配置可追溯例外；`scripts/check-sqlite-host-abi.sh` 在依赖/feature 改变或 **2027-02-08** 复核期限到达时 fail-closed。当前 `cargo deny check` 及正常/到期反例测试通过。宿主 SQLite 对不可信 DB/WAL 文件的风险由 [Runtime](../../design/runtime.md) 独立约束，例外不等于对宿主版本的豁免。
- 曾试用 `rusqlite 0.41.0`，但 SQLite 3.45.0 host 真实加载不兼容，已回退至兼容版本；不通过抬高最低兼容版本掩盖安全告警。
- 无 Checkpoint 的 ID 查询仍可能承担版本化 Snapshot overlay 解析成本；100k Node 双属性过滤缺少对应 Index 时仍需线性扫描。两者不属于本阶段已实现的 NodeIdSeek 常数访问路径，也不以新增 Hash Join、通用 Join Reordering 或 SIP 取代原定最小方案。
- 本阶段 synthetic fixture 与基准覆盖到 100k Node，构建、初次查询、热态、读/写、取消、RSS、TEMP、容器和 Checkpoint 分别记录。生产 OOM 没有在本地完全同因重现，不能将本阶段改善等同于生产修复部署。
- 此处验收记录属于发布前的本地工作区；**不能以本地验收代替 Commit、Tag CI、跨平台 Release 或部署验收**。既有 `CY25-2026.08` 语言兼容 profile 和覆盖分母没有改变，因此 `docs/development/cypher25-compatibility.md` 无需改变已有 feature 完成状态。

### 复验入口

- `cargo make quality` 与 `./scripts/ci.sh`。
- `tests/phase21_predicate_routing_sql.py`、`tests/phase21_predicate_routing_bench.py`（1k/10k/100k，Checkpoint/非 Checkpoint）。
- `scripts/phase21-vm-work.sh` 与 `scripts/check-sqlite-host-abi.sh`。
- Linux 容器：`--platform linux/amd64 --memory 1g --memory-swap 1g`，核验 `memory.max`、`memory.swap.max`、`memory.peak`、`memory.events` 和重启读取。当前运行输出保存在本机忽略目录 `target/phase21-review/`，不纳入代码仓库。

## 完成判定与跨项目边界

P21-01～P21-05 和 A21-01～A21-08 均满足本地工作区验收。Phase-level Review 未发现剩余的 Phase 21 阻断性 finding；这不是对所有未测试场景无缺陷的保证。完成 Phase 不授予 Commit、Push、Release 或生产部署权限。

KG OS 是否以及何时升级包含本阶段改动的 Lithograph 正式版本，由两个项目各自发布、集成和部署流程单独决定，Lithograph 不包含 KG OS 业务语义。

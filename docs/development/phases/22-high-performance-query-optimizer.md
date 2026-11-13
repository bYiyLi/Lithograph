# Phase 22：高性能查询优化器与可预算执行引擎

**状态：`in_progress`（S22.1–S22.7 已完成局部验收；S22.8 实施中）。** 本项目只有**一个 Phase 22、八个内部 Stage**。所有 Stage 和全部算法均为必交付；只完成部分 Stage、已有若干基础、计划通过 Review，都不能标记 Phase `done`。当前实现与实测证据续记于第 21–25 节，S22.5 最终记录见[第 24.9 节](#stage-s22-5-acceptance)，S22.6最终记录见[第25.7节](#stage-s22-6-acceptance)，S22.7最终记录见[第25.11节](#stage-s22-7-acceptance)；第 15–20 节保留先前规划审核、实施和修复的历史记录。

**仓库复核（2026-11-10）**：本地 `main` HEAD 为 `3f1f31e`，本地 `v0.3.5` Tag 指向该 Commit；Phase 21 实现已进入此源码基线。复核开始时工作区干净，随后本次文档校正产生三份未提交文档修改。旧的 `e245555` + 未提交 Phase 21 是合并计划时的历史状态，不再代表当前源码。Tag 存在不等于远端 Release 制品已经完成验证；实施时仍须重新核对 HEAD/dirty tree、依赖、源码/SQLite runtime 和 Tag/资产状态。**主工作区保留任务修改；独立`codex/phase22-query-optimizer-ci`分支已实际提交并推送至`34da2d0bf3792d8667b3f939ed73cd45d4580b11`。该精确提交的CI四项与六平台Matrix全部通过；S7四档、完整输出、最终版本受影响性能/512KiB资源及独立Review已闭环，见第25.11节。S8现已启动实现；未合并、发布或部署。第21–25节记录各自源码epoch及验收，旧审核章节仅为历史证据。**

## 1. 最终目标、权威设计和边界

在既有 Lithograph SQLite loadable extension、Cypher 25 Frontend、Logical/Physical Planner、Streaming、`QueryResources`、SQLite TEMP、Versioned Snapshot 和 Standard Index 上形成**实际可用的 Cost-Based Planner 与 Budgeted Executor**。先解决访问路径、统计、历史解析和资源预算，再实现 DPhyp/Greedy、Index Nested Loop、Grace Hash/Spill；随后**全部实现** Semi-mask/SIP、Join Dynamic Filter、TopK Pushdown、Factorized/ASP-Join 和 WCOJ/Multiway Intersection。

**不允许**将任何算法降格为“后续可选”。每项算法必须至少有一个确实启用、真实物理工作量改善的测试场景，同时对不安全/不适用场景正确回退。

| 拥有职责 | 设计真源 / 研究依据 |
| --- | --- |
| Planner/Executor、Cypher 语义、错误/访问控制 | [Query Engine](../../design/query-engine.md#optimizer-and-budgeted-executor)、[Join 适用条件](../../design/query-engine.md#join-algorithms-and-graph-optimizations) |
| Snapshot/Checkpoint/Commit/Layer、维护事务 | [Storage](../../design/storage.md#tiered-checkpoint-policy) |
| Property Type proof 与版本化 Index | [Schema and Indexes](../../design/schema-and-indexes.md#range-text-point) |
| 性能标准、资源、Linux cgroup、非回退目标 | [Runtime](../../design/runtime.md#optimizer-join-stress)、[Performance Evidence](../../design/runtime.md#performance-evidence)、[Fixed Targets](../../design/runtime.md#performance-targets) |
| 语言正确性与测试 Oracle | [Cypher Compatibility](../../design/compatibility.md)、[Profile Inventory](../cypher25-compatibility.md) |
| 现有算法与真实基线 | [Phase 21](21-predicate-routing-identity-seek.md)、[外部算法研究](../../research/query-optimizer-roadmap-evidence.md) |

本计划负责 Feature、依赖、Acceptance 和实现记录；技术语义由各设计真源拥有，不在 Phase 文档复制第二套合同。发现合同缺口先按现有源码和证据补齐对应最小设计。

### 1.1 必须维持的产品不变量

- 不 Fork SQLite、不引入独立数据库 Server/Daemon、不增加第二套 Planner/Executor/SQL-Cypher API 或特有 Cypher 方言；不把 KG OS 的 Knowledge/Agent/Ontology 语义写入 Lithograph。
- Commit/Layer/Schema/IndexDefinition immutable，Branch 可移动；Checkpoint/Index physical/Statistics/Mask 是可重建的 derived/query-local 数据，不使用当前 main 可变索引冒充历史 Snapshot。
- 所有算子保持 Cypher 25 的行数/重复行、独立 Relationship 身份、Path uniqueness、NULL/TYPE_ERROR/异常时序、OPTIONAL/Apply/Subquery、Graph View/权限、ORDER BY/ties、Search TopK/ANN recall 和事务写入/回滚。
- 继续使用现有 pull-based cursor、`QueryResources`、`memoryLimitBytes`、`sqlite3_interrupt` 和 SQLite TEMP；`memoryLimitBytes` 不是进程 RSS 硬上限，必须用 1 GiB/no-swap Linux cgroup 实测。
- Read-only query/EXPLAIN/历史读取不暗写 main 自动建 Checkpoint/Index；不为性能隐藏修改 Schema/Commit，不自动引入新持久调参或无消费者的抽象/依赖。
- **作用域与时钟**：单次普通 execution pin 一个 immutable Commit，但后续 clause 应观察前序 staged writes；`CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 的每个 inner batch 可独立 pin 不同 Branch head。`datetime.transaction()/statement()/realtime()` 与非确定性表达式不能因 Join/Pipeline 重排被错误合并、缓存或提前执行，测试以 [Snapshot Pinning](../../design/query-engine.md#snapshot-pinning) 和 [Temporal Clock](../../design/query-engine.md#temporal-clocks) 为准。
- **权限与可观察信息**：`graphView` 是调用方指定的可见性范围，不等同于完整认证机制；`readAccess.query`、`readAccess.result` 的边界各自遵守 [Query Access](../../design/query-engine.md#query-access-controls)。优化器使用统计、Index/Seek 或 PROFILE 的选择不得绕开查询权限、通过被禁止的属性进行搜索或泄露隐藏属性的分布；`EXPLAIN/PROFILE` 的访问准备检查不因零行结果而跳过。

## 2. 八个 Stage 的依赖关系（非独立 Phase）

| Stage | 完整交付 | 直接进入条件 | 当前开发状态 |
| --- | --- | --- | --- |
| **S22.1** | Planner Access/Cost candidates + Budgeted Executor | Phase 21 真实实现复核 | 局部验收通过（第 21 节） |
| **S22.2** | Tiered Checkpoint + Snapshot 成本控制 | S22.1 局部验收 | 冻结局部验收通过；新增 metadata 类型边界已修复并通过 Native/Linux 复验（第 22 节） |
| **S22.3** | Versioned Statistics + Relationship ID Seek + Predicate/Scan | S22.1–2 | 局部验收通过（第 22.7 节）；新冻结、独立 Review、资源与性能复核闭环 |
| **S22.4** | DPhyp/Greedy Join Order + Index Nested Loop Join | S22.3 | 本地局部验收通过；R2 同源门禁、四档压力与 Review 已闭环（第 23.9–23.12 节） |
| **S22.5** | Typed Grace Hash Join + TEMP Spill | S22.4 | 局部验收通过；R5 同源完整门禁、性能/非回退、远端原生资源/恢复及独立 Review 闭环（第 24.9 节） |
| **S22.6** | Semi-mask/SIP + Join Dynamic Filter + TopK Pushdown | S22.5 | 局部验收通过；三算法四档、真实物理收益、语义/预算/错误回退、1M hard512资源及6d完整CI/Matrix与Review闭环（第25.7节） |
| **S22.7** | Factorized Execution + ASP-Join + 精确 Flatten | S22.6 | 局部验收通过；四档/复制/ASP三阶段、最终普通release完整输出及512KiB资源、34da完整CI/Matrix与Review闭环（第25.11节） |
| **S22.8** | WCOJ / Multiway Intersection + 全算法集成 | S22.7 | 实施中；先补typed/directed有序邻接seek，再接成本选择与pull多路交集，尚未局部验收 |

各 Stage 可独立完成**局部测试**后推进，但只有最终所有 Stage 的 Acceptance 加上统一全局门禁同时通过，Phase 22 才能标为 `done`；不会再创建 Phase 23–29 的实现状态。

## 3. 开发前准备与统一基准

### 3.1 真实源码入口核对

1. `query/plan.rs`、`plan/build.rs`、`plan/predicate.rs`、`query/stats.rs`、`query/stream/`、`query/mutation/`、`query/completeness/`：现有 AST lowering、Logical/Physical Operator、Read/Write、NodeIdSeek、PROFILE 与预算。
2. `storage/snapshot.rs`、`snapshot_stats.rs`、`checkpoint.rs`、Commit/GC、`query/schema/index.rs`、SQL `lithograph()` / `lithograph_rows()`：实际 Versioned access、Index、维护写入与 staged revision。
3. 当前 Git HEAD 与 dirty-tree digest、Phase 21 已纳入的源码/测试及 `v0.3.5` 本地 Tag、Cypher 25 inventory、Phase 11/21 基线、真实 SQLite 版本/配置；区分已打 Tag 的源码、经确认的远端发行制品与当前工作区。

### 3.2 共用确定性 Fixture

| Fixture | 数据与工作负载 | 主要目标 |
| --- | --- | --- |
| **FX-ACCESS** | 1k/10k/100k/1M Node/Relationship，固定目标 0/1/多行，有/无 Index | Seek/Scan、VM_STEP/FULLSCAN_STEP、无关数据扩张 |
| **FX-HISTORY** | 同终态 10/100/1k/10k Commit、不同 Delta/Branch/GC、staged CREATE/DELETE、有/无 Checkpoint；事务子查询的多 inner batch 与 Branch head move | Snapshot 重建、每批次单独 pin、维护开销、并发/重启 |
| **FX-ESTIMATE** | 均匀/偏斜 MCV/NDistinct、稀疏/稠密 Label/Type、degree skew、缺统计 | Cost 估算、Index/Scan 选取与回退 |
| **FX-JOIN** | 3–8 连通 Part、20+ 极端复杂 Part、不同书写顺序、重复 key | DPhyp/Greedy、Index Nested Loop、Planner 预算 |
| **FX-HASH** | 大型 equijoin、碰撞、重度倾斜、NULL/混合类型、大于预算 | Grace Hash、真实 TEMP Spill、rollback |
| **FX-SIP-TOPK** | 少量候选/大量非候选、索引可跳读/不可跳读、同分 cutoff、Graph View/ANN | SIP、Join Dynamic Filter、TopK 正确剪枝 |
| **FX-FACT** | Star、fan-out、多对多 2–4 hop、巨大扁平中间结果、排序/写 | Factorized、ASP-Join、multiplicity、Flatten |
| **FX-WCOJ** | Triangle、4-cycle、多环、自环、parallel Relationships、方向/Type | Multiway Intersection、Path/Relationship bag |
| **FX-CROSS** | SQL scalar/rows、OPTIONAL/Apply/`CALL { ... } IN TRANSACTIONS`、Graph View/ReadAccess query/result、historical/staged、时间/非确定性表达式、Search/Provider | 跨 Stage 结果与重复行、时钟/权限、异常与资源回归 |

Fixture 保存 seed、真实 cardinality、目标 Commit、Index/Checkpoint、Graph View/权限及预期逐行输出；不得用 `count(*)` 或无真实输出的快捷路径替代长流完整消费；不会为了跑旧版大规模 Cartesian 而故意制造不可控 OOM。

### 3.3 共用性能/资源/语义门槛

- 分开计时 parse/plan、Snapshot resolve、Index/Checkpoint cache build/lookup、first-row、full-drain、SQL adapter、writer-hold。Core/SQL rows 和热态/新连接/真正 OS cold 分开；热态≥5 connections×20 次、新连接首条≥100 次、长流≥3 次逐轮记录。
- 记录真实 Physical Plan 与 estimated/actual rows、SQLite VM_STEP/FULLSCAN_STEP/SORT、Index/Adjacency Seek、Join 中间候选、Hash spill、SIP skip、Factorized bytes、WCOJ intersection。`EXPLAIN` 的名称或逻辑 dbHits 不能代替实际工作证明。
- 固定结果与相关 Delta，10×无关图增长保持 Runtime 的可寻址路径门槛 `work(10×) ≤ 2×work(base)+1000 VM steps`；无 Index 的 Scan 允许候选域线性开销，不得额外产生平方中间行。
- 所有高内存/Spill 阶段在**真实 Linux amd64、1 GiB/no-swap** 限制下测 `memory.max`、`memory.swap.max`、`memory.peak`、`oom_kill`、RSS、TEMP/WAL、cancel/early-close/rollback/reopen/quick_check。
- 全阶段继续遵守 Runtime 的 10M Node/100M Relationship release-tier、写入/History/Diff/Merge 非回退和冻结 Cypher 25 Profile。每算法至少一项真实物理性能收益、一项不适用安全回退，检查行重复、NULL/TYPE_ERROR 时序、Graph View/Access、Path、ANN recall、Write 原子性。

### 3.4 正确性 Oracle 与负例执行规范

- **比较粒度**：**没有 `ORDER BY`、也没有 `SKIP/LIMIT` 的完整纯读结果**，按每行完整 Value/NodeId/RelationshipId/Path 的多重集合（bag）和出现次数比较，不要求不同合法 Plan 的自然行序相同；`DISTINCT` 按唯一值集合比较。有 `ORDER BY` 时按 Cypher 排序/NULL/Tie 合同验证行序和分页；**`SKIP/LIMIT` 但无 `ORDER BY`** 时，不能要求两次执行选中完全相同的任意子集，只检验行数上界、输入成员/重数、过滤与分页语义，并另外用受控 `ORDER BY` fixture 验证可精确比较的输出。写入查询还需核对副作用发生次数、最终 Branch head/Commit、失败回滚，不能只比较 RETURN。
- **异常与非确定性**：类型、NULL、禁止权限、资源不足、`sqlite3_interrupt`、`.transaction()` / `.statement()` / `.realtime()` 及随机/外部调用在相应语义边界单独设置正反例。不可安全固定输出的非确定性表达式以执行次数、作用域、允许的值域和原合同验证，不与另一轮随机输出做逐值相等比较；必要时要求 Planner 不重排而走旧路径。
- **优化开关与独立 Oracle**：不能靠新增公开的 Cypher hint/用户配置禁用优化；测试可通过已验证的内部测试入口或构造明确不适用的查询执行旧计划，再与冻结 Cypher 25/TCK 及可审查的逐行 Oracle 交叉核对。旧实现存在已知错误时不得将它当语义真源。
- **观测不越权**：对被 `readAccess.query` 禁止的 Property 构造有 Index/统计、无 Index/统计、隐藏值热门/冷门三组输入；`EXPLAIN/PROFILE`、参数准备、成本选择、错误及返回都不得绕过访问禁令。`readAccess.result` 仅对最终结果递归编码生效，不误当作禁止合法查询计算的条件。

### 3.5 资源测试层级与平台隔离

- **运行时小内存场景**：在 Linux **x86_64** 可加载扩展中执行 1 GiB/no-swap cgroup，至少覆盖 100k 与适用 1M fixture、无法装入内存的 Hash/Bitmap/Factorized/Join 和资源失败清理，真实读取 `memory.max`、`memory.swap.max`、`memory.peak`、`memory.events`。若通过 QEMU 仿真须记录宿主架构与仿真模式，不与原生平台 wall-time 直接比较，也不得把编译期 OOM 当查询运行期 OOM。
- **Release 规模场景**：10M Node / 100M Relationship 与 [Runtime Fixed Targets](../../design/runtime.md#performance-targets) 的 Apple M2/Release/同一 SQLite 配置作为独立规模与非回退门槛；不能因为 1 GiB 容器无法同时容纳构建/fixture，就跳过或改写任一测试层级。
- **并发与稳定性**：沿用 [Runtime Stress](../../design/runtime.md#stress-workloads) 的 1/4/8 readers + 1 writer、每种配置至少 30 分钟、10K-conflict Merge、WAL/Lock、GC/derived rebuild、不同 Graph View 与 Provider-owned Cache 分离的计量；把每个配置的失败、writer-hold、P95、峰值内存和 reopen/quick_check 保留为可回放证据。

## 4. 各 Stage 的完整 Features 与局部 Acceptance

<a id="stage-s22-1"></a>

## 5. Stage S22.1：可扩展 Planner 与可预算 Executor 基础

### 目标与前置

基于现有 AST、LogicalPlan、PhysicalPlan、MatchCursor、QueryResources 与 TEMP，形成一个后续 Join/图算法能够共同使用的**实际计划选择与执行预算机制**。不创建第二套 Planner/Executor，不新增公共配置、Cypher 方言或持久 Cache。正式行为与资源合同在 [Optimizer / Budgeted Executor](../../design/query-engine.md#optimizer-and-budgeted-executor)、[Runtime](../../design/runtime.md#optimizer-join-stress)；已有实现证据与算法适用性见 [研究](../../research/query-optimizer-roadmap-evidence.md)。

### 实施步骤与交付

#### S22.1.1 现有结构与失败基线

- 检查 read、mutation、program、subquery、rows/scalar 的计划/执行入口，建立实际执行路径与 `EXPLAIN/PROFILE` 一致性 inventory；确认 Phase 21 的局部谓词路由已覆盖哪些语义，避免再次实现相同功能。
- 准备同一 Commit / Graph View 下有 Label、Type、Property Index、NodeId、Adjacency 可替代访问路径的真实测试，以及完整负例（NULL、TypeError、OPTIONAL、索引缺失、历史和 staged 修改）。固定基线、dirty-tree digest、SQLite 版本与冷/热类别。

#### S22.1.2 优化器候选与有界估算

- 沿用现有 Logical/Physical 结构，为真实 Scan/Seek/Expand 候选记录 binding/scope、估计行数区间、I/O/CPU/内存、类型/可见性与前置约束；同一物理调用点从这些候选中选择可证明等价且成本较低的路径。
- 只重构阻塞已有访问路径选择的共享逻辑，不加入尚无消费者的通用 Memo、规则注册器、插件框架或 JIT。未知统计使用确定性安全回退，不反复扫描全图创建 estimate。
- Planner 自身的候选枚举、分析深度、时间与分配设内部可检验的上限；超限仍能返回确定性旧计划而不抛出新的查询语义错误。
- **消除 Stage 依赖循环**：S22.1 的真实 Cost/Access 选择先复用当前已有的 Label/Type/Index 统计与身份访问；NDistinct/MCV/Skew 等新版本化统计由 S22.3 补齐。S22.1 不得以等待 S22.3 统计为由只交付没有执行消费者的 IR。
- **Planner 与执行预算分开**：[Execution Memory Boundary](../../design/runtime.md#execution-memory-boundary) 当前明确不把 Frontend AST/Plan 算入 `memoryLimitBytes`；本 Stage 必须给 Planner 自身的候选数、状态数、分配量、递归深度与取消检查提供**独立的内部上限/可重复触发的保护**，不能将 Executor 的 QueryResources 预算假称为已覆盖 Planner，也不引入额外公开预算参数。可使用确定性的状态/工作量上限检验回退，墙钟耗时只用于补充诊断。

#### S22.1.3 单一 Executor 资源合同

- 检查当前 `QueryResources` 对 row/temporary buffers、operator continuation、profile、batch、spill 和资源释放的覆盖；补齐后续候选算子必需且当前遗漏的 reservation / release / cancellation 接口，不新建 `work_mem`。
- 至少让一个现有算子的访问路径切换通过共同机制产生**真实** IO/CPU/Plan 改善，同时在预算不足或取消时正确回滚/释放；不只新增抽象数据结构。
- 集成 read/write SQL 双入口、`EXPLAIN/PROFILE` 与权限定界；维持同一 query-local Snapshot / staged revision 的访问和结果。

### 局部 Acceptance（全部必做）

- **A22-S1-01**：read/write/program/rows/scalar 共用的已有与新增候选计划映射通过正反测试；缺少数据、统计或类型证明时安全回退，未改变 Cypher 25 Profile 或输出。
- **A22-S1-02**：至少一个真实多候选访问场景根据基数/成本选择不同的正确 Physical Operator，实际 SQLite VM/dbHits 与 EXPLAIN/PROFILE 一致；相同输入与统计产生确定性选择。对同一 SQL prepared statement 以不同参数类型/选择性、Graph View、目标 Commit、IndexDefinition 更新后重新执行，也不能沿用不适用的旧候选计划或权限/类型证明。
- **A22-S1-03**：规模由少量增加至大量独立 Pattern 时，Planner 被独立于 Executor `memoryLimitBytes` 的内部工作/分配上限保护；以可重复的候选数/状态数/取消输入触发真实 fallback，报告计划耗时、实际候选数及 peak memory；超限不泄漏内存、不误返回空结果，也不借 “Planner 内存不计账” 忽视进程 RSS。
- **A22-S1-04**：已有 streaming / TEMP / cancellation / `memoryLimitBytes` 与 error/rollback 合同在各类新候选下保持；无 output-sized retained `Vec<Row>` 和隐藏后台执行。
- **A22-S1-05**：在 1 GiB/no-swap Linux amd64 容器及 1k/10k/100k 合成图验证；无 `oom_kill`、未授权外部 I/O 或读路径写入。读取/写入、历史/staged、0/1/多结果与对称参数场景都有对照。
- **A22-S1-06**：目标路径与 Phase 21 相比有真实工作量改进，非目标路径没有不可解释的显著回退；`cargo make quality`、`scripts/ci.sh`、适用 TCK/SQL、阶段 Review、文档/链接与最终 diff 全通过。

### 局部 Review 与交接

检查是否无意加了第二套 Plan IR、内存预算或全局状态；是否把 `EXPLAIN` 名字当真实算子；统计缺失是否被错误当作零行；OOM/cancel 后是否保留临时资源或半提交。发现设计缺口只在对应真源补最小合同并再测，不能降低现有 Runtime 目标。

只有上述 A22-S1-* 的真实证据通过才允许进入 S22.2；不因此修改整个 Phase 22 为 `done`。未获单独授权不 Commit、Push、Release 或部署，下一 Stage 先治理 Snapshot 而非直接引入 Hash Join。

**Stage Gate：** 本 Stage 所有 A22-S1-* 必须得到当前源码的实际正确性、性能/资源和 Review 证据才可进入下一 Stage。局部通过不能单独将 Phase 22 标成 `done`。


<a id="stage-s22-2"></a>

## 6. Stage S22.2：自动 Tiered Checkpoint 与 Snapshot 成本控制

### 目标与前置

解决当前无 Checkpoint 时 immutable Layer Overlay 重建成本随历史与 delta 增长的问题；复用现有 `storage/checkpoint.rs`、`Snapshot::resolve` 和 query-scoped read guard，不新建图版本模型。唯一合同见 [Storage 自动维护](../../design/storage.md#tiered-checkpoint-policy)、[Runtime 性能证据](../../design/runtime.md#optimizer-join-stress)；参考 [研究](../../research/query-optimizer-roadmap-evidence.md) 中的 TerminusDB Rollup。

### 实施步骤与交付

#### S22.2.1 固定真实历史基线

- 构造同一逻辑终态的 10/100/1000/10000 Commit、1/10/100 MiB 等不同 delta 分布、集中大 Layer 与不同历史 Branch；固定目标 ID/结果规模，分别测 no-checkpoint、现有 checkpoint、首次读、新 connection、staged revision 和 reopen。
- 收集 Snapshot resolve 层数、Overlay bytes/合并次数、SQLite VM、读取延迟、RSS、TEMP、写入 writer-hold 和 checkpoint disk bytes；先证明被优化成本来自 resolve，而非把 Seek/缓存命中伪装成 Snapshot 优化。

#### S22.2.2 内部自动维护策略

- 在现有获权的写入/maintenance boundary 以 Layer 深度、累计 delta bytes、历史解析成本和可用磁盘预算判断需要创建的 derived checkpoint；热点 Branch 可分层维护，避免每次普通 Commit 都完整复制图。
- **同时优化构建器而不仅增加触发器**：当前 `storage/checkpoint.rs` 的 `create_checkpoint` → `first_parent_layers` 会先收集从 Root 到目标的所有 Layer ID，再逐层回放。实施 Tiered Checkpoint 时必须优先从目标 first-parent ancestry 中最近的**完整且兼容的祖先 checkpoint** 构建 base，只重放该 base 到目标的增量；当前祖先 checkpoint 缺失/不兼容时允许受预算的 canonical fallback，不能以“自动调用旧全链构建器”交付 Tiered。Layer 遍历/收集、apply、统计计算与复制都需有可取消的 bounded work/temporary memory，不能在压测时依靠大于内存的 `Vec<LayerId>` 暗藏超预算重放。
- 构建过程分批可取消、原子发布，构建失败/并发 writer 不能保留半成品或移动 Commit/Branch ref；只允许从目标的 first-parent chain 复用有效 checkpoint。
- 限制单次 writer-hold、工作预算、同时维护数量与额外持久磁盘占用；到达资源限制安全延期，不得在 read-only SQL、历史查询、EXPLAIN 或已存在 active reader 时隐式写主库。

#### S22.2.3 Snapshot 复用与派生数据失效

- **普通单次 execution** 的一致 Snapshot 不应被每一 page/batch 重新 resolve；但 `CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 的每个 inner transaction batch **必须按当时 Branch head 独立 pin**，不能共享整个 outer execution 的同一 Commit。read-write query 的后续 clause 必须看见此前已完成的 staged writes；不能以“整条查询只 resolve 一次”为由冻结 graphView membership 或 staged revision。
- 必要时对纯 immutable 只读 base 建立有预算、可逐出的 query/connection-local 缓存，严格区分 database identity、pinned Commit/Schema、selector/access proof 和 staged revision；多个 inner batch 只在确认为相同的 immutable 部分才允许复用，绝不缓存跨 batch 的可变可见集合或 writer 状态。
- 缺失/删除/损坏 checkpoint、GC、Branch move、Reset、Merge、schema/index revision、staged CREATE/DELETE、不同 SQLite connection / reopen 均回退正确 canonical Snapshot，不因 cache miss 创建新的 correctness source。
- 历史 Commit、当前 Branch 和 SQL explicit transaction 的读写回归一起闭合，不更改 canonical Layer、Commit hash、storage format 或 public query interface。

### 局部 Acceptance（全部必做）

- **A22-S2-01**：相同最终图、不同历史深度与 delta 大小的 Before/After 数据可复现；自动策略确实在许可写入边界触发，另记录触发条件、Checkpoint 数与创建开销，不靠隐式预热。
- **A22-S2-02**：自动 Checkpoint 的构建/取消/中途失败/重启后原子与可重建性可验；旧 Commit/Branch hash 不变，不会把 derived metadata 当真源；GC/缺失 checkpoint 时结果相同。
- **A22-S2-03**：在满足维护条件的高 Layer/deep-history 数据上，**Checkpoint 构建与之后查询都**不再无条件从 Root 重放全部 Layer；分别记录构建器扫描/重放的层数、临时内存、SQLite VM、磁盘复制与生成时间，以及 read 的 Snapshot Layer 解析、VM、端到端时间和热态/新连接。建立第二个 checkpoint 后必须证明利用已有兼容祖先 base 只处理其后增量；遇到不兼容 base 时证明受预算 fallback，不隐去失败/重放成本。
- **A22-S2-04**：自动维护带来的 writer-hold、磁盘和写入放大被明确单独计量且受现有资源约束；不能为了点查速度让 1k Node batch/Commit 或 History/Diff/Merge 出现超出 [非回退门槛](../../design/runtime.md#performance-targets) 的性能回退。
- **A22-S2-05**：1 GiB/no-swap Linux amd64 容器、历史/当前/事务及两连接并发、WAL/rollback journal 模式下无 OOM、无意外写锁、无半成品；长生命周期 SQL rows cursor 的 Read Guard 从 version resolve 持续到释放，维护/GC 不得使已 pin 读状态漂移或绕过现有同连接 writer boundary；取消和 reopen 后正确恢复。
- **A22-S2-06**：真实 SQLite `lithograph()` / `lithograph_rows()`、Storage integrity、适用 History/Merge、`CALL { ... } IN TRANSACTIONS` 每批次新 Head 可见、`IN CONCURRENT TRANSACTIONS` 的 batch 所有权/取消和 staged clause 修订、`cargo make quality`、`scripts/ci.sh` 与最终 Review/diff 全通过。

### 局部 Review 与交接

尤其审核“读请求竟然写 main”“每次 Commit 全量 checkpoint”“跨 Branch 共享错误 Snapshot”“由缓存决定是否存在 Node”“维护中 GC/Read Guard 竞态”。不能为了简单而关闭版本化或用文件复制模拟 Branch。只有所有证据通过才进入下一个 Stage，不包含 Commit/Push/Release 授权。

**Stage Gate：** 本 Stage 所有 A22-S2-* 必须得到当前源码的实际正确性、性能/资源和 Review 证据才可进入下一 Stage。局部通过不能单独将 Phase 22 标成 `done`。


<a id="stage-s22-3"></a>

## 7. Stage S22.3：版本化基数估算、Relationship ID Seek 与安全谓词扩展

### 目标与前置

让 Planner 能在正确的 historical/staged Snapshot 中估算高选择性 access paths，扩展 Phase 21 的 NodeIdSeek 到 RelationshipId，并安全地识别更丰富的 Filter/Index 条件。设计真源为 [Optimizer](../../design/query-engine.md#optimizer-and-budgeted-executor)、[Join 适用条件](../../design/query-engine.md#join-algorithms-and-graph-optimizations)、[Schema/Index](../../design/schema-and-indexes.md#range-text-point) 与 [统计证据](../../research/query-optimizer-roadmap-evidence.md)。

### 实施步骤与交付

#### S22.3.1 Statistics 与 Cardinality Estimation

- 在现有 Node/Relationship/Label/Type total 的基础上补充**实际规划器会消费**的 Label 组合、relationship degree 分布、Property Index owner/NDistinct、选择性/MCV 与少量有根据的相关性；以 Snapshot / IndexDefinition / type proof 为 key，并限制采样、持久空间与 planning cost。
- 利用最近有效 Checkpoint statistics + bounded overlay 修正；丢失、老旧、损坏、无样本、0-result 或高 skew 时返回明确 unknown/估计范围，不当成精确零行，不要求每次查询全图 ANALYZE，也不修改 canonical history。
- 用同一语义不同分布的 fixture 验证估算改善：稀疏/密集 Label、热门/冷门 Property 值、高低 degree、历史及 staged additions/deletions；统计增加的写入/存储成本单独报告。
- 统计和 Cost/`EXPLAIN` 只能在已完成当前调用的 Query Access Validation、正确 Property Type Proof 和版本身份检查后被使用；`readAccess.query` 禁止的属性不得通过 Index/统计提前筛选或输出热门值/选择性详情。`readAccess.result` 对最终编码的约束与 query 拒绝规则不混同，按 [Query Access](../../design/query-engine.md#query-access-controls) 分别测试。

#### S22.3.2 Relationship Element ID Seek

- 识别 `elementId(r) = $id` 及左右对称的稳定参数/常量，只在变量绑定、类型、错误语义与版本图关系允许时用真实 `Snapshot::relationship(RelationshipId)` 定位；内置 `r:<positive-int>` 解码不变。
- 直接关系访问后仍验证 Relationship Type、起终点、Graph View/Access Policy、端点已删除或 staged 变更、残余谓词以及 parallel relationships。非法、非 String、NULL、无匹配和 error-timing 条件回退原 Scan+Filter。

#### S22.3.3 安全谓词路由与索引候选

- 在现有 AND prefix 基础上按 AST binding set / scope / nullability / deterministic / may-throw / type-proof 分析，支持证明等价的范围、等式、IN 与组合条件；对 OR、OPTIONAL、相关子查询、未证明类型的跨值比较、非确定性及 side effect 保留原语义/回退。
- 将符合版本化 Property Type Constraint 和 IndexDefinition 的条件交给现有 Range/Text/Point/Relationship Lookup 索引路径，不为无索引查询偷偷创建持久 Index 或“索引全部属性”；无 Index 的 Scan 改进仅限于已测重复读取、lazy property / predicate cost。
- 对单向 Seek 与 Scan 建立失效索引/fallback 真值表；不因为目前统计错误而选择会丢失 `TYPE_ERROR` 的窄 Index key range。

#### S22.3.4 无索引属性过滤的实际扫描优化

- 对确实没有适用 Index 的选择性属性查询，基于现有 Label/Type、Graph View 与 Snapshot cursor 缩小候选域，复用 prepared SQLite statement，优先对候选进行廉价谓词判定并延迟读取不参与 Filter/Projection 的 Property；合并已经有证据的重复 Scan、解码或行复制。
- 只做当前存储接口能实现的 lazy access / bounded batch/filter，不能承诺无索引点查为常数复杂度，也不能在只读 query 中隐式构建持久 Index/更改版本 Schema；保持 NULL、类型异常与求值顺序。

### 局部 Acceptance（全部必做）

- **A22-S3-01**：统计误差和 planning decisions 在均匀/倾斜/空数据下可量化，较旧统计选择有至少一个明显更优的访问路径；缺失/错误统计时 100% 保持正确结果，并有 bounded collection/refresh 与磁盘成本证据。对被 Query Access 禁止的 Property，在有/无 Index/MCV、不同值分布下，`EXPLAIN`/`PROFILE`、准备阶段与结果都按 Query Access 合同保持预期掩码/拒绝与不越权，不泄露禁止属性的频率或取值。
- **A22-S3-02**：Relationship ID 的正反向 equality、非法/非 String/NULL/0结果、方向/Type、Graph View、历史 Commit、staged create/delete/abort 和自环/parallel Relationship 有真实 Read 与 MATCH→write 测试；存在有效 ID 时 physical `RelationshipIdSeek` 不扫无关 Relationship。
- **A22-S3-03**：更多 WHERE/Index Rewrite 的正反/错误/TCK 包含 `IN` 的重复、NULL、类型与错误时序、不同 Property value family、OPTIONAL、跨 scope、访问控制；原计划和优化计划 Oracle 逐行一致。
- **A22-S3-04**：对 1k/10k/100k/1M 不相关 Node/Relationship 扩张，固定 ID/Ready Index 查询 SQLite VM 和实际访问不出现线性放大；无 Index 时扫描保持接近候选域线性，不增平方中间行，且至少一个有重复 Property 解码/SQLite 调用的可复现无索引 workload 在真实 VM/CPU/分配之一降低工作量。优化前后输出、错误和完整消费严格一致。
- **A22-S3-05**：1 GiB/no-swap Linux amd64、TEMP/取消/ResourceError/rollbacks、历史 Schema/Index 迁移和 Provider/Search 回归通过；原有 `cargo make quality`、`scripts/ci.sh`、相关 TCK 与最终 Review/diff 全通过。

### 局部 Review 与交接

重点检查统计误差成为 correctness gate、越权索引访问、读取当前 Branch 的 Schema 冒充历史、拿 `r:` 解释成 NodeId、过早过滤吞掉 Cypher TypeError。数据结构新增前证明被真实访问选择使用，未使用的复杂统计不提前构建。验收全绿才取得局部通过；Commit/Release 另计。

**Stage Gate：** 本 Stage 所有 A22-S3-* 必须得到当前源码的实际正确性、性能/资源和 Review 证据才可进入下一 Stage。局部通过不能单独将 Phase 22 标成 `done`。


<a id="stage-s22-4"></a>

## 8. Stage S22.4：DPhyp / Greedy Join Order 与 Index Nested Loop Join

### 目标与前置

对多 Pattern Part 的安全 inner Join 组件，使用版本化统计和可用 Index/Adjacency 路径选择更好的 Join 顺序；实施小型 DPhyp/Greedy 和参数化 Index Nested Loop。正式限制见 [Join 与图优化](../../design/query-engine.md#join-algorithms-and-graph-optimizations)、[优化器预算](../../design/query-engine.md#optimizer-and-budgeted-executor) 与 [性能证据](../../design/runtime.md#optimizer-join-stress)。参考 [DuckDB 算法研究](../../research/query-optimizer-roadmap-evidence.md)。

### 实施步骤与交付

#### S22.4.1 Join Graph / 等价类和边界

- 从现有已完成 scope/type validation 的 Pattern Part 与 cross-Part predicate 建立有界的 Join Graph；区分已绑定变量、Outer Apply、Variable/Path reuse、OPTIONAL、非确定性、可能抛错表达式与写 barrier，只对证明安全的 inner subgraph 重新排序。
- 非连接组件仍可能需要 Cartesian；必须估算其结果乘积并保持流式/预算，不把“没有 Join Predicate”变成任意自动去重或静默 0 结果。
- 特别构造 **outer 输入为空、inner 侧表达式可能 TYPE_ERROR/外部调用** 的情况：任何重排不能因为提前执行 inner/Build 分支而在原本不会求值的路径上产生异常或副作用。未证明安全的 cross-Part equality 仍按原值/错误语义位置验证，不把“存在等值连接条件”自动理解为可交换。

#### S22.4.2 DPhyp 与 Greedy Fallback

- 对少量 Pattern Part 以 DPhyp / DP 只枚举合法连通子计划，比较预计输出、I/O、CPU、内存/TEMP、可索引性与已满足绑定；用于重排序的统计来自当前 pinned Snapshot（Stage S22.3）。
- 超过内部搜索时间/状态/内存预算的大组件使用确定性 Greedy + bounded search fallback；无统计或无成本证明时保守旧计划。禁止为任意 N 的 Part 枚举全部 2^N 子集或引入新公开 Hint/Planner 调优语法。

#### S22.4.3 Index Nested Loop Join

- 使用外侧已绑定的 Node/Relationship/Property key 参数化现有内侧 Seek、Adjacency Expansion；适用候选缺失索引时回退 Scan/其它已有 Join。重复外侧 key 也必须产生原始 multiplicity，不因优化误用 cache 去重结果。
- Query-scoped memoization 仅对**已证明纯确定性、同一类型/绑定且不会改变可观察表达式调用次数、错误时序或外部副作用**的内侧访问开启；缓存 key/有效期还要涵盖完整绑定、pinned Commit、versioned Schema/Index generation、Graph View/Access Policy 与 staged revision，严格按预算逐出。不能把不同 Branch 的同名 key、不同参数类型/权限视为同一结果，不得缓存 `.realtime()`、随机/外部函数调用或含 write side effect 的内侧求值。

### 局部 Acceptance（全部必做）

- **A22-S4-01**：三到八个连通 Pattern 的不同书写顺序规划为可解释、实际更少的 I/O/中间组合；`EXPLAIN/PROFILE` 与执行工作一致。固定数据与成本下计划确定性，不声称所有表序在无 ORDER BY 下具有新增稳定排序语义。
- **A22-S4-02**：大量 Pattern Part 或故意复杂的 Join Graph 在内部规划预算耗尽时有稳定 Greedy Fallback；记录枚举状态、plan CPU/peak RAM 与最终正确结果，不因优化时间爆炸引发 OOM。
- **A22-S4-03**：Index Nested Loop 对稀疏/高选择性外侧、重复 keys、0/1/多结果、Property Type、双向端点/自环/parallel Relationship 的真实访问减少可量化；同一 pinned Snapshot 不因每条外侧 Row 反复完整 resolve/加载 Layer，且延迟和 SQLite VM 分别计量；没有相关 Index 时安全回退。
- **A22-S4-04**：OPTIONAL、相关子查询、Variable reuse、Path uniqueness、NULL/TYPE_ERROR、Graph View、历史/staged 以及 MATCH→CREATE/SET/DELETE 的错误和行数逐项与独立 Cypher 25 Oracle 一致；额外验证零行 outer + 可能抛错 inner 的求值时序。无 `ORDER BY` 的查询比较逐行 Value 的 bag/multiplicity，不因物理 Join 顺序变化要求天然返回顺序一致。
- **A22-S4-05**：10x 无关数据增长的 VM work 不出现额外平方爆炸；在 1 GiB/no-swap Linux amd64 下测跨 Join 深度、Peak RSS/TEMP/cancel/rollback，满足既有大规模性能非回退规则。
- **A22-S4-06**：Core/SQL scalar/rows 与 Index/Schema、Cypher TCK、`cargo make quality`、`scripts/ci.sh`、Phase Review/diff 全部通过；实测并记录 Planner overhead 和 Join runtime 独立时间。

### 局部 Review 与交接

重点审查跨语义 barrier 错误 Join reorder、基数估计失真导致极差计划、memo key 丢了 version/staged revision、泛化 Join Graph 搜索超时、关系唯一性与行倍数丢失。至此才可把 Planner 的 Join Order 与 Index Nested Loop 基础称为可用，并进入 Grace Hash Join；未验证不取得局部通过，不自行发布。

**Stage Gate：** 本 Stage 所有 A22-S4-* 必须得到当前源码的实际正确性、性能/资源和 Review 证据才可进入下一 Stage。局部通过不能单独将 Phase 22 标成 `done`。


<a id="stage-s22-5"></a>

## 9. Stage S22.5：有界 Grace Hash Join 与 SQLite TEMP Spill

### 目标与前置

解决两个大候选集合在有安全等值连接条件时的 Index Nested Loop / Cartesian 执行过量工作。以 [Join 算法与执行资源](../../design/query-engine.md#join-algorithms-and-graph-optimizations)、[Execution Resources](../../design/query-engine.md#execution-resources)、[Runtime 压测](../../design/runtime.md#optimizer-join-stress) 及 [PostgreSQL 研究](../../research/query-optimizer-roadmap-evidence.md) 为真源。

### 实施步骤与交付

#### S22.5.1 Typed Hash Equality 与物理计划

- 只对存在足够 Cypher type/equality proof 的 inner equijoin 构造 Hash Join 候选；正确选择 build/probe 侧，评估 hash table、预估行数、CPU/I/O、TEMP 和可索引替代计划。
- Hash key 的 canonical value equality/hash 必须保持 Integer/Float、String、NULL、结构类型等 Cypher 25 语义；对可能触发不兼容类型的表达式保留原错误时序/残余检查，不能把不能 hash 的值静默当不匹配。
- 处理重复 key、parallel Relationship、0/1/多输出、collision、历史/staged Snapshot 和每个 MATCH 内 Relationship uniqueness；保持确需保持的 ORDER BY/LIMIT/barrier 语义。
- **Build 侧时序防护**：对于外侧为空、Build 侧可能抛错、含非确定性求值或其它语义 barrier 的输入，不能让 Hash table 预构建改变原先不会执行的表达式/错误；不具备等价证明就回退原计划。比较器的整数/浮点数、NaN、负零、List/Map 及不可 Hash Value 的组合依冻结的类型比较合同逐项测试，不能只用 Rust 的 `Eq/Hash` 默认行为代替 Cypher equality。

#### S22.5.2 内存受控的 Grace Hash Join

- 小 build side 用 query-local reservation 按需建表；超过既有 `memoryLimitBytes` 可用内存时改为有限 partition 的 Grace Hash Join，通过 SQLite TEMP/disk 分批处理，不保留全体 build/probe rows 于 RAM。
- 检查现有 `query/spill.rs` / `spill/codec.rs` 的 typed Value/Binding 序列化和生命周期；Hash partition 不能直接假设现有 RowSpill `row_json` 足以保留全部 Join Key 的 Cypher equality、numeric family、NaN/负零、Node/Relationship identity、Nested List/Map、NULL 与附带资源 lease。缺少证明时补最小的有界、可取消 typed round-trip/partition 测试或针对必要 Value 的编码，不新增第二套不受控存储。
- partition 数、递归深度、skew/hot key 行数与单批状态受上限保护；严重倾斜无法 spill 时安全失败/回退，不能因递归分桶无止境产生磁盘或内存爆炸。
- 使用现有 cancel、request resources、cleanup 与 transaction fail-closed，错误/early-close 后删除本查询 TEMP；写入不可见性和 Commit publication 不随 Hash Join 批次改变。普通 invocation 的未发布写入必须回滚，active SQL explicit transaction 按原 fail-closed 规则处理；对 `CALL { ... } IN TRANSACTIONS`，已成功 durable 的 inner batch **不因后续 batch 失败而被重新回滚**，必须严格遵守 [Transaction Subqueries](../../design/storage.md#transaction-subqueries)。

#### S22.5.3 成本选择及 Profile

- DPhyp/Greedy 的计划代价包括 Hash build、probe、spill 与预计输出，和 Index Nested Loop/Adjacency/Cartesian 共同选择；只报告实际执行路径，不把全部 equijoin 都强迫换 Hash。
- PROFILE 同时记录 build rows、probe rows、hash collisions、spill partitions/batches、真实 SQLite VM/TEMP bytes、峰值 Engine/RSS，不输出原始属性或参数值。

### 局部 Acceptance（全部必做）

- **A22-S5-01**：在同一没有适用 Property Index 的大集合等值 Join 上，真实 Hash Join 相比旧执行显著减少候选组合/VM 工作，输出行数与 Cypher 25 Oracle 完全相同；少量记录/已有 Seek 场景仍可选择更低成本的原计划。
- **A22-S5-02**：NULL、跨类型 TYPE_ERROR、数值规范化（含 NaN/-0）、重复 key、多对多、多分区、collision、skew、self-loop、parallel Relationships 与输出 multiplicity 都有正反/类型测试；必须覆盖零行 outer 与错误 Build 表达式、错误应出现与不应出现两类反例，不能因 Hash 侧交换多抛错或少抛错。
- **A22-S5-03**：必须有**至少一个实际运行的、Build 大于可用 `memoryLimitBytes`、TEMP 可写的 Grace Hash Join**，真实触发 partition spill **且完整成功返回正确结果**；另设无法安全 spill、TEMP 不可写/空间不足的独立负例，按既有 RESOURCE_ERROR/事务边界回滚与 cleanup，不允许只跑到错误就宣称 Spill 算法已交付。Spill 之后逐行 Value/Identity/NULL/特殊 numeric 类型 round-trip 与未 Spill Oracle 一致，不允许先超预算分配再补预留。
- **A22-S5-04**：在 Linux amd64 1 GiB/no-swap、1k/10k/100k/1M 与 100k+ 重度倾斜 key 场景测内存峰值、TEMP bytes、partitions/cancel/writer-hold；无 OOM、取消及时、数据库重启后 integrity 正确。
- **A22-S5-05**：与 Planner/Index Nested Loop 的 mixed-plan 回归涵盖 read/write、OPTIONAL/Apply barrier、Graph View、pinned historical Commit、staged mutation、SQL rows/scalar、适用 TCK。
- **A22-S5-06**：`cargo make quality`、`scripts/ci.sh`、适用真实 SQLite 版本、性能非回退、Phase Review 和最终 diff 全通过；不能关闭 Spill 或缩小数据得到通过。

### 局部 Review 与交接

重点检查 Hash 键与 Cypher 比较不等价、build-side 误估、hot partition 退化、spill 文件泄漏、内存预算多层重复计账/未计账、错误时序/写入回滚。若 Hash 不适用，自动回退不影响结果。本阶段完成后再引入 SIP/Dynamic Filter，避免其重做 Join 与内存框架。

**Stage Gate：** 本 Stage 所有 A22-S5-* 必须得到当前源码的实际正确性、性能/资源和 Review 证据才可进入下一 Stage。局部通过不能单独将 Phase 22 标成 `done`。


<a id="stage-s22-6"></a>

## 10. Stage S22.6：Semi-mask / SIP、Join Dynamic Filter 与 TopK Pushdown

### 目标与前置

让已有 Join/Expand/TopK 中已经确定的候选信息反向约束后续物理 Scan/Seek，从源头减少 SQLite 读取。参考 [LadybugDB Semi-mask](../../research/query-optimizer-roadmap-evidence.md)、[DataFusion Dynamic Filter](../../research/query-optimizer-roadmap-evidence.md)；行为真源为 [Join 专项](../../design/query-engine.md#join-algorithms-and-graph-optimizations)、[Search 范围](../../design/query-engine.md#graph-view) 和 [Runtime](../../design/runtime.md#optimizer-join-stress)。

### 实施步骤与交付

#### S22.6.1 Query-scoped Semi-mask / SIP

- 从 Hash Join、Index Nested Loop 与已选端点的实际候选集合生成对 NodeId / RelationshipId 有效的有限集合或 Bitmap；只在能映射到物理 keyed range/Index/Adjacency 的 Scan 上启用，不能先全图扫描再说完成了“pushdown”。
- 查询内 mask 与同一 pinned Commit、Graph View/Access Policy、Schema/index generation、staged revision 绑定；Mask 是加速性证据而非可见性真源。若局部修改、缺少 index、越权、超预算或无法证明无 false-negative，就放弃该 mask 并走无过滤的正确路径。
- Bitmap/Set 的建构、更新、执行、清理与 TEMP spill 使用已有 QueryResources，明确处理重复 ID、极端稀疏/稠密 ID 范围、多个 Join producer 和 Graph View 移动边界。
- 识别 filter 的生产时机和消费时机：尚未完成的 Build/Join 不得通过不完整 Bitmap 排除后续合法候选；Query 内前序写入、`CALL { ... } IN TRANSACTIONS` 的新 batch pin 或权限/selector 差异必须重新评估 mask，而不能使用旧 ID 集合。

#### S22.6.2 Join Dynamic Filter

- 实现对相等连接键有严格安全证明的 typed min/max 或 KeySet 运行时过滤，允许 build/probe 传递；保证不改变 Cypher 错误/NULL/type semantics，并只在 SQL B-tree/Index 真正可以 skip 时采用。
- 对重度 skew、超大 key space、混合 property family 与运行时过滤构造成本过高的场景保守回退；将 filter build cost、跳过物理行/页、TEMP/CPU 纳入 PROFILE/Benchmark。

#### S22.6.3 TopK / Search Dynamic Filter

- 对有序 Index/Scan 的 `ORDER BY ... LIMIT`，在至少已有足够可信候选且 cutoff/ties/NULL 的语义证明完整时按需更新动态下界或上界；不能在相同 score cutoff 的候选未取齐时提前终止。
- 同时存在 `SKIP`/offset 时，仅在已证明的**`skip + limit` 个可见合格候选**的上界上剪枝；使用 checked arithmetic 防溢出，不能以 `limit` 个候选过早终止。无 `ORDER BY`、非可比较值或不能证明 tie/排序方向边界时回退而不假造全序。
- Full-text、Raw Vector、Managed Semantic 的 top-k 必须先遵守 Graph View 与 Access Policy 再排名；不改变 ANN recall、score、路径/排序的既有 contract，不为了“动态剪枝”错误丢掉范围内有效候选。

### 局部 Acceptance（全部必做）

- **A22-S6-01**：Semi-mask 在候选数为原图小比例、且扫描有 keyed access 的真实场景减少 SQLite VM/底层读取；不是单纯减少 Filter 比较次数。无法 skip 的 Scan 与无索引场景拒绝该计划，仍正确返回。
- **A22-S6-02**：Join Dynamic Filter 的 typed min/max/keyset 候选与未优化 Join 完全同一行/异常；正反测试含 skew、NULL、混合类型、重复、Graph View、历史/staged、不同权限和关系方向。
- **A22-S6-03**：TopK 动态剪枝在 0/1/多候选、相同 cutoff score/ties、NULL、ORDER BY 不可比较类型、skip/limit、Graph View、Search 的 top-k/ANN 指标下全部与稳定 Oracle 相符；没有类型/上下界证明时回退。
- **A22-S6-04**：所有 mask/filter 分配可预算、可取消、query-local，跨 state/revision/connection 不复用；对未完成的部分 Build、staged 修改与新 inner transaction batch 也不产生 false negative；1 GiB/no-swap Linux amd64 下无 OOM / RESOURCE_ERROR cleanup 不泄露候选或外部调用。
- **A22-S6-05**：1k/10k/100k/1M 的固定结果 + 10x 无关数据实验，记录 mask size/build、skip pages、VM/fullscan、RSS/TEMP 与 P50/P95；至少一类 SIP、Join Dynamic Filter 和 TopK 推送分别有真实执行与可复现物理收益。
- **A22-S6-06**：Read/Write/SQL scalar/rows、版本化 Search/Provider、Cypher TCK、`cargo make quality`、`scripts/ci.sh`、Phase Review 与最终 diff 全通过。

### 局部 Review 与交接

重视因优化导致的**假阴性**（被错过的节点）、staged revision 失效、先 TopK 后 Graph View 过滤、不可索引范围错误 Skip、过大 Bitmap 使 RSS 上升和取消后缓存残留。本 Stage 不是把 LadybugDB 的柱存/CSR 存储移植进 SQLite；复用 Lithograph 已有 B-tree/adjacency API。SIP、Join Dynamic Filter 与 TopK 都通过后才可进入 S22.7，不能降范围或仅完成其中一项便把整个 Phase 标为 `done`。

**Stage Gate：** 本 Stage 所有 A22-S6-* 必须得到当前源码的实际正确性、性能/资源和 Review 证据才可进入下一 Stage。局部通过不能单独将 Phase 22 标成 `done`。


<a id="stage-s22-7"></a>

## 11. Stage S22.7：Factorized Execution 与 ASP-Join

### 目标与前置

针对 Star、两跳/多跳、多对多扩展中的重复共享前缀，以 Factorized intermediate 减少重复行复制、重复谓词计算和不必要的扁平 Cartesian。参考 [Kùzu Factorization](../../research/query-optimizer-roadmap-evidence.md)；边界见 [Factorized / ASP-Join](../../design/query-engine.md#join-algorithms-and-graph-optimizations) 和 [Pull Streaming](../../design/query-engine.md#streaming)。

### 实施步骤与交付

#### S22.7.1 可预算的内部 Factorized Group

- 仅对已证明变量依赖独立的子计划，让一个共享前缀引用多个分组/有序序列，内部用有界 Factorized Group（flat / unflat）表示；所有 Node/Relationship ID、Value、multiplicity 和一致的访问证明继续来自当前 Snapshot。
- 因子数量、分组引用、分组累计 cardinality、乘积溢出、iterator continuation、取消点与拆组转换都纳入 QueryResources；不使用完全展开的 `Vec<Row>` 假装实现因子化。
- 无法证明关联性独立、出现跨组表达式/写 side effect/ORDER BY/OPTIONAL/Path 边界时精确展开或回退到 Stage S22.5 的 Flat/Hash 实现。

#### S22.7.2 Factorized Filter / Aggregate 与 ASP-Join

- 对能在分组内求值且不改变求值次数/错误时序的 Filter、Projection、COUNT/MIN/MAX 等操作复用因子化结果；其它表达式落回 flat row pipeline。不得对含非确定性、可抛错或 side effect 的表达式提前合并计算。
- 实现一个可以从 Stage S22.4–S22.6 Planner 选择的真实 ASP-Join 候选，复用已验证的 Adjacency、Hash、SIP/候选集路径，让多个分支保留共享前缀而不是再枚举全量中间结果。其 build/probe、mapping、内存与 TEMP 费用进入同一 Cost Model/PROFILE。
- **ASP-Join 算法身份必须明确**：依 [Kùzu CIDR 2023 原始论文](https://www.vldb.org/cidrdb/papers/2023/p48-jin.pdf) 的 **Accumulate–Semijoin–Probe** 三个真实 Pipeline 独立实现：(1) 累积 probe-side factorized tuples 到 query-owned、可预算的 Factorized Table 并生成无假阴性的**半连接过滤器（semijoin ID filter）**；(2) 将 filter 传给 build-side 可跳过实际 SQLite/Adjacency 读取的 Scan，构建有界 Hash table；(3) 回读积累的 factorized tuples 并 probe 形成完整输出。仅有 Hash Join + 一次额外 Bitmap 检查、或先扫描全部 build 输入再过滤，不算 ASP-Join。必须测量 Accumulate/Build/Probe 各阶段的实际时间、I/O、表大小、spill/取消成本；不能为了保留因子化把整个 probe 结果无界物化到 RAM。
- **Blocking 与 Streaming 的边界**：Accumulate 是真实算法工作，不能默认为 Cypher 语义要求的物化屏障；选用 ASP-Join 时要单独记录首行延迟、完成工作量、caller backpressure、early-close/interrupt，以及因累积而发生的 TEMP 与内存保留。只有在满足 [Pull-based Streaming](../../design/query-engine.md#streaming) 的原有 cursor 交互、预算与取消合同并有实测净收益时才由 Planner 选择；不满足时使用流式 Index Nested Loop/Hash/Expand，不能先跑完整 Query 再把结果分批返回冒充 pull cursor。
- `COUNT`、`DISTINCT`、`MIN/MAX` 等下推必须分别考虑分组 multiplicity、NULL、Empty Group、特殊数值/类型排序与 Path/Relationship 的 row identity；无法证明 group-level 与逐行表达式等价时不下推。特别验证 `.realtime()` / 随机表达式的每行求值次数没有因 factorization 缩减。

#### S22.7.3 Flat Output 与写入/事务边界

- 在 `lithograph_rows()` 最终 row、`lithograph()` adapter、相关子查询、需要全序的 Sort、具体写入/资源 barrier 等处按真实 Cypher multiplicity 正确枚举，不改变 visible column、路径/关系重复规则、skip/limit 和 transaction commit/rollback。
- Factorized state 必须 query-scoped 且在 historical/staged state 或 Graph View/access selector 变化后失效；不能把 grouped output 作为新的 application-facing result type。

### 局部 Acceptance（全部必做）

- **A22-S7-01**：Star、fan-out 与 2–4 hop 多对多场景在相同最终行数下，真实因子化中间表示的 bytes、复制次数和受影响的谓词计算次数低于 flat 基线；报告因子组合大小、实际 output rows、峰值 RSS/TEMP，不用 `count(*)` 替代完整 drain。
- **A22-S7-02**：ASP-Join 在适用的连接组件真实执行完整 **Accumulate → Semijoin-filtered Build → Probe** 三段流程，Plan/PROFILE 分别记录 factorized accumulation、semijoin filter 的真实物理 skip、hash build/probe、spill、首行延迟与完整输出工作量；至少一个场景相比原算法减少真实 I/O/中间复制且保留全部重复行。未到达 probe 的 early-close/Interrupt 能及时停止并释放 Accumulate/TEMP，不能发生后台全量完成；缺少可寻址 build-side skip、factorization 不安全、违反 Pull Streaming 或三段总成本超过收益时，可靠回退原已验收 Join 算法。
- **A22-S7-03**：精确 multiplicity、parallel edges、self-loop、NULL/TYPE_ERROR、非确定性、OPTIONAL、相关子查询、Path uniqueness、ORDER BY/TopK、SKIP/LIMIT 和 aggregation 的结果/异常与 Flat Oracle 一致；写入扩展后的每行副作用数无变化。
- **A22-S7-04**：跨 1k/10k/100k/1M、极端高 fan-out、超大输出与小 memoryLimitBytes 时，Factorized buffer 不随 flat intermediate row 数无界增长；无法保留压缩结构时 bounded spill/正确错误回退，在 1 GiB/no-swap Linux amd64 无 OOM。
- **A22-S7-05**：SQL scalar/rows 的完整结果消费、early-close、cancel、historical/staged/Graph View 和 provider/security 交互无回归；`cargo make quality`、`scripts/ci.sh`、Cypher TCK、Phase Review 与最终 diff 通过。

### 局部 Review 与交接

重点审核分组语义和多对多乘法基数溢出、错误被错误合并一次计算、提前展开导致内存反而更高、在 LIMIT/写入路径丢失行以及与已有 Row Cursor 分叉。不要整体重写所有算子为 Vectorized Executor；仅扩展能够明确收益的中间路径并完整验收。全部达到后才进入 WCOJ。

**Stage Gate：** 本 Stage 所有 A22-S7-* 必须得到当前源码的实际正确性、性能/资源和 Review 证据才可进入下一 Stage。局部通过不能单独将 Phase 22 标成 `done`。


<a id="stage-s22-8"></a>

## 12. Stage S22.8：WCOJ、多路邻接交集与复杂循环模式

### 目标与前置

对真实的三角形、四边环和更多可索引的循环 Graph Pattern，使用多路有序交集（WCOJ / Generic Join / Leapfrog 风格）减少二元 Join 产生的无用中间路径，同时保持所有 Cypher 25 Graph Pattern 语义。见 [Join 与图专项合同](../../design/query-engine.md#join-algorithms-and-graph-optimizations)、[Runtime 规模合同](../../design/runtime.md#optimizer-join-stress) 和 [Kùzu 研究](../../research/query-optimizer-roadmap-evidence.md)。

### 实施步骤与交付

#### S22.8.1 Cyclic Pattern 识别与选择

- 从 Stage S22.4 Join Graph 中识别可安全做多路交集的有向/typed/变量约束循环组件，确认每条所需边有有效的历史 Snapshot Adjacency/Type/Index access，决定变量交集顺序与可复用的 Stage S22.6 mask / Stage S22.7 factorization。
- 只有受支持的普通内连接循环可进入 WCOJ；OPTIONAL、可变长度路径、复杂 subquery/写 barrier、无法证明 equality/identity 关系或缺失必要访问时安全使用二元 Join/Expand。Cost Model 负责选择，不能因“环形”就无条件优先 WCOJ。

#### S22.8.2 Multiway Intersect 执行

- 对候选 NodeId/RelationshipId 有序列表执行 multiway intersection / leapfrog-style seek，按数据规模和选择性选择最少工作入口，消除无效两跳中间排列；扫描工作必须真实受 adjacency/index 可寻址性约束。
- 处理同一端点上多个不同 RelationshipId、不同 direction/type、自环、Node variable 允许/禁止相等、per-MATCH Relationship uniqueness、0/1/多结果与稳定 multiplicity；不把 set-based Node 交集误当最终 Cypher Relationship bag。
- 多路 cursor、buffer、semi-mask 与 factorized group 使用已有 QueryResources、TEMP 和 `sqlite3_interrupt`，支持大 degree/高 skew/small budget 的 bounded spill/拒绝；不能一次性物化全部组合。
- 实现前以实际 `Snapshot`/Adjacency/Index API 检查目标 Commit 中是否存在按所需 Key 排序、可 seek 的版本化迭代器；如果当前 API 不能满足，先补齐可测试的最小 ordered access（包括 committed base + staged delta），不能在无关全图里全量提取并排序后声称 WCOJ 的多路交集具有物理收益。

#### S22.8.3 Planner / 其它 Join 算法组合

- WCOJ 在复杂查询中只优化自身安全循环组件，余下连接仍可用 DPhyp、Index Nested Loop、Hash Join、SIP 与 Factorized 中间表示；跨组件的 Join 顺序和输出错误时序保持原语义。
- 真实 EXPLAIN/PROFILE 报告 `MultiwayIntersect` 等本项目拥有的算子、输入列表长度/交集步骤/VM/内存，而不是宣称所有 Query 都符合 worst-case optimal 理论上界。

### 局部 Acceptance（全部必做）

- **A22-S8-01**：Triangle、4-cycle、Clique 子集和多环 mixed Pattern 在真实数据/SQLite 上可由 Planner 选择 WCOJ；实测 join/intersection steps、生成中间候选/VM/RSS 与已验收 binary joins 对照，在至少一个选择明确的高 fan-out cyclic fixture 上减少工作量。
- **A22-S8-02**：所有测试结果与 Cypher 25 Oracle 完全一致：directed/typed edges、parallel Relationships、自环、重复 Node variable、Relationship uniqueness、NULL/error、OPTIONAL/Path 的拒绝优化边界、Graph View/Read Access、历史 Commit/staged revision、写入原子性。
- **A22-S8-03**：多路交集通过真实可寻址的 ordered adjacency/index 工作，不以扫描全部 Relationship 再排序判定空结果；当 index/adjacency 无法满足时退回二元计划，仍能在 1k/10k/100k/1M 及 10x 无关数据增长下解释实际访问工作。
- **A22-S8-04**：高 degree、heavy skew、结果体积大于内存、提前停止/取消、1 GiB/no-swap Linux amd64 压力下，WCOJ 与 Factorized/Hash 共存时无 OOM、半写入或泄露临时资源。
- **A22-S8-05**：涵盖 Stage S22.1–S22.8 所有新增算法的同一综合 benchmark/performance corpus、可回放 before/after、`EXPLAIN/PROFILE`、真实 SQL scalar/rows、历史/Schema/Index/Search 与适用 Cypher TCK；旧版功能及现有 10M/100M 硬性规模门槛不降低。
- **A22-S8-06**：`cargo make quality`、`scripts/ci.sh`、适用 release-tier 规模验收、文档链路、Phase 22 跨 Stage Review 和 diff 通过；八个 Stage 的实现与真实提交/发行状态分开记载。

### 局部 Review 与交接

重点检查有理论优势却错误适用于非循环/可选匹配、Node 集合交集消灭 parallel Relationship multiplicity、与 SQL NULL/TYPE_ERROR 不等价、异常 path uniqueness、构建期间资源爆炸以及低选择性时比简单 Index Nested Loop 更差。对不适用场景保留经过验证的旧算法；不能通过弱化正确性或缩小 fixture 标记 `done`。

本 Stage 局部通过只代表 WCOJ 算法已通过定向验收；整个 Phase 仍须完成第 13 节全局门禁。发布、Tag、Push、部署和 KG OS 集成均须单独授权及执行。

**Stage Gate：** 本 Stage 所有 A22-S8-* 必须得到当前源码的实际正确性、性能/资源和 Review 证据才可进入**第 13 节统一综合验收**。局部通过不能单独将 Phase 22 标成 `done`。

## 13. 统一综合 Acceptance 与唯一完成门槛

**不允许**仅因 S22.8 完成而自动将 Phase 标记为 `done`；必须同时取得全部 **A22-S1-*～A22-S8-*** 的局部证据和下列 A22-G01～G06 的全局证据。任何测试被 skipped、未运行、退出状态未知或在旧源码上通过，都不能标为本轮 `PASS`。

### A22-G01：完整 Cypher 25 正确性与跨算法互操作

- 在普通查询中使用同一 Fixture 和 pinned Commit 运行各目标优化计划及旧的正确 fallback，使用可审查的 Cypher 25 Oracle 对比**逐行 Identity/Value/Property、重复行/Relationship、排序/TopK、NULL/TYPE_ERROR 与错误时序**，而不是仅比较结果计数。没有 `ORDER BY` 时按完整 bag/multiplicity 对照；交易子查询每个 batch 以其独立 pin 为准，不能强制拿 outer query 的单一 Commit 比较。
- 必须覆盖 OPTIONAL/Apply/相关 subquery/Union、Graph View/Read & Write Access、历史与 staged Snapshot、Relationship uniqueness、Path、不同 Label/Type/Property family、Search 的范围内 TopK 与现有 ANN recall/tie 保证。
- 对非确定性/实时函数不要求不同轮随机结果字节级相同；对照可观察的求值次数、时钟边界、无非法提前调用与可固定的输入输出。Search 中 ANN 使用现有 exact oracle/recall 与 cutoff tie 合同，不误把相同设置下的近似结果按任意 ID 字节一致作为普遍 Cypher 等价要求。
- 混合计划至少包括 Index Nested Loop + Hash、Hash + SIP、SIP + Factorized、Factorized + WCOJ、WCOJ + 其余二元 Join；正确处理一个 Stage 不适用但其它 Stage 适用的情况。不改变 application-facing `lithograph()` / `lithograph_rows()` 输出合同。

### A22-G02：全过程资源、失败原子性与重启

- 在**真实 Linux amd64 1 GiB/no-swap** 容器执行 Planner 大组件、Hash build/Grace spill、重度 skew、SIP bitmap、Factorized group、Multiway intersect，以及 Snapshot Maintenance；记录 cgroup `memory.max`、`memory.swap.max`、`memory.peak`、`oom_kill`、进程 RSS、Engine memory、TEMP/WAL/disk bytes、spilled batches 与 writer-hold。
- 取消、`sqlite3_interrupt`、RESOURCE_ERROR、SQLite TEMP 不可写/空间不足、早停、Read/Write cursor 释放、事务回滚及 SQLite 进程重启后，没有 OOM/未释放 Temp、未提交部分可见性或不可恢复锁。必须**区分**普通 query 的全体未发布写入回滚、SQL explicit transaction fail-closed abort、`CALL { ... } IN TRANSACTIONS` 已 durable 的 inner batch 保留与后续失败 batch 的回滚；不得笼统要求回滚合法已 durable 的 batch。`memoryLimitBytes` 仍为 Engine 计账预算，不能替代 cgroup 实测。
- Checkpoint/Index derived cache 丢失、损坏、版本不匹配、GC/Branch move/staged revision 变化时，无数据串读或越权访问；对所有执行和维护路径保持 immutable history 与 durable Commit 一致性。

### A22-G03：性能/规模与非回退

- 对第 3 节全部九组 Fixture 建立固定 seed、真实 cardinality、State/Schema、同机相同 SQLite/build、warm/new-connection 的 Before/After；每种新算法必须有**至少一个真实执行且物理工作改善**的正例与**至少一个正确拒绝/回退**的反例。
- 1k→10k→100k→1M 和 10 倍无关图数据增长的访问/中间候选、SQLite VM_STEP/FULLSCAN_STEP、P50/P95（样本量支持时）、CPU、RSS/TEMP 正反证据完整；结构性门槛沿用 Runtime，**真实没有 Index 的属性 Scan 不承诺常数复杂度**。分离 Linux amd64 1 GiB/no-swap 的资源强压与 Apple M2/Release 的既有 10M/100M 性能 SLA；如使用 QEMU 标注仿真，不能用其 wall-time 对照原生 SLA。
- 适用的 10M Node / 100M Relationship release-tier 和写入/History/Diff/explicit transaction/Merge 固定非回退标准继续满足；不能通过关闭 durability、安全/Graph View/Provider、减少数据/输出、隐瞒 checkpoint build 或删除慢样本伪造通过。
- Stage 内减少物理工作但导致其它常用 workload 严重回退，应先修正 Cost Model、限定合法适用场景并重新测量，不直接降低门槛或宣布算法不需要。

### A22-G04：现行仓库质量与真实 SQLite 全集成

- 完成 `cargo make quality`、`scripts/ci.sh`，均以**真实 exit 0** 收尾；Rust/Clippy/Rustdoc/complexity/duplicate/coverage、stock SQLite 3.45.0+ loadable extension、Core + SQL scalar/rows、Cypher TCK、Storage/Schema/Index/History/Version/Mutation/Streaming/Cancel/Search/Provider 与实际 ABI 适用集无回归。
- 不改冻结的 `CY25-2026.08` Profile 分母、不降低覆盖率/资源/压力/性能门槛、不把编译成功、单个 microbenchmark 或仅 EXPLAIN 展示当作 Engine 验收。
- 真正影响用户 API、性能限制或兼容边界时同步对应设计、使用指南/参考与兼容 inventory；普通内部优化不新增不必要的用户可见公共参数或第二套调试接口。

### A22-G05：多轮 Review、文档与最终 Diff

- 每个 Stage 坚持「检查现状 → 判断最小方案 → 修改 → 正反与压力验证 → Review → 修复 → 受影响测试重新执行 → diff」，审核语义、类型、权属、版本状态、资源预算、TEMP/Spill、恢复、Benchmark 真伪、许可与侵入性。
- Review 对每个 Finding 记录严重度、复现 fixture/语句、文件/路径、结果/性能影响、最小修复、重新验证和剩余限制。**无本 Phase 验收范围内的未解决 blocking Finding** 才通过，不声称测试可证明绝对没有 bug。
- 逐项核对 Stage 状态/证据与 Git staged/unstaged/untracked，保留用户无关修改；新文件、Secret、生成数据库、日志、缓存、临时构建产物不得误提交；文档路径/锚点、`git diff --check` 与新增文件 whitespace 校验通过。

### A22-G06：单一 Phase 状态与交付事实

- 本文唯一持有 Phase 22 的状态与完整验收记录。所有八 Stage 的 A22-Si-XX 与 A22-G01～G05 真实 PASS、证据写入/回读后，才能将 Phase 从 `in_progress` 标为 `done`。
- 即使本地 Phase 已完成，仍须分别记录**已修改、已验证、已提交、已推送、已构建发布、已部署**；Commit、Push、Tag、Release、生产部署及 KG OS 集成各自需要对应授权和实际验收。
- 不再创建独立的 Phase 23–29 来接收本计划中的未完成工作；任何未完成算法必须保持在本 Phase 22 的对应 Stage，直到原验收全部满足。

## 14. Stage 实施证据记录规范

每个 Stage 的真实进展**只追加到本文件**，不为八个 Stage 建立八套独立路线或 `done` 状态。Stage 达到局部 Gate 时应记录：

| 字段 | 必须是真实执行取得的证据 |
| --- | --- |
| Stage / Acceptance | S22.i、A22-Si-XX、适用的 A22-G 编号，未通过的逐项 blocker |
| 源码版本 | 当前 Git HEAD、dirty-tree digest、变更文件/真实物理调用路径，是否仅本地未提交 |
| 测试环境 | OS/CPU/Rust/SQLite/build、Fixture seed、真实 Node/Relationship 数、Index/Checkpoint/Commit/Graph View/Access |
| Oracle 与回退 | 优化前/后逐行结果/异常/重复/写入副作用，算法实际命中与不适用回退各至少一例 |
| 性能与资源 | Planner 与 Snapshot 时间、first/full-drain、SQLite VM/IO、Join work、CPU、RSS、TEMP/Spill、writer-hold、Linux cgroup |
| 错误与恢复 | budget/取消/early-close/并发/GC/rollback/重启后 integrity 的实际结果 |
| Review 结论 | 发现的问题、修复、对应重测、是否可进入下一 Stage，以及剩余非阻断局限 |

## 15. 风险清单与持续审核重点

1. **错误语义**：Predicate/Join Reorder 提前抛 TypeError、吞掉 NULL、越过 OPTIONAL/Apply/写 barrier；Hash key equality/NULL/数值家族不符合 Cypher；Factorized 错合并非确定性求值、减少写入或 Relationship 重复；WCOJ 把 Node Set 交集误当 Relationship bag。
2. **版本与权限**：借 current Branch HEAD 的统计/Index 做历史查询、GC 后沿用旧 checkpoint、Query mask/TopK 跨 Graph View/Access 或 staged revision 复用、无 writer 授权的只读查询暗写 main。
3. **无界资源**：DPhyp 组合爆炸、Hash 极端 skew 无限分桶、Semi-mask 先完整 Scan 再过滤、Factorized group 乘积溢出/全部展开、TEMP/WAL/RSS 隐藏增长、取消后持锁或半提交。
4. **性能伪证**：只改 `EXPLAIN` 名称、把 fixture/Checkpoint build 藏在计时外、不同 SQLite/缓存状态直接比较倍数、少返回结果换速度、把 `memoryLimitBytes` 当进程 RSS 硬上限。
5. **过度设计**：新增没消费者的 Memo/Planner 插件、独立数据库 Server、SQLite Fork、重复资源管理、未审核许可证的第三方源码或无需求的新 Public API。

算法在某类输入不适用时必须**实现适用路径、证明真实收益，并让不适用输入安全回退**；不能把“不适用”当成取消该算法的理由。环境/测试执行失败如实标记 `blocked`，不删验收、不虚构测试通过。

**2026-11-10规划时状态（历史）**：当时仅完成一个 Phase 22 计划的整合与文档审核；S22.1–S22.8 的代码均未实现，Phase 维持 `ready`。本文件定义完整验收，不构成授权提交、推送、发布或部署；当前实现状态以上方状态表及后续实施记录为准。

## 16. 2026-11-10 文档规划与 Review 证据

- **合并范围**：原八份尚未提交的 Phase 22–29 计划已整合为**本文件一个 Phase 22**；原 25 个 Feature 和 46 项局部 Acceptance 逐项映射为 `S22.1.1～S22.8.3`（实际 S22.3 有四个 Feature）及 `A22-S1-*～A22-S8-*`，无遗失。另新增共用 Fixture、性能/正确性/资源测量合同和 A22-G01～G06 综合验收。
- **设计与研究引用**：已同步 [Query Engine](../../design/query-engine.md#optimizer-and-budgeted-executor)、[Storage](../../design/storage.md#tiered-checkpoint-policy)、[Runtime](../../design/runtime.md#optimizer-join-stress)、[开发路线](../README.md)、[算法研究](../../research/query-optimizer-roadmap-evidence.md)。冗余的旧八份 Stage 来源文件在确认所有外部引用已迁移后清理；单一 Stage Plan 无第二套状态真源。
- **Review 发现与修复**：修复原规划中的阶段状态自相矛盾（例如单独 Stage 写成 `done`、S22.8 仍写“进入下一 Stage”）、研究与设计对不存在旧 Phase 的引用、无索引扫描局部验收与统一内存/性能门槛没有明确挂钩等问题；保留所有算法的正反/性能/回退要求，没有放宽正确性或规模阈值。
- **规划文档基线检查**：2026-11-10 合并并同步 `v0.3.5` Git 基线时，Python 3.14 运行 `docs/guide/examples/check_docs.py`，**83 个 Markdown 文档**的链接/覆盖检查通过；当时专项检查的 6 个文件共 **204 个本地链接**，八 Stage / 25 Feature / 46 局部 Acceptance / 6 全局门禁完整。以下继续审核产生新的文档修订与更高的链接数，旧数字仅表示当次检查，不代表最新文件状态。
- **最新 Git 复核与交付边界**：仓库现为 `main@3f1f31e`（本地 `v0.3.5` Tag 指向该 Commit），Phase 21 代码已进入此历史；复核前工作区干净，本次仅形成三份未提交的文档修订。本轮 Phase 22 **文档审核没有执行 Commit、Push、Release 或部署**，也没有实施 Phase 22 的 Rust 代码。远端 Release 和平台制品是否通过，不能由本地 Tag 单独推断；后续开发重新读取真实工作区与测试结果后再维护 Stage 证据。

### 本轮实质性 Review 与修复（2026-11-10）

基于当前 `Query Engine / Storage / Runtime / Schema / Compatibility` 真源、现有 `Snapshot::resolve`、Planner、`QueryResources` 与 `v0.3.5` 源码，对全部 Stage、负例、输入、资源预算和 Done Gate 重新检查。本轮仅修订**本 Phase 22 计划**，没有新增独立 Phase、算法、公共配置或 Rust 实现。

1. **Snapshot 的多批事务语义**：将“一次 execution 只 resolve 一次”限定为普通语义，明确 `CALL { ... } IN TRANSACTIONS` / 并发事务子查询每个 inner batch 独立 pin Branch head，staged clause 可见性与 Graph View membership 随 graph state 更新，并在 A22-S2-05/06 和跨 Stage Oracle 中增加实际验收。区分普通执行的 rollback、SQL explicit fail-closed abort 与事务子查询已经 durable 的 batch 保留。
2. **Planner 与内存计账**：根据 Runtime `memoryLimitBytes` 不包含 AST/Plan 的现有合同，明确 S22.1 的 Planner 需要独立、内部、可重复触发的状态/分配上限和取消回退；Executor 仍使用原有 `QueryResources`，两种预算不能互相冒充。用现有 Label/Index 统计完成 S22.1 实际访问选择，避免与 S22.3 新统计发生循环依赖。
3. **Cypher Oracle 与时钟**：没有 `ORDER BY` 和分页时按真实行 **bag/multiplicity** 对照；有明确排序才验证其行序，没有 `ORDER BY` 的 `SKIP/LIMIT` 只检查规范允许的子集、成员和重数，不虚构确定性行序。零行 outer、可能出错的 Build side、非确定性/`.realtime()`、参数类型改变与 prepared statement 复用均纳入正反例，防止 Join/Hash/Factorization 提前求值造成原本不会出现的 TYPE_ERROR 或副作用。
4. **权限、派生统计与动态过滤**：S22.3 增加 Query Access 禁止 Property 时的 Index/MCV/`EXPLAIN/PROFILE` 不越权测试，区别 `readAccess.query` 与 `readAccess.result`；S22.6 对未完成 Build bitmap、staged 修改或新 inner batch 明确不能产生 false negative，权限/Graph View 仍由当前状态确定。
5. **WCOJ 可实施性、性能与平台**：S22.8 必须先验证真实版本化 Snapshot/Adjacency 是否提供有序可寻址访问，不允许全图物化+排序伪装 Multiway Intersect。明确 QEMU/原生 Linux amd64 与 Apple M2 Release Tier 的独立测量，保留 1/4/8 readers + 1 writer、30 分钟、10K Merge 的既定 Stress 非回退；不把构建 OOM 当运行时 OOM。
6. **ASP-Join 算法来源与实现判定**：核对 [Kùzu CIDR 2023 原论文](https://www.vldb.org/cidrdb/papers/2023/p48-jin.pdf)，明确 S22.7 的 Accumulate–Semijoin–Probe 三 Pipeline、可预算的 Factorized Table、真实 build-side skip、Probe 与各阶段成本；加上没有物理跳读时的安全回退，禁止以改名 Hash Join 或仅一次 Bitmap 检查冒充已交付。

所有这些是**已有设计合同的开发步骤/验收消歧**，未改变设计真源；全部原 25 Feature、46 局部 Acceptance 和 6 个全局 Gate 仍保留，Phase 22 状态仍为 `ready`，当前未发生代码验收。

**最终复验（本轮最后一次修订后）**：`/opt/homebrew/bin/python3.14 -B docs/guide/examples/check_docs.py` 实际 exit 0（83 个 Markdown 文件）；专项逐链接检查 6 个相关设计/开发/研究文档的 **212 个本地链接，0 断链**；八 Stage、25 Feature、46 局部 Acceptance、6 全局 Gate 的编号、唯一性和范围检查通过；`git diff --check` 通过。Git 无 staged 和 untracked 文件，未出现旧 Phase 23–29 文件；当前三份未提交 Markdown 差异中，本次持续 Review 只修改本 Phase 22 计划，保留此前既有的开发路线与研究文件修改。**在本次审阅范围内没有剩余阻断性文档 Finding**；这不表示 Phase 22 的 Rust 算法已实现，也不等于证明未来所有行为无缺陷。

### 本轮继续 Review：构建路径、算法交付与预算门槛（2026-11-10）

- **S22.2 Checkpoint 构建器风险**：当前 `create_checkpoint` 会通过 `first_parent_layers` 从 Root 收集全部 Layer，再逐层回放。仅加自动触发器并不能解决深历史上的维护成本。S22.2.2/A22-S2-03 现要求优先复用兼容的 first-parent 祖先 Checkpoint、只应用新增 Layer，构建器自身的 Layer 数、暂存内存、取消、I/O、Writer Hold 与 fallback 也必须纳入真实验收；不能只测建好 Checkpoint 后的读延迟。
- **S22.4 参数化 Lookup Memoize**：只允许纯确定性、不会改变值/异常/外部副作用求值次数的内侧 Seek 做 query-local 缓存；参数类型、schema/index generation、Graph View/Read Access 与 staged revision 都要参与有效性判断，不因 repeated outer key 重复调用 `.realtime()` 等非确定性表达式。
- **S22.5 Hash Spill 的可观测完成**：原 A22-S5-03 用“成功 Spill **或** 资源错误”可能使一个从未成功执行 Grace Spill 的实现也获得 PASS。现拆开强制的**实际成功 Spill**和资源/磁盘失败的独立负例；结合当前 `query/spill.rs` 的序列化实现，增加 Cypher typed Value/Join Key、NULL、NaN/负零、Element Identity 的跨 TEMP round-trip 检查。
- **S22.6 / S22.7 的阻断算子成本**：TopK 在 `SKIP` 存在时应按可见候选的 `skip + limit`（checked arithmetic）计算安全边界；ASP-Join 的 Accumulate 是真实阻断式工作，必须对 first-row、backpressure、early-close/cancel、Spill 与净收益做审查，不能把先跑完整查询再切块输出冒充既有 Pull Streaming 合同。

上述为**已经写入各 Stage 的新增验收消歧**，无新 Phase、新公共配置或实现范围外扩；原算法全覆盖和所有 A22 编号保留。新增条目属于未来代码的 Acceptance，尚无任何 S22.x 可以因此声明实现通过。

**本轮最终文档校验**：2026-11-10 在 `main@3f1f31e` 的未提交工作区运行 `/opt/homebrew/bin/python3.14 -B docs/guide/examples/check_docs.py`，返回 exit 0，83 个 Markdown 文件通过；独立检查 6 份相关文档的 **213 个本地链接，0 断链**；8 Stage、25 Feature、46 局部 Acceptance、6 全局门禁的编号、唯一性、Stage Gate 和完整性检查通过；`git diff --check` 通过。没有 staged/untracked 文件，Git 仅保留三份原有的 Markdown 修改范围，本次只修改 Phase 22 计划。**本轮审阅范围中没有剩余的阻断性文档问题**；实际 Phase 22 实现、全量 Rust Quality/CI、压力测试与发布均尚未执行，不能因文档 Review 通过而标记为完成。


## 17. S22.1 本地实施与验收（2026-11-10，持续验证中）

本轮仅实施 S22.1，Phase 22 为 `in_progress`，S22.2–S22.8 尚未实施。基线 `main@3f1f31e` / 本地 `v0.3.5`，初始三份用户 Markdown 差异已保存；第 15–16 节的规划审核记录原样保留。当前没有 Commit、Push、Release 或部署。

### 17.1 实现和 Review

- 在原 `MatchStep` / Logical / Physical Planner 上比较现有 ID、Standard Index、Pattern Label 与 Graph View 必需 Label；按目标 Snapshot 的 Schema/type proof、参数、可用 bounded statistics 选择实际 cursor，不增加公开 hint、第二套 Plan IR 或统计正确性真源。未知估计不等于零行。
- optional planner 内部状态、candidate、workspace 与递归深度独立有界，超限走现有安全路径；保留 `QueryResources`、TEMP、取消与 SQL 事务清理。program 只对能够消费相同 MatchStep 的 streaming/TEMP 路径声明 costed access；写后 MATCH 不复用 base-Snapshot proof。
- Review 已修复：Index 消除本应发生的除零/nullable 后续错误；小图误丢既有 Index；Point Map 遍历遗漏；volatile/expanding 函数在 optional folding 中提前求值；seek 参数复制前未预留完整租约；无谓词的小查询错误触发 planner fallback。
- 全量 TCK 首次发现 EXISTS 聚合子查询误入 scalar MATCH cursor，已增加 read/PROFILE 回归并修复 eligibility；仍须确认修复后完整 TCK/CI 终态。program 写入前缀改用现有 BindingSpill 写入执行器，正在验证完整流、quota 与失败回滚。

### 17.2 当前真实门禁

证据保存在本地忽略目录 `target/phase22-s1/`；本节记录可审查的命令、计数、限制与未通过项，不把启动或日志中间进度当 PASS。

| Acceptance | 当前状态 | 证据与待完成项 |
| --- | --- | --- |
| A22-S1-01 | 验证中 | 原 S1 12/12、Phase07 38/38；真实 SQL 48 checks、C `sqlite3_prepare_v2`/reset/rebind 435 checks。最新 EXISTS 回归与 program TEMP 修复后需重跑全套。 |
| A22-S1-02 | 已取得物理工作改善，最终源码重测待收口 | 同 fixture 1k/10k/100k：Phase21 VM_STEP 99,454 / 5,011,675 / 454,386,613；After 均 2,468，实际 `LabelIndexScan(Rare)`、dbHits=6、正确两行。prepared statement 的参数、Graph View、Access、历史与同名 IndexDefinition 修订由 C 测试覆盖。 |
| A22-S1-03 | 已触发，最终重测中 | 16/128 Pattern 不 fallback；256 Pattern 与 600 连续 MATCH 触发 fallback，仍返回 7。600 MATCH 计数 states=3018、candidates=1006、charged workspace=262063 bytes，未越内部上限。该数为 planner 工作计费，不等于 RSS；另测进程/cgroup peak。 |
| A22-S1-04 | 验证中 | SQL cancellation/quota/early-close、历史/staged/失败写已覆盖；program prefix TEMP 改动还需小预算完整消费与失败后 Head/值不变验证。复杂 eager fallback 不新增 costed cursor 或新 output-sized Vec。 |
| A22-S1-05 | 已取得 QEMU Linux 资源证据，最终源码重测待收口 | Colima ARM 宿主、Linux x86_64 QEMU；memory.max=1073741824、memory.swap.max=0。1k/10k/100k 各 5 connections×20、100 reopen、3 次完整流及 interrupt 通过；cgroup peak=656183296、oom/oom_kill=0，100k RSS=278511616。不将仿真 wall-time 声称原生 AMD64 性能。持续混合压力与最终 artifact 重测待完成。 |
| A22-S1-06 | 未通过 | 首次 CI exit 1：一个适用 TCK correctness Finding；已定位修复，必须重跑。Quality 首次缺 lizard，使用既有 `.quality-venv` PYTHONPATH 后发现生产复杂度超限；正在局部拆分并重验，未降低标准。最终 diff/链接与全文验收未结束。 |

### 17.3 基准环境和边界

宿主 macOS 26.7.1 arm64、Rust 1.98.1、Python 3.14 / SQLite 3.51.0；Linux Python / SQLite 3.54.1，runtime 容器 `--network none`、仓库只读挂载，合成 fixture 单独构建并 checkpoint。主库读路径使用 SQLite `mode=ro`，完整流逐行校验数量与 checksum，并比较查询前后 SHA256。

After release artifact（本轮 program 修复前）的 SHA256 为 `87515963bb878b6fa0692c30292c16a71accc5b5d454f977378ed5642f67a795`。原生 macOS warm P50/P95（微秒）1k=681/1291、10k=702/1609、100k=734/1852；100 reopen P50/P95=725/2006、735/2257、1032/2264。100k 三次完整消费 100000 行约 2.00/1.92/1.81 秒，RSS peak 82919424 bytes，实际 interrupt 约 48 ms。该轮与构建并行，wall-time 不作为无干扰 SLA；VM 工作量证据独立于调度噪声。最终 artifact 将另记重测结果，不沿用旧二进制假称新修复已验证。

共享 Runtime 的 10M/100M release-tier、全部八 Stage/全局 A22-G、原生 AMD64 wall-time 与 30min×并发档位全规模压力不由上述 S1 小规模证据代替。S1 六项未全部真实 PASS 前不进入 S22.2。

## 18. 2026-11-11 开发续接与阶段性实现（未通过 Phase 22 验收）

本节续接第 17 节的历史进度，不覆盖原始基线或把早期测试替换为当前证据。目标仍是一个包含八个必实现 Stage 的 Phase 22。所有新增代码均在本地未提交工作区；没有 Commit、Push、Release 或部署。

### 18.1 已实现但尚未取得完整 Stage Gate 的部分

- **S22.1**：此前的 Costed Access/Planner Budget/Program Streaming 保留。本轮 `cargo test --locked -p lithograph-core --test phase22_access` 的当前 15 项集成测试通过，其中包含新增 Relationship ID 测试；完整 Quality/CI、资源与混合压力须按最终二进制独立回验。
- **S22.2 局部实现**：现有实现新增 first-parent 兼容祖先 Checkpoint 复用、bounded lineage/replay 估算、Commit writer boundary 上的抽样自动维护、metadata 缺失/损坏时的 canonical fallback 和中断回滚。本轮额外约束：当 derived-only maintenance 遇到明确的 SQLite FULL/NOMEM/TOOBIG/BUSY 时在已回滚自身 savepoint 的前提下延期，`SQLITE_INTERRUPT`、损坏与 canonical 错误继续失败传播。新增对应错误分类单测；深历史、大图、writer hold、重启与 GC 的全套接受证据仍需验证。
- **S22.3 Relationship ID Seek 局部实现**：`elementId(r)=<合法的 r:ID 常量或参数>` 及对称形式可以在实际 Snapshot 上以 Relationship ID 直接查找，接入 Logical/Physical、Query Cursor、PROFILE 与 program plan。只允许前导可安全路由的 AND equality；仍校验 Type、端点、方向、Graph View、绑定、残余 WHERE，非法/非 String/NULL 保留既有语义。回归测试覆盖当前/历史、已删除关系、方向/Type/Graph View 与不存在的合法 ID。Self-loop、parallel Relationship、staged create/delete/abort、1M 规模与 Type/Index 全家族组合尚待补齐，**A22-S3-02 尚不能判 PASS**。
- **S22.3 其他部分**：Versioned Statistics 的准确度/样本/degree、Property Index MCV、更多谓词与无索引扫描优化尚未完整交付。S22.4–S22.8 的 Join/SIP/Factorized/WCOJ 算法尚未实现，不得以 Access 或 ID Seek 的回归通过代替。

### 18.2 质量 Review 与环境边界

- 原生 macOS Rust Core/Support 全量测试已在本轮完成过一次 exit 0；新 Relationship Seek 的 Core 集成测试为 15/15。新增变更需经最终同源码 `cargo make quality`、`scripts/ci.sh` 和真实 Release 扩展压力复验。
- 第一次完整 CI 因选中没有 `sqlite3.enable_load_extension` 的 Python 3.9 运行时而在 SQL 扩展阶段停止；本机 Homebrew Python 3.14 / SQLite 3.51.0 已被确认支持扩展加载。随后 CI 中 Clippy 报告了可合并条件、Quality 报告函数圈复杂度及代码行数超限；已按原有阈值拆分/修复并安排重新运行，不降低门禁。
- 旧的 QEMU 1 GiB/no-swap 与 1k/10k/100k 数据不能直接算作本轮新 Release artifact 的最终压测；必须重新记录当前 SHA、独立连接、完整 drain、取消、RSS/TEMP、并发与磁盘，并对未覆盖的 10M Node/100M Relationship 以及 30 min×并发档位保留阻断状态。

**当前结论**：Phase 22 仍为 `in_progress`，任何一个 Stage 的局部代码与测试通过均不能宣称 Phase 完成。完整局部 Acceptance 与 A22-G01–G06 仍须全部获得当前源码的证据。

## 19. S22.1/S22.2 续接实现与实测（2026-11-11，冻结制品历史证据）

本节更新第 17–18 节的阶段性事实；历史失败与旧二进制的数据保留，不冒充最终通过。工作区仍未提交、未推送、未发布。续接时 S22.2 与 Relationship ID Seek 局部代码已经存在，不能据此补记其进入条件已经通过；当前先闭合 S22.1/S22.2，再开始后续 Stage 的其余实现。

### 19.1 本轮 Review 修复

- 普通 write/program 从已准备的 immutable Snapshot 创建 mutation context，Commit validation 复用同一 base；独立 transaction batch 继续各自 resolve。Commit validation 仍校验 connection identity、expected Commit 和实际 graph invariant；错 connection/错 Commit/非法 Property owner 的负例均验证回滚。真实 query instrumentation 覆盖 read、write、program、staged CREATE→MATCH，每次普通 execution 的 `resolved_state_builds=1`。
- 自动 Checkpoint 以一个 savepoint 完成 build、automatic marker 和 retention；只保留四个策略拥有的 Checkpoint，不接管或删除手动 Checkpoint。取消、分页复制后的删除/属性变更、first-parent 与 sibling/second-parent 隔离已加入永久测试。实现合同继续由 [Storage](../../design/storage.md#tiered-checkpoint-policy) 拥有。
- 为 Checkpoint statistics 补齐 group/metadata 边界与取消检查；超限返回可安全延期的资源错误，所有部分构建回滚。非法 count、重复 ID、超大或未知版本 metadata 不供 Snapshot 使用，回退 canonical history。该限制不改变 storage format、Commit ID 或 canonical history。
- 修复基准测量入口：C worker 直接只读打开已经初始化的 fixture，不在计时前通过 `lithograph_init()` 全量 integrity check 隐式预热；扩展始终指定 `sqlite3_lithograph_init`，兼容旧 Python 的 `load_extension` 签名。Checkpoint worker 用安全 rusqlite trace 记录真实全部 SQLite VM，不输出 SQL 或凭据。构建本身的 RSS 由独立 child process 测量。

### 19.2 当前制品与可复现证据

完整原始数据在忽略目录 `target/phase22-current/`；Before 为 `main@3f1f31e` 的独立已冻结二进制，SQLite runtime/缓存类别分别记录。本节 frozen 生产源码清单 `s22-reviewed-source.json` 的 SHA256 为 `3c626569cc7f5741421ed51a70fc5194dabda095d6d9bfb7aa8202f704dc83ba`。该次 macOS Release 扩展 `s22-reviewed.dylib` SHA256 为 `bd96697d01027fc06698932d780dd4927edff6717021037c6e078dceccaf739e`；Linux amd64 `s22-reviewed.so` 为 `698b2e8fadb6f7324f16386d26b997c1c5d0b7dbf8ffa55a0d1cbb72820a0b64`。Linux 为 Colima ARM 宿主上的 x86_64/QEMU、Python SQLite 3.46.1；macOS 使用 SQLite 3.51.0。仿真 wall-time 不作原生 AMD64 SLA。

| 检查 | 已取得的真实结果与边界 |
| --- | --- |
| S22.1 物理工作 | 原 Before 1k/10k/100k 图为 99,454 / 5,011,675 / 454,386,613 VM。current reviewed artifact 的三规模均为 2,475 VM，实际 `LabelIndexScan(Rare)`、dbHits=6、正确两行，5×20 warm、100 reopen、三次完整流与取消全部通过；C prepared statement 721 checks 通过。Linux 三规模相同完整复验 exit 0，cgroup peak 分别 327,823,360 / 366,514,176 / 427,798,528 bytes，OOM 为零。修正后的同一 C worker 对 Before 复测为 99,442 / 5,011,663 / 454,386,601 VM；前述旧口径保留为历史，不与新口径混算。 |
| 同终态、不同历史深度 | `s22-history-depth` 的 10/100/1k/10k Commit、1000 Node 全部完成 5 connections×20 warm 与 100 reopen。10k 深度 missing/automatic/manual read VM 分别为 1,226,116 / 56,503 / 2,134；同一终态与主库 SHA256 均验证。该次 artifact 是 `s22-final.dylib`（随后只有等价的 metadata totals helper 拆分）。 |
| 祖先 Checkpoint 复用 | 上述四种深度的第一个 build 分别 replay 12/102/356/459 layers；每一种第二次 build 均只 replay 新增 2 layers，使用 1 个兼容祖先，并分页复制已有 rows。原始报告分别保留 builder VM、时间、RSS、复制行数、文件体积与 read 延迟。 |
| 大 Delta | 1/10/100 MiB 合成 Property Delta 已完成 missing/manual 对照；100 MiB 分布在 1000 Node。当前 reviewed Linux artifact 的 100 MiB 两种状态各 100 warm/100 reopen、资源拒绝、真实 interrupt 与恢复全部 exit 0。1 GiB/no-swap 的 `oom=oom_kill=0`；manual 读取的 cgroup peak 达到 1 GiB 且发生 page reclaim，进程 peak RSS 为 684,449,792 bytes，不能把此结果说成没有内存压力。 |
| SQL Read Guard 与维护 | current reviewed macOS SQL 测试 exit 0：live WAL cursor 经 768 次 writer 操作仍返回 pin 时的值；同连接 writer 被拒绝；rollback journal 下阻塞 writer 按既有 whole-rollback/quarantine 合同恢复；readonly/EXPLAIN/PROFILE/reopen 无主库写入。checkpoint publication 一次，事件 hold 484,271 µs；768 次 writer hold P50/P95 9,974/155,950 µs，peak WAL 8,647,912 bytes。Linux 1 GiB 对应正确性测试此前也 exit 0。 |
| Checkpoint 构建与进程崩溃 | 永久脚本 `tests/phase22_maintenance_work.py` 的 Linux 1 GiB/no-swap 实测 exit 0：100 MiB 的 first build replay 13 layers；第二次利用祖先复制 5000 rows，只 replay 2 layers。构建工具使用 SQLite 3.53.4，独立 child peak RSS 两次均为 46,133,248 bytes；整个容器 peak 875,171,840 bytes，无 OOM。journal 已实际写入后 SIGKILL 专用子进程，重开 quick_check、完整读取与主库 SHA256 均恢复，不留部分 checkpoint；原始输入从未改写。 |
| 短时与历史压力 | 前轮 `stress-{1,4,8}.json` 保留。current reviewed artifact 的 1/4/8 readers+1 writer 三档各 1803–1804 秒，均 exit 0：47,034 / 172,491 / 259,812 次读，483 / 331 / 234 次成功写及同数预期 TYPE_ERROR 回滚，unexpected failures=0。peak RSS 296,173,568 / 305,217,536 / 303,153,152 bytes。writer hold P95 4.888 / 6.652 / 10.033 秒，原始报告保留全部延迟、early-close、interrupt、WAL 和 reopen/quick_check；各配置与构建并行，不将该 wall-time 宣称无干扰 SLA。 |

### 19.3 尚未闭合的 Gate 与明确 Finding

- 新增验证已通过：Checkpoint 14 个单测、Commit 4 个单测、Snapshot 单次构建 instrumentation 测试；当前真实 SQL access 61 checks。最终 `cargo make quality` exit 0（2080.91 秒），region/function/line coverage 85.15% / 86.88% / 88.39%，每文件门槛均通过。首次 fresh quality 的 `decode_statistics` CCN 17 已拆分 totals 校验，保持原阈值。当前源码的 `scripts/ci.sh` 也已 exit 0，含适用 TCK、真实 SQL/Provider 双扩展、SQLite 3.45.0/3.53.4 与文档可执行示例；不以先前一次 green 替代。
- 三轮成对的 1000 Node batch+Commit、paged History、Diff、SQL explicit transaction 和 10K-conflict Merge 已对同一 100k 合成 fixture 的独立副本执行完毕，`nonregression-s22.exit=0`。Before/After 中位数（ms）依次为 41.166/41.644、197.745/202.237、212.189/95.753、108.018/97.191；Merge start/scan/resolution/inspection/finalize/GC 各项及 finalize preparation/wait/hold 全部满足原非回退阈值。每个 Merge 真实处理 10,000 conflicts、40 页与 40 轮 resolution；writer hold 中位数 5.270/5.117 ms，未以 finalize total 冒充 hold。完整样本及第一轮的并行负载慢样本全部保留于 `nonregression-s22/report.json`。本证据针对上述 frozen artifact，后续 Review 修复另计。
- 10M Node/100M Relationship 独立 release-tier 正在构建并执行原有规模流程。构建完成、Checkpoint、integrity 与后续 workload 都须实际成功后才能计入证据，不能用 100k 读压力替代。
- **未解决性能 Finding**：修改 `Common.hit` 会重复验证只涉及 `Common.v` 的 Property Type Constraint，导致与本次改动无关的 100k owner 扫描。Snapshot 复用已消除额外 resolve，但不能解决该重复扫描；须按现有 Schema/Constraint 合同最小化修复并重新测 writer hold，不能将其解释为已经优化完成。
- S22.3 的统计、谓词扩展与无索引扫描，S22.4–S22.8 的所有 Join/Spill/SIP/TopK/Factorized/ASP/WCOJ 及 A22-G01–G06 仍未全部实现和验收。当前任何局部 green 均不构成 Phase 22 完成。

## 20. 独立 Review 的阻断与修复（2026-11-11，验证中）

第 19 节 frozen artifact 的 green 和压力数据保留为原始证据。独立代码审核随后发现以下问题，S22.1/S22.2 Gate 继续保持未通过；不能因为已有 quality/CI green 就忽略可复现缺口。

1. **S22.1 参数遍历预算**：`folding_fits` 在发现超限前完整遍历 List/Map 参数，内部 states/bytes 上限没有覆盖这部分可选优化工作。当前最小修复在容量/entry 数量层先拒绝，并将逐键、逐值与嵌套深度计入同一 PlanningBudget；不改变完整的 executor 参数计账。专项测试已通过，仍须合并当前其他修复后的完整门禁。
2. **S22.2 冷 Branch 的 Checkpoint 失效**：四个 automatic checkpoint 并存时，冷 Branch 从最旧 base 继续写，自动发布第五个 checkpoint 会清理该 execution 的 lazy RETURN 仍依赖的 base。旧 artifact 的纯内存真实 SQL 已两次复现 `Node n:1 is no longer present in the active graph state`，不是只检查 EXPLAIN 的推断。补充复现还证明，同一 query 多次 Version Procedure 会跨越多个新 checkpoint，仅保护最新 builder base 不足以保护初始 Snapshot。因此修复同时使用现有 ReadLease/QueryResources 持有有预算的 query-local Snapshot 引用，清理时跳过仍活跃的 base；无候选时延期而不突破四个上限。不增加全局/持久 registry 或公共配置。eager program、Merge 与 transaction batch 的实际 Snapshot 生命周期已单独核对。
3. **S22.2 维护估算工作**：原 estimate 中的全量 `count/sum/length` 会在决定延期之前扫描过大 Layer/Property 内容。正在按现有 64 MiB maintenance 预算限制实际检查行数与剩余字节；大 TEXT 长度使用已在三套实际 SQLite 验证的 header-only 路径。必须以 SQLite VM 与大值 fixture 证明有界，不以返回值改名作为修复。

4. **同-query Reset + GC 的 Snapshot 失效**：纯内存旧 artifact 在无 checkpoint 时可执行、存在 automatic checkpoint 时同样查询失败；失败后外层 savepoint 正确恢复 Commit/CP 与数据。最小修复复用上述 Snapshot lease，将活跃 Commit 作为本 execution 的临时 GC root；查询结束后独立 GC 仍须回收，不能偷偷创建持久 Tag 或保留所有历史。拥有该边界的 Storage/Versioning 设计已澄清，回归与释放验证正在进行。

10M/100M 独立流程当前已完成 canonical seed、checkpoint 与完整 integrity gate，正在构建索引及后续 workload；尚未报告全部完成。上述 Review 修复、最终同源码 quality/CI、定向资源/回滚和最终 diff 全部完成前不推进 S22.3 的其余实现。没有提交、推送或发布。

### 20.1 集成回归与后续实证

- 首轮统一门禁在 `storage/mod.rs` 的两处 import 排序停止，已统一格式；下一轮查出 file-backed bare Core fixture 未填 metadata row 而被 pin identity 误拒绝。已按真实 Core 初始化边界修复身份作用域，原 Phase05 71/71 通过，未调整旧测试。
- 再次全门禁在原 Phase18 Index Access 的三项物理计划检查失败：借用参数 map 被重复计入执行期 B-tree 分配开销，导致正常公开 Index 失选。正在修正借用检查与实际 clone 工作空间的区别，保留 256 KiB/4096 states/64 depth 原阈值，不把 Label Scan 改成测试期望来掩盖回退。
- 真实事务子查询计数确认 outer prefix 重复 resolve：12 rows、batch 1/4/12 的总构建数为 28/19/17，24 rows、batch 4 为 34；prepare 均为一次。复用已准备 immutable outer state 后分别降到 14/5/3/8，inner batch 仍独立 pin；增量可见性回归与最终门禁尚在收口。
- `s22-pins-final.dylib` 的 1/4/8 readers+writer 各 1802–1803 秒均 exit 0：74,898/204,849/241,815 次读，600/405/326 次成功写及同数预期失败回滚，unexpected failures=0，RSS 301,137,920/312,688,640/304,070,656 bytes。该冻结制品早于 bare Core 与最新预算/prefix 修复，不能冒称最后源码的整体通过。
- 独立 release-tier 流程 exit 0：10M Node、100M Relationship，SQLite 3.51.0，database bytes=29,255,856,672，canonical seed/checkpoint/integrity 分别 739.220/1635.054/1124.928 秒，integrity 零问题；索引构建耗时另列于 `target/phase22-release-scale/baseline.json`，未藏入查询延迟。Core 首轮 10M Label 流 3.900 秒、1M hub 流 19.166 秒，indexed equality/range 3/35 ms、1000 Node batch+Commit 42 ms、History/Diff/Search 等完成。
- 同机真实 `main@3f1f31e` Before 制品已补齐三轮长流与 SQL 5×20 warm/100 reopen：Core Label 5.866/6.587/4.513 秒，hub 19.637/17.910/18.394 秒；SQL Label 6.392/6.547/7.016 秒、hub 7.576/7.831/8.259 秒，全部完整消费。Core/SQL 长流进程 peak RSS 30,326,784/37,994,496 bytes，SQL 短查询 adapter 为 37,732,352 bytes；SQL warm adjacency/equality P95 2.783/2.909 ms，reopen 2.906/14.684 ms，零失败。原始数据为 `target/phase22-current/release-before/`，后续仍须新最终制品在同条件对照。

## 21. S22.1/S22.2 最终冻结与局部验收（2026-11-11）

第 20 节的具体修复与集成问题已完成定向收口：原 Phase18 Index Access 6/6、Access unit 7/7、Phase22 access 19/19、原 Phase05 71/71、Snapshot/GC/pin 生命周期专项与原阈值 Clippy/Lizard 通过。借用参数表和实际会 clone 的 Map 值分别计费；500 个单元素 Map 的边界回归会在完整遍历/clone 前停止。事务 outer prefix 只共享原有 immutable resolved state/lease，inner batch 仍按最新 head 独立 resolve；没有新增前缀 materialization、SQLite connection 或持久 cache。

本次 frozen 源码清单 `target/phase22-current/stage12-final-source.json` SHA256 为 `99c7c4cbd4e035a65687bbbe642a7722f0c06b27ab62131a242927227481ad24`；macOS Release `stage12-final.dylib` 为 `00f0f81958eccbe78cd9d87374a531ee4951fd83116a2cbe94b2ce0681b56f25`，Linux amd64 `stage12-final.so` 为 `8fb5467b60c7ebd32a17407e907289d253e1d2a627c5c0f1a0041f489e57f7d9`。

- 当前 Native 真实 SQL access 61 checks、C prepared/reset/rebind 721 checks、checkpoint 七组 named checks 全部 exit 0；后者覆盖冷 Branch、2600 次同-query Version Procedure 和 Reset+GC，均含 scalar/rows，且确认 query 结束后下一次 GC 可回收临时保护的历史。
- 当前 1k/10k/100k Native 完整流、5×20 warm、100 reopen 与取消均通过；实际 VM_STEP 三规模均为 **2508**，FULLSCAN_STEP 均 94、SORT 均 2。相对前轮增加的常数工作来自 Snapshot pin 的数据库身份查询，未随无关图规模线性放大。
- 最终同源码 `cargo make quality` 和 `scripts/ci.sh` 均 exit 0；日志为 `quality-stage12-final` / `ci-stage12-final`。Quality 用时 1324.76 秒，region/function/line coverage 为 85.22% / 86.89% / 88.43%，原有全局与每文件门槛不变；CI 包括适用 TCK、真实 SQL/Provider 双扩展、SQLite 3.45.0/3.53.4 与文档可执行示例。完整源码清单重新核对无漂移。
- S22.3 现有代码已覆盖 Relationship ID 的 parallel/undirected self-loop 以及 explicit staged read/delete/abort；第 18 节的早期漏测说明不代表最新测试库存。更多统计/谓词/无索引优化和全部 S22.3 Gate 仍未交付；S22.4–S22.8 尚未实施。

当前没有新的已确认 S22.1/S22.2 correctness finding。以下记录当前 frozen artifact 的补充证据与局部 Gate；未完成的后续算法、统一压力和最终 Review 继续阻断 Phase `done`。所有修改仍未提交、未推送、未发布。

### 21.1 最终资源、历史与非回退证据

| 项目 | 最终制品的实际结果 |
| --- | --- |
| Linux 1 GiB/no-swap | `linux-stage12-access`、`linux-stage12-checkpoint` 均 exit 0，实际 memory.max=1073741824、memory.swap.max=0、OOM=0；同一 amd64 artifact 完成 61 SQL checks、三规模 warm/reopen/full-drain/cancel 与 checkpoint 七组。100k 查询 cgroup peak=369106944 bytes、RSS=306872320 bytes；checkpoint peak=240766976 bytes。QEMU wall-time 不作为原生 SLA。 |
| 当前历史四档 | `stage12-history.exit=0`；同终态 10/100/1k/10k Commit 在 automatic/missing/manual 三状态分别完成 100 warm、100 reopen 和校验。每档第二次构建均只 replay 2 个新 Layer 并利用 1 个兼容祖先；10k 缺失/manual 查询 VM 为 1226149/2167，额外 33 VM 为 pin 身份常数开销。 |
| 当前版本操作三轮 | `nonregression-stage12/report.json` 全部满足 max(1.20×Before, Before+10ms)。1000 Node batch+Commit、History、Diff、SQL explicit transaction 的 Before/After 中位数(ms)为 45.401/49.113、228.174/198.907、105.543/91.557、125.770/95.869。10K-conflict Merge start/scan/resolution/inspection/finalize/GC 全通过；每轮真实 10000 conflicts、40 页和 40 次 resolution。finalize prepare/wait/hold 中位数为 267.780/256.654、0.039/0.048、5.448/4.574 ms；未删并行慢样本。 |
| 当前 Native 维护 | 同 artifact 的 768 次 writer hold P50/P95 为 6.413/13.929 ms，checkpoint 事件 hold=69.464 ms、WAL peak=8380112 bytes；只修改无关 Property 引发约束 owner 重扫的具体性能问题按 S22.3.4 独立修复，不把它混作 checkpoint 开销。 |
| 10M/100M 安静配对 | `release-paired/before-core.json` 与 `after-core.json` 各三次完整输出、相同固定数据库/SQLite/缓存类别。10M Label 中位数 4.940/4.628 秒，1M hub 中位数 18.808/18.008 秒；各轮 rows/checksum 相同且 failures=0。较早与构建/仿真/Merge 并行的慢样本仍保留在 `release-after-stage12/`，不冒充本次安静环境结果。 |

- 安静环境的同一 C SQL worker 顺序加载 Before/After 制品，各三次完整输出：10M Label 中位数为 6.936/6.840 秒，1M hub 为 7.636/7.655 秒；两类各 0 failure。原始报告为 `release-paired/{before,after}-sql-long.json`。Core 和 SQL 分别计量，未把连接热态描述为 OS cold。

### 21.2 局部 Acceptance 闭合

| Acceptance | 结论与直接证据 |
| --- | --- |
| A22-S1-01/02 | PASS：Core access 19 项、SQL 61 checks 与 prepared/reset/rebind 721 checks；包含 read/write/program、历史/Schema/View/参数重新绑定，实际候选选择与 PROFILE、VM 对应。 |
| A22-S1-03/04 | PASS：内部 states/bytes/depth、巨型 List/Map 的早停与取消永久测试；普通 streaming、TEMP、资源错误及写入回滚经当前全量 CI，未新增结果集物化。 |
| A22-S1-05 | PASS：当前 Linux amd64 1 GiB/no-swap 三规模 warm/reopen/full drain/interrupt、SQL 与恢复；无 OOM、只读主库 SHA 不变。 |
| A22-S1-06 | PASS：1k/10k/100k 固定结果均 2508 VM，Before 为 99442/5011663/454386601；三轮版本操作非回退、独立 release-tier、安静 Core/SQL 对照、当前 quality/CI、Review/diff 全通过。 |
| A22-S2-01/03 | PASS：四种历史深度、1/10/100 MiB Delta 的各冻结证据与当前历史完整复验；第二次构建真实利用兼容祖先只 replay 2 个新 Layer，读/构建/复制/VM/空间分别记录。 |
| A22-S2-02/05 | PASS：18 个 checkpoint 单测、Snapshot pin/GC 释放、真实 SQL 七组、两连接 WAL/rollback-journal、取消与当前 Linux 资源复验；100 MiB 维护的独立进程中断/重启/原子恢复证据保留。 |
| A22-S2-04 | PASS：当前 768 次 writer hold、磁盘/WAL 和三轮 batch/History/Diff/10K-conflict Merge 非回退均单独计量，自动维护有界延期且不清理活跃 base。 |
| A22-S2-06 | PASS：当前 scalar/rows、Storage/Version、原 Phase05/18、事务子查询逐批新 Head、concurrent batch、staged clause、完整 quality/CI/TCK 与最终 Review/diff 通过。 |

S22.1/S22.2 的局部门禁通过，开始 S22.3。历史长压力记录按各自 artifact 保留；全 Phase 的最终源码还须在所有算法集成后执行统一 30min×1/4/8 readers+writer 与资源/规模门禁，当前不提前宣称 A22-G01–G06 完成。

## 22. S22.3 实施、Review Finding 与阶段验收（2026-11-11）

第 21 节的 S22.1/S22.2 green 是对应冻结源码的真实历史证据。后续统计审核又发现派生 metadata 的 SQLite 类型边界；该依赖问题必须修复并纳入 S22.3 当前源码的完整复验，不把前一轮 green 解释成没有遗漏。S22.4–S22.8 仍未实施。

- 版本化统计已接入 bounded ready-generation 频次、NDistinct/MCV、Label 组合与 typed degree 采样及真实 Access 消费；仍在 targeted、预算和兼容审核，不因代码存在判 PASS。没有新增持久统计表、public tuning 或全图 ANALYZE。
- 无索引 READ 配对以同一只读 synthetic checkpoint fixture 完整消费：1k/10k/100k 的 SQL 调用分别 3039→2038、30075→20074、300426→200425；VM 分别 58168→52162、562672→502666、5607586→5007580。固定 dictionary/property 语句的实际 prepare 次数从 N+1→1，输出均 `[[0]]`、hash 不变、无 busy statement。原始数据在 `s3-noindex-baseline/`；After 为早期 S3 artifact `bc436b05…`，最终源码另行冻结，不冒称相同。
- 开启现有 rusqlite 的 `cache` feature，用于本 Stage 明确要求的固定 prepared statement 复用；lock 因该 feature 新增 hashlink/hashbrown/foldhash 三个传递依赖。没有自建 cache 层；默认 16 entries，SQL callback 的临时 Connection wrapper 释放时清理自身缓存。
- 无关 Property Constraint 重扫已有真实 SQL 对照：1k/10k `Common.hit` 写入 VM 从 50466/477291 降到 3049/3049；受约束 `Common.v` 的写入仍执行完整验证，VM 50517/477183→50527/477193。结构/Label/Type 变化保留原保守验证，新的 key 集合有资源与取消检查。
- Relationship ID 新 SQL same-handle rebind、explicit staged 观察与失败回滚已在前一冻结制品通过；1k/10k/100k/1M Relationship、各自明确 CP Snapshot 的 valid/missing VM 为固定 2120/2041（warm=fresh），完整输出且原 fixture hash 不变。Before 无 RelIdSeek 的 1k/10k/100k 为 107079/1052931/10511367 VM；1M 的 1M Node/1M Relationship 与前三档 2N Node/N Relationship 分别列明，未混用 Node-only fixture。最终 S3 artifact 仍需复验。

### 22.1 必须闭环的新增类型边界

1. Ready Index 的 `indexed_entities` / `entry_count` 在 SQLite 非 STRICT 表中被置为 TEXT 后，新 cost advice 会把 EXPLAIN、MATCH 和 PROFILE 变成 `LITHOGRAPH_STORAGE_ERROR`。统计应回退 unknown，不成为合法查询的新 correctness gate。修复须只容错派生统计字段，保留 canonical/SQLite I/O/取消错误；独立 SQL fixture 覆盖 TEXT/BLOB/REAL/负数、缺失 generation 与完整输出。
2. 既有 `load_checkpoint_statistics` 对 metadata 按 `Option<Vec<u8>>` 直接读取；TEXT 类型坏值同样使查询错误，巨型 TEXT 长度探针还可能在拒绝前扫描内容。修复须先验证 BLOB 类型再有界读取，坏统计回退，同时保持 checkpoint 图数据、Commit 和历史不变。该点属于此前 S22.2 冻结验收后新发现的遗漏，不能用先前 PASS 忽略。

当前尚未取得 S22.3 Gate，也未提交、推送或发布。

### 22.2 Review 闭环与仍在验证的执行范围

- 派生 Index counts 的 TEXT/BLOB/REAL/负值与缺失 generation 已在候选 `s3-candidate-a.dylib` 完成 EXPLAIN/MATCH/PROFILE 双 adapter 回退验证。该候选 SHA256 为 `5ccf9969d18b860d2b8b55e651c71a90656da968cbf18c582c1092dfde6c038f`；不等同于最终 S3 源码冻结。后续自查还为 sampled derived owner ID 增加类型保护，须随最终候选统一复验。
- Checkpoint metadata 同族读取点已统一先检查 BLOB 类型，再有界读取。当前 unit metadata 5/5、checkpoint 21/21 与公开 SQL 18 cases 全通过；短 TEXT、合法 JSON 的 TEXT、16 MiB TEXT 都精确返回32行，未写 main/Branch，实际维护采样仍成功。永久公开回归已加入 `tests/phase22_checkpoint_sql.py` 的第八组，完整 S3 gate 会重新运行。
- 新增可选 proof 工作量必须接入同一 PlanningBudget：约束×IN 成员、常量预检、超长 Property name 在 clone 前计费；取消与真实 ResourceError 继续传播，无 Index 时不再为了不存在的候选 fold 参数。不得用第二个 fresh budget 绕过全 query 上限。
- 独立审核确认未知外层 Relationship/Path/Node 绑定可能被 WHERE 提前过滤或 Property Index 剪掉，吞掉原 binding error。已采用最小 scope gate：没有 incoming entity type proof 时，对复用外层 Pattern 变量的 step 保守禁止 WHERE-based narrowing/early predicate，正常 Label/Type Pattern 绑定顺序保持；planning 和 runtime 的 incoming columns 必须一致。该矩阵仍在当前源码集成验证。
- 部分 eager/streamed 写程序的后续 MATCH 原来没有真实 Relationship ID Seek，另有超预算 suffix 实际仍 seek、展示却为 Scan 的不一致。当前统一 Node/Relationship fresh-identity 许可，在同一 global planning budget 内选择，并于每个 clause 当前 staged Snapshot 重新 lower；不复用旧 dictionary impossible/LabelId 或 Schema proof。160 MATCH eager 与80+80 MATCH streamed 预算回退分别验证，新增 scope gate 后须再统一复验。

以上是实施与修复记录，不是 S22.3 或全 Phase 完成声明。全部八个 Stage 与统一大规模/长压力条件保持原门槛。

### 22.3 首次完整门禁与维护性修正

首次 S3 Rust 源码冻结为389文件，SHA256 `2b2ba52b1aa438f015b4332509a06bf3468037258a3debbe282e60ebe1514193`，对应正确独立构建的 Release 扩展为 `dc17462aadac0505f22174feb70b36b15b55c70b4f27bce55a5ddec21e8a692c`。pre-quality source/artifact 及原始日志已分别保留，不覆盖旧结果。

- 该候选真实 SQL access 61 checks、C prepared 721 checks、checkpoint 八组、statistics 九组和增强 Relationship SQL 均通过；Relationship 四档固定结果的 warm/fresh VM 为命中2125、缺失2046，RSS peak43532288 bytes，所有 source hash不变。
- `cargo make quality` 首次 exit105，原 production Lizard 指出六项 CCN/NLOC/PARAM 超限：Index seek selection、identity match、read-clause entry、eager MATCH、program planning、write preparation。算法/语义 green 不能替代维护性门禁；按原 CCN15/NLOC100/PARAM10 拆分，不降低阈值或增加豁免。旧冻结版 CI 与 Linux 编译已在确认具体 PID/容器归属后停止，分别留 `ci-stage3-attempt1.cancelled` 与 `stage3-linux-attempt1.cancelled`，不是已完成的验收。
- 初次误将 extension 与 standalone Core tools 合并 Cargo build，会统一 SQLite ABI feature；其产物在执行 benchmark前已隔离，正确命令分开构建后重新记录上述哈希。未把组合产物用于验收。
- cache feature新增的三项传递依赖已核对实际许可。`cargo deny` 首次因 foldhash0.2.0 的 Zlib未列入许可策略而失败；现仅对这个确切crate/version加 scoped许可，原全局策略不变。三组件原文保留于 `THIRD_PARTY_NOTICES.md`，zip与tar.gz本地打包验证逐字保留，未发布。`s3-deny-scoped.exit=0`，advisories/bans/licenses/sources均通过。

函数拆分后必须重新冻结并完成统一 quality/CI、当前 artifact的资源/规模及最终diff，不把这次中止门禁补记PASS。新点查/完整流harness只有1k smoke已通过，另三档、100fresh与Linux 1GiB大档尚待运行。

### 22.4 重新冻结后的完整验证与新增阻断项

维护性拆分后的389文件源码 SHA256 为 `eb6f42048c8fefd7df014978e8d1b38c04a2707fd286e2d024f83cf8ec8af1f5`；独立构建的 Native Release 扩展为 `85240eb1666c76b6a3bb8f55894e964d21976d87e4e94fb7ce855f09bcfdd6ae`，Linux amd64 dev/no-debug 扩展为 `2d0b55ca8d28715ec016d98bb74cb3256784486d149dae1d50d7a33a994c9689`。以下均对应该冻结版，不代表随后修复后的源码已通过。

- `quality-stage3-final.exit=0`，完整 quality 用时1592.57秒，regions/functions/lines coverage为85.44%/87.03%/88.64%，沿用原阈值；`ci-stage3-final.exit=0`，完整Rust/TCK/SQL、Provider双扩展和SQLite版本回归通过。Native与Linux真实SQL均通过access61、statistics9组、checkpoint8组及Relationship rebind/write206检查。源码389文件在构建、quality/CI及复验前后逐项hash一致。
- Native NodeId/Ready Index与Relationship四档均达到每case 5个warm连接各20次、另100个独立fresh首条查询；warm连接的首次样本单列。NodeId rows VM为1274/1274/1274/1509，Ready Index warm/fresh为1679/1900（前三档）、1906/2127（1M）；Relationship valid/missing为固定2125/2046。完整ID/值Oracle、VM增长门槛和fixture hash均通过。
- Linux amd64通过QEMU运行于Apple M2宿主，1GiB/no-swap、2CPU、network none、rootfs/repo只读。NodeId rows VM为1268/1268/1268/1512，Ready Index warm/fresh为1673/1894（前三档）、1909/2130（1M）；Relationship valid/missing为2150/2071（前三档）、2152/2073（1M）。Python/C driver均为实际SQLite3.46.1；首次误用3.53.4 driver的版本不匹配失败单独保留，未放宽校验。点查与关系组process peak RSS分别不超过286326784与329940992 bytes；cgroup peak包含page cache，关系组达到1GiB并发生57次max事件，但oom/oom_kill均为0，不将cgroup peak冒称进程RSS。
- 无Index READ最终Native三档各三轮均完整返回`[[0]]`，dictionary/property实际prepare各1次，cached idle为0。1k/10k/100k VM为52167/502671/5007585，较同runtime Before降低10.32%/10.66%/10.70%；CPU中位为6.035/47.077/447.150ms。Linux100k三轮为200425 SQL/5008392 VM、CPU23.270193/23.219774/23.883554秒，cgroup peak137515008 bytes、OOM全0。跨runtime只分别记录，不计算混合改善。
- Native四档各三次完整Label流、首行后真实interrupt、64B ResourceError、early-close与同连接恢复均通过。1M每轮核对全部ID/值及唯一性，分别21.616/21.129/22.498秒，bitset125000 bytes、process peak46137344 bytes。Linux1M随后实际完成三轮，exit0，每轮1M unique/逐值Oracle正确；耗时811.456/728.674/683.367秒（QEMU，不与Native SLA混比），process peak130580480 bytes、cgroup peak744747008 bytes，events全0。首行后interrupt108390.667us、64B ResourceError、early-close和同连接恢复通过，main hash不变、WAL/journal为0，SHM32768 bytes。此项同属修复前冻结版本证据。
- 三轮版本操作非回退`nonregression-stage3/report.json`全部13项通过，保留原始慢轮次。Before/After中位数为batch43.678/44.747ms、History205.702/218.484ms、Diff99.675/111.006ms、explicit107.100/114.586ms；10K-conflict Merge的start135/145ms、scan6103/6460ms、resolution155273/150764ms、inspection262/280ms、finalize296/301ms、GC71/41ms。Finalize prepare265.421/267.964ms、writer wait60/78us、writer hold4.806/6.133ms单列，未以total finalize替代hold。

在上述green之后的独立review又确认两项P2，因此**S22.3 Gate继续保持未通过，S22.4–S22.8未实施**：

1. `mode=ro`加`PRAGMA query_only=ON`时，完整Ready Index仍先依赖TEMP绑定；typed AND选择Index后，scalar/rows均报只读IO错误，而禁止rewrite的canonical对照均正确返回。独立fresh连接的旧baseline `36932d90…`亦复现同错误，属于既有cache/合同缺口，不声称S3新引入；已建TEMP的连接会掩盖该问题。设计要求宿主禁止TEMP写时流式canonical fallback。修复采用明确宿主能力限制现有候选，使实际Scan及EXPLAIN/PROFILE一致，不混同普通只读database与禁TEMP，不捕获并吞掉其他I/O或取消错误。
2. 派生generation的numeric counts虽符合SQLite CHECK，仍可能与已观察数据矛盾。10k Common、512 Rare、全v=0的fixture中，仅将`indexed_entities/entry_count`改为0，会把正常Label路径1536 dbHits变成Index路径10512 dbHits；采样已观察128匹配，`owners.max(lower)`却把矛盾变成伪精确区间。修复须把观察下界、removed或unique sample超出manifest等矛盾返回unknown，不能用max掩盖。

两项修复、永久红转绿回归、独立复核及新冻结版本的必要门禁尚待完成。旧冻结长测继续保留原始证据，不回写为新版本验收。10M/100M补充Core流程中两个delta用例最初误用未修改owner的基础fixture，结果不符合预期，失败原样保留；现已在独立APFS副本通过公共写入建立1000 changed-owner状态，setup耗时91.206秒，未改原数据库，后续读取另计。未提交、推送或发布。

### 22.5 末轮 Finding 的定向修复与独立复核

- Numeric counts问题已经定向闭环。最初4个numeric红测和追加的129-owner union红测均真实失败；最终private12/12通过（52.15秒）、all-targets clippy通过（32.84秒），原Lizard/format阈值不变。修复实际读取第129个sentinel，以有序频次前缀与first/last/sample、removed的互不重叠观察构造checked下界；不把矛盾压成精确区间，不重复计数，不保留129个ID。150/151与151/152的正反边界、真实空generation、无效payload的row presence、10k完整512行/Label路径及无TEMP sort的EQP永久回归均通过。独立storage review确认本scope无剩余可复现P1/P2；生产/测试文件SHA分别为`94f1e092c8f26ee0a24068b1e8280e7729924265e8c35991bb6f3b0b4d65ae70` / `804e3bcdda3494a969bdb81da56360cba764223519783ea12d3ce292ae1b7b8e`。
- 禁TEMP同族后续确认更严重的P1变体：host authorizer对TEMP写返回合法`SQLITE_IGNORE`时，旧debug `8b19dd2b…`先warm Node/Relationship cache，再正常CREATE新Commit的v=2，优化查询均静默返回`[]`，canonical对照均`[[2]]`。不能只容错`SQLITE_AUTH`或依赖cache表已经存在。
- 该修复共用现有四条固定TEMP初始化DDL，只prepare、不step；`query_only`明确禁止、该可选DDL的`SQLITE_AUTH`或prepare后`readonly()==true`时拒绝TEMP依赖候选。正常`CREATE TABLE IF NOT EXISTS`即使表已存在仍按可写statement分类；无需catalog存在性放行。Node Lookup原本使用canonical Label路径，IDs-only/schema-empty/Node Lookup-only不做无消费者的能力探测。主数据库READ/PRAGMA拒绝、其他I/O及取消仍传播，不替换宿主authorizer。
- 新debug artifact `2c7ca4005947ac73efa94a7a184ec167dc7f457e23f7f12bae039ac41d487169`已通过48组readonly/generation/history矩阵及cold/warm-new-target DENY/IGNORE双adapter测试；独立after复验确认Node/Relationship均正确返回新v=2、Plan/Profile如实回退，正常warm仍IndexSeek，TEMP完整catalog不变。初次catalog count=0断言失败是测试假设错误：fresh load已经有3张连接状态表，已改为前后完整目录比较，未将该失败改写成生产DDL执行。

以上只关闭各自的定向finding；新统一Rust源码/Release/Linux冻结、完整quality/CI、最低SQLite3.45及受影响资源/规模复验仍待执行。S22.3 Gate仍open，尚未进入S22.4，不据debug局部green标记Phase完成。

### 22.6 统一冻结复验与性能 Finding（S3 Gate仍open）

两项定向修复统一进入390文件源码，SHA256 `6e0cf300e05b5ae9d7fdc20d958124fdef8bdfdb0302dfb3b75ac4248cf2a051`。独立Release扩展`stage3-reviewed-final.dylib`为`1de15a93b17447059e57e7eade5c6fa2ebc79aa9a477b225528d9642c1299dea`，Linux amd64扩展`stage3-reviewed-final.so`为`ab5db909e67dca8a255e99598646fc0c9c402b373feee008aa66ffdd79176bb4`。另冻结91个验证输入，SHA256 `8f96107eec06b894e5a396e166930591f409a8e8cb6026d61d239daaf2559992`；两套清单在门禁前后均无文件变化，旧制品与失败日志未覆盖。

- `quality-stage3-reviewed-final.exit=0`，1672.60秒，regions/functions/lines为85.48%/87.05%/88.69%；`ci-stage3-reviewed-final.exit=0`，1971.31秒。Rust、原维护性/coverage阈值、TCK、SQL、Provider双扩展、SQLite3.45/3.53.4与文档例子通过。新增最低3.45 C API回归在当前Release上939 checks通过，旧85240在同一binary真实漏行red仍保留。
- Native与Linux均完成61 access、9组statistics、48项readonly加DENY/IGNORE矩阵、206项Relationship rebind/write及checkpoint8组。Native首批driver漏指定匹配C库，在运行Relationship case前因系统库缺load-extension symbol退出，原exit1保留；已用明确Brew3.51库完成剩余Relationship/checkpoint，未重复已通过的前三组，不把该调用失败称生产回归。
- Native四档NodeId rows VM仍为1274/1274/1274/1509；Ready Index warm/fresh为1684/1905（前三档）、1911/2132（1M），较旧候选只增加固定5 VM。Linux相应为Node1268/1268/1268/1512，Index1678/1899与1914/2135。四档每case5×20 warm及100fresh、完整ID/值/hash与2×+1000门槛全部通过；Relationship完整采样亦通过。Linux point process peak286740480 bytes，cgroup peak1067380736 bytes；Relationship cgroup peak1030369280 bytes，均OOM/max事件0。
- 当前Native四档各三次完整流均通过，1M完整ID/值/唯一性与取消/配额/early-close/恢复正确，process peak47284224 bytes；其25.349/26.457/53.407秒原始慢轮次照实保留，包含Python逐行Oracle和并行gates，不冒充固定hub SLA。当前Linux100k三轮161.707/124.127/75.298秒、process peak128933888 bytes，worker cgroup peak200273920 bytes、全parent peak259407872 bytes，均OOM0。Linux1M完整三轮保留第22.4节修复前冻结证据，未改称当前制品复跑；本轮按改动影响复验1M点查、当前100k流与Native1M流，最终全算法Linux压力仍须单独完成。
- 当前无Index Native9轮和Linux100k三轮全部通过，prepare各1/idle0、完整输出与恢复/close/hash均正确；VM仍为52167/502671/5007585，Linux100k5008392，Linux cgroup peak136736768 bytes、events全0。冷热384同fixture的当前三轮hot VM8303、dbHits48，旧baseline30317/383；cold VM5154、dbHits2，旧baseline2788/2。cold的固定采样准备代价继续披露，不描述成全面改善。
- 当前10M Node/100M Relationship原生Release规模：Core Label三轮5.500/5.503/5.442秒，hub三轮19.128/18.110/18.141秒；SQL Label中位8.366秒/max8.860秒，hub中位10.624秒/max10.779秒，全部完整输出且满足原固定长流目标。1000 changed-owner副本的Core旧值0行与range1000行均通过、full generation builds=0。Core短case只有3样本，不称P95；SQL的5×20 warm/100fresh则实际测得adjacency P95为3.962/4.421ms，Index equality为4.288/18.502ms，全部failures0。完整Phase短查询与全算法综合门禁仍按第13节执行。

本轮非回退复用上一轮真实Before三组，仅新测当前After三组，清楚记录为retained-baseline对照，不冒充新交替配对。`nonregression-stage3-reviewed.exit=1`：13项中12项通过，candidate inspection为Before305/262/244ms、After422/328/249ms，中位262→328ms超过314.4ms门槛。全部慢轮次保留，未按第三轮249ms单独判PASS；正隔离同一10K candidate、安静输入和真实prepare/runtime成本，以判断是否有代码回退。该Finding及最终diff/证据同步未闭环前，**S22.3 Gate保持open，不进入S22.4**。


### 22.7 S22.3 局部 Gate 闭环

第22.6节的原非回退失败原样保留，未改写exit1。对唯一超线的inspection单独准备同一10K candidate，在无其它仓库压力任务时，精确链接原NR两套release Core rlib、同一Apple SQLite3.51/sourceid做三轮交替配对；不是改用SQL envelope替代Core。candidate-first中位251.786→267.203ms，门槛302.143ms；同连接warm中位246.130→252.784ms，门槛295.356ms，均通过，422.543ms尾样本仍保留。独立CPU/wall补充及SQL模板观察未发现持续CPU放大、新宽扫描或执行写；增加14 SQL/221 VM约0.0104%，四条DDL准备仅约29us，不足以解释原66ms差值，不为该微小工作追加代码。诊断fixture用公开reset/start/conflicts/resolve准备，操作性revision2不同于原40轮revision41；不冒充重新执行了resolution benchmark。源和候选hash、状态、十行完整Oracle与资源释放均通过，`candidate-inspection-isolation/verification.exit=0`，原NR其它12项通过，性能Finding据同条件对照闭环。

最终验证仍绑定390源码/`6e0cf300…`、Native `1de15a93…`、Linux `ab5db909…`。91-entry验证清单在quality/CI前后相同；收尾发现其中包含3个此前诊断生成的Python bytecode，已删除。原清单保留，剔除生成物后的88个验证源文件逐项仍等于门禁前hash，clean manifest SHA256为`d6600258b7f673e8349f9985afd68f7c2bc4877b5d0789578d200d37a59dc91f`。没有删除测试、降低阈值或修改Rust，文档链接、最终diff和未跟踪文件卫生检查完成。

| 局部Acceptance | 当前真实证据与结论 |
| --- | --- |
| A22-S3-01 | PASS：uniform/skew/empty统计量化、当前384同fixture直接旧新物理对照、query/result权限分离、32 strata/128频次/64 delta有界费用和零持久统计写入；numeric/prefix-union错误统计回退及独立Review闭环。 |
| A22-S3-02 | PASS：Node/Relationship身份不同，方向/Type/self-loop/parallel、合法/非法/非String/NULL、historical/staged/create/delete/abort及MATCH→write；Core与真实scalar/rows、四档有效/缺失ID实际VM、同handle rebind均通过。 |
| A22-S3-03 | PASS：range/IN/组合的值/重复/NULL/错误/OPTIONAL/未知incoming kind与预算回退、隐藏属性、真实程序staged clause；原WHERE/error保留，适用TCK与完整quality/CI通过。 |
| A22-S3-04 | PASS：当前四档NodeId/Ready Index/Relationship固定输出完整Oracle、VM增长门槛；无Index三档Native及Linux100k真实VM/prepare/CPU对照，不增加平方中间行，读取hash不变。 |
| A22-S3-05 | PASS：当前1GiB/no-swap Linux SQL/point/R/100k完整流、Native1M与10M/100M规模；TEMP禁写DENY/IGNORE含P1漏行修复、最低3.45、cancel/64B入口RESOURCE/early-close/rollback/reopen，历史Schema/Index/Provider/Search和完整quality/CI；12项非回退及inspection隔离复核、独立Review与final diff闭环。 |

**S22.3取得本地局部Gate，进入S22.4。** 这不是整个Phase22完成：S22.4–S22.8所有算法及第13节全算法综合/最终30min×1/4/8 readers+writer压力仍必做。所有修改仍未提交、未推送、未发布。

## 23. S22.4 实施开始（未通过 Gate）

已基于当前设计与源代码完成只读接口审定：A负责typed/scope/provider proof，B负责真实CSG/CMP连通枚举、受限左深cost和预完成Greedy，C负责同一StepCursor/PartCursor的实际order、lazy bound Index/Adjacency、原列序与精确Profile slot。共同状态只保留当前消费者需要的单个JoinPlan，原Pattern Part顺序保留，不增加第二executor、公开hint、预算或全局cache。实现前先在拥有合同的Query Engine正文明确实际搜索空间；接口提案本身不计完成或验收。

### 23.1 首片实现与独立 Review（尚未冻结）

当前实现已进入真实规划和执行路径：整段 WHERE 的 typed/total/scope proof、neighborhood/exclusion/canonical CSG/CMP 枚举、仅为可执行左深计划计价的 memo、同一预算内预完成的 Greedy，以及原 StepCursor/PartCursor 中的次序、方向和参数化 probe。Index binding 由 clause owner 持有，首次真实 outer 后 lazy bind；每个 probe 只保留自己的 key/page，重复 key 不缓存或去重结果。原 Part ordinal、执行 depth、公开列顺序和 PROFILE site 分别处理，完整 WHERE residual 保留。生产代码首轮统一 check 已通过，仍在集成定向测试，不能据此取得局部 Gate。

- analysis 的9项 unit、15项语义集成、bound Index 的10项 unit、新 distribution 的9项 unit及原 statistics 的12项回归已通过。对应日志为 `target/phase22-current/s4-{analysis-unit,join-semantics,bound-unit,distribution-unit,statistics-regression}.log`；尚不是最终同源码全量 quality/CI 的证据。
- 独立 analysis Review 未发现新的可确认 correctness finding；已修复一项实施期拓扑问题：一组可替代的 provider 应产生充分 provider 对之间的连接，不能把 any-provider group 当作必须全部完成的 Hyperedge 端点。永久 unit 和多 Part semantic fixture 已加入，完整集成结果仍待收口。
- 独立 cost Review 发现实际 P2：单个 `a.x=b.y` 在两侧都有 Range Index 时，双向模板按不同 source property 被重复当作两条等式，导致 NDV 被除两次；传递冗余的等值环也有同类问题。正在以 B 私有的两端属性身份及 active equality forest 修复，并验证不同属性名、独立 AND 等式、provider group 与左右交换；在真实定向回归通过前保持 open。
- 同轮审核还确认 unknown cost 与强制 connected 优先可把原序中间的已知空 Part 推后，在 no-CP 大 overlay 的 hub fixture 中先产生平方两跳组合再返回空；旧谓词集另有一项真实 Cartesian range/IN 性能门禁失败。已保留测试/阈值并补真实 unknown fixture，以有界取消/VM 检查防止坏实现把回归本身变成长时间平方运行。修复与实测尚待闭环。
- cursor/Profile 审核确认已绑定目标 Node 仍可能获选 IndexEquality，但实际 bound-node 优先而跳过 Index scan，造成成本/展示与实际访问不一致；正在用私有 target-provider 信息及真实 bound-start cost 修复。独立 bound 审核还指出小 slots Vec 的实际 capacity 与 lease 不一致，按现有 reserve/exact-allocation 合同最小修正，不新增缓存架构。
- 独立拓扑模型与 subset/split-DP Oracle 的小图对照未发现漏项、额外项或重复；这是诊断模型证据，不替代 Rust unit、真实计划或工作量验收。20+ Part 的完成 Greedy、共享预算与确定性仍须用实际执行证明。

### 23.2 S4 数据与测量准备

新增 `lithograph-phase22-join-fixture` 使用现有 storage/version API 创建 synthetic directed cycle；1k/10k/100k/1M 各为 N Node、N Relationship、N+1 Label、零 Property，Root 后一个 graph Commit，head 上一个 manual checkpoint。四档逐节点/逐关系 topology、canonical integrity、quick_check 与 user_version 检查均通过；源文件、head/CP、完整3–8 Part单行 Oracle、造数/CP时间、bytes和native RSS保存在 `target/phase22-current/s4-fixtures/`。该 builder 没有改动原大规模 fixture，也没有把造数 RSS 当作查询内存或 Linux 门禁结果。

1k 的完整 scalar/rows Oracle 已在接受的 S3 制品上验证，旧 dbHits 随3–8 Part为21000/28000/35000/42000/49000/56000。新的 SQL/Core harness 将分别测 prepare、first-row、full-drain、真实 VM 与 counters，并按 warm-first、5×20 warm、100 fresh 分开；目前只允许1k smoke，未宣称 S4 After 或大档压力完成。大档旧宽扫描按小档实测后排期，不为对照故意制造不可控 Cartesian 工作。

INL 的重复 key fixture 还需确认实际启用计划：新 distribution 对 changed owner 返回 unknown，旧 generation 上追加数据可能正确回退。可以在 private fixture setup 显式 rebuild 到当前 head，单独记录构建开销，再对同一 ready fixture 做旧新对照；不能为了获得 benchmark 名称而扩大 proof 或把回退的24行结果冒称 IndexNestedLoop。历史/staged/无 Index 的正确 fallback 另行验证。S4 的6项 Acceptance、统一资源/非回退、quality/CI/TCK及最终 Review/diff 全部通过前，不进入 S22.5。

### 23.3 首轮成本修复的真实闭环与新增依赖问题

21项 join unit（包含前述9项 analysis）及 order4、unknown2、旧 predicates16 的集成回归均已通过，原日志分别为 `s4-b-join-unit.log` 和 `s4-b-order-regression.log`。独立 reviewer 重读源码、测试与完整日志，关闭双向等式/等值环重复选择率和 unknown+connected 平方回退两项 P2；没有把模型测试替代真实执行。

- 3–8 Part 的实际 Rare 起点和24 Part的完成 Greedy均有超过4倍dbHits收益；24 Part真实记录 `join_greedy_fallbacks>0`，仍在4096 states、1024 candidates、256KiB原上限内执行完整结果。
- 2201 Node/2200 Relationship/no-CP 的 unknown 空查询从 Missing Part开始，dbHits为0，原序Oracle为13202；两者VM均28946，固定resolve开销未被隐藏。非空mixed对照的全部六个Node/Relationship ID相等，均28987 VM/5509 dbHits。测试实际安装progress和Engine检查上限，不允许错误平方计划无限执行。
- 原range/IN性能失败已通过原门槛：`n.v>252 AND n.g<=7` 优化/Oracle的VM为22213/128358、dbHits为562/4626，其余相关组合也满足原2倍门槛。私有诊断另保存3-Part和skew INL的prefix估计、实际行数/访问量，不将估计当正确性证明。

INL首轮15项中11项通过、4项失败，原 `s4-inl-tests.log` 保留。结果规则的一项预期涉及既有Core runtime Value与最终公开JSON编码边界，带WITH/聚合的写程序还受真实consumer准入限制；须依据拥有合同修正测试层级并保留SQL/回滚覆盖。另两项跨clause/反向Profile测试继续核对实际计划、共享预算、合法顺序和精确slot，未凭首轮失败笼统宣称结果错误，也未删除真实工作断言。

独立 bound Review又确认S3既有persistent helper的派生manifest类型缺口：完整execution之间将temporary fixture的`definition_blob`置为合法SQL TEXT，cold/warm × scalar/rows的旧S3公开查询4/4报`STORAGE_ERROR`，等价canonical查询均返回`[[7]]`且quick_check正常。首次candidate和已有TEMP binding两条路径都直接取BLOB；依[Schema/Index的失效边界](../../design/schema-and-indexes.md#adoption-boundary)应在首行前拒绝坏generation并fallback。已授权最小类型安全修复及永久cold/warm回归；真实I/O、权限、取消和canonical corruption仍传播，continuation已消费后的来源失效仍终止，不从头重启。该新增Finding以及剩余cursor/成本验证尚未闭环，S4继续保持open。

### 23.4 后续定向结果与压力基线（仍未冻结）

新增修复后的 bound14、query-owned advice cache2、join32及INL18的当前定向测试已通过。join32包含前述analysis、拓扑、等式及9项degree测试，不与早先unit数量重复累加；原始日志为 `s4-b-{bound-unit,budget-unit,join-unit,inl-integration}.log`。这些是尚未冻结的局部结果，不替代后续完整quality/CI或新制品SQL验收。

- 稀疏Rare关系起点已按真实physical scan域及唯一小cohort degree采样计价，不把logical多Label最小基数冒充实际读取域，也不把cohort均值当某个hot outer的上界。具名/匿名反向fixture分别为1406/1362 VM，原序34714/34670；dbHits均23/1832，真实Rare首seed与两个独立Adjacency slot各为1行。
- 原跨clause失败的直接原因是`folding_fits`把全query累计states再次乘Index属性数预收；现按本step inspection及本次parameter traversal的增量收费，同一总预算不重置。原强期待两个真实IndexSeek、完整576行bag的回归通过：2032 states、26 candidates、244130 bytes、0 fallback、2次exact、resolve1、Index build0。另有同Snapshot/definition的有界query-owned分布复用，消除重复统计SQL，不把缓存误写成重复计费的根因。
- Profile首access重复计row与mixed fallback偷前slot已有修复及定向证据，但审核又核到[拥有该合同的公开结果规范](../../design/interfaces.md#lithograph-json)要求operator hits总和与query hits一致。当前无独立slot的bound-Node fallback曾保留未归属hits，仍违反该要求；正在接真实point-validation boundary并补严格总和断言。不能改设计放宽、均摊或任意尾补到首slot以取得green，该项继续阻断最终冻结。
- 派生manifest类型回退已通过14项Core定向测试，含cold/warm、首次Bound bind、坏TEMP来源、权限错误传播与main不写；新增永久公开SQL `phase22_join_manifest_sql.py` 已接入quality/CI，但新制品的8个独立fixture验证仍待执行。

四档INL private delta/ready已准备：每档新增18个Node、53条raw graph delta和3个Layer，完整24行Oracle逐档核对。10k/100k/1M ready owners为10008/100008/1000008，重建wall为0.208337/2.122121/24.975414秒，构建成本独立记录；1k先前准备保留。1M原source当前含后写的1100个Node，private reset到指定1M anchor后才新增18个，实际分配ID从`n:1001101`起，未因reset复用已提交ID；32个Document是既有Node的额外标签。只读WAL首次访问创建32KiB SHM的正常transition、初次严格sidecar guard和诊断记录均保留，main/hash/head/history未变，不改journal mode或重跑已完成的1M旧宽查询。

`s4-join-work/before-full-1000/` 已完成33种cycle安全写序与1个ready INL，共34 cases、102次完整instrumented执行，scalar VM、rows VM和rows PROFILE分列；全部bag/hash/head/history/handle/cleanup通过。INL scalar/rows VM为680856/680037，dbHits20180；cycle rows VM范围304408–1019513，dbHits仍为3–8 Part的21k–56k。该数据只作真实工作量Before，不把三类样本混成P95或宣称quiet延迟收益。

SQL与Core harness还加入显式选择的24-Part Rare-last、64/129-Part Rare-first压力case，默认旧33种rotation不变；三者完整结果固定为实际Rare ID。24测完成Greedy的真实收益，64/129在原序Rare首位确保硬预算fallback也不制造旧宽扫描。当前只有1k S3功能smoke，尚未把case名称当S4启用证据；新冻结制品的四档5×20 warm/100 fresh、Core prepare/runtime、Linux1GiB/no-swap、取消/恢复及最终全门禁仍待执行。

### 23.5 动态模板与实际 key 消费边界修复

后续源码交叉审核发现A的Range模板准入宽于BoundRead实际key消费者。已在256个distinct DATE inner、重复稀疏outer及NULL、ready Range generation/CP的同一fixture真实复现：canonical oracle返回4行，原计划实际选择IndexNestedLoop后报`Internal: unsupported bound equality key family`，证据为 `s4-date-consumer-red-oracle.log`。早先每族两三个Node的测试会选择便宜Scan，因此没有触发该分支，不能将其green解释成全部动态Index值族已实现。

最小修复只将动态模板限制到真实消费者：Range的Boolean/Number/String和Text的String；其它已证明total的族仍可安全Join重排并保留完整residual，不把不支持的key交给BoundRead，也不为本修复扩codec/Indexkind。相同Date fixture修后真实NestedLoop/LabelScan、无IndexNL/IndexSeek，完整4行bag与oracle相等。最新analysis10/10、semantics16/16通过，14种PropertyType/IndexKind准入正反例永久覆盖，原Lizard/format门槛不变。公开SQL的独立Date fixture继续补充双surface和对称比较验证；全阶段冻结、Profile求和合同及最终门禁仍未完成。

### 23.6 冻结前合同修复和定向验证

第23.4节的Profile求和Finding已在真实bound-node验证处闭环：后继独立`MATCH(a)`使用既有NodeIdSeek表示实际`node_exists`访问，按clause/original Part/role分配自己的slot，不分摊或尾补hits。WITH、无WITH和连续两次bound clause的operator/dbHits总和分别为6/6、5/5、6/6；各bound点验独立rows=1/hits=1，首Rare producer仍为1行。所有成功的INL Profile测试严格断言总和相等，普通query、EXPLAIN与PROFILE使用相同生产者。独立Reviewer重读源码、完整18项日志和新增预算测试，关闭已知slot、首access重复计row及求和Finding，未放宽公开合同。

新budget end-to-end在同一个全query预算内真实触发4096 states上限，保留完成Greedy与全部7个join；后继未计划的Incoming EDGE仍保持正确Type/Direction。完整7字段Node/Relationship ID和属性bag同时等于直接Outgoing和整条OR-false oracle，Profile总和相等、drain/complete通过。原planner trigger测试仍要求16 Parts不fallback、256/600必须fallback及所有4096/1024/256KiB限制；仅移除没有合同依据的“任何128 Parts都不得fallback”，四档均完整返回唯一`[7]`。两项均有真实通过日志和独立只读复核。

统一新集成targets的INL19、order4、semantics16、unknown2全部通过，后续旧query_only2、relationship12、statistics4全部通过；Core query unit为332/332，Core all-target/all-feature Clippy以`-D warnings`通过。原批次仍保留exit101：旧predicates的15/16通过，唯一未过断言禁止所有IdSeek，将上述真实bound-node存在性验证也误列为WHERE narrowing。正保留全部scalar/null/Relationship/Path错误和原seek禁止规则，最小区分真实bound Node，并增加相反property/elementId residual的完整输出反例；该target新结果未通过前不冻结。

为直接记录A22-S4-02/06要求的prepare CPU，Core测量工具在相同prepare/first-pull/consume边界增加process CPU采样，复用lock中已有的rustix1.1.4安全time API，仅增加test-support的macOS/Linux依赖边。offline metadata核对package/version/source/checksum集合完全不变，不新增production依赖、unsafe或公开配置。旧S3无CPU日志和4个直接rlib另存保留，不回填虚构CPU；新工具仍须实际编译/功能smoke后才用于冻结制品的完整测量。以上仍不是Native扩展、四档压力、Linux资源、quality/CI或S4 Gate的通过声明。

新增真实Node反例已取得可复现red：原完整predicates为15/16，通过的结果/错误路径保留，唯一失败是无S4 Join的`MATCH(n) WITH n MATCH(n)`中bound NodeIdSeek实际rows=0/dbHits=2，而最终Project产出1行（`s4-predicate-bound-point-red.log`）。在真实producer补row后，两个同名bound clause又明确暴露legacy按名字取首slot：首点rows=2/hits=4、第二点0/0（`s4-predicate-bound-chain-red.log`）。正在仅为含新增bound点查的查询复用既有clause/Part精确site，不删逐点rows=1或总和断言，不新增registry，也不扩大参数合同。Core Value::Node参数只用于既有内部可达路径的首点计量，不据此宣称SQL JSON支持entity参数。

CPU工具的Debug编译和targeted Clippy已分别exit0，35.32/22.12秒；1k cycle3、ready INL和24/64/129 Part各4次完整功能查询，共20次通过，其中5次独立counter probe。process CPU字段均Some、非负、阶段加和和full/first单调成立，完整columns/bag、used=0、head/hash与两项故障报告保留均通过；实测Mac clock resolution为1000ns。该结果仅验证测量工具，不将Debug wall/CPU当作Release性能。最终四源码hash、负例和二进制见`s4-core-work/cpu-debug-final-checks.json`，正式压力仍等待新冻结。

### 23.7 最后消费者与来源成本复核（尚未冻结）

pure/参数/同名chain的Profile修复后，完整predicates16/16、INL19/19、Profile unit5/5、check与Clippy均通过，记录为`s4-bound-point-targeted.json`；这些结果只对应当时测试源，不覆盖随后新加的聚合反例。聚合后的真实`WITH n,count(*) ... MATCH(n) WHERE n.v=6`另已复现点验rows=0/hits=1，最终空结果与总和正确（`s4-predicate-bound-aggregate-red.log`）。该consumer使用既有path matcher，正在其逐Part循环按clause/原ordinal记完整WHERE前实际matches，防止外层输出或OPTIONAL空延长重复计点；多Part和mixed access也纳入必要反例，不将当前可达的新NodeIdSeek限定为未来工作。

最终B只读Review的其余指定项无新Finding，唯一来源成本候选已真实确认：34 Node、32条R的单Rare hot hub，完整4个ID输出正确；两个书写rotation均选Rare→Existing邻接→Chosen。Existing按无Label的全图均值估1，同一Part/方向已收集的Rare degree却为32；实际展开32行/160 hits，总227 hits，`s4-bound-source-diagnostic-red.log`的exit101保留。最小修复只让native bound邻接在provider guard已就绪时复用既有来源advice，保留RelID/Index优先和unknown/mixed cohort回退；固定方向slot预收内存和同一预算work，不建立条件基数框架，不提高任何限制。修后成本一致、实际更低工作量与原join回归仍须真实通过才关闭。

相同M32反例修后已真实通过（`s4-bound-source-diagnostic-green.log`，build20.29秒/test0.05秒）：两个rotation都选择Chosen→反向IndexProbe→Rare点验，Existing的来源expected/upper均为32，与实采advice一致；实际Adjacency rows从32降到1，总hits从227降到11，完整`[n:1,r:1,n:2,n:34]`不变。没有靠改变prefix估计框架或固定唯一书写序取得结果；修后原join完整单测和统一集成仍待最终批次，局部绿色不替代Stage Gate。

压力命令准备已通过独立复核，尚未执行：Native四档148个case实例/61568次完整measurement，Linux代表24个/9984次，分别444/72个相邻10× VM检查；每surface保持5×20 warm、100fresh和独立diagnostics，另计取消/恢复。8个Linux小manifest只改顶层操作性path/database，源JSON的前后hash及其余字段相同，原数据库不写。完整argv、未替换制品placeholder、只读repo/独立可写输出与精确1GiB/swap0前置在`s4-join-work/pressure-command-plan-s4.json`，不将排期或旧S3smoke视为S4 After验收。

最终定向批已真实全通过，427个Rust/Cargo输入SHA为`4c105589cfc54bc78064e42f16eee360d1a2065689b3aa4a882c1e07f2ab63ec`，每段后逐项无漂移：query336/336（含36个唯一Join tests，74.656秒）、INL19/order4/semantics16/unknown2/predicates16共57/57（127.215秒）、Core all-target/all-feature Clippy以`-D warnings`通过（38.884秒），见`s4-final-targeted.json`。该清单不是最终workspace freeze，未包含未变的`cypher.pest`；后续正式源码清单按与S3相同的全部crates文件范围包含该grammar。B三项具体P2已由独立Reviewer关闭；pure/同名chain/aggregate多Part及bound关系组后继Fan的点验slot、完整bag与总和在本批通过。当前新boundpoint查询复用既有完整sites，原pointCount/group-count补丁已删除，nonpoint用clause数值slot，普通无point路径不变。

最后有限consumer审读又确认初始`PreparedProgram`丢弃已生成sites，UNION/多SingleQuery在specialized planner早退时不能恢复；当前正常single-query绿色不能覆盖该路径。已准备双分支bound点验的完整反例，待本批结束后最小保留既有metadata并定向复验；不新增registry/executor。该项闭环、正式冻结、新Native/Linux制品及所有压力/全局门禁未完成前，S4仍保持`in_progress`。

该constructor缺口随后已取得真实红绿：`s4-union-point-red.log`中第一branch point为rows2/hits4，第二branch point为0/0；最小修复让`prepare_program`直接move既有`logical_operators_with_sites`的Vec，删除仅丢弃metadata的wrapper，不clone或新增mapper。包含原incoming scope、pure/参数/chain、aggregate多Part、mixed group和新增UNION的同一永久test修后1/1通过（50.466秒含build），最终Core Clippy以`-D warnings`exit0（35.337秒），每段输入无漂移；`s4-point-constructor-final-source.json`的427输入SHA为`b232ab2bb2bc62b224f0a825818a9a5e3e59778db415246f8387db68338972ca`。它比前述全绿色批只改保存sites、移除wrapper的unit及UNION反例三文件；不将这个单target等同于全部query重跑。当前代码静稳，Root将冻结含grammar的正式源码并以最终同源码quality/CI与真实SQL/压力完成后续门禁。

### 23.8 首版冻结、真实SQL与安全门禁失败

首版正式428个编译输入SHA为`6cf3cf3a61e0c5a964e619c0f6b52b9a2ed5c42fe51a2979f72eeb0df0657ef5`，921个全部仓库输入为`0edb8f415606f7e4cbd791d3256cbf43719f21c8dcb74d4e4fd23d7be0a5a9a8`。独立Native Release extension为`9103ab5a66b0fbc3b2790e7c7029c8ead520ce5996712e8c5c533e43a5b8890d`，Linux amd64 dev/no-debug为`c2c901896926efaa35e3a2fb43d78a8cd968a6aae42582ee688c617607c4f906`；Core tools另一次Cargo invocation构建并归档，未与extension ABI混用。构建与各检查前后输入及HEAD均不变。

- Native真实SQL通过access61、statistics9、readonly48及DENY/IGNORE矩阵、Relationship206、checkpoint8组和statement721；新manifest独立8/8全通过，首读不先EXPLAIN修cache，主库hash/head/canonical digest不变。manifest P2的旧红、Core14、独立Review和public SQL8形成定向闭环。
- 新Join strict numeric10+Date2、1k默认及24/64/129 smoke全部通过，21个实际PROFILE总和精确。rows VM/dbHits分别为INL8148/92、3-Part1741/21、8-Part2586/56、Greedy24为2528/48；64/129诚实fallback为4231/128、7416/258。首行后interrupt、早关闭、同handle恢复及fixture/37个旧Before artifact hash均通过；这些不是完整四档或quiet性能验收。
- Linux真实1GiB/swap0 `.load`/init/scalar/完整rows事件smoke通过，process peak111661056B、cgroup peak136364032B、events全0。原CLI不存在的加载前exit1保留，随后使用runtime实际具备的stock SQLite3.46.1 SQL load_extension；未将该输入检查失败改称通过。Linux correctness/wide pressure在本轮依赖重冻结前尚未启动。

`quality-stage4-reviewed-final`真实exit105，用时218.128秒，停在cargo-deny而未进入完整coverage：rustix1.1.4命中`RUSTSEC-2026-0486`，原日志、输入和制品均保留，不称quality通过。按[官方RustSec记录](https://github.com/RustSec/advisory-db/blob/main/crates/rustix/RUSTSEC-2026-0486.md)与1.1.6修复源码，最小升级test-support精确版本及lock中的rustix为1.1.6，不ignore advisory。lock package对照只有rustix版本/checksum改变，依赖边、其他package/version/source均不变；实际下载crate SHA等于lock checksum，许可与最低Rust兼容现仓库，time/clock和ClockId源码逐字节相同。新lock SHA为`281176722ed0112da3487140e61598fe7bd4db0ffd466dd8eaa1eb66561013c0`，详细证据在`stage4-rustix-security/`。

S4 Gate继续open。首版SQL绿色不代替升级后的新制品；第二版仍须安全门禁、正式冻结/构建、完整quality/CI、同源四档Core/SQL、Linux资源和最终Review/diff。尚未开始Native fullmeasure，也未开始Linux宽压力，不丢弃原失败或把其结果回写成新版本验收。

升级后独立`cargo deny check`已exit0：advisories/bans/licenses/sources均ok，原getrandom双版本warning保留，未改变policy或增加ignore；日志为`stage4-rustix-security/deny-after.log`。本次compiled输入相对首版只变Cargo.lock和test-support manifest；查询、存储与测量Rust源码未改。time API/后端共10文件对照仅一处cfg(test)宏限定名不同，实际production时钟实现不变。第二版完整quality/CI及性能压力仍未通过，不把这次安全检查单独作为S4 Gate。

### 23.9 R2 同源全门禁与四档压力（2026-11-12）

S4 R2 的428个编译输入为 `7b29fed00430d6a7754f079b809446c7e517f86323d98db7b3abc880c945ae89`，验收期间921个仓库输入为 `97a6f8a24ae6c85ebaf52499b3b6bac9c571b154cddf2a339c706b38ec48db91`，HEAD始终为 `3f1f31e48668b8b3836f944f1f7466db5804ce96`。Native Release extension SHA为 `032e201a9920876b9cb57e248c025e409d221fd1103887b1bf5914b1995df919`，Linux amd64 dev/no-debug为 `3cf309513191148749ec1f538225955d6b902c3e5e7ae9f331831afc7cbd1535`。以下结果均在这些冻结输入上取得；本节写入后只同步development状态，未重写此前freeze或失败记录。

- `quality-stage4-reviewed-r2-final` 实际exit0，用时1143.054秒，regions/functions/lines为85.87%/87.38%/88.99%；原覆盖率、Lizard、文件和warning门槛均未降低。`ci-stage4-reviewed-r2-final` 实际exit0，用时2012.787秒，包含Rust、适用TCK、真实SQL、Provider dual-extension、SQLite 3.45/3.53.4与文档验收。每段前后428/921及HEAD逐项无漂移；首版exit105仍保留。
- Native SQL四档1k/10k/100k/1M共148个case实例、61,568次完整measurement、444项增长检查和296个精确PROFILE求和全部通过。每case、每surface保留5个connection各20次warm、100次fresh及另列diagnostics。cycle rows VM四档恒为1741–2586，INL rows fresh为8366/8366/8367/8642，24/64/129-Part分别恒为2528/4231/7416。完整ID/value bag、重数、Rare seed、实际producer、中断/提前关闭/同handle恢复和原fixture/旧报告保护均通过。最大独立case warm/fresh P95为14.001/41.140ms，真实最大尾部40.857/101.409ms未删除；process peak RSS最高121,208,832B，包含Python与SQLite。
- Core同四档148个After case共30,488次完整查询；S3归档Core的1k工作量对照37个case仅3fresh+1probe，共148次，不计算P95。每个After case的warm/fresh完整采样、typed bag、CPU边界、资源归零均通过。cycle dbHits四档恒为21..56、ready INL恒92、24-Part恒48、64/129恒128/258；相对1k旧引擎，cycle与24-Part工作量减少1000倍，INL为20180→92。3–7 Part完成exact，8-Part在同一4096-state预算内使用已完成Greedy，24-Part真实Greedy，64/129保留旧路径预算fallback；未将Greedy称为exact。Core Join process RSS最高76,070,912B，Engine预留峰值2,742,441B。
- Linux实际运行在amd64/QEMU、1GiB/no-swap容器，真实stock SQLite 3.46.1 load/init、七类正确性及checkpoint原完整规模通过；四个串行压力job共24个代表case、9984次measurement、72项增长检查全部通过。所有full warm/fresh、严格bag/PROFILE、INL中断/提前关闭恢复和fixture/输入保护保留。representative job process RSS为832,425,984B、cgroup peak达到1GiB、`memory.events.max=5705`，OOM和oom_kill均0；其余job RSS约435MB。没有把QEMU的wall-time对照M2 SLA，也没有把高回收压力隐藏为低RSS。
- Linux高峰的有限诊断确认1M ready INL目标距可用checkpoint仅7个successor commit，并非重放267层完整图；一次Native同Core smoke的Engine peak为374,704B、process RSS约15.3MB、resolve1/layers2/build0。此结果不用于推断全部Linux峰值成因，未发现额外线性canonical扫描；没有扩展无关优化。
- 两个stock CLI的直接S4正例已实际运行：3.45.0/3.53.4各INL、cycle3、cycle8、Greedy24，共8job、48次完整双surface查询和8次EXPLAIN全部通过。实际INL、Rare seed和精确PROFILE Σ分别为92/21/56/48；版本/sourceid/compile options、readonly main、head/hash/bytes/sidecars均验证，补齐通用CI未直接断言新producer的覆盖。

证据均位于 `target/phase22-current/`：`s4-join-work/native-r2-quiet-full/`、`s4-core-work/quiet-r2-control/`、`stage4-reviewed-r2-linux-runtime/`、`s4-join-work/after-linux-results-s4-r2/`、`s4-stock-cli-executed-r2/`。各目录保存raw、命令/exit、环境/数据规模、输入与制品证明及独立重算；不是以摘要布尔值代替完整结果。

### 23.10 Release 短查询、尾延迟复核与非回退

10M Node/100M Relationship原主库和1000 changed-owner私有delta上，S3/S4按case交替完成10对短查询，共20个engine-case、4120次完整查询。每侧、每case仍为5×20 warm与100fresh，完整ID/value oracle、实际SQLite/pragmas、资源归零及零full generation build均通过；此前只有三样本且预热Snapshot的Core短查询记录不作为P95依据。S4 typed adjacency、ready equality、ready range1000的warm/fresh P95分别为2.369/3.070、2.358/16.581、29.503/39.984ms；三类delta均低于原1s。Engine峰值12,026,027B。27GiB主库只作inode/size/mtime、16KiB首尾、head/generation与sidecar证明，没有声称全文件hash或OS cold。

保留具体性能观测：原full中delta新range fresh P95由64.771升至109.609ms，CPU中位数约增10.7%，未以绝对SLA合格直接忽略。有限源码/原样本审查确认相同IndexSeek→Filter→Project、相同2000 hits/2100 changed-owner checks/resolve1/layers3/build0，单Part不进入Join。随后用相同两个Core binary完成100组平衡交替process的800次完整查询，每侧100个primary fresh，其余三类diagnostics分列；全部bag/资源/工作量通过。CPU P50为48.495→48.434ms、P95为51.783→52.663ms，wall P95为57.446→58.667ms，pair CPU差中位为−154µs。独立复核未确认持续的engine回退或需修改的Cost/准入问题。新协议的process生命周期不同，不能替换原full分布、排除长进程状态影响或声称已定位OS原因；该限制与原慢尾全部保留。

首次isolation controller把warm资源峰值误按fresh检查，产品CLI四次查询自身exit0；先核实只读状态后，在新目录修正角色诊断并完成100pair。原失败source/raw/exit未覆盖，原quota、资源归零、bag和工作量断言未降低。证据为 `release-short-core/quiet-r2-paired/` 与 `release-short-core/isolation-r2-corrected/results/`。

NR13实际三轮exit0，用时473.680秒，13/13原门槛通过。SQL四项使用本轮Before→After、After→Before、Before→After的新配对；History262行、Diff3000 operations及写入/显式事务副作用校验完整。Merge九项使用原真实Before3与R2新After3，均为10K conflicts、每页256项、40页/40轮resolution、完整candidate与GC root；旧Before的quiet状态未知，未称为新配对。中位inspection270ms≤314.4ms；376ms inspection、450ms finalize及345701µs prepare尾样本完整保留。单位按原规则ms→µs，门槛仍为 `max(1.20×Before, Before+10000µs)`，没有选择性重测。完整raw与独立复核在 `nonregression-stage4-reviewed-r2/`、`s4-r2-nonregression-preparation/`。

### 23.11 固定运行时的10M/100M完整长流

补强旧SQL工具只数行、不读值且原地排序丢执行顺序的测量缺口：ignored最小工具读取每个真实返回值、验证类型与固定值、完整行数及checksum，保留三轮chronological raw。Core固定原System SQLite 3.51.0（Apple `f0ca…aapl`），SQL固定原Brew SQLite 3.51.0（`fb2c…527b`）；两个surface分别验收，不混比运行库或把三样本称P95。全部以只读连接、固定 `commit/c04e6955…fbbcd5` 和显式read transaction执行；长流仍为原 `RETURN 1`，没有用count或预知行数替代实际消费。

| Surface / workload | 三轮完整消费秒数（执行顺序） | 中位 / 最大秒数 | 原门槛 |
| --- | --- | --- | --- |
| Core / 10M Label | 5.931554 / 7.073371 / 5.656227 | 5.931554 / 7.073371 | 60 / 90，通过 |
| Core / 1M hub | 20.855013 / 16.047737 / 16.568492 | 16.568492 / 20.855013 | 30 / 45，通过 |
| SQL rows / 10M Label | 8.401453 / 8.444304 / 8.728822 | 8.444304 / 8.728822 | 60 / 90，通过 |
| SQL rows / 1M hub | 8.194811 / 8.946014 / 8.580095 | 8.580095 / 8.946014 | 30 / 45，通过 |

Core九次primary关闭test性能计数，另列三个完整counter probe验证resolve1、generation build0、资源释放0；每行实际Value::Integer(1)与BLAKE3相符。SQL每行验证实际TEXT `[1]`、完整columns/summary/DONE、FNV checksum，另通过真实interrupt/reset、early-close、同statement与reopen恢复。两侧均按同一Label查询另测LIMIT1M：Core process RSS最大22,642,688B、保守 `max(10M)-min(1M)` 为2,916,352B；SQL分别为21,037,056B、131,072B，均满足1GiB总峰与128MiB增量。open/load、setup、CPU与清理单独报告；无输出规模同阶retained rows。

首次新Core工具误用Brew `-L/rpath`，虽然数学SLA通过，最终链接复核仍撤回其固定Core基线资格；原source/build/raw/summary全部保留为单独诊断，SQL结果有效。随后复制独立source，按原System SDK默认链接编译，`otool`确认 `/usr/lib/libsqlite3.dylib`，在graph prepare前对12个实际连接硬核完整Apple sourceid，再运行9primary+3probe。修正没有改生产源码、查询、数据或门槛。最终有效证据为 `s4-r2-long-preparation/execution-system-r2-01/` 的Core与 `execution-r2-01/` 的SQL；后者Core仅诊断，资格说明为 `runtime-eligibility-correction-r2.json`。三次probe、全部21次有效完整长流及SQL lifecycle均exit0，前后编译输入、制品、归档依赖、HEAD与主库采样证明一致。

### 23.12 S22.4 局部门槛与 Review 结论

| Acceptance | 真实结果与主要证据 | 状态 |
| --- | --- | --- |
| A22-S4-01 | 四档33 cycle case、不同书写顺序、实际Rare producer/完整bag/工作减少；DPhyp与Greedy真实区分 | PASS |
| A22-S4-02 | 24/64/129及永久预算端到端，4096/1024/256KiB不变；真实CPU/内存、Greedy与fallback正确结果 | PASS |
| A22-S4-03 | INL19/类型16等永久语义，四档24-row重复bag、实际Index访问、resolve1/build0与损坏manifest fallback | PASS |
| A22-S4-04 | query336及57项定向集成、最终同源CI/TCK、双SQL；权限/GraphView/NULL/error/历史/staged/write/relationship uniqueness | PASS |
| A22-S4-05 | Native/Linux四档增长、1GiB/no-swap与恢复；固定运行时10M/100M长短流、NR13原门槛 | PASS |
| A22-S4-06 | 同源quality/CI、两个stock runtime直接producer验证、独立Review与最终diff/文档核查 | PASS |

已知Profile slot/constructor、Date动态key消费者、bound-source估算、persisted manifest和预算finding均有真实红绿/永久反例及独立闭环。最终 `s4-r2-final-diff-review.md` 确认无scope内未解决blocking finding、无generated/secret/误纳入fixture；S4未新增allow或放宽门槛。累计HEAD差异中的一处S3 `too_many_arguments` 有原reason且SHA与已验收S3相同，不将累计差异误称零allow。

本轮完成S22.4本地局部验收，可以进入S22.5。Phase22仍为 `in_progress`；S22.5–S22.8和最终全算法/三种30分钟并发稳定性等全局门槛尚未完成。代码与文档仅本地修改，未commit、未push、未发布或部署；之后每Stage新源码必须另行冻结验收，不能复用本节旧绿作为新实现通过。


## 24. S22.5 实施中（2026-11-12，未验收）

进入条件已由第23.9–23.12节及 `target/phase22-current/stage4-reviewed-r2-local-gate.json` 证明；S4冻结编译输入仍为7b29fed…ae89。S5新改动另行记录来源，不能引用S4旧门禁作为Hash或Spill已通过的证据。

当前按第9节依赖先验证既有typed Value/BindingSpill真实round-trip与数值equality，再接现左深计划、同一MATCH cursor和clause-owned状态中的Hash/两侧Grace分区。先保留实际codec失败再做最小修复；共享DTO与运行时接线期间不启用S22.6–S22.8。新的公开Profile可选字段在拥有接口设计补齐，并以真实执行验证后才记录完成。

本节当前只记录S5开工，尚无A22-S5-*完成声明；内存Hash、成功Grace、四档/Linux资源、完整门禁及Review均须继续取得当前源码的实际证据。当前所有改动未commit、未push、未发布或部署。

### 24.1 Typed spill 首个实际修复与配置回归

既有codec已在直接row与真实BindingSpill TEMP两条路径复现finite Float的1ULP漂移：51.248178375505404变成51.24817837550541，bits为4632409357216853856→4632409357216853857。两项真实runtime red保存在 `s5-codec-roundtrip-runtime-red.log`；首次默认Core编译E0432另存，未把编译错误当codec语义红。

最小修复复用既有checked Value JSON与私有spill codec，以短scalar marker保护finite bits，并对整个私有DOM中的原前缀字符串作对称转义；不改变公开JSON、全局serde_json feature、依赖版本或canonical history。新增marker scratch分配前计账，并复用既有encoded JSON大小预估，补足compact I8/I16原DOM及F32/F64 marker的容量差额，费用持至DOM释放。reserved Map、Node/Relationship/Path、Point/Vector、order/CSV exactmarker、特殊数值、低quota与序号未推进等永久用例均纳入。最终8项新codec及13项既有spill/sort/cancel共21/21通过，原strict sort limit保留；fmt与C15/L100/a10通过。记录为 `s5-a-codec-implementation.md`、`s5-a-codec-stable.json`、`s5-codec-all-spill-green2.log`。

同时修复Phase22遗留的默认Core unit配置缺口：cache/tests.rs无条件引用feature-gated performance，CI同时选Core和test-support包的特性合并曾掩盖它。现只对instrument导入、启动/快照和计数断言加feature cfg，两项完整语义路径及其它cache测试继续默认执行；默认和test-support配置同一target各6/6实际通过。未整体skip测试、开启默认feature或改production。初轮被尚未接完的Grace模块挡住的编译失败也保留；这项修复不重写S4历史冻结记录。

上述结果仅完成codec与测试配置的有限切片，不代表Hash/Grace运行或任一A22-S5-*已通过；共享计划、真实cursor、分区与Profile仍在接线，随后须继续完整Stage验收。

### 24.2 真实 Hash / Grace 接线与定向验证

本节续记24.1之后的实现。Hash 已进入原左深计划的成本比较及同一 MATCH cursor：prefix probe 与独立 singleton build 复用既有 clause owner，跨 incoming row 和原256行 write batch 只 build 一次。成本与 Index Nested Loop、身份 Seek 和原路径比较，不强迫所有 equijoin 使用 Hash。完整 WHERE、可见性、Relationship uniqueness、typed equality 与原输出身份继续保留。

Grace 使用一个 private TEMP connection/table，双侧有限分区，深度及 fanout 有界；hot leaf 以有限 build chunk 重扫该 leaf 的 Probe。全部 row、key、table 扩容峰、reader、输出及 Rust prepared-cache 容量在分配前预费，SQLite native VM/pager 与 Engine ledger 分列。row/lease 在 fallible 交接期间保持完整 tuple，已独立关闭提前归还 reservation 的 P2；资源最终回零不代替析构顺序的源码证明。

截至本轮定向结果：完整 query lib **416/416**、Hash integration **16/16**、相邻 INL/order/semantics/unknown/predicates **57/57** 通过；另有完整 digest collision **1/1**、extension encoder/RSS **4/4** 及最终 Core/extension all-target/all-feature Clippy 通过。日志分别为 `s5-c-query-second.log`、`s5-c-hash-integration-final.log`、`s5-c-neighbor-integration.log`、`s5-c-final-collision.log`、`s5-c-extension-profile.log` 和 `s5-c-final-clippy2.log`。

这些记录各自保留实际输入范围：416项完整 query lib 对应 `s5-c-query-inputs.json`，不是 Native 候选同源的完整重跑。此后两个Rust文件发生变化：`hash/grace.rs`增加有限重分区后的TEMP峰值刷新，由后续fresh Grace、16项integration及最终Clippy覆盖；`hash/probe.rs`只新增`cfg(test)`碰撞单元入口，新unit另行1/1通过，没有运行时增量。不能把这些定向检查合并声称最终全部源码已通过完整query suite。最终同源完整quality/CI仍须在冻结后运行。

成功 Grace 已实际完整消费两侧各1000个非空 numeric key：默认 resident soft target 和显式512KiB均触发真实分区/VM/TEMP，并核对全部1000条ID/value bag；双侧 hot fixture完整返回3996行，任一合法 build 方向均含超过两块容量的 matched hot group。UNWIND270与 direct write260分别证明 build64一次、probe17280/16640；后者经原 write bridge，核对260项写入、最终值和完整结果。首次错误地强迫 cheap NodeIdSeek 使用 Hash、及 hot fixture 交换 build 后不足以证明 chunk 的失败均保留；修正 fixture/断言，没有提高生产 quota 或强迫物理算子。

PROFILE 两个 SQL surface 复用同一 encoder，分别报告实际 Hash retained peak、整个 execution ledger peak 和进程 lifetime RSS high-water mark；RSS 不冒充当前查询增量。Native 候选以 `cargo build --locked --offline --release -p lithograph-extension` 构建成功，前后 product、全部编译输入、仓库输入和 HEAD 均无漂移；候选 SHA256 为 `b650116119848076362569bab114d7d64117280ef2b3a7fc7ed4ac65398a0ca1`。`stage5-smoke-candidate-r1` 保存完整来源与构建记录，它是快验候选，尚非最终 Stage freeze；构建和 encoder unit 不能代替真实 SQLite load/SQL 验收。

当前继续验证 Native SQL、变长 key 临界配额候选、四档和百万完整消费、100k+ hot、Linux Release 1GiB/no-swap 真实磁盘 TEMP、完整 quality/CI、性能非回退与独立 Review。A22-S5-* 均尚未汇总为 PASS，S22.6–S22.8未实施；整个 Phase22仍为 `in_progress`。所有改动仍未commit、未push、未发布或部署。

### 24.3 Native 候选真实 SQL 快验与有限资源候选复核

同一 Native 候选在实际 Homebrew SQLite3.51.0上通过9组 SQL scalar/rows 语义用例，包括完整ID/value bag、对称及交换书写顺序、NULL、Graph View、query/result mask、零outer不触发错误，以及非空错误后的同连接恢复。实际connection的SourceID在load之前校验为 `2025-11-04 19:38:17 fb2c931ae597f8d00a37574ff67aeed3eced4e5547f9120744ae4bfa8e74527b`。主文件hash、head与sidecar保持不变；记录位于 `s5-hash-work/candidate-r1-functional`。

该候选的真实SQL rows另完整消费1000行uniform Grace，默认/512KiB分别记录9/49个分区、197445/268011 private spill VM steps、execution peak1167318/481770B、process lifetime RSS peak20119552/19529728B。完整行Oracle、Hash build/probe各1000、operator dbHits之和等于summary8309/8684、statement finalize及close均通过。主connection trace与private spill VM分列；TEMP逻辑页348160/360448B不能充当真实磁盘文件证明。512KiB下另做一行后early-close与interrupt，正常完成同statement恢复及reopen；这些负例不声称完整bag通过，也不代替大输入build期间取消测试。

变长key静态候选只检查一组预先固定的CP输入：两侧各128个typed STRING节点、单个512KiB长key、hard quota1310720B，对比长key位于首/尾ID。single-ID峰1120565B，两个Join峰1140980B，全部成功且完整空结果、资源归零、Σ及head不变。两Join实际选择合法NestedLoop，未复现product Resource问题；独立复核确认没有降低计账、提高quota或吞错误。该组作为安全fallback负证据关闭，**不覆盖actual Hash长key/preflush成功路径**；没有对应production修复，不扩大参数搜索。首次测试配置的E0451编译失败与后续真实2/2运行通过分开保留。

以上均为候选快验，尚非四档分布、Linux资源或完整Stage门禁。下一步冻结最终全部输入，按第9节完成剩余验收；S5仍未标记完成。

### 24.4 R1 冻结后的首轮门禁与恢复

R1冻结478个编译输入（SHA256 `d80bb50a394135e584abecc8f2a109093650a983f2a14e34e5489f40a1084dcc`）及973个仓库输入（`f2f6fb15d60d52b5397860bf55d69f978581c624bd9a1468d4be90925c1d3a21`），HEAD不变。Native Release构建、ABI检查和七个独立Core Release工具构建均通过，构建前后输入无漂移；Native制品与24.3候选为相同bytes/SHA。Core归档记录为 `stage5-reviewed-r1-core-link-inputs/manifest.json`，Core rlib SHA为 `d51f1b731d330376cdea7355cb11af77eb2b90fbc46ddf62e495c4c407b04cb5`。

R1完整quality在Clippy步骤退出105（100.56秒），未计PASS；唯一诊断为test-support `phase22_hash_work/measure.rs:208` 的`collapsible_if`。现只把SourceID guard改为等价let-chain，校验、错误文本及行为不变，无allow或门槛放宽。原始日志、退出状态和R1冻结保持原样；修复后重新冻结并重跑完整门禁，不将先前局部绿拼成完整quality通过。

R2重新冻结后，Native与七个Core Release工具构建仍通过，实际所有二进制及五个direct rlib均与R1相同。第二轮完整quality退出105（154.77秒）：workspace Clippy、rustdoc和production Lizard通过，support Lizard发现新工具三处超原CC20门槛，分别为 `phase22_hash_fixture/verify.rs::fixture` 的22、`phase22_hash_work/fixture.rs::load` 的33、`phase22_hash_work/measure.rs::sample` 的26。继续做最小helper拆分并保持guard、Oracle、采样次数与计时边界；不加allow或降低门槛。R2日志、源码清单与退出状态同样保留，完整quality仍未计PASS。

Linux首次Release构建也未通过：`prettyplease` build script的`cc`链接步骤报告SIGSEGV11，命令含`-fuse-ld=lld`。失败发生在构建期间，没有运行查询，不能称为查询OOM或Linux资源验收；`stage5-reviewed-r1-linux-build`保留原始命令、输出和退出记录。继续用现有工具做最小链接诊断，恢复后的构建另存记录，不覆盖此次失败、不安装新工具链或以dev制品代替Release验收。

现有工具的最小恢复验证已实际通过：同一cached Linux amd64 image/rustc1.98.1以 `-C linker-features=-lld` 使用GNU ld2.40编译并执行Release hello，exit0；probe实际4GiB/no-swap/2CPU、peak198295552B、oom/oom_kill均0。首probe已成功链接但因disposable `/tmp` 的noexec退出126，另起带exec的临时mount完成运行验证，两份原始记录均保留；不据此推断首次SIGSEGV的根因。随后沿既有volume、offline与Release配置启动GNU候选构建，product前后不得漂移；D的support helper修订与文档变化另记，候选只准备缓存，最终全部输入冻结后的build/ABI与Linux查询资源验收仍须继续。

### 24.5 TEMP 不可用的真实错误分类修复

R3冻结前，用同一b650…Native候选、实际Brew3.51.0 SourceID、readonly uniform100k及512KiB运行一次公开SQL负例。宿主VFS只对TEMP_DB的xOpen返回SQLite READONLY8，实际命中1次（flags542），终态及reset均为8，公开消息为 `LITHOGRAPH_IO_ERROR: attempt to write a readonly database`。这与第9节A03及Query Engine的不可安全spill资源合同不符，是实际分类red，不是编译或工具假设。same retained rows handle恢复、close/VFS恢复、readonly reopen quick_check及主文件SHA/head/sidecars不变均已通过。记录为 `s5-linux-runtime-preparation/native-temp-deny-r1/result.json`；该宿主注入不冒充OS chmod失败、物理ENOSPC或Linux资源门禁。

发现该问题后先核正在运行的GNU候选仍为running、OOMKilled=false，再仅停止本任务构建container，docker stop退出0。候选以143结束、未取得Release制品；product、编译输入及HEAD前后未漂移，文档输入变化单独记录。`stage5-linux-gnu-candidate-r1/controlled-stop.json`及原始log/result均保留，不把受控停止称为产品编译失败，也不拿中断制品做压力验收。

修复仅在query-private Grace store的connection open、SQL prepare及apply边界，将拒写/无法打开/权限不可用的SQLite8/14/3转换为既有Resource/TOOBIG18；消息只包含原numeric code。canonical database、global QueryError和extension mapper不改，真实IOERR、损坏、取消、锁冲突、AUTH及FULL保留原分类。Query Engine拥有正文已明确该范围，没有新增公开选项或错误方言。

永久回归在实际private connection上开启query_only，再执行真实append：旧实现语义red为Io而预期Resource，exit101；修复后原36项spill加两项新回归共38/38通过，另验证global readonly仍Io及无关错误原样保留。日志为 `s5-private-temp-query-only-red.log` / `s5-private-temp-spill-green.log`；Core all-target/test-support Clippy和Grace C15/L100/a10通过。新Native制品的同一VFS公开SQLgreen、最终冻结和完整压力仍待继续，不能仅凭Core green标记A03通过。

同时做未冻结静态预检：依赖使用及供应链检查exit0；原1% duplication gate发现1303/125368重复行（1.04%）而失败。只从本次新增测试提取真实共用的准备与断言，保留全部预算、owner/lease、失败和结果检查；不提高门槛、不改变ignore或挪测试规避扫描。完整quality/CI尚未通过，所有预检结果按实际范围保存。

上述测试复用完成后，原duplication命令实际exit0：1249/125405重复行（约0.995976%，console四舍五入为1%），减少54行，阈值和扫描范围不变。只整理Grace测试及Hash table的`cfg(test)`段，Table生产前缀11886bytes逐字未改。新的溢写38/38、Hash table8/8定向复验通过；这两组不冒充整个Core suite。记录为 `s5-private-temp-duplicates.log`、`s5-private-temp-helpers-spill.log`、`s5-private-temp-helpers-table.log` 及 `s5-table-test-duplicate-repair/result.json`。同样，D的三处support函数提取已通过全support复杂度、all-target/all-feature Clippy与完整1000行Core快验；独立将测量helper机械inline后可逐字还原R2，确认计时边界未移动（`s5-measure-r3-independent-review.md`）。

提取后的Core all-target/test-support Clippy最终exit0（89秒），四个production crate原C15/L100/a10检查亦exit0；`s5-private-temp-helpers-clippy.log`和 `s5-private-temp-production-lizard`保留实际记录。此后源码停止修改，继续新的Native公开VFS复验、全部输入冻结下完整quality/CI及Linux Release资源门禁，尚无S5完成声明。

### 24.6 R3 完整质量、平台快验与大档预算回归

R3冻结478个编译输入，SHA256为 `4054a3e82e6c3e95ce7212bae07854d5b8de2588f2fc1ce507479d7f487c5dc8`；973个仓库输入为 `3ff65c699b6ea3d520d71ae765c3ce1f2acbdd5fdab0facbf33890f1adc75897`。Native Release构建、ABI及七个Core Release工具构建均通过，Native制品SHA为 `96272e9c0742dfde3333aa175aff2fb59c8945416d99cf32fb5de16566d3df92`，Core rlib为 `3d3b3860462630b186401a4052a9c46c09e960918f9ddb1729781c96df2c157f`。同一VFS/100k/512KiB负例已实际返回Resource18，并通过恢复、close及immutable检查，旧red保持原样。

同源 `cargo make quality` 实际exit0（917.827秒），region/function/line coverage为86.20%/87.55%/89.30%，原门槛不变。Linux GNU Release亦实际构建成功（861.379秒）并通过ELF64/amd64/entrypoint及无私有SQLite ABI检查，制品 `stage5-reviewed-r3-linux-release-final.so` SHA为 `1df90f1614b87ef66165499bac0850e36d5db565b9e453e73fb19e1211fa3e45`。这些是R3的真实结果，不代表后续修订已经通过。

R3 Native九组双surface语义、default/512KiB两个完整1000行Grace均通过，20份PROFILE的Σ精确等于summary。另实际运行stock SQLite3.45.0/3.53.4共四个进程，核对18个完整1000行bag、六份Hash spill PROFILE及28次恢复；两版512KiB rows peak均481770B，scalar完整collector按原合同返回资源错误。3.53 shell错误文本格式使原runner失败，保留原run/exit1，对同一raw按实际shell源码做精确版本解析后exit0，独立复核无finding；该shell未输出的数字错误码明确记为未直接观测。证据位于 `s5-stock-cli-preparation`，没有重放查询或改写原失败。

Linux默认1k完整Hash/Grace与1GiB/no-swap成功；512KiB产品worker也完整成功，但旧外部observer遇到退出后的`mountinfo EINVAL`，整job保留failed。仅在复读同PID/starttime确认Z/X或已消失时识别退出竞态，14项正反自检和独立review通过后另起标签复验，worker/observer/外层全部exit0，完整1000行、Σ8684、ledger481773B、wait4 RSS110223360B、cgroup peak170790912B、OOM/events全0。reset/close两次ACK都在worker存活时观察TEMP FD为0；小图无实盘FD，未冒称磁盘证明。原失败和新证明分别保存；大档实盘、八次canonical integrity及全部39-job计划仍未执行完。

正式quiet采样前的1M/512KiB Core预检发现实际Resource：first1/follow256原协议运行305.415秒后exit1，RSS peak100253696B，未完成bag，rows进程未启动；fixture及全部冻结输入保持不变。固定10k同协议另一次完整成功，目录585、ledger481379B、Σ110380及used0都通过，排除“目录满即必失败”。随后100k有限诊断确认首批1行成功，释放批次后仍live437915B；紧接请求256时Resource，peak524015B，错误后used0。没有将调用边界观测冒称确切reserve栈；详细记录为 `s5-resource-diagnostic-root-10k-r1` 与 `s5-resource-diag-r3/uniform100k-512-once`。

据此在原direct读取路径增加共享预算驱动的完整行边界partial yield，调用方仍请求256，不改变quota、规模或完整Oracle。旧小型真实Grace/hot用例以Resource语义red退出101，修复后完整3996行green；原18项Hash加该回归及None批次、LIMIT、Type error、取消四项相邻永久用例，共23/23通过，resources16/16、stream33/33及production Lizard通过。两个独立窄review确认无row丢失、重放、额外求值或无预算行为变化；移除新增预算块可逐字还原R3生产文件，详见 `s5-resource-fix` 和 `s5-next-direct-partial-yield-independent-review.md`。

为避免in-flight修改，先受控停止R3 CI：process exit -15、1735.281秒，478/973及HEAD前后无漂移；该CI未完成，不计PASS。新源码仍须另行冻结、重新构建，并通过原100k及1M协议、完整quality/CI、平台/性能及压力门禁。S5及Phase22继续 `in_progress`，没有提交、推送、发布或部署。

该小修最后Core all-target/all-feature Clippy exit0（87.775秒），479个编译输入在本轮检查前后无漂移，实际增量仅一个生产文件及三个测试文件（包含一个新文件）。`s5-resource-fix/closure.json`与`implementation.md`记录本地定向closure，未标100k、1M或Stage完成；从此保持源码稳定，另行冻结R4并复验原大档协议。

### 24.7 R4 大档预算闭环、完整门禁与正常压力发现

R4 冻结 479 个编译输入（`ac772bb32a1ac0f664f1ad1d9ac05f90ba531dbb32fc0ab0c38929940d5a88d2`）、974 个仓库输入（`6e764896f763474477c50a0d9bd5ebf698d8d8812dde251e0f342b1d09befe0f`）。原 first1/follow256、hard512KiB 的 100k 和 1M Core 全量逐行结果通过，ledger peak481379B，used0、完整 typed bag、PROFILE/summary 求和及 immutable guard 均通过；1M 的 instrumented wall790.118秒只作诊断。Native 九组双入口与 stock3.45.0/3.53.4 同源检查通过；新预算让出也使原 scalar512 场景完整成功，原 R3 Resource 记录继续保留，不能再将其解释为必然 collector 下限。

同源完整 quality exit0/960.8999秒，region/function/line coverage86.20%/87.55%/89.31%；完整 CI exit0/2092.121秒，输入和 HEAD 前后未漂移。Linux GNU Release/ABI通过，1k/10k×default/512KiB×三轮共12项通过，10k全部有实盘TEMP与宿主/外部inode交叉证据，1GiB/no-swap及OOM事件均符合。100k worker完整正确返回100000行，但 observer 重复记录文件涨缩触20000上限，whole job 保留失败，不用 worker0代替整体通过。

正常 Native 12组压力完成10组后，SQL1M第一个普通完整样本耗396.334626秒、CPU389.325361秒、wait4 RSS25444352B。源码确认私有 Grace connection未批量事务，每行INSERT、UPDATE与DELETE各自处于隐式事务。为实测该候选受控停止剩余压力，12组整体未PASS。停止时探测PGID的EPERM暴露工具过程记录缺口；原10组与普通样本、失败和203份raw均保留。后续新runner以真实dummy验证原红1项/修复绿6项及独立review闭环，未回填缺失的旧wait4/process记录。

### 24.8 私有 TEMP 事务与资源错误来源的最小修复

生产增量只在Grace owner及其reader/io和Hash loader：在同一私有connection的CREATE前经原SQL计数路径BEGIN，finish先采TEMP统计再自然关闭/回滚；不改变共享RowSpill、宿主事务、soft1MiB、hardquota、fixture或partial-yield。真实SQLiteTOOBIG与本地预算都可能code18，增加窄的本地预留拒绝来源位，只有该来源且private txn仍活时允许chunk retry；其余read错误关闭owner，closed-owner原错误不被继续吞作overflow。

永久回归取得真实旧红→同样例新绿：private txn跨repartition/retire/finish/abort/drop而host事务不变；SQLite在read step执行zeroblob(2147483648)触TOOBIG18后必须关闭owner，未构造QueryError或分配巨量Rust值。候选spill40/40、stream33/33、Hash23/23、Core全目标全feature Clippy、production Lizard、fmt/whitespace通过；独立有限review无finding。物理NOMEM未另外注入，不把错误路由证明扩大为全部底层故障覆盖。

候选冻结480个编译输入`fe5e02dc0eb4ef401577f51aca912dc96fa7067b716828e00e12ccde3fbdf0c7`、975个仓库输入`680f6e70f227fcf16b15aad6b298705147e81e986eb91b472aa7442fee317acf`。Release build exit0/68.705秒，正确制品为Cargo compiler-artifact选择的liblithograph.dylib，归档v2 SHA`bab2142572dc48458b93aab18dff50393a4c3883226d04a9e37e0be13a161a5b`；ABI及九组双SQL入口通过。初次误拷Oct30旧文件的无v2候选与其SQL失败明确作废保留，不能归因于新源码，也未用于性能样本。

100k同原rows完整3普通+1独立诊断均PASS，中位30071959→8179783微秒（3.676376倍）。小JSON独立复核确认SQL、参数、Commit、fixture、预算、helper、runtime/cache类与完整bag一致；hostVM/fullscan/sort相同，privateVM仅增加BEGIN的3步，partition/batches/bytes/enginePeak/TEMP相同，wait4RSS均约27MB。三次样本不报P95，诊断不混入median。

1M同原rows完整3普通+1独立诊断亦全部PASS；三轮普通171847654/173020430/172100145微秒，中位172100145、最大173020430微秒，三轮CPU分别170478126/171512843/170653999微秒，独立wait4 RSS25657344/27000832/27328512B。诊断完整1000000行，真实Hash build/probe各1000000、585分区、457461批、438908659 private spill VM、205777792编码字节、341913600逻辑TEMP字节及Hash retained peak1177708B。fixture完整bag/主文件与元数据不变、480/975和HEAD前后guard均通过。旧R4只有一个完成的普通1M样本396334626微秒，因此这里只报告该单样本与新三轮原始值，不构造旧median或P95。100k和1M两档实测支持保留该候选；不由Native低RSS推断Linux cgroup已经验收。

上述候选结果不替代新源码完整quality/CI、Core/SQL全部正常采样、全部39项Linux资源/故障/八次canonical、NR13和release10M/100M门禁。S5及Phase22继续in_progress，S6–S8未实施，未提交、推送、发布或部署。

<a id="stage-s22-5-acceptance"></a>

### 24.9 R5 最终同源门禁、原生资源验收与局部通过

**2026-11-12：S22.5 的 A22-S5-01–06 已完成局部验收。** 第24.1–24.8节保留当时的失败、候选和待测状态；本节记录随后取得的终态，不回填旧red为PASS。S22.1–S22.5局部通过，S22.6现已进入实施，S22.7–S22.8未实施；Phase22仍为`in_progress`。

#### 冻结身份与完整门禁

本轮R5测试的HEAD为`3f1f31e48668b8b3836f944f1f7466db5804ce96`；480个编译输入SHA为`fe5e02dc0eb4ef401577f51aca912dc96fa7067b716828e00e12ccde3fbdf0c7`，975个仓库输入SHA为`3a324cbd2659726dee0459f59dbcd72ead7b2d5db8da397a2c317eeec6d599cc`。macOS制品`bab2142572dc48458b93aab18dff50393a4c3883226d04a9e37e0be13a161a5b`是正确candidate-v2的同字节晋升；Linux GNU制品为`0a61767b299e226ddea84553ad3267e57dd6a0508c1404c1197e2c96d9d878b1`。旧无v2误制品仍为INELIGIBLE，不用于本轮结果。

| 范围 | 实际结果与计量边界 | 证据索引 |
| --- | --- | --- |
| 完整质量与CI | `cargo make quality` exit0/935.3079s，region/function/line为86.20%/87.55%/89.31%；`scripts/ci.sh` exit0/2022.4324s，适用TCK、SQL/Provider及既有回归通过，输入与HEAD前后相同，原阈值/分母不变 | quality：`target/phase22-current/quality-stage5-reviewed-r5-final.json`、CI：`target/phase22-current/ci-stage5-reviewed-r5-final.json` |
| 原生macOS正常Hash压力 | 12job/3780次完整测量：3732short＋48long，原5×20warm/100fresh和3normal＋1diag完整保留，30份PROFILE求和、12项VM增长与cleanup通过。Before仅1k；SQL rows中位6.931366→0.041847s，Core9.485847→0.067706s，dbHits2002000→8309。1M default rows/Core中位173.610677/175.686797s；长三样本不报P95，不冒充hard512 | 独立复核：`target/phase22-current/s5-hash-work/native-r5-pressure-01/_runner/independent-review.json` |
| Stock SQLite | 3.45.0/3.53.4各default及512KiB，4个进程、24个完整typed1000-row bag、8份真实Hash spill PROFILE、28次恢复全过；不是独立磁盘或性能分布证明 | stock独审：`target/phase22-current/s5-stock-cli-preparation/independent-r5-actual-review.json` |
| 非回退与release规模 | NR13/13、Core short20engine-case/4120次完整query、Core/SQL long22process全过；原非回退公式、10M Label/1M hub、1GiB/128MiB门槛不改。Core固定Apple SQLite、SQL固定Brew SQLite，各自同条件核验；旧Merge Before quiet状态仍UNKNOWN | NR/short/long独审：`target/phase22-current/s5-legacy-acceptance-preparation/s5-r5-independent-legacy-review.md` |
| 显式hard512成功Grace | 原first1/follow256的Core100k＋1M全量通过，1M ledger481379B/585parts/used0；SQL rows1M完整typed bag、Σ27604924、execution481770B/585parts/reset0及恢复通过。instrumented功能证据不混入正常P95，SQL没有公开used0字段 | Core汇总：`target/phase22-current/s5-hash-work/functional-large-r5-preflight/summary.json`、rows汇总：`target/phase22-current/s5-hash-work/functional-rows-1m-512-r5-preflight/summary.json` |
| 普通与显式事务 | ordinary同prepared实际Hash后Type错误的head/bag/autocommit/used0回滚通过；active explicit强setup后TEMP_DB VFS FULL13/reset13，零参commit精确NoActive21、autocommit恢复与fault次数不增，恢复/reopen/integrity通过。01错误工具断言与02未触fault旧红保留 | ordinary：`target/phase22-current/s5-ordinary-write-physical-probe-r5/closure.json`、explicit TEMP03：`target/phase22-current/s5-native-temp-full-rollback-r5-03/actual-result.json` |

#### 原生Linux的13项资源与恢复证据

用户要求正常有效测试、避免无意义重复，并停止Mac上的VM测试环境；后续高资源/跨平台验证使用获授权的远端原生机器或GitHub CI。Colima/Lima、相关VM数据及无用Rust cache/旧制品已清理，所有Mac测试DB已删除，项目目录`du`快照由约372.32GiB降至约0.497GiB。精简源码、小报告与旧失败证据保留；不要求重建本地VM。清理终态：`target/phase22-current/colima-uninstall-final.json`、测试DB清理：`target/phase22-current/mac-final-dataset-cleanup-r1.json`。

29,250,646,016B release主库经direct gzip流迁移，接收端独立完整SHA后atomic rename，两端SHA均为`19436863d816f91a04c35db1559adbe696968a2fba3f758d41b29449a9c05d90`；成功后删除Mac原库。传输结果：`target/phase22-current/remote-native-r1/scale-gzip-receive-r1.json`只证明迁移完整性，不是远端release查询已通过。

资源验收在原生x86_64 Linux/LXC共享kernel环境直接运行，无Mac VM/QEMU执行。既有GNU制品直接native加载，实际system SQLite为3.46.1；没有把另一个3.53.4编译header/CLI混称运行库，也没有把既有制品改称native重建。先完成唯一10k/hard512准入：10000完整typed bag，Σ87649、execution481773B、585parts、3907batches、4022414 private VM、实盘allocated peak3391488B；实际1GiB/no-swap，cgroup peak52178944B、OOM0，reset/recovery/readonly reopen及完整canonical检查全过，unit和owned PID均gone。初次r1是0worker的entry解析失败，原红保留；r2只修正解析接线，未改变worker/Oracle/资源条件。10k终态：`target/phase22-current/s5-native-openclaw-preparation-v2/admission-r2-raw/verified-summary.json`。

随后严格单slot完成6个full＋6个不同failure，共12/12 whole PASS；连同独立10k准入为13项，不与旧VM/QEMU cohort相加：

| 新代表矩阵范围 | 实际终态 |
| --- | --- |
| uniform100k、uniform1M、双侧100k热点各default/hard512 | 全部完整typed ID/key bag、真实Hash/Grace、精确PROFILE Σ；输出分别100000、1000000、400002行。hard512三项execution peak均481773B≤524288；1M有真实allocated341909504B。default不冒称hard quota |
| early-close、首行后interrupt、disk出现后build interrupt | 分别1行/reset0、1行/error-reset9、0行/error-reset9；没有success summary。build取消由实际disk inode出现触发；各自partial Oracle、恢复、live-worker FD0及清理通过 |
| active Resource64、active TEMP FULL | Resource18/reset18和FULL13/reset13；成功未发布setup后whole explicit abort，精确NoActive21/head-marker恢复。64B为plan N/A，不称Hash后内存耗尽；FULL是TEMP_DB VFS注入，不称物理盘满 |
| TEMP READONLY与caller-owned writer-hold | VFS code8映射private Resource18/reset18；真实caller `BEGIN IMMEDIATE`→`ROLLBACK`间隔与恢复通过，不称OS ACL拒绝、纯writer wait或Engine内部持锁 |

每项实际memory.max=1GiB、swap.max=0、OOM/oom_kill=0；1M/default的cgroup峰值达到1GiB、max事件1856如实保留，不能把page cache/cgroup总量写成query heap或声称没有回收压力。每项完整文件SHA、head/sidecars/quick与seed canonical绑定；另有上述唯一10k canonical，不反复扫描同一65-Commit 1M全history。完整输出、失败原子性、资源及恢复合同均保留，代表矩阵不是减少行数或放宽预算。12项终态：`target/phase22-current/s5-native-matrix-r1/final-evidence-raw/verified-summary.json`。

全部owner/worker退出0并实际wait，37个已知PID均ESRCH、12个精确unit及cgroup路径消失、12组TEMP目录空；输入/tool pins前后相同。S1独立读取全部终态raw，核case顺序、typed bag/error/reset、Σ、disk/fstat、cgroup、NoActive/fault、host-hold及cleanup，未发现新增可执行P1/P2。closure：`target/phase22-current/s5-native-matrix-r1/final-evidence-raw/closure-proof.json`、独立验收Review：`target/phase22-current/s5-native-matrix-independent-acceptance-review.md`。这些是instrumented功能/资源证据，不跨Mac/QEMU比较wall，不构造P95。旧23whole＋1qualified仅历史；A原wholeFAIL及startup UNKNOWN/fork非因果fix均不改写。

#### 局部Acceptance与后续边界

| 条目 | 最终局部结论 |
| --- | --- |
| A22-S5-01 | PASS：真实无Index Hash正例、1k Before/After物理收益与四档完整消费；廉价identity INL和不适用fallback保留 |
| A22-S5-02 | PASS：typed key/NULL/numeric边界、collision、duplicate/Relationship bag、自环/方向与错误时序的unit、集成和双SQL语义通过 |
| A22-S5-03 | PASS：default及显式hard quota真实成功Grace、双侧重热点、完整typed payload、真实磁盘与故障恢复证据闭环 |
| A22-S5-04 | PASS：原生Linux 1GiB/no-swap下完整输出、cancel/early-close/Resource/TEMP失败、rollback/reopen及owned清理通过 |
| A22-S5-05 | PASS：INL/Hash mixed、两Hash stage、incoming/write256批build-once、GraphView/Access、historical/staged、普通/显式回滚及适用TCK通过；事务子查询保留合法fallback和既有durable batch合同 |
| A22-S5-06 | PASS：同源quality/CI、stock、性能非回退、当前代码与raw独审及文档/diff收口；只关闭S5局部Gate |

最终代码Review：`target/phase22-current/s5-r5-final-code-independent-review.md`针对R5的19个关键Rust文件及既有独立边界未见新增P1/P2；以上资源raw复核补齐其成稿时仍在途的证据。§3.2 remote bulk/helper的同源编译及2024 Nodes完整selfcheck、6 Commits/唯一CP/ready验证另有记录：`target/s6-fx-shared-bulk-r1/remote-1k-selfcheck-report.md`，仅为共用fixture准备，不当作S6算法完成。

R5的480/fe5与975/3a是**测试时历史epoch**。本地S6已开始修改源码，当前工作区不再冒称同一冻结输入；本次三个development文档同步也改变仓库inputs。新的实际membership/count/digest由收口记录另采；R5测试制品与独立CI快照保持其原source lineage，不能用在途S6代码冒充S5已测源码。远端仍使用旧epoch的独立fixture任务不由本次本地文档更新同步或改写。

**全Phase未完成**：S6实施中，S7–S8未实施；全算法混合与九组corpus、最终同机Before/After和10M/100M综合G03、1/4/8readers＋writer各30分钟共90分钟、独立Search tiers、最终跨平台CI及A22-G01–G06仍需后续完成。这里通过的Stage NR/规模子集不能替代全局验收。冻结Cypher Profile分母不变；提交、推送、分支CI、合并和发布按实际执行另记，不从本节局部PASS推导。

<a id="stage-s22-6-implementation"></a>

## 25. S22.6 实施记录（局部验收未完成）

### 25.1 已提交的S5基线与在途S6分离

2026-11-12，独立`codex/phase22-query-optimizer-ci`分支已提交并推送[`2854fcc901cd0b895c681d53c56a5221cbbf3f2e`](https://github.com/bYiyLi/Lithograph/commit/2854fcc901cd0b895c681d53c56a5221cbbf3f2e)。它来自已验收R5源码快照及三份development文档，不包含主工作区在途S6实现；编译源码仍为480/`fe5e02dc…`。原975/`3a324cbd…`仅是先前测试epoch，不能称包含后续文档或S6变更的当前inputs。

- [CI 43185249151](https://github.com/bYiyLi/Lithograph/actions/runs/43185249151)实际`completed/success`，Windows、macOS/Linux canonical、quality四个job全部成功；最后Linux canonical于2026-11-12 09:14:20 UTC完成。
- [Release Matrix 43185249033](https://github.com/bYiyLi/Lithograph/actions/runs/43185249033)实际`completed/success`：storage fixture及macOS/Linux/Windows各x64/arm64六个平台job成功，最后macOS x64于09:02:56 UTC完成。`publish-release`按分支条件skipped，没有发行；构建及运行验收结果不扩张为S6通过。

主工作区S6继续边开发边定向验证，未提交、未合入上述快照；远端保留的独立R5 fixture任务继续使用原epoch，不因本地S6修改被同步覆盖。S7/S8仅可继续只读准备，尚未启用实现。

### 25.2 首轮定向结果与未闭环项

以下是S6实施中的一次受控验证快照，不是Stage完成，也不覆盖S6完整质量、SQL/资源、四档压力及跨算法验收。测试目标为`phase22_sip`、`phase22_topk`、`phase22_dynamic_filter`；在途输入清单保存于`target/phase22-current/s6-active-01/`，不拿该清单本身替代测试结果。

| 路径 | 本轮实际状态 | 尚未证明或需继续处理 |
| --- | --- | --- |
| SIP | 定向10/10通过 | 只对应当前定向范围，尚未完成S6整体资源/性能与跨路径Gate |
| TopK | 定向14/14通过 | 不外推所有排序、Search或分页场景，也不代替真实四档工作量验收 |
| Dynamic Filter公开集成 | 9项中5通过、4失败；4项实际CBO仍选择INL | 保留原红；未取得这些case的目标DF生产路径，不把结果正确或INL名称当DF命中，也不为强选DF放宽成本/Oracle |
| 私有DF consumer | resident路径用例通过 | Grace路径尚无终态；late-quota的资源断言未闭环，均保持待验证，不能据resident成功标记整个consumer完成 |

后续继续按实际失败完成最小修复与受影响验证，保留廉价INL和安全fallback；不修改冻结Cypher Profile分母、不清除原红或减少资源/功能合同。S22.6保持实施中，S22.7–S22.8未实施，Phase22及最终G01–G06仍未完成；不因S5分支CI成功提前跳过后续Stage。

### 25.3 当前定向闭环与增量 CI 准备

本节续记第25.2节后的实际进展，首轮及中间失败raw继续保留，不改写为首次即通过。S6仍为`in_progress`，以下结果不覆盖尚未完成的四档性能/压力、完整quality/CI或最终全Phase验收。

| 路径 | 已取得的实际结果 | 当前边界 |
| --- | --- | --- |
| SIP | 11/11通过，包含rows计数由6修正为3的单行修复 | 不把旧计数错误或首轮10/10报告覆盖；只记录该次真实复验 |
| TopK | 14/14集成、3项private、5项storage、4项encoder通过 | 各组范围分别记载，不合并成所有排序/Search均已通过 |
| 真实SQL入口 | scalar与rows smoke均通过 | smoke不是完整分布采样、四档压力或S6 Stage Gate |
| Dynamic Filter公开集成 | 8个未改用例通过，正常CBO的64-row resident场景真实执行DF，first-page计数为1；修正后的低预算用例也已独立复验通过 | 1024-row/hard512KiB实际选择INL，完整1024行bag与341/342/341重数保持；原“必须DF”的错误成本期待及失败raw保留，尚未将分轮结果写成一次完整9/9运行 |
| DF相关units | `dynamic_filter`过滤的15项通过，包含长定义预算、derived missing、collector、late quota和严格Memory/Grace consumer | 内部强制512KiB Grace仍要求真实DF/spill与完整bag；它与正常CBO选择分开，不要求所有合法查询均选DF |
| Codec与Hash估算 | codec layout 2项、Hash工作量模型4项、Hash估算13项通过 | 逻辑编码流量与内存scratch分开，保留原预算headroom；输入清单为`target/phase22-current/s6-active-01/inputs-final-unit-test-only-04.json`，实际结果为`target/phase22-current/s6-final-units-04/`对应远端记录 |

当前原生Release已实际构建exit0；这是构建结果，不是S6性能、资源与全CI的替代。继续完成SIP/DF/TopK四档测量和全部局部acceptance，增量源码通过已授权CI分支验证；Profile分母、原资源预算、完整bag及失败/回滚边界保持。S6与整个Phase仍未完成，合并、发布和部署不属于本次交付。

### 25.4 S6 首次分支 CI、定向修复与四档压力边界

S6已在`codex/phase22-query-optimizer-ci`分支实际提交并推送[`05caab67ad03ba03f0e1cb5584f33a5bae3d7984`](https://github.com/bYiyLi/Lithograph/commit/05caab67ad03ba03f0e1cb5584f33a5bae3d7984)。以下续记第25.1–25.3节之后的结果，原历史及失败raw保持原样；S5旧提交的成功CI不能替代本次结果。

| 范围 | 实际结果与边界 |
| --- | --- |
| [CI 43198628829](https://github.com/bYiyLi/Lithograph/actions/runs/43198628829) | `completed/failure`：Windows成功；Linux/macOS canonical均在`phase22_hash_join`的`large_string_key_last_preserves_complete_empty_bag`失败，22/23通过，实际错误为execution memory limit的Resource/code18；quality在原1% duplication门槛失败，1308/129311重复行、1.01%。没有将其归因于已确认的基础设施瞬态 |
| [Release Matrix 43198629184](https://github.com/bYiyLi/Lithograph/actions/runs/43198629184) | `completed/success`：storage fixture及六个平台build/runtime/load/ABI job成功，`publish-release`按分支条件skipped，没有发行；该结果不覆盖上述canonical和quality失败 |
| 后续原生定向复验 | full Hash 23/23、SIP 11/11、新增admission 3/3、Dynamic Filter 15/15实际通过；只记录各目标的真实范围，不标记完整Stage通过 |
| 后续质量复验 | 同一`jscpd-rs 0.1.12`、原扫描范围和1%门槛实际exit0：156 clones、1275/129713重复行、0.98%；全部production严格C15/L100/a10、`cargo fmt --all --check`、all-target/all-feature Clippy `-D warnings`实际exit0，Clippy耗26.79s。修复提交和该提交的新CI仍待执行 |

SIP的旧R5四档Before batch在1M时实际达到1GiB cgroup限制，leaf与metadata worker被OOM kill；unit记录`Result=oom-kill`、signal9及MemoryPeak=1GiB，整batch保留failed。1k/10k/100k已完成的个别raw不计作whole batch通过，也不构造完整Before分布。缺少worker最终metadata的owner记录路径已最小修复，真实workload exit保持未知；原OOM raw保留，不重跑该OOM或提高内存限制。

`05caab6`的1M SIP After在scalar与rows接口均完整返回同一组三行结果，SQLite VM分别3315/2412，与1k相同；wait4 RSS为38596608B。cgroup峰值达到1GiB，其中包括SHA读取产生的page cache，实际OOM事件为0；TEMP目录空、unit/cgroup及已知PID清理通过。该小组完整结果不替代S6四档正常分布、全部资源/失败路径或Stage验收。四档数据已全部ready，完整性能/压力仍待完成。

另一次review发现SIP不适用时legacy eager fallback仍有无界Vec，已交由对应executor实现做窄修，目前尚未闭环；不能由SIP命中路径的资源证据推导fallback也符合预算。S6继续`in_progress`，S7–S8未实施，Profile分母与既定功能/资源合同不变。后续修复提交及CI、全Phase综合与最终门禁均按新源码实际结果另记。

### 25.5 修复提交的 CI 终态与新回退路径

第25.4节后的修复已实际提交并推送为[`7e15ffb273feedc99f0b03974ed4ef5da7e6da31`](https://github.com/bYiyLi/Lithograph/commit/7e15ffb273feedc99f0b03974ed4ef5da7e6da31)。[CI 43205581489](https://github.com/bYiyLi/Lithograph/actions/runs/43205581489)实际`completed/success`，Linux、macOS、Windows及Quality四项job全部通过；[Release Matrix 43205581628](https://github.com/bYiyLi/Lithograph/actions/runs/43205581628)实际`completed/success`，storage fixture及六平台全部通过，`publish-release`为skipped，未发行。旧`05caab6`的失败保留，不由后续绿灯改写。

同一原生Release候选上的SIP、Join Dynamic Filter、TopK均已完成1k/10k/100k/1M四档After普通时延采样、独立VM及PROFILE诊断，完整固定结果和外层owner检查均通过。SIP/DF的Engine预算为512KiB；TopK沿脚本默认512MiB。所有工作负载受原生Linux amd64的1GiB/no-swap限制，三类结果不合并成同一内存预算。对应普通时延P50/P95、物理工作量和Before对比仍须按各自报告闭环；旧R5 SIP百万节点OOM与TopK百万节点600秒受控超时保留为失败边界，不重复运行或提高限制，不编造成功baseline/P95。

新的plain单Node回退已用query-private TEMP分阶段保存候选及pattern结果，保持原pattern-before-WHERE求值顺序；旧输入上的CF8与SIP11定向测试通过。随后发现的真实dictionary错误被当作优化拒绝的问题，正通过共享lowering分类及实际入口one-shot AUTH回归修复。该新增量的源码、运行验证、百万节点回退压力和新提交CI仍在途；上述`7e15ffb`CI不覆盖它。S6继续`in_progress`，S7/S8尚未启用。

后续修复源码已在原生Linux完成139项定向测试：completeness 29、实际builder 1，以及access 19、DF 9、INL 19、Join semantics 16、Relationship 12、single-node fallback 9、SIP 11、TopK 14，均无失败。它们包含三个helper、实际builder及实际program planning共5个首次dictionary AUTH回归、SIP预算/分配后取消、回退中途取消和同连接恢复。首次编译因辅助函数提取后的`unused_mut`失败，已删除该无用mut并保留原失败；修正后全组通过。相同源码Clippy all-target/all-feature `-D warnings`、format、严格Lizard及原1% duplication gate均通过（后者显示1306/130138重复行、1.00%、exit0），输入前后不变。新Release构建、百万节点非SIP回退资源测试与新提交完整CI仍在途，不能把本组结果称为S6已验收。

新增量已提交并推送为[`2df1ee715b9d8e6d440558455d0c9facc8e35807`](https://github.com/bYiyLi/Lithograph/commit/2df1ee715b9d8e6d440558455d0c9facc8e35807)，触发[CI 43213444853](https://github.com/bYiyLi/Lithograph/actions/runs/43213444853)及[Matrix 43213444939](https://github.com/bYiyLi/Lithograph/actions/runs/43213444939)。Linux/macOS canonical在既有`phase22_predicates::incoming_pattern_bindings_keep_errors_before_index_and_early_where_filtering`真实失败：末段无WHERE的Fan LabelIndexScan新增候选落盘/逐ID验证，dbHits由期待16变为实际32，15/16通过；不修改断言或隐藏真实额外读取。Windows canonical已通过，其他job按终态另记，不能称本提交CI通过。

相同源码的原生Release已构建成功，随后plain单Node非SIP的1M/512KiB测试真实返回Resource/code18（CPU约8.34s、wait4 RSS41250816B），未发生OOM，fixture/manifest/artifact/sidecars保护及unit/cgroup/PID/TEMP清理均通过；该结果仍为失败。1k相同查询也复现；新增永久hard512回归证明128候选完整成功、1000候选失败。固定256个候选的BindingRow保留开销本身已超过524288B，正在按实际剩余预算、候选行及解码workspace改为有界页；同时收窄无WHERE场景的回退准入，避免无收益的额外扫描。首次压力launcher曾因错误DB路径在打开DB前退出，按已有成功invocation修正后才运行上述真实查询；原失败均保留。S6仍在修复验收，未进入S7/S8生产实现。

预算分页与无WHERE准入修正已完成原生定向复验：CF10、完整predicates16（保原dbHits16）、SIP11、codec2全通过，另一个两property成员的hard512小例独立通过；Clippy `-D warnings`、严格复杂度及原dup门槛保持通过。该候选原生Release的1M hard512重测仍实际Resource18（CPU约8.64s、RSS41893888B），同候选的原frozen1k也复现；全部输入保护与清理通过而查询失败，不能由自建小例绿灯关闭S6。当前继续用小frozen fixture定位实际失败阶段，不提高预算或重跑未改动的百万用例。

`2df1ee7` CI终态为failure：Linux/macOS及Quality coverage均因相同predicates回归失败，Windows canonical成功。其Matrix另有Windows x64的独立Provider并发publish失败：`concurrent Provider publish failed: result=cache database operation failed`；已证明两worker都通过HTTP barrier，失败范围在HTTP后的cache发布，具体SQLite根因未知。新增固定publish begin/body/commit/rollback阶段及SQLite primary/extended code诊断，保持原Provider分类、500ms/10ms重试、两个worker、HTTP barrier和3-entry/24-byte断言；测试记录两侧安全结果，不回显config/SQL/path/secret。原生Provider25项unit、support编译与Clippy通过，其中新增构造错误单测仅证明诊断分类与去敏，不冒称真实Windows复现或故障修复。新提交CI与剩余回退问题继续按实际结果验收。

### 25.6 Node-only 回退修复与百万节点资源结果

后续将候选及matched TEMP都限制为现有codec的单Node binding；每个outer的原绑定只保留一次，完整pattern结束后才在预算内恢复原绑定、order/CSV context并执行原WHERE。没有引入新的因子化IR、公开配置或codec。原frozen1k的私有只读诊断先精确定位`spill_matched_record`首次写入失败（候选1000已完成、matched0、live150856B），修正后同一fixture/Commit/SQLite和524288B预算实际完整通过。该诊断与验证仅存在ignored disposable源码，正式实现不含tracker、临时日志或新ABI。私有验证脚本最初在仍持有QuerySummary的72B正常lease时提前断言used0；已改为验证完整结果后释放summary再断言归零，原脚本失败记录保留，未改变生产释放行为。

当前正式实现的CF11、SIP11、完整predicates16实际通过，原dbHits16断言不改；shared-pressure永久回归从Resource失败变为完整成功。Clippy all-target/all-feature `-D warnings`、format、严格复杂度及原dup门槛通过（1302/130304重复行、显示1.00%、exit0）。同源原生Release的SHA256为`28de76577f69c114583b04b2cd561788a9de9b021d7eba63a8f8eaae864b0fd7`，测试输入1002/`608132d6995c77b65392d5a3f13f0af088e644c5914d2a1c77272d911fe78778`在前后检查相同。

| 实测范围 | 结果与边界 |
| --- | --- |
| SQL rows，原immutable 1M Probe，非SIP的`(a IN bs) OR false`，Engine524288B，Linux amd64 1GiB/no-swap | 完整三行typed bag、summary3、最终Project3、LabelIndexScan消费1000000行、无semiMask、dbHits求和通过。真实leaf/worker/controller均exit0；wait4 RSS41803776B，CPU user98.417142s/sys1.546149s；OOM事件0。此次单次PROFILE是功能/资源证据，不构造P50/P95 |
| 资源与恢复 | 实际TEMP落盘；cgroup峰1073741824B含文件页，与RSS分列。unit/cgroup路径消失，两已知PID实际ESRCH，TEMP空、连接关闭；fixture/manifest/artifact/sidecars前后不变，Source1002项postguard通过 |
| SQL scalar，原frozen1k，同Release/同query/Engine524288B/同外层限制 | 完整三行与PROFILE/summary/访问行数检查通过，真实controller/leaf成功及清理通过；该smoke不替代rows百万资源或四档普通时延 |

上述结果关闭本次已复现的回退资源问题，不改写旧Resource/OOM/timeout。`b380324`的新Windows x64 Matrix gate已实际成功，新增Provider诊断unit25项及support/check/Clippy通过；旧`2df1ee7`的并发publish一次失败仍保持根因未知，不将这次绿灯写成已证明修复。Node-only修复的精确新提交完整CI/Matrix与S6最后证据收口仍需完成，S6继续`in_progress`，不以资源子集通过进入S7/S8生产实现或将Phase标done。

<a id="stage-s22-6-acceptance"></a>

### 25.7 S22.6 最终局部验收与 S22.7 启动

`6d122ca5f98fda15afd3b2fba9d8fea5ebdb9316`的[CI 43221810009](https://github.com/bYiyLi/Lithograph/actions/runs/43221810009)已实际`completed/success`，Windows、Quality、macOS、Linux canonical四项全部通过，最后Linux于2026-11-12 13:26:27 UTC完成；[Release Matrix 43221809736](https://github.com/bYiyLi/Lithograph/actions/runs/43221809736)亦实际`completed/success`，storage fixture＋六平台共7项成功，`publish-release`为skipped，无发行。原`b380324`两条运行也独立成功，未拿它们替代6d。

| Acceptance | 结论与证据边界 |
| --- | --- |
| A22-S6-01 | PASS：SIP11、Node/elementId/null/重复/权限/history/staged反例、真实3 keys/3 seeks及两SQLsurface四档固定结果/VM；未命中路径不假报mask |
| A22-S6-02 | PASS：公开DF9＋private15与mixed-plan反例；完整typed bag/异常/状态/权限保持，完整Build EOF后keyset真实约束未读尾部；1024/hard512正常CBO仍可选INL，不强迫DF |
| A22-S6-03 | PASS：TopK14、private/storage/codec边界与四档完整20行；只在既定Range排序/可见域/类型/唯一性证明下读取skip+limit=277，其余保原Sort/Search合同 |
| A22-S6-04 | PASS：query-local预算、分配后取消、旧pin/staged更新与真实AUTH错误传播；原frozen1k、1M rows hard512/1GiB/no-swap及scalar1k通过；独立资源review核实实际TEMP/FD、RSS、wait/reap及unit/CG/PID清理，无OOM |
| A22-S6-05 | PASS：SIP/DF/TopK四档完整普通采样及10×物理工作检查；源7e15性能epoch明确保留，后续相邻变更由6d受影响target/资源/全集回归承接。Before OOM/受控timeout及DF首行增加/fullscan+3均保留，不伪造分布或全指标改善 |
| A22-S6-06 | PASS：6d精确完整quality、canonical/TCK/SQL/Provider及六平台load/runtime/ABI；源码与资源独审、最终diff和文档状态收口。旧Provider一次未知失败保历史限制，未宣称诊断提交为因果修复，也不新增连续N次CI门槛 |

当前compiled输入504/`3a6eeb9106e5c093a6b0bc5c92adf9f32b12615a0b920296983ab41ed5022ce4`，提交输入1002/`81d4a49af46b15aae6f8762ea9453f7b7403769b8e1def3f4600fce336e273db`；百万资源测试时1002/608 epoch与此仅Phase记录不同，编译代码相同。独立收口映射为`target/phase22-current/s6-final-acceptance-map.md`，资源review为`target/phase22-current/s6-identity-spool-final-01/resource-review.md`，精确提交输入为`inputs-commit-6d122ca.json`。当前Stage没有剩余已确认blocking finding。

S22.6局部通过后进入S22.7实际开发：在原pipeline引入可预算的Flat/Group、共享FT及延迟Flatten，完成group-aware消费和真实Accumulate→Semijoin-filtered Build→Probe；当前尚未实现验收。已有旧S6引擎的独立小Star Before完整32768行、原查询/pin、真实CBO、VM及资源记录，不把它称为S7实现或P95。三域Search仅完成Native数据/CP/definitions/完整Oracle与integrity准备，查询/物理build/recall/性能仍待全局Gate。S8、全算法综合、10M/100M、90分钟并发、最终Search tiers与A22-G01–06均未关闭，Phase22继续`in_progress`。

<a id="stage-s22-7-checkpoint"></a>

### 25.8 S22.7 开发检查点：真实 Group 与待通过的候选

S7 已在原 pull pipeline 接入 Flat/Group、紧凑 FactorTable、分组过滤/投影/COUNT/MIN/MAX、延迟 Flatten 与复用原 Hash/Grace 的 ASP pipeline。字段名与顺序在 arm 内共享，typed 成员可预算驻留，溢出继续使用原 private Binding codec；没有新增公开 hint、预算配置或第二执行器。此处记录开发中的真实状态，S7 与 Phase22 仍为 `in_progress`。

原生源码11为1039个输入、`caa8c497bf1dd864ac4278dfba5b2120ee8e17eedbe2ec126def5ea79dcddd2b`，其中编译输入541个、`ae007dcc4199351aa80b5d5fabfe99eeb38ea0668f5e2c9ad0472d69f5b19aac`。Core/lib与集成目标真实编译通过；Mac仅编辑和静态检查，运行使用原生Linux。该源码在1GiB/no-swap中的实际结果：

- `phase22_factorized`：21/22通过，覆盖完整32768五ID/三跳七ID bag、并行关系、别名与丢列重数、nullable聚合、逐行非确定性、错误/权限、历史pin和写barrier/rollback。四跳正例仍因CBO未选Group而失败，完整bag与实际算子断言保留。
- `phase22_asp`：1/2通过，NULL边界通过；256行正例尚未选择ASP。执行器与真实有限Build/Hash桥存在，不以旧INL结果或三段名称代替尚未通过的优化验收。
- private Factor tests20、Planner tests23、conditional-prefix回归1及真实64-entry Build首输出前取消1全部通过。取消仍检查`built=false`、probe0、真实profile token恢复、Σ与资源归零。
- worker最大RSS235552768B，OOM0，TEMP空，源码前后guard通过。source09的Hash23、SIP11、谓词16、Dynamic Filter9、TopK14共73项受影响回归通过；后续普通Group/PROFILE修复由source10的17项集成及21项private测试承接，未拿旧源码绿灯冒充source11完整CI。

真实Star已选择`FactorizedAccumulate`：A128/B256共384个FT成员，观察到owner峰值145014B、spill0，实际Flatten32768。全局分组聚合真实产出一行，未进入Flatten时展开计数为0。Σ少1已按bound-start Node验证的真实Adjacency边界修复，未补造dbHit。

新1k pressure fixture的公开CREATE Oracle、checkpoint、完整integrity及Before scalar/rows各普通1次与PROFILE/VM1次均通过，四次完整消费32768行。新库另含路径辅助域，不继承旧fixture时延、VM或pin；这些少量功能样本不构成P95或quiet性能验收。四档、巨大输出、小预算与取消的原生测量工具已具备，After release候选和剩余选择问题继续验证。

本增量用于独立`codex/phase22-query-optimizer-ci`分支的开发CI；`6d122ca`仍是上节S6验收的精确提交。新commit/push和CI以实际Git/Actions记录为准，不由本检查点预先声明成功。S7尚需关闭四跳/ASP实际选择与净收益、完整资源/两SQLsurface/quality/CI；S8、全算法综合、10M/100M、Search tiers、90分钟reader/writer及A22-G01–06仍未验收。

<a id="stage-s22-7-selection-fix"></a>

### 25.9 S22.7 选择修复、原生回归与 SQL 持续集成（2026-11-13）

第25.8节的source11失败保留为历史。本次固定源码为1044个输入、SHA-256 `e80913a5121a0465a0d899209e4f9bab80aef03a115f545fb06246b17635fe19`；编译输入已逐文件核对到实际提交并推送的`eb841fe23f7ae150ca4acc4d333ce5199441c2ec`，不把随后新增的Python验收/文档当作已包含在该提交。Mac只做编辑、格式和差异检查，构建及运行通过Nexum连接既有`openclaw`原生Linux执行，没有启动虚拟机或Codex任务。

本轮修复与Review：Factor planner在同一有限PlanningBudget内比较两个已证明安全的arm方向，修正四跳方向成本；小型typed producer的端点加权degree采样有工作/内存上限，部分样本保持unknown upper，不以采样估计过滤结果；ASP complete-ID set对迁移重叠、read page和private TEMP采样记录真实资源。新增诊断只在`test-support`中统计成功BindingRow复制、accounted footprint和predicate转换，不增加正常构建的检查/预留，也不把accounted bytes解释为allocator实测字节。原始失败、修复及源码输入记录保留在`target/phase22-current/nexum-s7-01/`和远端同名evidence下。

| 当前固定源码检查 | 实际结果 |
| --- | --- |
| Core lib | 599/599 PASS |
| Factorized / ASP | 22/22、2/2 PASS，包含四跳和真实ASP选择正例 |
| Index Nested Loop / Join Order / Join Semantics / Unknown | 19/19、4/4、16/16、2/2 PASS |
| Hash / SIP / Dynamic Filter / TopK | 23/23、11/11、9/9、14/14 PASS |
| Statistics / Relationship / 独立Extension invocation | 4/4、12/12、27/27 PASS |
| 工作区Clippy / Mac rustfmt / staged diff | 全workspace/all-targets/all-features Clippy、`cargo fmt --check`及`git diff --check`通过 |

以上共764项原生回归运行于`MemoryMax=1073741824`、`MemorySwapMax=0`，所有进程exit0，无ignored测试、超时或OOM；最大子进程RSS236593152B，cgroup峰值1016688640B（含该组收费的cache等，不与RSS混用），TEMP最终为空，源码及可执行文件前后SHA guard一致。证据为远端`evidence/nexum-s7-fix08-runtime/{runs,result,executables}.json`。正常release扩展SHA-256为`3c576f308a7722f41d84436f8bde2366805061202aecc46f9c7a1660116e85c4`，同源checkpoint工具为`b44fef62766f81ff3a48127ecfd1579438da489c9071b63a937cb73ec2835ca0`；各自独立Cargo invocation及receipt保留于`evidence/nexum-s7-fix08-release/`。

新增`tests/phase22_factorized_sql.py`并接入`scripts/ci.sh`：公开CREATE结果生成独立Node/Relationship bag oracle，再由checkpoint helper构建派生索引；真实stock SQLite 3.46.1上，scalar/rows分别完整消费Star32768行和ASP256行，校验实际Group/Flatten/ASP物理计数、重复关系、NULL重绑、聚合不提前展开、权限/Graph View、提前关闭/Interrupt后的同连接恢复、staged可见性/abort和历史pin。1GiB/no-swap实际执行exit0、OOM0、TEMP清空，结果与工具pin见`evidence/nexum-s7-sql-02/result.json`。首轮`sql-01`把写连接内部未提交chain当作durable Branch而失败；已使用S6与当前扩展复核这是测试观察面错误，修正为第二只读连接的head/history/完整bag不变及public `summary.commit=null`，并保留abort后内部history完全恢复断言，不删除事务隔离验收。

`eb841fe`的[CI 43338418369](https://github.com/bYiyLi/Lithograph/actions/runs/43338418369)及[Release Matrix 43338418269](https://github.com/bYiyLi/Lithograph/actions/runs/43338418269)已触发，记录时仍在运行，不能标PASS。此处未完成四档/大输出/净收益、小预算及完整quality/CI的S7收口，S22.7和Phase22继续`in_progress`；S8及G01–G06未关闭，未合并、发布或部署。


<a id="stage-s22-7-demand-and-coverage"></a>

### 25.10 S22.7 四档/资源实证、部分消费修复与统一聚合路径（2026-11-13）

本节记录随后完成的开发证据，不覆盖第25.9节的源码身份和原始失败。`60092e4`已提交真实SQL生命周期门禁，`7858bdbb62dda0aa5e412dc84577e7bb1777ba14`已提交并推送去重后的共同proof/packet drain/单节点与SIP生命周期；两者均位于独立开发分支，未合并或发行。`7858bdb`的[Release Matrix 43343587278](https://github.com/bYiyLi/Lithograph/actions/runs/43343587278)六平台与storage fixture通过、publish跳过；[CI 43343587167](https://github.com/bYiyLi/Lithograph/actions/runs/43343587167)的quality失败原因是`stream/project.rs`每文件行覆盖26.89%，低于原50%门槛，并非complexity失败。该轮总regions/functions/lines为86.09%/87.56%/89.30%；不以整体通过掩盖单文件失败。

#### 正常发布构建的四档与资源结果

固定`7858bdb`的普通release扩展`fc9a62ec0175a3fdf4a0c00034bcb05b0428bc549226a3f4abbf49022ca690cb`，与冻结S6扩展`28de76577f69c114583b04b2cd561788a9de9b021d7eba63a8f8eaae864b0fd7`在原生Linux同机、相同SQLite/pin/完整CREATE identity oracle上比较。四档为1k/10k/100k/1M无关节点，真实总节点/关系含各case辅助域，以各manifest为准。Star/三跳/四跳各32768行，scalar/rows各3次普通完整消费及独立PROFILE/VM；这是长流median/max，不报告P95。ASP各256行，两SQLsurface每档采用5连接×(首个冷样本+20warm)及100fresh连接，PROFILE/VM独立，不混进普通分布。全部输出bag一致，四档物理工作与fixture guard通过，详细结果在远端`work/nexum-s7-pressure-01/`、`work/nexum-s7-pressure-02/`。

ASP 1k rows warm完整消费median从85.069ms到8.356ms、P95从89.188ms到8.958ms；首行median却从1.813ms增加到5.019ms、P95从2.064ms增加到5.466ms。这是真实Accumulate的首行成本，不能宣称所有延迟改善。随后发现`LIMIT 1`仍支付整组累积，实际dbHits从旧路径145增加到672；该明确回退进入下面的修复，而不是把完整输出收益解释成短查询收益。

在1GiB/no-swap条件下，百万无关数据上的Star/三跳/四跳与ASP通过512KiB query budget的完整rows消费和恢复；大输出fixture完整消费1048576行，也在同一512KiB预算通过，worker RSS峰值40148992B、cgroup峰值43233280B。ASP与大输出的early-close/Interrupt及恢复通过。17组测量的真实子进程exit0、source/artifact/fixture/sidecar guard不变、OOM事件为0、TEMP为空、unit/cgroup及所属PID清理完成；资源结论来自worker退出前的`cgroup_final_before_exit`及`results/postflight.json`，不是systemd移除后读取到的ENOENT。部分百万图测试的cgroup峰值达到1GiB并触发cache reclaim，不能把这一指标等同于低得多的进程RSS；没有OOM。汇总为`work/nexum-s7-pressure-02/verified-runtime-summary.json`。

#### Review后的实质性修复

- **部分消费成本边界**：原成本模型证明的是完整MATCH消费。增加私有clause消费证明，普通LIMIT及不确定的WITH/RETURN管线保留已有流式Join；已有真实Sort屏障仍可采用全量成本。覆盖literal/parameter LIMIT0/1/10、SKIP、WITH LIMIT、完整bag子集、零limit零访问和排序正例，不引入公开hint。原`count(*) LIMIT 1`已经走eager Program fallback，保留其结果和边界，不伪造它之前会选ASP。
- **不可达重复聚合路径**：现有`requires_program`已将所有aggregate函数路由到Program/ProgramStream，旧read cursor的count-only分支不可达。删除这套重复实现及对应状态/标记，而非构造生产不可能创建的PreparedQuery来抬高覆盖率。分组COUNT/MIN/MAX、错误及不适用时逐行回退仍由统一Program负责，新增真实parser/planner/cursor回归覆盖空/非空输入、NULL、distinct/group/order及终端SKIP/LIMIT。
- **完整复制计数**：首次诊断只记录cooperative copy而漏掉普通`BindingRow::clone`，不能用于宣称S6复制为0。改为仅在`test-support`中对两条成功整行复制路径采用同一hook；正常构建保留derive(Clone)。计数不增加control调用/预算预留、不吞掉执行错误，深复制独立性及不重复计数均有测试。计数bytes是reservation footprint估计，不是allocator实测内存或所有Value复制总量。

`lithograph-phase22-factor-work`使用公开CREATE固定oracle、全identity tuple位图而不是首条关系ID去重，分别完整运行普通和instrumented消费，验证两者结果/计划一致及fixture不变；不把诊断耗时混入latency。S6与`7858bdb`分别添加相同的test-support-only计数hook后，Star成功整行复制33025→391、三跳33153→519、四跳98689→1031、ASP1289→206；对应受控footprint总量分别为309434224→779870B、523478000→2578654B、1985586160→7394526B、17033000→811144B。Boolean/null predicate转换尝试也减少，但不等于所有表达式求值次数。小Star的总query resource peak约190877B→303160B，保留这一开销而不声称所有峰值降低。八次完整bag、enabled/disabled结果和plan、源/可执行pin、资源释放及1GiB/no-swap运行通过，精确计数与SHA见`evidence/nexum-s7-copy02-runtime/`。

#### 当前候选验证边界

实现候选为1049输入`c1d193513ac3b1d21eaee0dfeeee663d4b5b7ae18c3e920e66ccdfc5935607e3`：Core原600项通过，新增聚合测试因复用已有3节点fixture却错误期望空图而失败。随后只更正该测试为实际count=3并加入独立Missing-label空输入，冻结为`ca8137ef03a2eeed608b934825f5d13177354d2f2c09ffe116119061b96363bc`，生产编译代码无变更；该新增测试实际复验通过，不抹去旧red。110项关联/Extension测试通过，其中ASP5、Factor22、SIP11、fallback11、Join Order4、TopK14、Join Semantics16、Extension27。完整workspace Clippy和本机format/production complexity/duplication/doc链路/diff通过。

普通release扩展`7ff6bf1ded6becfa6a0d13a3d50f3efdbfe2f143abbc3bb105fdd992f8b066ec`及同源checkpoint工具`f445707c4e29b6f06bfb18c67b1a668a116a77f7e7cd59dd74e0890595c224a6`通过扩展SQL gate，包括上述新部分消费正反例、原32768/256完整bag、PROFILE、权限、取消、staged/abort/history和integrity，RSS86470656B、OOM0、TEMP为空。该普通release与最终测试修正候选仅cfg(test)数据预期不同。当前尚需最终完整CI/coverage、新源码受影响性能复验及S7收口；S7/Phase22仍为`in_progress`，S8及G01–G06未完成，未发布或部署。


<a id="stage-s22-7-acceptance"></a>
### 25.11 S22.7 最终局部验收与 S22.8 启动（2026-11-13）

最终实现已实际提交、推送到独立开发分支`codex/phase22-query-optimizer-ci`，Commit为`34da2d0bf3792d8667b3f939ed73cd45d4580b11`。主工作区与该提交全部1049个tracked文件逐字节相同；后续S8修改另计，不覆盖这一验收源码身份。[CI 43351052373](https://github.com/bYiyLi/Lithograph/actions/runs/43351052373)的quality、Linux、macOS、Windows四项全部成功；[Release Matrix 43351052466](https://github.com/bYiyLi/Lithograph/actions/runs/43351052466)的storage fixture与六平台构建/load/ABI门禁全部成功，publish-release跳过。quality的regions/functions/lines为86.24%/87.66%/89.45%，原每文件门槛保持不变；TCK为3897 total、3777 applicable/pass、0 failed、120既有不适用分类，没有修改Profile分母。

最终普通release `7ff6bf1ded6becfa6a0d13a3d50f3efdbfe2f143abbc3bb105fdd992f8b066ec`的受影响性能复验位于远端`work/nexum-s7-pressure-03/`。1M无关节点上的Star/三跳/四跳scalar及rows分别完成3次普通完整消费与独立PROFILE/VM，dbHits分别为2049/2433/3969；ASP两surface各205次普通完整消费及3次独立诊断，dbHits672。所有typed bag、pins、guard及清理通过。大输出rows完整1048576行，三轮普通消费中位31.586s，相同现场S6中位81.304s；dbHits11265对5250048。该轮query预算是512MiB，不将它改称512KiB资源验收，也不把三样本当P95。

为绑定最终生产代码，仅补两组512KiB资源验证，未重复既有四档/205次时序。远端`work/nexum-s7-pressure-04/verified-runtime-summary.json`（SHA-256 `8cefbcd9ccbd5be5a7214b7aa3c0f8b7446a293d98657c7484e4d6a26d14653f`）记录普通及PROFILE完整消费、同handle恢复、独立PROFILE/VM、全部冻结输入与清理。ASP百万无关图完整256行、dbHits672，RSS峰值42213376B；大输出完整1048576行、dbHits11265，RSS峰值41811968B。两组实际1GiB/no-swap、controller及worker exit0、reset0、退出前OOM事件全0、TEMP为空、unit/cgroup/所属PID均清理。ASP的cgroup峰值达到1GiB并有cache reclaim，不能与进程RSS混同；大输出cgroup峰值33325056B。receipt记录full source `c1d19351…`、compiled input `8051a7e2…`和与34da生产代码相同、只含cfg(test)预期及文档差异的来源链。

| 局部Acceptance | 实际结果 |
| --- | --- |
| A22-S7-01 | PASS：第25.10节的四档Star/2–4hop完整identity bag、复制/footprint及受影响谓词计数改善；明确保留小Star总ledger峰值增加、ASP首行成本和旧失败。最终普通release的受影响性能复验通过。 |
| A22-S7-02 | PASS：真实Accumulate→可寻址semijoin Build→Hash Probe减少工作且保全部parallel bag；不安全/成本不合算与部分消费回退。生产accumulator八行后、Hash Build读取64个Node后且probe0、私有ID insert/page的取消测试通过；静态review核实同步owner错误展开及顶层cancel/clear释放，无后台完成路径。公共SQL lifecycle只证明首行后close/interrupt，不冒称公共pre-probe集成实测。 |
| A22-S7-03 | PASS：Factor22、ASP5与Join/Hash/SIP/TopK、聚合及写入/事务相关语义测试；完整multiplicity、类型/NULL、权限、历史/staged及失败边界保持。 |
| A22-S7-04 | PASS：原四档和17组资源报告，加最终普通release两组512KiB完整输出/恢复；1GiB/no-swap、OOM0、TEMP及owner清理，不用count替代完整结果。 |
| A22-S7-05 | PASS：两SQLsurface/PROFILE、close/interrupt、历史/staged、Graph View/Access、Provider与事务完整门禁；精确34da quality/CI/TCK及六平台Matrix、独立Phase范围review和最终diff通过。 |

S22.7局部验收通过后启动S22.8：复用既有版本化typed directed邻接覆盖索引，先补neighbor下界seek，再实现成本选择和pull多路交集；不新增持久索引或公开hint。S8仍须完成自身全部验收，10M/100M、全算法混合、最终Search tiers、90分钟reader/writer及A22-G01–G06仍未关闭，Phase22保持`in_progress`。没有合并、发布或部署。

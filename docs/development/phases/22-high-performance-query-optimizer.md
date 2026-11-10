# Phase 22：高性能查询优化器与可预算执行引擎

**状态：`ready`（计划已完成，源码未实现）。** 本项目只有**一个 Phase 22、八个内部 Stage**。所有 Stage 和全部算法均为必交付；只完成部分 Stage、已有若干基础、计划通过 Review，都不能标记 Phase `done`。

**当前基线（2026-11-10）**：本地 `main` HEAD `e245555`，Phase 21 已在未提交工作区完成本地验收，但未 Commit/Push/Release。实施时重新核对 HEAD/dirty tree、源码/SQLite runtime，不把 v0.3.4 旧 Release 误当包含 Phase 21。**此文档只定义开发及验收，不代表 Phase 22 代码存在。**

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

## 2. 八个 Stage 的依赖关系（非独立 Phase）

| Stage | 完整交付 | 直接进入条件 | 当前开发状态 |
| --- | --- | --- | --- |
| **S22.1** | Planner Access/Cost candidates + Budgeted Executor | Phase 21 真实实现复核 | 未开发 |
| **S22.2** | Tiered Checkpoint + Snapshot 成本控制 | S22.1 局部验收 | 未开发 |
| **S22.3** | Versioned Statistics + Relationship ID Seek + Predicate/Scan | S22.1–2 | 未开发 |
| **S22.4** | DPhyp/Greedy Join Order + Index Nested Loop Join | S22.3 | 未开发 |
| **S22.5** | Typed Grace Hash Join + TEMP Spill | S22.4 | 未开发 |
| **S22.6** | Semi-mask/SIP + Join Dynamic Filter + TopK Pushdown | S22.5 | 未开发 |
| **S22.7** | Factorized Execution + ASP-Join + 精确 Flatten | S22.6 | 未开发 |
| **S22.8** | WCOJ / Multiway Intersection + 全算法集成 | S22.7 | 未开发 |

各 Stage 可独立完成**局部测试**后推进，但只有最终所有 Stage 的 Acceptance 加上统一全局门禁同时通过，Phase 22 才能标为 `done`；不会再创建 Phase 23–29 的实现状态。

## 3. 开发前准备与统一基准

### 3.1 真实源码入口核对

1. `query/plan.rs`、`plan/build.rs`、`plan/predicate.rs`、`query/stats.rs`、`query/stream/`、`query/mutation/`、`query/completeness/`：现有 AST lowering、Logical/Physical Operator、Read/Write、NodeIdSeek、PROFILE 与预算。
2. `storage/snapshot.rs`、`snapshot_stats.rs`、`checkpoint.rs`、Commit/GC、`query/schema/index.rs`、SQL `lithograph()` / `lithograph_rows()`：实际 Versioned access、Index、维护写入与 staged revision。
3. 当前 Git HEAD 与 dirty-tree digest、Phase 21 未提交代码/测试、Cypher 25 inventory、Phase 11/21 基线、真实 SQLite 版本/配置；区分已发布资产与当前工作区。

### 3.2 共用确定性 Fixture

| Fixture | 数据与工作负载 | 主要目标 |
| --- | --- | --- |
| **FX-ACCESS** | 1k/10k/100k/1M Node/Relationship，固定目标 0/1/多行，有/无 Index | Seek/Scan、VM_STEP/FULLSCAN_STEP、无关数据扩张 |
| **FX-HISTORY** | 同终态 10/100/1k/10k Commit、不同 Delta/Branch/GC、staged CREATE/DELETE、有/无 Checkpoint | Snapshot 重建、维护开销、并发/重启 |
| **FX-ESTIMATE** | 均匀/偏斜 MCV/NDistinct、稀疏/稠密 Label/Type、degree skew、缺统计 | Cost 估算、Index/Scan 选取与回退 |
| **FX-JOIN** | 3–8 连通 Part、20+ 极端复杂 Part、不同书写顺序、重复 key | DPhyp/Greedy、Index Nested Loop、Planner 预算 |
| **FX-HASH** | 大型 equijoin、碰撞、重度倾斜、NULL/混合类型、大于预算 | Grace Hash、真实 TEMP Spill、rollback |
| **FX-SIP-TOPK** | 少量候选/大量非候选、索引可跳读/不可跳读、同分 cutoff、Graph View/ANN | SIP、Join Dynamic Filter、TopK 正确剪枝 |
| **FX-FACT** | Star、fan-out、多对多 2–4 hop、巨大扁平中间结果、排序/写 | Factorized、ASP-Join、multiplicity、Flatten |
| **FX-WCOJ** | Triangle、4-cycle、多环、自环、parallel Relationships、方向/Type | Multiway Intersection、Path/Relationship bag |
| **FX-CROSS** | SQL scalar/rows、OPTIONAL/Apply/subquery、Graph View/Access、historical/staged、Search/Provider | 跨 Stage 结果、权限、异常与资源回归 |

Fixture 保存 seed、真实 cardinality、目标 Commit、Index/Checkpoint、Graph View/权限及预期逐行输出；不得用 `count(*)` 或无真实输出的快捷路径替代长流完整消费；不会为了跑旧版大规模 Cartesian 而故意制造不可控 OOM。

### 3.3 共用性能/资源/语义门槛

- 分开计时 parse/plan、Snapshot resolve、Index/Checkpoint cache build/lookup、first-row、full-drain、SQL adapter、writer-hold。Core/SQL rows 和热态/新连接/真正 OS cold 分开；热态≥5 connections×20 次、新连接首条≥100 次、长流≥3 次逐轮记录。
- 记录真实 Physical Plan 与 estimated/actual rows、SQLite VM_STEP/FULLSCAN_STEP/SORT、Index/Adjacency Seek、Join 中间候选、Hash spill、SIP skip、Factorized bytes、WCOJ intersection。`EXPLAIN` 的名称或逻辑 dbHits 不能代替实际工作证明。
- 固定结果与相关 Delta，10×无关图增长保持 Runtime 的可寻址路径门槛 `work(10×) ≤ 2×work(base)+1000 VM steps`；无 Index 的 Scan 允许候选域线性开销，不得额外产生平方中间行。
- 所有高内存/Spill 阶段在**真实 Linux amd64、1 GiB/no-swap** 限制下测 `memory.max`、`memory.swap.max`、`memory.peak`、`oom_kill`、RSS、TEMP/WAL、cancel/early-close/rollback/reopen/quick_check。
- 全阶段继续遵守 Runtime 的 10M Node/100M Relationship release-tier、写入/History/Diff/Merge 非回退和冻结 Cypher 25 Profile。每算法至少一项真实物理性能收益、一项不适用安全回退，检查行重复、NULL/TYPE_ERROR 时序、Graph View/Access、Path、ANN recall、Write 原子性。

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

#### S22.1.3 单一 Executor 资源合同

- 检查当前 `QueryResources` 对 row/temporary buffers、operator continuation、profile、batch、spill 和资源释放的覆盖；补齐后续候选算子必需且当前遗漏的 reservation / release / cancellation 接口，不新建 `work_mem`。
- 至少让一个现有算子的访问路径切换通过共同机制产生**真实** IO/CPU/Plan 改善，同时在预算不足或取消时正确回滚/释放；不只新增抽象数据结构。
- 集成 read/write SQL 双入口、`EXPLAIN/PROFILE` 与权限定界；维持同一 query-local Snapshot / staged revision 的访问和结果。

### 局部 Acceptance（全部必做）

- **A22-S1-01**：read/write/program/rows/scalar 共用的已有与新增候选计划映射通过正反测试；缺少数据、统计或类型证明时安全回退，未改变 Cypher 25 Profile 或输出。
- **A22-S1-02**：至少一个真实多候选访问场景根据基数/成本选择不同的正确 Physical Operator，实际 SQLite VM/dbHits 与 EXPLAIN/PROFILE 一致；相同输入与统计产生确定性选择。
- **A22-S1-03**：规模由少量增加至大量独立 Pattern 时，Planner 被内部工作上限保护；报告计划耗时、实际候选数及 peak memory，超限可正确执行 fallback，不泄漏内存。
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
- 构建过程分批可取消、原子发布，构建失败/并发 writer 不能保留半成品或移动 Commit/Branch ref；只允许从目标的 first-parent chain 复用有效 checkpoint。
- 限制单次 writer-hold、工作预算、同时维护数量与额外持久磁盘占用；到达资源限制安全延期，不得在 read-only SQL、历史查询、EXPLAIN 或已存在 active reader 时隐式写主库。

#### S22.2.3 Snapshot 复用与派生数据失效

- 一个 execution 只 resolve 一次；必要时对纯 immutable 只读 base 建立有预算、可逐出的 query/connection-local 缓存，严格区分目标 Commit/Schema、selector/access proof 和 staged revision。
- 缺失/删除/损坏 checkpoint、GC、Branch move、Reset、Merge、schema/index revision、staged CREATE/DELETE、不同 SQLite connection / reopen 均回退正确 canonical Snapshot，不因 cache miss 创建新的 correctness source。
- 历史 Commit、当前 Branch 和 SQL explicit transaction 的读写回归一起闭合，不更改 canonical Layer、Commit hash、storage format 或 public query interface。

### 局部 Acceptance（全部必做）

- **A22-S2-01**：相同最终图、不同历史深度与 delta 大小的 Before/After 数据可复现；自动策略确实在许可写入边界触发，另记录触发条件、Checkpoint 数与创建开销，不靠隐式预热。
- **A22-S2-02**：自动 Checkpoint 的构建/取消/中途失败/重启后原子与可重建性可验；旧 Commit/Branch hash 不变，不会把 derived metadata 当真源；GC/缺失 checkpoint 时结果相同。
- **A22-S2-03**：在满足维护条件的高 Layer/deep-history 数据上，建立 checkpoint 后查询不再每次从 Root 或大段链重放；相比对应 no-checkpoint 基线实际减少 Layer 解析、SQLite VM 和端到端时间，且读取热态/新连接分别报告。
- **A22-S2-04**：自动维护带来的 writer-hold、磁盘和写入放大被明确单独计量且受现有资源约束；不能为了点查速度让 1k Node batch/Commit 或 History/Diff/Merge 出现超出 [非回退门槛](../../design/runtime.md#performance-targets) 的性能回退。
- **A22-S2-05**：1 GiB/no-swap Linux amd64 容器、历史/当前/事务及两连接并发、WAL/rollback journal 模式下无 OOM、无意外写锁、无半成品；取消和 reopen 后正确恢复。
- **A22-S2-06**：真实 SQLite `lithograph()` / `lithograph_rows()`、Storage integrity、适用 History/Merge、`cargo make quality`、`scripts/ci.sh` 与最终 Review/diff 全通过。

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

- **A22-S3-01**：统计误差和 planning decisions 在均匀/倾斜/空数据下可量化，较旧统计选择有至少一个明显更优的访问路径；缺失/错误统计时 100% 保持正确结果，并有 bounded collection/refresh 与磁盘成本证据。
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

#### S22.4.2 DPhyp 与 Greedy Fallback

- 对少量 Pattern Part 以 DPhyp / DP 只枚举合法连通子计划，比较预计输出、I/O、CPU、内存/TEMP、可索引性与已满足绑定；用于重排序的统计来自当前 pinned Snapshot（Stage S22.3）。
- 超过内部搜索时间/状态/内存预算的大组件使用确定性 Greedy + bounded search fallback；无统计或无成本证明时保守旧计划。禁止为任意 N 的 Part 枚举全部 2^N 子集或引入新公开 Hint/Planner 调优语法。

#### S22.4.3 Index Nested Loop Join

- 使用外侧已绑定的 Node/Relationship/Property key 参数化现有内侧 Seek、Adjacency Expansion；适用候选缺失索引时回退 Scan/其它已有 Join。重复外侧 key 也必须产生原始 multiplicity，不因优化误用 cache 去重结果。
- Query-scoped memoization 只在完整绑定、pinned Commit、Graph View/Access Policy 与 staged revision 允许时开启，严格按预算逐出；不把不同 Branch 的 key 相同认作相同权限/历史结果。

### 局部 Acceptance（全部必做）

- **A22-S4-01**：三到八个连通 Pattern 的不同书写顺序规划为可解释、实际更少的 I/O/中间组合；`EXPLAIN/PROFILE` 与执行工作一致。固定数据与成本下计划确定性，不声称所有表序在无 ORDER BY 下具有新增稳定排序语义。
- **A22-S4-02**：大量 Pattern Part 或故意复杂的 Join Graph 在内部规划预算耗尽时有稳定 Greedy Fallback；记录枚举状态、plan CPU/peak RAM 与最终正确结果，不因优化时间爆炸引发 OOM。
- **A22-S4-03**：Index Nested Loop 对稀疏/高选择性外侧、重复 keys、0/1/多结果、Property Type、双向端点/自环/parallel Relationship 的真实访问减少可量化；没有相关 Index 时安全回退。
- **A22-S4-04**：OPTIONAL、相关子查询、Variable reuse、Path uniqueness、NULL/TYPE_ERROR、Graph View、历史/staged 以及 MATCH→CREATE/SET/DELETE 的错误和行数逐项与旧语义 Oracle 一致。
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

#### S22.5.2 内存受控的 Grace Hash Join

- 小 build side 用 query-local reservation 按需建表；超过既有 `memoryLimitBytes` 可用内存时改为有限 partition 的 Grace Hash Join，通过 SQLite TEMP/disk 分批处理，不保留全体 build/probe rows 于 RAM。
- partition 数、递归深度、skew/hot key 行数与单批状态受上限保护；严重倾斜无法 spill 时安全失败/回退，不能因递归分桶无止境产生磁盘或内存爆炸。
- 使用现有 cancel、request resources、cleanup 与 transaction fail-closed，错误/early-close 后删除本查询 TEMP；写入不可见性和 Commit publication 不随 Hash Join 批次改变。

#### S22.5.3 成本选择及 Profile

- DPhyp/Greedy 的计划代价包括 Hash build、probe、spill 与预计输出，和 Index Nested Loop/Adjacency/Cartesian 共同选择；只报告实际执行路径，不把全部 equijoin 都强迫换 Hash。
- PROFILE 同时记录 build rows、probe rows、hash collisions、spill partitions/batches、真实 SQLite VM/TEMP bytes、峰值 Engine/RSS，不输出原始属性或参数值。

### 局部 Acceptance（全部必做）

- **A22-S5-01**：在同一没有适用 Property Index 的大集合等值 Join 上，真实 Hash Join 相比旧执行显著减少候选组合/VM 工作，输出行数与 Cypher 25 Oracle 完全相同；少量记录/已有 Seek 场景仍可选择更低成本的原计划。
- **A22-S5-02**：NULL、跨类型 TYPE_ERROR、数值规范化、重复 key、多对多、多分区、collision、skew、self-loop、parallel Relationships 与输出 multiplicity 都有正反/类型测试。
- **A22-S5-03**：`memoryLimitBytes` 小于 build side 且实际数据大于该预算时，真实触发 TEMP partition spill 并成功完成或按既有资源错误回滚；不允许在内存溢出后才补预留，TEMP 失败/空间不足无半写入。
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

#### S22.6.2 Join Dynamic Filter

- 实现对相等连接键有严格安全证明的 typed min/max 或 KeySet 运行时过滤，允许 build/probe 传递；保证不改变 Cypher 错误/NULL/type semantics，并只在 SQL B-tree/Index 真正可以 skip 时采用。
- 对重度 skew、超大 key space、混合 property family 与运行时过滤构造成本过高的场景保守回退；将 filter build cost、跳过物理行/页、TEMP/CPU 纳入 PROFILE/Benchmark。

#### S22.6.3 TopK / Search Dynamic Filter

- 对有序 Index/Scan 的 `ORDER BY ... LIMIT`，在至少已有足够可信候选且 cutoff/ties/NULL 的语义证明完整时按需更新动态下界或上界；不能在相同 score cutoff 的候选未取齐时提前终止。
- Full-text、Raw Vector、Managed Semantic 的 top-k 必须先遵守 Graph View 与 Access Policy 再排名；不改变 ANN recall、score、路径/排序的既有 contract，不为了“动态剪枝”错误丢掉范围内有效候选。

### 局部 Acceptance（全部必做）

- **A22-S6-01**：Semi-mask 在候选数为原图小比例、且扫描有 keyed access 的真实场景减少 SQLite VM/底层读取；不是单纯减少 Filter 比较次数。无法 skip 的 Scan 与无索引场景拒绝该计划，仍正确返回。
- **A22-S6-02**：Join Dynamic Filter 的 typed min/max/keyset 候选与未优化 Join 完全同一行/异常；正反测试含 skew、NULL、混合类型、重复、Graph View、历史/staged、不同权限和关系方向。
- **A22-S6-03**：TopK 动态剪枝在 0/1/多候选、相同 cutoff score/ties、NULL、ORDER BY 不可比较类型、skip/limit、Graph View、Search 的 top-k/ANN 指标下全部与稳定 Oracle 相符；没有类型/上下界证明时回退。
- **A22-S6-04**：所有 mask/filter 分配可预算、可取消、query-local，跨 state/revision/connection 不复用；1 GiB/no-swap Linux amd64 下无 OOM / RESOURCE_ERROR cleanup 不泄露候选或外部调用。
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

#### S22.7.3 Flat Output 与写入/事务边界

- 在 `lithograph_rows()` 最终 row、`lithograph()` adapter、相关子查询、需要全序的 Sort、具体写入/资源 barrier 等处按真实 Cypher multiplicity 正确枚举，不改变 visible column、路径/关系重复规则、skip/limit 和 transaction commit/rollback。
- Factorized state 必须 query-scoped 且在 historical/staged state 或 Graph View/access selector 变化后失效；不能把 grouped output 作为新的 application-facing result type。

### 局部 Acceptance（全部必做）

- **A22-S7-01**：Star、fan-out 与 2–4 hop 多对多场景在相同最终行数下，真实因子化中间表示的 bytes、复制次数和受影响的谓词计算次数低于 flat 基线；报告因子组合大小、实际 output rows、峰值 RSS/TEMP，不用 `count(*)` 替代完整 drain。
- **A22-S7-02**：ASP-Join 在适用的连接组件真实执行，Plan/PROFILE 记录 build、group、probe、skip 与实际工作量；不满足独立性条件时可靠回退原已验收 Join 算法。
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

#### S22.8.3 Planner / 其它 Join 算法组合

- WCOJ 在复杂查询中只优化自身安全循环组件，余下连接仍可用 DPhyp、Index Nested Loop、Hash Join、SIP 与 Factorized 中间表示；跨组件的 Join 顺序和输出错误时序保持原语义。
- 真实 EXPLAIN/PROFILE 报告 `MultiwayIntersect` 等本项目拥有的算子、输入列表长度/交集步骤/VM/内存，而不是宣称所有 Query 都符合 worst-case optimal 理论上界。

### 局部 Acceptance（全部必做）

- **A22-S8-01**：Triangle、4-cycle、Clique 子集和多环 mixed Pattern 在真实数据/SQLite 上可由 Planner 选择 WCOJ；实测 join/intersection steps、生成中间候选/VM/RSS 与已验收 binary joins 对照，在至少一个选择明确的高 fan-out cyclic fixture 上减少工作量。
- **A22-S8-02**：所有测试结果与 Cypher 25 Oracle 完全一致：directed/typed edges、parallel Relationships、自环、重复 Node variable、Relationship uniqueness、NULL/error、OPTIONAL/Path 的拒绝优化边界、Graph View/Read Access、历史 Commit/staged revision、写入原子性。
- **A22-S8-03**：多路交集无需扫描全部 Relationship 才知道空结果；当 index/adjacency 无法满足时退回二元计划，仍能在 1k/10k/100k/1M 及 10x 无关数据增长下解释实际访问工作。
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

- 同一 Fixture 和 pinned Commit 运行各目标优化计划及旧的正确 fallback，使用可审查的 Cypher 25 Oracle 对比**逐行 Identity/Value/Property、重复行/Relationship、排序/TopK、NULL/TYPE_ERROR 与错误时序**，而不是仅比较结果计数。
- 必须覆盖 OPTIONAL/Apply/相关 subquery/Union、Graph View/Read & Write Access、历史与 staged Snapshot、Relationship uniqueness、Path、不同 Label/Type/Property family、Search 的范围内 TopK 与现有 ANN recall/tie 保证。
- 混合计划至少包括 Index Nested Loop + Hash、Hash + SIP、SIP + Factorized、Factorized + WCOJ、WCOJ + 其余二元 Join；正确处理一个 Stage 不适用但其它 Stage 适用的情况。不改变 application-facing `lithograph()` / `lithograph_rows()` 输出合同。

### A22-G02：全过程资源、失败原子性与重启

- 在**真实 Linux amd64 1 GiB/no-swap** 容器执行 Planner 大组件、Hash build/Grace spill、重度 skew、SIP bitmap、Factorized group、Multiway intersect，以及 Snapshot Maintenance；记录 cgroup `memory.max`、`memory.swap.max`、`memory.peak`、`oom_kill`、进程 RSS、Engine memory、TEMP/WAL/disk bytes、spilled batches 与 writer-hold。
- 取消、`sqlite3_interrupt`、RESOURCE_ERROR、SQLite TEMP 不可写/空间不足、早停、Read/Write cursor 释放、事务回滚及 SQLite 进程重启后，没有 OOM/未释放 Temp、未提交部分可见性或不可恢复锁。`memoryLimitBytes` 仍为 Engine 计账预算，不能替代 cgroup 实测。
- Checkpoint/Index derived cache 丢失、损坏、版本不匹配、GC/Branch move/staged revision 变化时，无数据串读或越权访问；对所有执行和维护路径保持 immutable history 与 durable Commit 一致性。

### A22-G03：性能/规模与非回退

- 对第 3 节全部九组 Fixture 建立固定 seed、真实 cardinality、State/Schema、同机相同 SQLite/build、warm/new-connection 的 Before/After；每种新算法必须有**至少一个真实执行且物理工作改善**的正例与**至少一个正确拒绝/回退**的反例。
- 1k→10k→100k→1M 和 10 倍无关图数据增长的访问/中间候选、SQLite VM_STEP/FULLSCAN_STEP、P50/P95、CPU、RSS/TEMP 正反证据完整；结构性门槛沿用 Runtime，**真实没有 Index 的属性 Scan 不承诺常数复杂度**。
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

**当前状态说明**：本轮只是一个 Phase 22 计划的整合与文档审核；S22.1–S22.8 的代码均未实现，Phase 维持 `ready`。本文件定义未来的完整验收，不构成授权提交、推送、发布或部署。

## 16. 2026-11-10 文档规划与 Review 证据

- **合并范围**：原八份尚未提交的 Phase 22–29 计划已整合为**本文件一个 Phase 22**；原 25 个 Feature 和 46 项局部 Acceptance 逐项映射为 `S22.1.1～S22.8.3`（实际 S22.3 有四个 Feature）及 `A22-S1-*～A22-S8-*`，无遗失。另新增共用 Fixture、性能/正确性/资源测量合同和 A22-G01～G06 综合验收。
- **设计与研究引用**：已同步 [Query Engine](../../design/query-engine.md#optimizer-and-budgeted-executor)、[Storage](../../design/storage.md#tiered-checkpoint-policy)、[Runtime](../../design/runtime.md#optimizer-join-stress)、[开发路线](../README.md)、[算法研究](../../research/query-optimizer-roadmap-evidence.md)。冗余的旧八份 Stage 来源文件在确认所有外部引用已迁移后清理；单一 Stage Plan 无第二套状态真源。
- **Review 发现与修复**：修复原规划中的阶段状态自相矛盾（例如单独 Stage 写成 `done`、S22.8 仍写“进入下一 Stage”）、研究与设计对不存在旧 Phase 的引用、无索引扫描局部验收与统一内存/性能门槛没有明确挂钩等问题；保留所有算法的正反/性能/回退要求，没有放宽正确性或规模阈值。
- **实际文档检查**：使用 Python 3.14 运行现有 `docs/guide/examples/check_docs.py`，82 个 Markdown 文档的链接/覆盖检查通过；专项校验 6 个受影响的规划/设计/研究文件共 199 个本地链接，八 Stage / 25 Feature / 46 局部 Acceptance / 6 全局门禁完整；`git diff --check` 通过，全部新文件空白检查通过。**这只是文档 Review，不是 `cargo make quality` / `scripts/ci.sh` 的未来 Phase 22 代码验收。**
- **当前 Git/交付边界**：已有 Phase 21 的 Rust 代码与其它无关用户修改均保留；本轮只新增/整合规划与同步相关文档，未实施 Phase 22 代码，未 Commit、Push、Release 或部署。后续开发须重新读取真实工作区与测试结果，再维护本文件的 Stage 实施证据。

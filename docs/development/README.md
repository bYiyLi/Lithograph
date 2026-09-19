# Lithograph 开发计划

本目录把 [设计文档集](../design.md#design-ownership) 的目标产品设计拆成可连续执行、可验证的开发路线。Design 定义产品行为；Development 只定义依赖顺序、实现单位、状态和验收。

## 1. 开发执行模型

```text
Design baseline
     ↓
Phase
     ↓
Feature dependency order
     ↓
Implementation
     ↓
Targeted validation
     ↓
Phase integration / compatibility acceptance
     ↓
Phase review + findings closure
     ↓
Documentation/status sync
```

Feature 是实现单元；Phase 是默认交付单元。不得用“Feature 已完成”“代码已写”“基础框架已建立”替代 Phase acceptance。

## 2. 状态模型

| 状态 | 含义 |
| --- | --- |
| `planned` | Design 已确定，但前置 Phase 未完成 |
| `ready` | Design、依赖和 acceptance 已齐全，可以开始 |
| `in_progress` | 正在实现或验证 |
| `blocked` | 存在无法由当前仓库、Design、reference 或已授权能力解决的真实阻塞 |
| `done` | 实现、自动化验证、集成验收、review 和文档同步全部完成 |

状态只描述当前仓库真实情况。

## 3. 当前基线

**状态分界：Phase 00–13 已 `done`；Phase 14 为 `in_progress`。** Phase 11 是在已完成的基础功能/规模验收之上增加的性能专项，不回开Phase 10，也不把Phase 10单次scale通过解释成全场景低延迟保证。Phase 11 已完成format3、persistent Standard Index、邻接keyset、query-owned resolved state/read guard、增量index overlay、实测热点优化以及固定性能机/并发/repository gate。Phase 12 在实现提交 `ec3be9ae26f0d756135cb838008971b1eeb5a4ff` 中闭合原生 FTS5 tokenizer specification、versioned definition、历史/cache、query-time analyzer、失败原子性及 SQL/Native/真实 SQLite 3.45.0/3.53.4 验收；后续 `a76fdbcfb26bfd811c2845a1039b11be88bcae56` 修正 Linux Native ABI gate 对 SQLite 上游 amalgamation warning 的处理。该 revision 的 repository CI 与六目标 hosted Release Matrix 均已通过。

**Phase 12 已完成并进入 v0.1.1 发布基线。** [Full-text / FTS5 Tokenizer 扩展](phases/12-fulltext-tokenizer.md) 根据 [Full-text](../design/full-text.md) 接入宿主 connection 已注册 tokenizer 的原生 specification，移除两个旧 analyzer 名称的特殊映射，并闭合配置、历史/cache、query-time analyzer、失败原子性、version publication 与 Native/SQL integration。

**Phase 13 已完成开发验收并进入 v0.2.0 发布基线。** [Managed Semantic Vector / Embedding Provider](phases/13-managed-semantic-vector.md) 已闭合 public `EmbeddingProviderV1`、独立 `openai-compatible` Provider、Semantic Index/query、format4 persistent Embedding Result Cache、cache/rebuild maintenance、Graph View/history、Diff/Patch/Merge/Rebase/Revert publication validation、Native/SQL boundary、SQLite 3.45.0/3.53.4 dual-extension smoke、quantitative provider/cache/writer-hold gate，以及 Linux/macOS/Windows x64/arm64 六目标 hosted Release Matrix。实现 revision `672c36043b1b05805900220355876a5ae20b7a08` 的 repository CI `35433504169` 与 Release Matrix `35433504174` 均通过。

**Phase 14 已完成实现与本地验收，等待 hosted release gate。** [SQL Explicit Transaction Adapter](phases/14-sql-explicit-transaction.md) 在不改变 C ABI 和既有 transaction 语义的前提下，为普通 SQLite driver 增加 `lithograph_tx_begin/execute/commit/abort` SQL 入口；SQLite 3.45.0/3.51.0/3.53.4 real-load、Native ABI 与 repository quality/coverage 已通过，六目标 Release Matrix 尚待当前 revision 的真实结果。

当前仓库已经完成 Phase 00 Engineering Foundation、Phase 01 SQLite Extension Boundary、Phase 02 Version-aware Storage Core、Phase 03 Cypher Frontend and Value Semantics、Phase 04 Read Query Engine、Phase 05 Mutation, Transaction and Commit、Phase 06 Cypher 25 Query Completeness、Phase 07 Schema, Constraint and Standard Indexes、Phase 08 Search and Data Ingestion、Phase 09 Versioned State Operations、Phase 10 Compatibility Closure and Release Hardening、Phase 11 Performance Optimization、Phase 12 Full-text / FTS5 Tokenizer 扩展、Phase 13 Managed Semantic Vector / Embedding Provider 与 Phase 14 SQL Explicit Transaction Adapter。现有 current-graph engine 已闭合 query composition、aggregation、advanced path、expression/function/value、mutation、versioned Graph Type/Constraint、lookup/range/text/point/full-text/vector index、Raw Vector `SEARCH`、Managed Semantic、`LOAD CSV`、Cypher transaction batching 与 SQL/Native explicit transaction，并在同一 Graph View、version-aware storage、Commit/savepoint 与 SQL Bridge/Native 边界上执行。Phase 13 从 v0.2.0 起属于正式发布能力。

当前 Design 进一步确认了通用版本化状态能力：Commit 保持 immutable；Commit Data 是可修改 JSON sidecar；Tag 是显式可移动但不会随写入自动前进的 named ref；允许显式创建 empty-delta Commit；History 需要 opaque cursor 做 bounded DAG traversal；SQL / Native explicit transaction 可以把多个标准 Cypher current-graph execution 组合为一个最终 Commit，并通过 `expectedHead` 提供 transaction-start CAS；Merge 使用 durable Merge Session，把大量 conflict 的分页/逐步 resolution 与最终 Commit/Branch move 分开，并通过 session revision + target-head CAS 支持上层在 finalize 前验证 exact candidate。它们不回开已完成的 Phase 02/05：Phase 05 保持“一次普通 top-level mutating query -> 一个 Commit”的 auto-commit foundation，Phase 08 的 `IN TRANSACTIONS` 继续“每个 mutating batch -> 一个 Commit”，多 execution 单 Commit与 Merge Session 的 version-atomicity completion owner 都是 Phase 09。Phase 09 同时把 storage format 从 development baseline `1` 显式迁移到首个公开 release 的 format `2`；Phase 10 负责 migration/scale/recovery closure。

- Phase 00：`done`；
- Phase 01：`done`；
- Phase 02：`done`；
- Phase 03：`done`；
- Phase 04：`done`；
- Phase 05：`done`；
- Phase 06：`done`；
- Phase 07：`done`；
- Phase 08：`done`；
- Phase 09：`done`；
- Phase 10：`done`；
- Phase 11：`done`；
- Phase 12：`done`；
- Phase 13：`done`；
- Phase 14：`in_progress`（等待六目标 hosted Release Matrix）；
- `docs/development/cypher25-compatibility.md` 保留 Phase 00–13 已执行证据；Full-text family、Phase 12 provider supplemental inventory 与 Phase 13 Managed Semantic supplemental inventory 均已闭合为 `done`。Phase 14 是 adapter 扩展，不改变冻结 Cypher 语言 coverage 分母。

## 4. 路线总览

| Phase | 状态 | 交付结果 | 主要依赖 |
| --- | --- | --- | --- |
| [00 Engineering Foundation](phases/00-engineering-foundation.md) | `done` | Rust/CI/test/compatibility harness 与可重复工程基线 | Design |
| [01 SQLite Extension Boundary](phases/01-sqlite-extension-boundary.md) | `done` | 可跨平台 `.load`、初始化、SQL Bridge、Native ABI | 00 |
| [02 Version-aware Storage Core](phases/02-versioned-storage-core.md) | `done` | Root Commit、immutable layers、snapshot resolver、branch main、checkpoint | 01 |
| [03 Cypher Frontend and Value Semantics](phases/03-cypher-frontend-values.md) | `done` | `CY25-2026.08` parser/AST/scope/type/value foundation | 00–02 |
| [04 Read Query Engine](phases/04-read-query-engine.md) | `done` | Graph View read boundary、MATCH/RETURN vertical slice、planner/executor、indexed traversal、streaming | 02–03 |
| [05 Mutation, Transaction and Commit](phases/05-mutation-transaction-commit.md) | `done` | Graph View write boundary、Cypher writes、普通 top-level mutation 自动 Commit、rollback/concurrency | 02–04 |
| [06 Cypher 25 Query Completeness](phases/06-cypher25-query-completeness.md) | `done` | current-graph query/path/expression/function/subquery semantics 完整并继承 Graph View | 03–05 |
| [07 Schema, Constraint and Standard Indexes](phases/07-schema-constraint-index.md) | `done` | Graph Type、Constraint、lookup/range/text/point index；indexed read 遵守 Graph View | 05–06 |
| [08 Search and Data Ingestion](phases/08-search-ingestion.md) | `done` | Full-text、Vector/HNSW、SEARCH、LOAD CSV 并遵守 Graph View | 06–07 |
| [09 Versioned State Operations](phases/09-version-control-operations.md) | `done` | Native explicit transaction、format 1→2、Commit Data、Tag、explicit Commit、cursor History、branch/time-travel/diff/patch、resumable Merge Session、rebase/squash/reset/revert/gc | 05、07–08 |
| [10 Compatibility Closure and Release Hardening](phases/10-compatibility-release.md) | `done` | 100% applicable Profile、10M/100M scale、recovery/migration、跨平台 release | 00–09 |
| [11 Performance Optimization](phases/11-performance-optimization.md) | `done` | format3、persistent index/keyset/query-owned state、性能/压力/quality/CI与六目标hosted Release Matrix全部闭合 | 00–10 |
| [12 Full-text / FTS5 Tokenizer 扩展](phases/12-fulltext-tokenizer.md) | `done` | 原生 tokenizer specification、Schema/历史/cache、query analyzer、失败原子性与真实 SQLite/Native 扩展验收 | 08、09、11 |
| [13 Managed Semantic Vector / Embedding Provider](phases/13-managed-semantic-vector.md) | `done` | 保留 Raw Vector；新增 SQLite Embedding Provider、Semantic Index、format4 Embedding cache、文本 query/rebuild/cache maintenance | 01、07–12 |
| [14 SQL Explicit Transaction Adapter](phases/14-sql-explicit-transaction.md) | `in_progress` | 新增四个 SQL `tx_*` scalar，复用既有 Native explicit transaction core；等待六目标 hosted release gate | 01、05、09 |

关键依赖原则：**Version-aware graph storage 在 Phase 02 建立，不能拖到后期再 retrofit。** Phase 09 在该 immutable history foundation 上完成 transaction -> Commit 行为：增加 Native explicit transaction，把多个 execution 的最终 net delta 写成一个 Layer/Commit；增加 Merge Session，把长时间 conflict resolution 保存在非历史 operational workspace 中并只在 finalize 形成最终 Merge Commit/ref move；同时增加用户级状态 sidecar/ref 与版本操作，并通过显式 `1 -> 2` migration 增加 Commit Data / Tag / Merge Session storage。这不改变 Phase 02 的 Layer / Commit / Snapshot 核心合同，也不要求重开 Phase 02/05。Phase 13 同样不回开 Phase 08 Raw Vector：它只在既有 Index/History/HNSW foundation 上增加 String -> derived Vector 的 managed path，Raw Vector 继续作为标准 Cypher 25 contract。

## 5. Phase 完成标准

Phase 11 的Feature顺序与量化验收由其独立计划引用[Large-scale Invariants](../design/runtime.md#large-scale-invariants)维护；不在路线总表重复一份延迟阈值。性能基线、SQLite证据及已知测量限制见 [Phase 11性能证据](../research/phase11-performance-evidence.md)。Phase 13 的 Provider/cache/query acceptance 由其计划引用 [Vector](../design/vector.md) 与 [Embedding Provider 研究证据](../research/embedding-provider-contract.md)，不在 Development README 复制接口合同。

每个 Phase 的具体 acceptance 在对应文件中。所有 Phase 共同要求：

1. Scope 内功能是真实实现，不使用 mock 代替核心行为；
2. Targeted unit/integration tests 通过；
3. 该 Phase 要求的 SQLite / Cypher / Storage / Version integration acceptance 通过；
4. 成功路径以及会改变 correctness 的 parse/type/error/rollback/conflict/recovery 路径通过；
5. Phase-level review 完成，scope 内 correctness、compatibility、storage-integrity、security 和 performance finding 已修复；
6. `docs/development/cypher25-compatibility.md` 与实际结果同步；
7. Phase status 与仓库事实同步；
8. final diff 无临时文件、无 unrelated refactor、无 secret、无 generated junk；
9. 文档链接、Markdown、`git diff --check` 与新文件 trailing-whitespace 检查通过。
10. repository-wide `cargo make quality` 通过；如果 Phase 修改了 SQLite Extension 行为，再同时通过 `scripts/ci.sh` 对应的真实 SQLite/ABI/compatibility gate。

Commit / push 是独立 repository action。只有真实执行后才记录对应状态。

## 6. Compatibility 完成规则

“完整 Cypher 25”不按总代码量或 clause 名单主观判断，而按 `docs/development/cypher25-compatibility.md` 的冻结 Profile 判断。

`graphView` 是 Lithograph-specific execution option，不属于 Cypher grammar/compatibility inventory；它的 correctness 由 Phase 04–10 acceptance 单独验证，不能为了实现它修改 Cypher 25 语法或把 `USE` 纳入不同语义。

最终 Phase 10 必须同时满足：

- openCypher TCK 中适用于 Lithograph current-graph Profile 的 scenario 全部通过；
- `CY25-2026.08` 新增/改变 feature matrix 全部通过；
- 所有 built-in current-graph function/procedure/type/index/schema surface 均有自动化 inventory 和测试；
- 没有未解释 expected-failure、skip 或 compatibility waiver；
- SQL Bridge 与 Native API 对两者共同支持的 query 返回相同 Cypher semantic result；
- Native API 的 transaction-owning query path 通过独立 transaction acceptance。

## 7. Storage / Version 完成规则

最终产品不得把“当前图工作”与“版本历史工作”分成两套 source of truth。Phase 02 起每个 graph write 就必须走 immutable layer + commit。

最终 acceptance 至少覆盖：

- fresh DB -> Root -> main；
- Node/Relationship/Property/Label/Type 全版本 identity；
- transaction rollback 不产生 durable commit/ref move；
- SQL / Native explicit transaction 中多个 current-graph execution 共享 staged state，成功时至多形成一个最终 Commit；任一 execution/commit failure 原子 abort，`expectedHead` mismatch 在写入前失败；
- concurrent stale branch head detection；
- checkpoint 删除后历史 snapshot 仍可重建；
- schema/index definition 在 time-travel 中与 commit 一致；
- branch divergence、three-way merge、delete-vs-modify、property conflict、constraint conflict；
- Merge Session 可跨多次调用分页读取大量 conflict、逐步 resolution、restart 恢复；candidate inspection 固定 session revision，finalize 用同一 revision + target-head CAS，期间不产生 intermediate Commit/ref move；
- rebase 全量 rollback、squash snapshot-equivalence 与旧 history 保留；
- reset/revert 后旧 history 仍可查询；
- Commit Data 可 set/replace/clear 而不改变 Commit ID/Snapshot，也不进入 Diff/Patch/Merge；
- Tag 只通过显式操作移动，作为 GC reachability root，Branch write 不自动移动 Tag；
- explicit empty-delta Commit 可以建立新的 immutable state node，不引入 working tree/staging；
- History/DAG traversal 使用 immutable start Commit + opaque cursor 分页，Branch/Tag 后续移动不改变已开始的 traversal；
- explicit GC 只清理 Branch/Tag/open Merge Session 都不可达的 canonical history，并随被删除 Commit 清理其 Commit Data；
- crash/reopen 后 Commit DAG、Branch/Tag refs、Commit Data 与 open Merge Session/resolution 完整。

## 8. Verification Layers

开发采用从小到大的验证顺序：

```text
unit
  ↓
targeted component
  ↓
SQLite extension integration
  ↓
Cypher semantic / compatibility fixtures
  ↓
Version/storage integration
  ↓
Phase acceptance
  ↓
full repository gates
```

没有新失败或跨模块影响时，不因为“更彻底”在每个小 Feature 后重复完整 release gate。

`rusqlite/loadable_extension` 会把 SQLite 调用切换到 host `sqlite3_api_routines`。因此 standalone Core storage tests 与 Extension ABI tests 必须使用独立 Cargo invocation，避免 workspace feature unification 把 loadable-extension ABI mode 强加给 standalone SQLite connection；workspace `clippy --all-features` 仍用于验证联合编译。

## 9. Development Artifacts

- [架构实现依赖图](architecture-roadmap.md)
- [Cypher 25 Compatibility Matrix](cypher25-compatibility.md)
- [Phase 00](phases/00-engineering-foundation.md)
- [Phase 01](phases/01-sqlite-extension-boundary.md)
- [Phase 02](phases/02-versioned-storage-core.md)
- [Phase 03](phases/03-cypher-frontend-values.md)
- [Phase 04](phases/04-read-query-engine.md)
- [Phase 05](phases/05-mutation-transaction-commit.md)
- [Phase 06](phases/06-cypher25-query-completeness.md)
- [Phase 07](phases/07-schema-constraint-index.md)
- [Phase 08](phases/08-search-ingestion.md)
- [Phase 09](phases/09-version-control-operations.md)
- [Phase 10](phases/10-compatibility-release.md)
- [Phase 11](phases/11-performance-optimization.md)
- [Phase 12](phases/12-fulltext-tokenizer.md)
- [Phase 13](phases/13-managed-semantic-vector.md)
- [Phase 14](phases/14-sql-explicit-transaction.md)
- [FTS5 Tokenizer 研究证据](../research/fts5-tokenizer-contract.md)
- [Embedding Provider / Cypher Vector 研究证据](../research/embedding-provider-contract.md)
- [性能证据与测量限制](../research/phase11-performance-evidence.md)
- [Phase 13 Semantic performance evidence](../research/phase13-semantic-performance-evidence.md)

## 10. 最终产品完成条件

以下保留Phase00–10的首个功能版本完成标准。Phase11另需满足其性能验收与[Performance Evidence Contract](../design/runtime.md#performance-evidence)、[固定基线性能目标](../design/runtime.md#performance-targets)、[扩展压力场景与范围控制](../design/runtime.md#stress-workloads)，Phase12另需满足其FTS5 provider验收与[Full-text](../design/full-text.md)；Phase13完成时还必须满足其Managed Semantic/format4验收与[Vector](../design/vector.md)、[Managed Semantic Storage Format 4](../design/storage.md#storage-format-4)。不能以基础功能版本或 v0.1.1 已经完成代替后续专项完成。

Lithograph 可以宣告首个完整版本完成，只有以下事实同时成立：

- 可在支持平台的 stock SQLite 上加载，不需要 SQLite fork 或 server；
- `CY25-2026.08` current-graph compatibility acceptance 全部通过；
- graph/schema/index write 全部具有 immutable Commit-DAG history；
- SQL / Native explicit transaction 可把多个标准 Cypher current-graph execution 原子组合成一个 Commit，而 caller-owned SQLite transaction 与 `IN TRANSACTIONS` 保持各自独立语义；
- Branch、Tag、Commit Data、explicit Commit、可分页 History、Time-travel、Diff、Patch、可恢复/可逐步解决冲突的 Merge Session、Rebase、Squash、Reset 与 Revert 可用；
- Full-text 与 Vector `SEARCH` 可用且历史 Snapshot correctness 保持；
- transaction、crash recovery、format migration、integrity check 通过；
- 10M Node / 100M Relationship release benchmark tier 正确完成且没有 OOM / unintended full scan；
- macOS/Linux/Windows 发布矩阵完成 build + real load smoke；
- README、Design、Development 和 compatibility docs 与实际 release 一致。

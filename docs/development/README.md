# Lithograph 开发计划

本目录把 `docs/design.md` 的最终产品设计拆成可连续执行、可验证的开发路线。Design 定义产品行为；Development 只定义依赖顺序、实现单位、状态和验收。

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

当前仓库已经完成 Phase 00 Engineering Foundation、Phase 01 SQLite Extension Boundary、Phase 02 Version-aware Storage Core、Phase 03 Cypher Frontend and Value Semantics、Phase 04 Read Query Engine、Phase 05 Mutation, Transaction and Commit、Phase 06 Cypher 25 Query Completeness 与 Phase 07 Schema, Constraint and Standard Indexes。现有 current-graph engine 已闭合 query composition、aggregation、advanced path、expression/function/value、mutation、versioned Graph Type/Constraint，以及 lookup/range/text/point standard index，并在同一 Graph View、version-aware storage、Commit/savepoint 与 SQL Bridge/Native 边界上执行。LOAD CSV/Search、Version operations 和 release-scale streaming/compatibility closure 仍分别属于 Phase 08–10。最低 SQLite 3.45.0 与当前 SQLite 3.51.0 的真实 `.load`、Phase 01–07 probe、Native ABI、质量与 compatibility regression 均已通过。

当前 Design 进一步确认了通用版本化状态能力：Commit 保持 immutable；Commit Data 是可修改 JSON sidecar；Tag 是显式可移动但不会随写入自动前进的 named ref；允许显式创建 empty-delta Commit；History 需要 opaque cursor 做 bounded DAG traversal；Native explicit transaction 可以把多个标准 Cypher current-graph execution 组合为一个最终 Commit，并通过 `expectedHead` 提供 transaction-start CAS。它们不回开已完成的 Phase 02/05：Phase 05 保持“一次普通 top-level mutating query -> 一个 Commit”的 auto-commit foundation，Phase 08 的 `IN TRANSACTIONS` 继续“每个 mutating batch -> 一个 Commit”，多 execution 单 Commit 的 version-atomicity completion owner 是 Phase 09。Phase 09 同时把 storage format 从 development baseline `1` 显式迁移到首个公开 release 的 format `2`；Phase 10 负责 migration/scale/recovery closure。

- Phase 00：`done`；
- Phase 01：`done`；
- Phase 02：`done`；
- Phase 03：`done`；
- Phase 04：`done`；
- Phase 05：`done`；
- Phase 06：`done`；
- Phase 07：`done`；
- Phase 08：`ready`；
- Phase 09–10：`planned`；
- `docs/development/cypher25-compatibility.md` 已把 Phase 07 闭合的 Graph Type、Constraint、lookup/range/text/point standard index 与 Point index integration 标为 `done`；LOAD CSV 上下文函数、Search、version procedure、完整 SHOW、PROFILE 与 release error closure 继续保持其后续 owner 状态。

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
| [08 Search and Data Ingestion](phases/08-search-ingestion.md) | `ready` | Full-text、Vector/HNSW、SEARCH、LOAD CSV 并遵守 Graph View | 06–07 |
| [09 Versioned State Operations](phases/09-version-control-operations.md) | `planned` | Native explicit transaction（multi-execution → one Commit）、format 1→2、Commit Data、Tag、explicit Commit、cursor History、branch/time-travel/diff/patch/merge/rebase/squash/reset/revert/gc | 05、07–08 |
| [10 Compatibility Closure and Release Hardening](phases/10-compatibility-release.md) | `planned` | 100% applicable Profile、10M/100M scale、recovery/migration、跨平台 release | 00–09 |

关键依赖原则：**Version-aware graph storage 在 Phase 02 建立，不能拖到后期再 retrofit。** Phase 09 在该 immutable history foundation 上完成 transaction -> Commit 行为：增加 Native explicit transaction，把多个 execution 的最终 net delta 写成一个 Layer/Commit；同时增加用户级状态 sidecar/ref 与版本操作，并通过显式 `1 -> 2` migration 增加 Commit Data / Tag storage。这不改变 Phase 02 的 Layer / Commit / Snapshot 核心合同，也不要求重开 Phase 02/05；只是把已有单-query auto-commit 扩展为明确的多-execution version-atomicity surface。

## 5. Phase 完成标准

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
- Native explicit transaction 中多个 current-graph execution 共享 staged state，成功时至多形成一个最终 Commit；任一 execution/commit failure 原子 abort，`expectedHead` mismatch 在写入前失败；
- concurrent stale branch head detection；
- checkpoint 删除后历史 snapshot 仍可重建；
- schema/index definition 在 time-travel 中与 commit 一致；
- branch divergence、three-way merge、delete-vs-modify、property conflict、constraint conflict；
- rebase 全量 rollback、squash snapshot-equivalence 与旧 history 保留；
- reset/revert 后旧 history 仍可查询；
- Commit Data 可 set/replace/clear 而不改变 Commit ID/Snapshot，也不进入 Diff/Patch/Merge；
- Tag 只通过显式操作移动，作为 GC reachability root，Branch write 不自动移动 Tag；
- explicit empty-delta Commit 可以建立新的 immutable state node，不引入 working tree/staging；
- History/DAG traversal 使用 immutable start Commit + opaque cursor 分页，Branch/Tag 后续移动不改变已开始的 traversal；
- explicit GC 只清理 Branch/Tag 都不可达的 canonical history，并随被删除 Commit 清理其 Commit Data；
- crash/reopen 后 Commit DAG、Branch/Tag refs 与 Commit Data 完整。

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

## 10. 最终产品完成条件

Lithograph 可以宣告首个完整版本完成，只有以下事实同时成立：

- 可在支持平台的 stock SQLite 上加载，不需要 SQLite fork 或 server；
- `CY25-2026.08` current-graph compatibility acceptance 全部通过；
- graph/schema/index write 全部具有 immutable Commit-DAG history；
- Native explicit transaction 可把多个标准 Cypher current-graph execution 原子组合成一个 Commit，而 caller-owned SQLite transaction 与 `IN TRANSACTIONS` 保持各自独立语义；
- Branch、Tag、Commit Data、explicit Commit、可分页 History、Time-travel、Diff、Patch、Merge、Rebase、Squash、Reset 与 Revert 可用；
- Full-text 与 Vector `SEARCH` 可用且历史 Snapshot correctness 保持；
- transaction、crash recovery、format migration、integrity check 通过；
- 10M Node / 100M Relationship release benchmark tier 正确完成且没有 OOM / unintended full scan；
- macOS/Linux/Windows 发布矩阵完成 build + real load smoke；
- README、Design、Development 和 compatibility docs 与实际 release 一致。

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

当前仓库已经完成 Phase 00 Engineering Foundation：Rust workspace、SQLite extension load boundary、SQLite/test fixture、Cypher compatibility harness、locked Cargo gate、vendored openCypher TCK byte/revision integrity 与 CI 基线均已建立并通过最终 review。Phase 00 不包含 graph product behavior，Engine 产品功能从 Phase 01 开始实现。

- Phase 00：`done`；
- Phase 01：`ready`；
- Phase 02–10：`planned`；
- `docs/development/cypher25-compatibility.md` 的 capability family 仍全部为 `planned`，Phase 00 只建立 inventory 与验收基础，不把未实现 Cypher 能力标记为完成。

## 4. 路线总览

| Phase | 状态 | 交付结果 | 主要依赖 |
| --- | --- | --- | --- |
| [00 Engineering Foundation](phases/00-engineering-foundation.md) | `done` | Rust/CI/test/compatibility harness 与可重复工程基线 | Design |
| [01 SQLite Extension Boundary](phases/01-sqlite-extension-boundary.md) | `ready` | 可跨平台 `.load`、初始化、SQL Bridge、Native ABI | 00 |
| [02 Version-aware Storage Core](phases/02-versioned-storage-core.md) | `planned` | Root Commit、immutable layers、snapshot resolver、branch main、checkpoint | 01 |
| [03 Cypher Frontend and Value Semantics](phases/03-cypher-frontend-values.md) | `planned` | `CY25-2026.08` parser/AST/scope/type/value foundation | 00–02 |
| [04 Read Query Engine](phases/04-read-query-engine.md) | `planned` | MATCH/RETURN vertical slice、planner/executor、indexed traversal、streaming | 02–03 |
| [05 Mutation, Transaction and Commit](phases/05-mutation-transaction-commit.md) | `planned` | Cypher writes、constraint hook、每次写入 Commit、rollback/concurrency | 02–04 |
| [06 Cypher 25 Query Completeness](phases/06-cypher25-query-completeness.md) | `planned` | current-graph query/path/expression/function/subquery semantics 完整 | 03–05 |
| [07 Schema, Constraint and Standard Indexes](phases/07-schema-constraint-index.md) | `planned` | Graph Type、Constraint、lookup/range/text/point index | 05–06 |
| [08 Search and Data Ingestion](phases/08-search-ingestion.md) | `planned` | Full-text、Vector/HNSW、SEARCH、LOAD CSV | 06–07 |
| [09 Version Control Operations](phases/09-version-control-operations.md) | `planned` | branch/history/time-travel/diff/patch/merge/rebase/squash/reset/revert/gc | 05、07–08 |
| [10 Compatibility Closure and Release Hardening](phases/10-compatibility-release.md) | `planned` | 100% applicable Profile、10M/100M scale、recovery/migration、跨平台 release | 00–09 |

关键依赖原则：**Version-aware storage 在 Phase 02 建立，不能拖到后期再 retrofit。** Phase 09 只是交付完整用户级 Git-like operations。

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

Commit / push 是独立 repository action。只有真实执行后才记录对应状态。

## 6. Compatibility 完成规则

“完整 Cypher 25”不按总代码量或 clause 名单主观判断，而按 `docs/development/cypher25-compatibility.md` 的冻结 Profile 判断。

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
- concurrent stale branch head detection；
- checkpoint 删除后历史 snapshot 仍可重建；
- schema/index definition 在 time-travel 中与 commit 一致；
- branch divergence、three-way merge、delete-vs-modify、property conflict、constraint conflict；
- rebase 全量 rollback、squash snapshot-equivalence 与旧 history 保留；
- reset/revert 后旧 history 仍可查询；
- explicit GC 只清理 unreachable canonical history；
- crash/reopen 后 commit DAG 与 branch refs 完整。

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
- graph/schema/index write 全部具有 Git-like immutable history；
- Branch、Time-travel、Diff、Patch、Merge、Rebase、Squash、Reset、Revert 和 History 可用；
- Full-text 与 Vector `SEARCH` 可用且历史 Snapshot correctness 保持；
- transaction、crash recovery、format migration、integrity check 通过；
- 10M Node / 100M Relationship release benchmark tier 正确完成且没有 OOM / unintended full scan；
- macOS/Linux/Windows 发布矩阵完成 build + real load smoke；
- README、Design、Development 和 compatibility docs 与实际 release 一致。

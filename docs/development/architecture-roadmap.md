# Lithograph 架构实现路线

本文只表达实现依赖，不重新定义 [设计文档集](../design.md#design-ownership) 的产品 contract。

## 1. Critical Path

```text
Phase 00 Engineering Foundation
        |
        v
Phase 01 SQLite Extension Boundary
        |
        v
Phase 02 Version-aware Storage Core
        |
        +----------------------+
        |                      |
        v                      v
Phase 03 Cypher Frontend   Snapshot / Commit foundation
        |
        v
Phase 04 Read Query Engine
        |
        v
Phase 05 Mutation + Transaction + Commit
        |
        +-----------------------------+
        |                             |
        v                             v
Phase 06 Cypher 25 Complete      Version write primitives
        |
        v
Phase 07 Schema / Constraint / Index
        |
        v
Phase 08 Search / LOAD CSV
        |
        v
Phase 09 Versioned State Operations
        |
        v
Phase 10 Compatibility / Scale / Release Closure
        |
        v
Phase 11 Performance Optimization (done)
        |
        v
Phase 12 Full-text / FTS5 Tokenizer (done)
        |
        v
Phase 13 Managed Semantic Vector / Embedding Provider (done)
        |
        v
Phase 14 SQL Explicit Transaction Adapter (done)
        |
        v
Phase 15 SQL Execution / True Streaming / Provider Cache (done)
```

Phase 00–15 均已完成开发验收。Phase 15 的 EX15-01–19 全部闭合：15.1 host feasibility probe 确认 same-connection SAVEPOINT / repeated inner transaction 可行并收敛 `xClose` cleanup-failure contract；本地 correctness/resource/quality/CI/docs review 已通过；revision `b0d5a3da9c5c0266218e01c51d6d34a1be0a6b9d` 的 repository CI `35586275713` 与六目标 Release Matrix `35586275736` 全部成功。Phase 13/14 的 Provider/format4/Native/SQL tx adapter 记录继续作为历史实现证据，当前产品合同以 Phase 15 收敛后的 SQL-only / format3 / Provider-owned cache 设计为准。具体证据见 [Phase 15 计划](phases/15-sql-execution-provider-cache.md)。

Phase 12 的目标由 [Full-text](../design/full-text.md) 定义，当前实现与 FT12-01–20 开发验收已经闭合；依赖 Phase 08/09/11，不重开已完成 Phase。具体实现顺序与证据见 [Phase 12计划](phases/12-fulltext-tokenizer.md)。该成果已经进入 v0.1.1 发布基线，对应 repository CI 与六目标 hosted Release Matrix 均已通过。

Phase 13 **当时的实现基线**保留 Phase 08 的 Raw Vector/Cypher 25 `SEARCH`，新增 SQLite Embedding Provider contract、Managed Semantic Index 与 Lithograph-owned persistent Embedding Result Cache，并把 storage format 提升到 4；具体历史范围与验收见 [Phase 13计划](phases/13-managed-semantic-vector.md)。最新 Vector/Storage 合同已经由 Phase 15 变更，不能用当前 Design 链接反推 Phase 13 当时的 cache ownership。

Phase 13 的实现从 v0.2.0 起进入正式发布基线；Native ABI 与 `CY25-2026.08` 保持不变，storage format 提升为 4。

Phase 14 **当时的实现基线**是在 Phase 09 已有 state machine 上增加 `lithograph_tx_begin/execute/commit/abort` SQL adapter，不新建 transaction/storage abstraction；具体历史顺序与验收见 [Phase 14 计划](phases/14-sql-explicit-transaction.md)。最新 Design 已由 Phase 15 删除专用 `tx_execute`，因此 Phase 14 只作为实现来源，不再作为当前 transaction API 合同。

Phase 14 已进入 v0.2.1 正式发布基线。实现 revision `2f617f13e007ce713bd48078396c40bf0a963c7c` 的 repository CI `35448157779` 与六目标 Release Matrix `35448157766` 均通过。

Phase 15 的目标由 [Interfaces](../design/interfaces.md)、[Explicit Transaction](../design/storage.md#native-explicit-transaction)、[Vector](../design/vector.md) 与 [Runtime](../design/runtime.md#large-scale-invariants) 共同提供输入：Application execution 只保留 SQLite SQL；`lithograph_rows()` 成为 read/write/external-I/O/transaction-owning query 的真正 pull-based execution stream；normal execution surface 直接加入 active explicit transaction；Native query ABI 与 `tx_execute` 删除；Lithograph-owned embedding cache/format4 删除，OpenAI-compatible Provider 自己使用独立 SQLite cache DB。它不回开 Cypher 语言 Profile，也不修改 KG OS。

Phase 15 已进入 v0.3.0 正式发布基线。实现修复 revision `b0d5a3da9c5c0266218e01c51d6d34a1be0a6b9d` 的 repository CI `35586275713` 与六目标 Release Matrix `35586275736` 均通过。

## 2. 为什么 Version Storage 必须早于 Cypher Engine

如果先把 Node/Relationship 作为 mutable current-state rows 实现，再在后期增加 Commit/Branch，会导致以下基础合同全部返工：

- element identity；
- write transaction；
- schema/index history；
- historical query visibility；
- merge conflict unit；
- cache/index lifecycle。

因此 Phase 02 就建立 immutable layer、Commit DAG、Branch ref 与 Snapshot Resolver。Phase 03–08 的每个 write 从一开始就写版本历史。

## 3. Component -> Phase Map

| Component | First owning Phase | Completion Phase |
| --- | --- | --- |
| Rust workspace / CI / test harness | 00 | 10 |
| SQLite loadable extension / SQL application surface | 01 | 15 |
| Application-facing Native query ABI | 01 | 15（删除） |
| internal storage metadata/migration | 01 | 15 |
| IDs / dictionaries | 02 | 02 |
| immutable Layer / Commit | 02 | 05 |
| Branch `main` / checkout context | 02 | 09 |
| Snapshot Resolver / checkpoint | 02 | 10 |
| Cypher lexer/parser/AST | 03 | 06 |
| Scope/type/value semantics | 03 | 06 |
| Graph View execution boundary | 04 | 10 |
| Logical / physical planner | 04 | 10 |
| Row/path executor | 04 | 06 |
| Mutating operators | 05 | 06 |
| Transaction -> Commit behavior（auto-commit + SQL explicit transaction） | 05 | 15 |
| Complete clause/expression/function coverage | 06 | 10 |
| Graph Type / constraints | 07 | 07 |
| lookup/range/text/point index | 07 | 10 |
| FTS5 full-text | 08 | 10 |
| HNSW / vector SEARCH | 08 | 10 |
| Embedding Provider C ABI / SQLite client-data binding | 13 | 13 |
| Managed Semantic Index / `db.index.semantic.*` | 13 | 13 |
| Lithograph-owned Persistent Embedding Result Cache / storage format 4 | 13 | 15（删除） |
| OpenAI-compatible Provider-owned SQLite embedding cache | 15 | 15 |
| LOAD CSV / transaction batching | 08 | 10 |
| Diff / Patch | 09 | 09 |
| Three-way Merge / Merge Session / conflict resolution | 09 | 09 |
| Rebase / Squash | 09 | 09 |
| Reset / Revert / History / GC | 09 | 09 |
| Commit Data / Tag / Merge Session operational storage + storage format 1→2 migration | 09 | 09 |
| Explicit empty-delta Commit / cursor-based DAG history | 09 | 09 |
| Compatibility closure | 10 | 10 |
| Scale / crash / migration / cross-platform release | 10 | 15 |

## 4. Vertical Slice 顺序

### Phase 11性能纵向闭环

```text
reproducible before + physical-work counters
 -> adjacency keyset aligned with existing B-tree
 -> query-owned resolved state + read lifetime
 -> format3 + persistent index generation + reopen
 -> ancestor base + relevant delta + staged/candidate correctness
 -> measured residual executor hotspots
 -> Search/Merge/mixed workload + quantitative acceptance
 -> migration/recovery/compatibility/six-platform closure
```

上面的Component表保留首次功能完成Phase，不把原有成果重新标为未完成。追加改动的owner如下：

| 本轮变更 | 首次基线 | Phase 11 owner |
| --- | --- | --- |
| Benchmark口径、physical counters与定量门禁 | 10 | 11.1、11.8 |
| Adjacency复合cursor与overlay归并 | 02/04 | 11.2 |
| Query-scoped state、Graph View proof/cache与read guard | 04/09 | 11.3、11.6 |
| format3、persistent Standard Index与rebuild入口 | 07/09 | 11.4 |
| Index base/delta、历史/staged/candidate可见性 | 07/09 | 11.5 |
| Search/Merge与并发压力证据 | 08–10 | 11.7 |

Phase 11 当时不因增加持久cache重新设计canonical history，也不以性能为由改变Cypher类型/错误或Constraint。Phase 15 进一步把 Managed Semantic text->Vector cache移出 Lithograph `main`；Standard Index 的 format3/persistent generation仍属于 Lithograph derived storage，Provider cache则不属于 canonical/derived graph storage。

每个复杂子系统先形成最小真实纵向闭环，再扩 coverage。

### Phase 12 Full-text 扩展纵向闭环

```text
FTS5 specification / synthetic native tokenizer oracle
 -> versioned configuration / DDL / SHOW
 -> current + historical TEMP cache / connection lifecycle
 -> query-time analyzer native delegation / result semantics
 -> Version + Native + SQL cross-surface closure
 -> compatibility / quality / documentation evidence
```

| 追加变更 | 首次基线 | Phase 12 owner |
| --- | --- | --- |
| Specification 安全边界与真实 tokenizer fixture | 08 | 12.1 |
| Versioned 配置、DDL constructor 验证与 introspection | 07/08 | 12.2 |
| FTS cache、历史和 connection 注册边界 | 08/11 | 12.3 |
| Query analyzer override、score/Graph View/pagination | 08 | 12.4 |
| Schema 发布入口、Native staged 与 SQL adapters | 09 | 12.5 |
| 全部新验收与原有 regression / gates | 10/11 | 12.6 |

行为和取舍只由 [Full-text](../design/full-text.md) 定义，不把本表当第二份配置合同；FTS/HNSW 共享辅助代码的改动需要回归 Vector，但不据此扩大 Vector 设计范围。

### Phase 13 Managed Semantic Vector 纵向闭环

```text
SQLite provider extension + client-data ABI oracle
 -> versioned Semantic IndexDefinition / create / SHOW / DROP
 -> format4 persistent embedding cache + config/stats/clear
 -> text query / Graph View / history / TEMP HNSW reuse
 -> explicit rebuild + batch de-dup + external-I/O boundary
 -> Raw Vector regression + real SQLite / quality / cross-platform closure
```

| 追加变更 | 首次基线 | Phase 13 owner |
| --- | --- | --- |
| Provider pointer/lifecycle 与 deterministic synthetic extension | 01/12 | 13.1 |
| Semantic Schema kind、create/SHOW/DROP/version operations | 07/09 | 13.2 |
| format4、persistent text->Vector cache、policy/clear | 11 | 13.3 |
| Managed query、Graph View/history、现有 HNSW复用 | 08/11 | 13.4 |
| Rebuild、batch去重、external-I/O transaction boundary | 09/12 | 13.5 |
| Raw Vector/compat/performance/CI/六目标artifact回归 | 10–12 | 13.6 |

上表只记录 Phase 13 当时的实现依赖，不作为当前 providerConfig/cache/procedure 合同；最新行为只看 [Vector](../design/vector.md) 与 Phase 15。Raw Vector Property/`CREATE VECTOR INDEX`/`SEARCH` 继续由 Phase 08/10 的既有实现拥有，Phase 15 同样不能把旧入口重解释成自动 Embedding。

### Phase 15 SQL execution / streaming / Provider cache 纵向闭环

```text
minimum/current SQLite same-connection feasibility gate
 -> vtab write SAVEPOINT / xClose error propagation / repeated inner transaction boundary
 -> shared QueryCursor / execution state
 -> read + mutation + transaction-program incremental production
 -> lithograph_rows columns/row*/summary SQL stream
 -> normal SQL execution joins active explicit transaction
 -> autocommit IN TRANSACTIONS + external-I/O closure
 -> remove application Native query ABI / tx_execute
 -> remove Core embedding cache / format4
 -> OpenAI-compatible independent SQLite cache
 -> resource / compatibility / six-platform closure
```

| 追加变更 | 当前实现基线 | Phase 15 owner |
| --- | --- | --- |
| read/write/transaction-program true streaming core | 04/05/08 | 15.1 |
| `ordinal/event/data` rows surface 与 cursor cleanup | 01/04 | 15.2 |
| normal SQL surfaces 复用 explicit tx、SQL transaction subquery/external I/O | 08/09/14 | 15.3 |
| 删除 application Native query ABI / `tx_execute` | 01/09/14 | 15.4 |
| 删除 Core format4 embedding cache；OpenAI Provider 独立 cache DB | 13 | 15.5 |
| compatibility/performance/release/docs closure | 10–14 | 15.6 |

Phase 15 只安排实现依赖；SQL event、transaction、Provider cache 与 storage contract 仍分别由 Design 专题拥有。Phase 13/14 的旧行为只作为历史基线，不作为 Phase 15 目标语义。

### Cypher vertical slice

```text
Cypher text
 -> parse
 -> semantic/type
 -> logical plan
 -> physical plan
 -> snapshot storage
 -> execute
 -> typed result
```

第一条 read slice 必须真实执行 `MATCH ... RETURN`，不能用 hard-coded result 或绕过 Planner。

### Write vertical slice

```text
CREATE / SET / DELETE
 -> write operator
 -> transaction
 -> canonical delta
 -> layer
 -> commit
 -> branch compare-and-move
 -> reopen / historical read
```

第一条 write slice 必须在 Commit history 中可见，并通过 rollback acceptance。

### Explicit transaction vertical slice

```text
tx_begin(branch, expectedHead)
 -> pin base under writer ownership
 -> execute Cypher A against staged state
 -> execute Cypher B and observe A
 -> canonicalize base -> final net delta
 -> one layer
 -> one commit
 -> one branch compare-and-move
 -> commit / abort + reopen history
```

Phase 09 必须证明多个 execution 只是一个版本写单元，而不是先生成多个 immutable Commit 再隐藏或 squash；失败路径不能留下 intermediate Commit/ref move。Phase 05 的普通单-query auto-commit 与 Phase 08 的 transaction batching 不因此改变。

### Search vertical slice

```text
DDL index definition
 -> versioned schema commit
 -> derived physical index
 -> planner/search operator
 -> current query
 -> historical snapshot rebuild/fallback
```

### Merge vertical slice

```text
branch A/B
 -> divergence
 -> diff to merge-base
 -> merge.start pins ours/theirs
 -> durable Merge Session
 -> bounded conflict pages
 -> incremental resolve (revision++)
 -> exact candidate inspection at revision R
 -> merge.finalize read-prepare/validate at revision R
 -> short writer: revision CAS + target-head CAS
 -> install prepared canonical/derived result
 -> one two-parent commit / fast-forward
 -> session cleanup
 -> time-travel verification
```

Phase 09 必须证明 conflict resolution 与 finalize preparation 期间没有长期 SQLite writer ownership、没有 intermediate Commit/ref move；Session restart 后可恢复 resolution 进度。Candidate inspection 与 finalize 必须由同一 session revision 绑定，昂贵的 merge/candidate/constraint 计算在 read phase 完成，短 writer 只负责重新验证 revision + target Branch、安装 prepared result 与原子清理 Session，避免上层 validation 与最终提交之间出现 TOCTOU。

## 5. Cross-cutting Gates

以下 contract 不是单一 Phase 最后才补：

- `CY25-2026.08` matrix：Phase 03 开始持续更新；
- parser error location / stable error category：Phase 03 开始；
- no full-result materialization：Phase 04 开始；
- Graph View visibility：Phase 04 建立 read boundary，Phase 05 建立 write boundary，Phase 06–08 覆盖新增 operator/index/search path，Phase 10 做跨 surface closure；
- transaction rollback：Phase 05 开始；
- historical correctness：Phase 02 后所有 storage/index Feature 都验证；
- storage migration：Phase 01 建立 versioning；Phase 09 实现 Commit Data / Tag / Merge Session 所需的 format `1 -> 2` 显式迁移且保持既有 Commit ID；Phase 10 完成 release-grade fixtures；
- fuzz / crash / scale：对应模块成熟后逐步加入，Phase 10 做 closure。

## 6. 禁止的返工路径

以下实现路线与 Design 冲突，不得作为“先跑起来”捷径：

```text
mutable graph first -> later add version history
openCypher-only parser -> declare Cypher 25 complete
all Cypher transpiled to SQL text -> no native path/version operators
full result JSON materialization -> later add streaming
schema/index metadata kept outside Commit history
merge implemented as blind replay without merge-base/conflict model
vector scan called “vector index”
```

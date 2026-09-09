# Lithograph 架构实现路线

本文只表达实现依赖，不重新定义 `docs/design.md` 的产品 contract。

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
Phase 09 Version Control Operations
        |
        v
Phase 10 Compatibility / Scale / Release Closure
```

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
| SQLite loadable extension ABI | 01 | 10 |
| SQL Bridge / Native ABI | 01 | 10 |
| internal storage metadata/migration | 01 | 10 |
| IDs / dictionaries | 02 | 02 |
| immutable Layer / Commit | 02 | 05 |
| Branch `main` / checkout context | 02 | 09 |
| Snapshot Resolver / checkpoint | 02 | 10 |
| Cypher lexer/parser/AST | 03 | 06 |
| Scope/type/value semantics | 03 | 06 |
| Logical / physical planner | 04 | 10 |
| Row/path executor | 04 | 06 |
| Mutating operators | 05 | 06 |
| SQLite transaction -> Commit behavior | 05 | 09 |
| Complete clause/expression/function coverage | 06 | 10 |
| Graph Type / constraints | 07 | 07 |
| lookup/range/text/point index | 07 | 10 |
| FTS5 full-text | 08 | 10 |
| HNSW / vector SEARCH | 08 | 10 |
| LOAD CSV / transaction batching | 08 | 10 |
| Diff / Patch | 09 | 09 |
| Three-way Merge / conflict | 09 | 09 |
| Rebase / Squash | 09 | 09 |
| Reset / Revert / History / GC | 09 | 09 |
| Compatibility closure | 10 | 10 |
| Scale / crash / migration / cross-platform release | 10 | 10 |

## 4. Vertical Slice 顺序

每个复杂子系统先形成最小真实纵向闭环，再扩 coverage。

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
 -> slot merge
 -> conflict or resolved patch
 -> constraint validation
 -> two-parent commit
 -> time-travel verification
```

## 5. Cross-cutting Gates

以下 contract 不是单一 Phase 最后才补：

- `CY25-2026.08` matrix：Phase 03 开始持续更新；
- parser error location / stable error category：Phase 03 开始；
- no full-result materialization：Phase 04 开始；
- transaction rollback：Phase 05 开始；
- historical correctness：Phase 02 后所有 storage/index Feature 都验证；
- storage migration：Phase 01 建立 versioning，Phase 10 完成 release-grade fixtures；
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

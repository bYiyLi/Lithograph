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
Phase 09 Versioned State Operations
        |
        v
Phase 10 Compatibility / Scale / Release Closure
        |
        v
Phase 11 Performance Optimization (ready)
```

Phase 00–10保留已完成状态；Phase 11是追加的性能专项。产品目标行为仍由Design定义，实际执行/验收见 [Phase 11计划](phases/11-performance-optimization.md)。

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
| Graph View execution boundary | 04 | 10 |
| Logical / physical planner | 04 | 10 |
| Row/path executor | 04 | 06 |
| Mutating operators | 05 | 06 |
| Transaction -> Commit behavior（auto-commit + Native explicit transaction） | 05 | 09 |
| Complete clause/expression/function coverage | 06 | 10 |
| Graph Type / constraints | 07 | 07 |
| lookup/range/text/point index | 07 | 10 |
| FTS5 full-text | 08 | 10 |
| HNSW / vector SEARCH | 08 | 10 |
| LOAD CSV / transaction batching | 08 | 10 |
| Diff / Patch | 09 | 09 |
| Three-way Merge / Merge Session / conflict resolution | 09 | 09 |
| Rebase / Squash | 09 | 09 |
| Reset / Revert / History / GC | 09 | 09 |
| Commit Data / Tag / Merge Session operational storage + storage format 1→2 migration | 09 | 09 |
| Explicit empty-delta Commit / cursor-based DAG history | 09 | 09 |
| Compatibility closure | 10 | 10 |
| Scale / crash / migration / cross-platform release | 10 | 10 |

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

不因增加持久cache重新设计canonical history，不以性能为由改变Cypher类型/错误、Constraint或Native transaction合同。只读query不写main的cache，迁移与显式rebuild的副作用由其独立boundary承担。

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

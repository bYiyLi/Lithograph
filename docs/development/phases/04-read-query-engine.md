# Phase 04：Read Query Engine

**状态：`done`**

## 1. 目标

形成第一条真实 Cypher read vertical slice，并建立后续完整 Cypher 25 共用的 Graph View execution boundary、logical planner、physical planner、row/path executor、statistics 与 streaming framework。

## 2. 依赖

- Phase 02–03 `done`。

## 3. Design Inputs

- [Query Options](../../design/interfaces.md#query-options)、[Engine Structure](../../design/query-engine.md#engine-structure)、[Query Planning 与 Execution](../../design/query-engine.md#planning-and-execution)、[Version-aware Storage Model](../../design/storage.md#version-aware-storage)、[Result 与 Error Contract](../../design/interfaces.md#results-and-errors)、[Large-scale Invariants](../../design/runtime.md#large-scale-invariants)。

## 4. Features

### Feature 04.1 Execution context and logical plan

- 解析并规范化 `options.graphView`，但不把它写入 Cypher AST；
- query 开始时先 pin Branch/Commit Snapshot，再基于该 Snapshot 的 Label membership 解析 Graph View；
- omitted / empty Graph View 等价于完整 Snapshot graph；
- Graph View object 只接受 `requireAllLabels` / `excludeAnyLabels` string array；重复 Label 按 set 去重，require/exclude 交集、未知 object member、错误 type 或显式 `null` 返回 `INVALID_ARGUMENT`；
- unknown required Label 对当前 read graph state 产生空可见 Node 集，unknown excluded Label 没有效果；resolution 不得创建 dictionary entry；
- plan/execution context 保存 selector 而不是预计算完整 element-ID allowlist，为 Phase 05 的 staged-write visibility 保留正确边界；
- Graph View 不持久化、不产生 Commit，不修改 connection checkout。

最小 operator set：

```text
NodeScan
LabelScan
RelationshipScan
TypeSeek
ExpandAll
ExpandInto
Filter
Project
Sort
Skip
Limit
Aggregate
Distinct
Optional
Cartesian
```

从 AST 生成 typed logical plan，plan node 不包含 SQL text。

所有 graph access operator 都必须携带同一个 execution-level Graph View 语义：NodeScan/LabelScan 只产生可见 Node；RelationshipScan/ExpandAll/ExpandInto 只产生两个端点均可见的 Relationship。不能在 formatter/result adapter 阶段补过滤。

### Feature 04.2 Physical planner and statistics

- label/type counts；
- relationship degree stats；
- basic selectivity estimates；
- adjacency seek vs scan selection；
- deterministic plan explain output。

Graph View 不要求建立 per-view statistics。没有 view-specific estimate 时可以使用完整 Snapshot statistics 做保守 cost estimate，但 physical result correctness 必须始终经过 visibility check；不能因为 estimate 不精确而遗漏或泄漏元素。

Snapshot statistics 是 checkpoint derived metadata：checkpoint 构建时随现有 checkpoint row traversal 同步生成 Node / Relationship 总量及 label/type cardinality；planner 只读取该 metadata 并按 checkpoint -> target overlay 的变化增量修正。统计缺失或不可解析时使用 conservative unknown estimate，不允许回退到每次 query 全图 cardinality scan。

### Feature 04.3 Read executor

首个真实 query family：

```cypher
MATCH (a:Label)-[:TYPE]->(b)
WHERE ...
RETURN ...
ORDER BY ...
SKIP ...
LIMIT ...
```

包括 multi-pattern、undirected relationship、property access 和 basic aggregation。

### Feature 04.4 Streaming/spill

- pipeline batch；
- `lithograph_rows` row streaming；
- sort/aggregate spill 到 SQLite TEMP；
- cancellation/cleanup；
- no full-result JSON materialization on rows API。

### Feature 04.5 Snapshot read

同一个 query 对：

- active branch head；
- explicit `options.at = commit/...`；
- checkpoint + overlay；
- 同一 Snapshot 上的 `options.graphView`；

返回完全一致的 Snapshot semantics。

### Feature 04.6 EXPLAIN / PROFILE foundation

- `EXPLAIN` 返回 physical plan 不执行；
- `PROFILE` 执行并记录 rows/dbHits-like storage access/time counters；
- Profile counters 不改变结果。

## 5. Acceptance

- [x] `MATCH/WHERE/RETURN` 通过真实 parser -> planner -> executor -> storage；
- [x] omitted / empty `graphView` 与完整 Snapshot query 结果完全一致；
- [x] `requireAllLabels` / `excludeAnyLabels` 在 NodeScan、LabelScan、bound/direct lookup path 上产生一致 visibility；
- [x] duplicate selector Label 与去重后结果一致；unknown required Label 返回空 read result，unknown excluded Label 与未指定该 exclude 的结果一致，且两者都不新增 Label dictionary entry；
- [x] Relationship 只有 source/target 都可见时才进入 scan/expand/path，路径不能穿过隐藏 Node；
- [x] `count()`、aggregation、DISTINCT 与 OPTIONAL 的 cardinality 在 Graph View 内计算，而不是 full graph 后过滤；
- [x] unknown graphView object member、错误 JSON type、显式 `null` 或同 Label 同时 require/exclude 返回 `INVALID_ARGUMENT`，并在 query graph access 前拒绝；
- [x] outgoing/incoming/undirected traversal 正确；
- [x] planner 对 typed relationship expansion 使用 adjacency seek；
- [x] OPTIONAL null preservation fixtures 通过；
- [x] duplicate rows / aggregation / ordering 基础语义通过；
- [x] 1M-row synthetic streaming fixture 在 rows API 下不按总结果线性增长内存；
- [x] `lithograph()` read query 返回[Lithograph JSON](../../design/interfaces.md#lithograph-json)完整 JSON envelope；`lithograph_rows` columns/row encoding 与同一 query 一致；
- [x] Native read event 顺序固定为 `COLUMNS -> ROW* -> SUMMARY`；callback cancel 返回 `SQLITE_INTERRUPT` 且停止继续发 event；
- [x] sort spill fixture 与 in-memory result 相同；
- [x] historical commit read 与同一 commit 创建时 read 一致；
- [x] `options.at` + `graphView` 使用目标历史 Snapshot 的 Label membership，不读取 current head visibility；
- [x] EXPLAIN 不触发 graph query execution；缺失 derived statistics 时也不会回退为 graph-row cardinality scan；
- [x] PROFILE result 与 normal execution result 相同。

## 6. Review

检查 planner 是否退化成 clause dispatcher、executor 是否绕过 Snapshot Resolver、Graph View 是否被实现成 query rewrite/result post-filter、scan/seek/expand 是否存在 visibility bypass、`MATCH` 是否隐式 full scan、streaming adapter 是否在 formatter 再物化全部 rows。

Phase-level review 已闭环：

- parser / semantic frontend 输出进入 Lithograph-owned typed logical/physical plan，未引入 Cypher -> SQL text dispatcher；
- executor 统一通过 pinned `Snapshot` 与 bounded node/label/relationship/adjacency scan primitive 访问图，typed expansion 使用 source/target/type adjacency seek；
- Graph View visibility 位于 graph access / bound lookup / path expansion 边界，Relationship 必须同时满足两个端点可见；没有 query rewrite、result post-filter 或整图 allowlist materialization；
- planner statistics 从 rebuildable checkpoint metadata 读取，并只按 checkpoint -> target overlay 增量修正；review 中发现并移除了 prepare-time full Snapshot cardinality scan，缺失统计现只影响 cost estimate；
- `lithograph_rows` 使用 bounded batch prefetch；无 semantic barrier 的 read 不物化完整结果。`ORDER BY` / `DISTINCT` 使用独立 SQLite TEMP connection spill，避免宿主正在执行的 SQLite VM 与 spill 在同一 connection 上发生锁冲突；
- DISTINCT spill 使用 Cypher equality-compatible numeric key，`1` 与 `1.0` 不会错误地产生两个 distinct row；sort merge 的比较错误不会被 heap comparator 吞掉；
- relationship uniqueness 只在单个 `MATCH` graph pattern 内生效，独立 `MATCH` clause 可以合法复用同一 Relationship；physical plan 中每个 expand 保留自身的 relationship type/direction，不再按起点变量错误复用第一个 pattern spec；
- Phase 04 未拥有的 pattern/function semantics 不做近似执行：negated/disjunctive/dynamic label/type expression、inline property/`WHERE`、variable/quantified path、显式 Match Mode/SEARCH 与 `count(DISTINCT ...)` 等会明确拒绝并留给后续 owning Phase；
- projection alias 可以用于 `ORDER BY` expression，缺失 parameter 在执行前拒绝，Float `/ 0.0` 保持 Cypher/openCypher 已验证的 IEEE `NaN` / infinity semantics；
- Snapshot pinning、historical `options.at=commit/...`、historical Graph View membership、EXPLAIN/PROFILE 与 SQL Bridge/Native adapter 都有真实集成验证；
- query executor 在 batch/operator、aggregate、TEMP spill population/output 与 sort merge boundary 观察 host SQLite interrupt state；loadable-extension boundary 从 SQLite 3.41+ append-only API table 取得 `sqlite3_is_interrupted`，最低 SQLite 3.45 fixture 已验证该 ABI 路径。Native callback cancellation 继续保持 `SQLITE_INTERRUPT` primary code，并在 callback 返回非零后停止继续发送 event；
- Phase 04 只闭合首个 read vertical slice；`RETURN *`、grouped aggregation、完整 built-in function inventory、variable/quantified path、subquery/composition 等完整 Cypher 25 语义继续由 Phase 06 接管，没有在本 Phase 虚报完成。

## 7. 验证证据

- `cargo test -p lithograph-core --test phase04_query`：26 个 Phase 04 targeted read/planner/Graph View/history/streaming-spill/cancellation regression 全部通过；
- `lithograph-phase04` 真实 `.load` probe：scalar / rows 同编码、Node/Path tagged value、historical read、Graph View、EXPLAIN/PROFILE、public `INVALID_ARGUMENT` 路径通过；
- `scripts/native-abi-smoke.sh`：Native `COLUMNS -> ROW* -> SUMMARY` 与 callback cancel / `SQLITE_INTERRUPT` 通过；
- `lithograph-phase04-streaming`：1,000,000 Node checkpoint 上，10,000-row baseline RSS `80,576,512 B`，1,000,000-row full scan RSS `80,576,512 B`，测得增长 `0 B`；结果集规模没有带来线性内存增长；
- missing-statistics regression：移除 checkpoint statistics 后，即使 graph checkpoint Node table 不可用于全量 cardinality scan，`EXPLAIN` 仍可 conservative plan 并且不执行 graph access；
- `cargo make quality` 全绿：duplicated lines `0.94%`；coverage regions `81.84%`、functions `83.70%`、lines `83.56%`；fmt、Clippy、Rustdoc、production/support/C complexity、dependency policy 与 supply-chain gate 全部通过；
- `scripts/ci.sh` exit 0：当前 SQLite 3.51.0 与 SQLite 3.45.0 minimum fixture 的真实 `.load`、Phase 01/02/03/04 probes、artifact inspection、Native ABI、compatibility harness/inventory 全部通过；Windows smoke 同样包含 Phase 04 functional probe。1M RSS scale probe 单独作为 Phase acceptance 证据，不放入每次普通 CI。

## 8. 完成条件

Read vertical slice、planner/executor foundation 与 snapshot read acceptance 全部通过；Phase 05 转 `ready`。

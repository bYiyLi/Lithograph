# Phase 04：Read Query Engine

**状态：`ready`**

## 1. 目标

形成第一条真实 Cypher read vertical slice，并建立后续完整 Cypher 25 共用的 Graph View execution boundary、logical planner、physical planner、row/path executor、statistics 与 streaming framework。

## 2. 依赖

- Phase 02–03 `done`。

## 3. Design Inputs

- `docs/design.md` 第 4.4、6–8、13、17 节。

## 4. Features

### Feature 04.1 Execution context and logical plan

- 解析并规范化 `options.graphView`，但不把它写入 Cypher AST；
- query 开始时先 pin Branch/Commit Snapshot，再基于该 Snapshot 的 Label membership 解析 Graph View；
- omitted / empty Graph View 等价于完整 Snapshot graph；
- Graph View object 只接受 `requireAllLabels` / `excludeAnyLabels` string array；重复 Label 按 set 去重，require/exclude 交集、未知 member、错误 type 或显式 `null` 返回 `INVALID_ARGUMENT`；
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

- [ ] `MATCH/WHERE/RETURN` 通过真实 parser -> planner -> executor -> storage；
- [ ] omitted / empty `graphView` 与完整 Snapshot query 结果完全一致；
- [ ] `requireAllLabels` / `excludeAnyLabels` 在 NodeScan、LabelScan、direct element lookup 上产生一致 visibility；
- [ ] duplicate selector Label 与去重后结果一致；unknown required Label 返回空 read result，unknown excluded Label 与未指定该 exclude 的结果一致，且两者都不新增 Label dictionary entry；
- [ ] Relationship 只有 source/target 都可见时才进入 scan/expand/path，路径不能穿过隐藏 Node；
- [ ] `count()`、aggregation、DISTINCT 与 OPTIONAL 的 cardinality 在 Graph View 内计算，而不是 full graph 后过滤；
- [ ] unknown graphView member、错误 JSON type、显式 `null` 或同 Label 同时 require/exclude 返回 `INVALID_ARGUMENT` 且不访问 graph rows；
- [ ] outgoing/incoming/undirected traversal 正确；
- [ ] planner 对 typed relationship expansion 使用 adjacency seek；
- [ ] OPTIONAL null preservation fixtures 通过；
- [ ] duplicate rows / aggregation / ordering 基础语义通过；
- [ ] 1M-row synthetic streaming fixture 在 rows API 下不按总结果线性增长内存；
- [ ] `lithograph()` read query 返回第 13.1 节完整 JSON envelope；`lithograph_rows` columns/row encoding 与同一 query 一致；
- [ ] Native read event 顺序固定为 `COLUMNS -> ROW* -> SUMMARY`；callback cancel 返回 `SQLITE_INTERRUPT` 且停止继续发 event；
- [ ] sort spill fixture 与 in-memory result 相同；
- [ ] historical commit read 与同一 commit 创建时 read 一致；
- [ ] `options.at` + `graphView` 使用目标历史 Snapshot 的 Label membership，不读取 current head visibility；
- [ ] EXPLAIN 不触发 graph read/write side effect；
- [ ] PROFILE result 与 normal execution result 相同。

## 6. Review

检查 planner 是否退化成 clause dispatcher、executor 是否绕过 Snapshot Resolver、Graph View 是否被实现成 query rewrite/result post-filter、scan/seek/expand 是否存在 visibility bypass、`MATCH` 是否隐式 full scan、streaming adapter 是否在 formatter 再物化全部 rows。

## 7. 完成条件

Read vertical slice、planner/executor foundation 与 snapshot read acceptance 全部通过；Phase 05 转 `ready`。

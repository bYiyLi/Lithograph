# Phase 04：Read Query Engine

**状态：`planned`**

## 1. 目标

形成第一条真实 Cypher read vertical slice，并建立后续完整 Cypher 25 共用的 logical planner、physical planner、row/path executor、statistics 与 streaming framework。

## 2. 依赖

- Phase 02–03 `done`。

## 3. Design Inputs

- `docs/design.md` 第 6–8、13、17 节。

## 4. Features

### Feature 04.1 Logical plan

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

### Feature 04.2 Physical planner and statistics

- label/type counts；
- relationship degree stats；
- basic selectivity estimates；
- adjacency seek vs scan selection；
- deterministic plan explain output。

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

返回完全一致的 Snapshot semantics。

### Feature 04.6 EXPLAIN / PROFILE foundation

- `EXPLAIN` 返回 physical plan 不执行；
- `PROFILE` 执行并记录 rows/dbHits-like storage access/time counters；
- Profile counters 不改变结果。

## 5. Acceptance

- [ ] `MATCH/WHERE/RETURN` 通过真实 parser -> planner -> executor -> storage；
- [ ] outgoing/incoming/undirected traversal 正确；
- [ ] planner 对 typed relationship expansion 使用 adjacency seek；
- [ ] OPTIONAL null preservation fixtures 通过；
- [ ] duplicate rows / aggregation / ordering 基础语义通过；
- [ ] 1M-row synthetic streaming fixture 在 rows API 下不按总结果线性增长内存；
- [ ] sort spill fixture 与 in-memory result 相同；
- [ ] historical commit read 与同一 commit 创建时 read 一致；
- [ ] EXPLAIN 不触发 graph read/write side effect；
- [ ] PROFILE result 与 normal execution result 相同。

## 6. Review

检查 planner 是否退化成 clause dispatcher、executor 是否绕过 Snapshot Resolver、`MATCH` 是否隐式 full scan、streaming adapter 是否在 formatter 再物化全部 rows。

## 7. 完成条件

Read vertical slice、planner/executor foundation 与 snapshot read acceptance 全部通过；Phase 05 转 `ready`。

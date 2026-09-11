# Phase 06：Cypher 25 Query Completeness

**状态：`ready`**

## 1. 目标

在已有 frontend/planner/executor/write foundation 上闭合 `CY25-2026.08` 的 query、path、expression、function 和 mutation semantics，Search/Schema 专项能力由后续 Phase 接管。

Phase 04/05 已建立的 Graph View 是 execution context，不是新 Cypher 语法；本 Phase 新增的 query/path/subquery/function/mutation surface 必须自动继承同一 visibility/write boundary。

## 2. 依赖

- Phase 03–05 `done`。

## 3. Design Inputs

- `docs/design.md` 第 3、4.4、5–7 节；
- `docs/development/cypher25-compatibility.md`。

## 4. Features

### Feature 06.1 Query composition

完整实现：

- `WITH` / `LET`；
- `UNWIND` / `FOR`；
- `UNION`；
- `WHEN`；
- `NEXT`；
- `CALL {}` nested/correlated subqueries；
- EXISTS/COUNT/COLLECT subquery expressions。

### Feature 06.2 Aggregation

- implicit grouping；
- explicit `GROUP BY`；
- aggregate aliases；
- `DISTINCT`；
- aggregation in `ORDER BY/WHERE` 按 Profile；
- complete aggregate function inventory。

### Feature 06.3 Pattern/path completeness

- quantified / variable-length；
- group variables；
- match modes；
- path modes including `ACYCLIC`；
- path selectors；
- shortest path families；
- restrictive selector + explicit path mode combinations。

### Feature 06.4 Expression/function completeness

逐项闭合 frozen inventory：

- boolean/numeric/string/list/map；
- predicates/reduce/allReduce；
- temporal/duration/format patterns；
- point/spatial；
- vector value/functions；
- UUID；
- string interpolation；
- casting/type predicates；
- dynamic label/type/property expressions where Profile requires。

### Feature 06.5 Mutation completeness

闭合：

- CREATE/INSERT variants；
- MERGE multi-row/correlated semantics；
- SET map/replace/merge；
- label/type expression interactions；
- FOREACH；
- DELETE/DETACH edge cases。

### Feature 06.6 Current-graph procedure/function registry

建立统一 registry，支撑：

- built-in functions；
- built-in procedures；
- `SHOW FUNCTIONS` / `SHOW PROCEDURES`；
- Lithograph version procedures Phase 09 接入同一 registry。

## 5. Acceptance

- [ ] compatibility matrix 中 query/path/value/function/mutation families 全部达到 `done`，除明确归 Phase 07/08/09 的 family；
- [ ] applicable openCypher TCK 相关 scenario 无 regression；
- [ ] cross-clause composition suite 覆盖至少 MATCH/WITH/subquery/aggregate/write 的组合边；
- [ ] Graph View 下 write→read clause、`UNION` 与多次 `CALL {}` invocation 的 visibility 仍遵守 Cypher clause composition：后序 clause/invocation 能看到允许范围内的前序 writes，前序 clause 不能看到后序 writes；
- [ ] WITH/UNION/WHEN/NEXT/subquery/quantified path/shortest path/function dereference 不存在 Graph View visibility bypass；
- [ ] mutation completeness 中新增的 MERGE/SET/FOREACH/DELETE 变体继续满足 `GRAPH_VIEW_VIOLATION` 原子边界；
- [ ] timezone/DST、numeric overflow、NaN、Unicode fixtures 通过；
- [ ] quantified path / shortest / match/path mode cardinality oracle 对齐；
- [ ] MERGE multi-row 与 concurrent foundation tests 通过；
- [ ] UUID/vector/string-interpolation typed result 对齐 frozen Profile。

## 6. Review

重点检查为单个 TCK case 添加的无语义模型 special-case、operator 顺序依赖、变量 scope 泄漏、Graph View 在 nested/subquery/path/function 中丢失、path duplicate/cardinality、temporal precision 和 mutation finalize order。

## 7. 完成条件

非 Schema/Search/Version 专项的 Cypher 25 current-graph family 闭合；Phase 07 转 `ready`。

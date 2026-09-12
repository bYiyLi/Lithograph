# Phase 06：Cypher 25 Query Completeness

**状态：`done`**

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

- [x] compatibility matrix 中 query/path/value/function/mutation families 全部达到 `done`，除明确归 Phase 07/08/09 的 family；
- [x] applicable openCypher TCK 相关 scenario 无 regression；
- [x] cross-clause composition suite 覆盖至少 MATCH/WITH/subquery/aggregate/write 的组合边；
- [x] Graph View 下 write→read clause、`UNION` 与多次 `CALL {}` invocation 的 visibility 仍遵守 Cypher clause composition：后序 clause/invocation 能看到允许范围内的前序 writes，前序 clause 不能看到后序 writes；
- [x] WITH/UNION/WHEN/NEXT/subquery/quantified path/shortest path/function dereference 不存在 Graph View visibility bypass；
- [x] mutation completeness 中新增的 MERGE/SET/FOREACH/DELETE 变体继续满足 `GRAPH_VIEW_VIOLATION` 原子边界；
- [x] timezone/DST、numeric overflow、NaN、Unicode fixtures 通过；
- [x] quantified path / shortest / match/path mode cardinality oracle 对齐；
- [x] MERGE multi-row 与 concurrent foundation tests 通过；
- [x] UUID/vector/string-interpolation typed result 对齐 frozen Profile。

## 6. Review

重点检查为单个 TCK case 添加的无语义模型 special-case、operator 顺序依赖、变量 scope 泄漏、Graph View 在 nested/subquery/path/function 中丢失、path duplicate/cardinality、temporal precision 和 mutation finalize order。

Phase-level review 已闭环：

- query composition 使用统一 row/scope 模型；`WITH` / `LET` 同步绑定、`UNWIND` / `FOR` rebinding、`CALL` import/YIELD、`NEXT` row handoff/作用域清空、`UNION` column/flavor 和 query termination 均有正反例；
- aggregation、expression subquery、dynamic label/type/property、advanced path、mutation 与 Graph View 都经过真实 parser -> planner -> executor -> versioned storage 路径，不为单一 fixture 增加 hidden compatibility mode；
- current-graph function/procedure registry 同时驱动 semantic validation、runtime dispatch 和 `SHOW FUNCTIONS` / `SHOW PROCEDURES`，未知名称及参数错误在执行前拒绝；LOAD CSV 上下文函数和 Phase 09 version procedures 仍由各自 owner 接入同一 registry；
- temporal clock、IANA timezone/DST、numeric overflow/NaN、Unicode、UUID、VECTOR、Point 与 string interpolation 已覆盖 typed/null/error 边界；
- mutation program 保持一次 top-level query 的 savepoint/Commit 原子性，跨 clause staged visibility、FOREACH、NODETACH/DETACH、path delete、dynamic SET/REMOVE 与 correlated multi-row MERGE 均复用 storage/version API；
- 普通标量函数不再触发 Phase 06 program materialization；复合 operator 的大规模 streaming/spill 与 10M/100M memory gate 按既有路线由 Phase 10 验收，不在本 Phase 冒充 release-scale 证据。

## 7. 验证证据

- `phase06_query` 53/53，覆盖 composition、aggregation、path、function/value、procedure/SHOW、mutation、Graph View、temporal 与错误语义；
- Phase 04/05 regression 为 26/26 与 67/67；`phase03_parser_tck` 4/4，继承 4,224 parser、3,312 frontend-success、585 compile-error inventory，剩余 7 个 deferred scenario 全部由后续 procedure catalog/signature owner 接管，另有 1 个旧 openCypher relationship-reuse scenario 被 Cypher 25 新语义取代；
- `phase06_compatibility` 对 17 个 frozen fixtures 报告 15 passed、2 planned、0 failed；两个 planned family 仅为 Phase 07 Graph Type 与 Phase 08 SEARCH；
- SQLite Extension 真实 `.load`、SQL Bridge/Native ABI、workspace format/check/Clippy/test、canonical CI 与 repository quality gate 均通过；最终 quality snapshot 为 duplicated lines 0.99%、coverage regions 80.24%、functions 83.39%、lines 82.55%，production file hard budget 无违规；
- 对 frozen Manual 与本地 Neo4j 2026.07.1 oracle 复核了 clause composition、NEXT/CALL/UNION、SHOW metadata、path uniqueness、string/null、temporal、aggregate 边界与错误类别；2026.08 delta 以 frozen official Manual 为准。

## 8. 完成条件

非 Schema/Search/Version 专项的 Cypher 25 current-graph family 闭合；Phase 07 转 `ready`。

# Cypher 25 Compatibility Matrix

## 1. Profile

**Profile ID:** `CY25-2026.08`

**Frozen evidence date:** `2026-09-09`
**Product contract:** [Cypher 25 兼容合同](../design/compatibility.md)

Profile 由 Neo4j Cypher 25 current-graph Manual + 2026.08 已公开 Cypher additions 冻结。未来 Cypher 25 新功能建立新的 Profile，不回写本 Profile 的 acceptance meaning。

Phase 12 的 FTS5 provider binding 不是新增 Cypher grammar 或新的语言 Profile。其非可移植 analyzer 取值、破坏升级和新增验收按 [Full-text](../design/full-text.md) 与下文 supplemental inventory 单列；已有 `done` 只代表对应已执行基线，不代表任意 tokenizer 或所有 analyzer 组合已通过。

## 2. Status Semantics

| 状态 | 含义 |
| --- | --- |
| `planned` | Profile 已要求，尚无实现证据 |
| `partial` | 部分 fixtures 通过，但该 capability family 未闭合 |
| `done` | inventory 全部存在，positive/negative/composition fixtures 全部通过 |

Phase 03 已交付 parser/AST/scope/type/value frontend foundation；Phase 04/05 已交付 version-aware read/write vertical slice；Phase 06 已闭合非 Schema/Search/Version 专项的 current-graph query composition、aggregation、advanced path、expression/value/function 与 mutation semantics。对应 family 具备 parser、semantic/error、result、composition、Graph View 与 versioned write 自动化证据并标为 `done`。仍为 `partial` / `planned` 的 family 均保留明确后续 owner，不能从 Phase 06 的完成状态推导为已实现。

## 3. Profile Boundary

计入 Compatibility：

- current graph query / mutation；
- values / types / expressions / functions；
- pattern / path semantics；
- subqueries / query composition；
- Graph Type / constraints / indexes；
- current-graph `SHOW`；
- Full-text / Vector / `SEARCH`；
- `LOAD CSV`；
- `EXPLAIN` / `PROFILE`；
- `CALL ... IN TRANSACTIONS` query semantics。

不计入 Cypher language coverage：Neo4j DBMS database/alias/server/cluster/user/role/privilege/auth administration、system database operations、Java UDF deployment API，以及 `USE` / `graph.byName()` / `graph.names()` 等 composite-database graph-selection surface。它们是 Neo4j product management / multi-graph surface，不属于 Lithograph 单个 embedded current graph contract。

## 4. Capability Matrix

| Family | Required coverage | 状态 | Owning Phase |
| --- | --- | --- | --- |
| Lexical / identifiers / parameters | keywords、escaped/unescaped identifiers、Unicode、parameter naming、comments、query options | `done` | 03, 06 |
| Literals / operators | boolean、numeric、string、list、map、temporal/spatial/vector/UUID literals/constructors、operator precedence | `done` | 03, 06 |
| Null / equality / ordering | three-valued logic、comparison、ordering、NaN、type ordering where specified | `done` | 03, 06 |
| MATCH | node/relationship pattern、labels/types/property predicates、multiple patterns | `done` | 04, 06 |
| OPTIONAL MATCH | outer/null preservation、predicate placement/composition | `done` | 04, 06 |
| FILTER / WHERE | predicate expressions、pattern predicates、scope | `done` | 04, 06 |
| RETURN / WITH / LET | projection、alias/scope、star、aggregation boundary、ordering | `done` | 04, 06 |
| UNWIND / FOR | list-to-row expansion、null/empty semantics、scope | `done` | 04, 06 |
| Aggregation / GROUP BY | implicit/explicit grouping、aggregates、distinct、ordering interaction | `done` | 06 |
| ORDER BY / SKIP / LIMIT | expression visibility、parameters、aggregation composition | `done` | 04, 06 |
| UNION | column contract、ALL/DISTINCT、type reconciliation、subquery composition | `done` | 06 |
| WHEN | conditional composed queries、scope/result compatibility | `done` | 06 |
| NEXT | sequential query composition and row handoff | `done` | 06 |
| CALL subqueries | imported variables、scope isolation、nested subqueries、UNION | `done` | 06 |
| EXISTS/COUNT/COLLECT subquery expressions | correlation、aggregation、null/empty semantics | `done` | 06 |
| CREATE / INSERT | node/relationship creation、properties、multi-row behavior | `done` | 05, 06 |
| SET / REMOVE | property/label mutation、map update、null-removes-property | `done` | 05, 06 |
| DELETE / DETACH DELETE | entity deletion、relationship integrity、row behavior | `done` | 05, 06 |
| MERGE | match/create branches、ON MATCH/CREATE semantics、locking/concurrency | `done` | 05, 06 |
| FOREACH | update-only iteration and variable scope | `done` | 06 |
| Pattern semantics | directions、anonymous elements、label expressions、relationship type expressions | `done` | 04, 06 |
| Quantified / variable-length patterns | group variables、bounds、predicates、result cardinality | `done` | 06 |
| Match modes | repeatable/different relationship semantics | `done` | 06 |
| Path modes | walk/trail/acyclic constraints as Profile specifies | `done` | 06 |
| Path selectors / shortest | ANY/ALL shortest、k/group selectors、mix with path modes | `done` | 06 |
| Runtime values | Boolean/Integer/Float/String/List/Map/Node/Relationship/Path | `done` | 03, 06 |
| Temporal / Duration | constructors、arithmetic、formatting、timezone/DST semantics | `done` | 03, 06 |
| Spatial Point | CRS、constructors、comparison/functions、point index integration | `done` | 03, 06, 07 |
| VECTOR | coordinate types、dimension、storage、conversion、similarity/distance | `done` | 03, 06, 08 |
| UUID | native type、constructors、bit helpers、storage/equality | `done` | 03, 06 |
| String interpolation | `s"...{expr}..."` / `S"..."` semantics | `done` | 03, 06 |
| Built-in functions | complete current-graph function inventory from frozen Manual | `done` | 06, 08 |
| Aggregating functions | complete frozen Manual inventory and null/group semantics | `done` | 06 |
| Procedure CALL | built-in current-graph procedures、Lithograph Version Procedures、YIELD、scope/result contract | `done` | 06–09 |
| Graph Type | SET/EXTEND/ALTER/SHOW/DROP element semantics、open schema | `done` | 07 |
| Constraints | key/unique/existence/type and Profile-defined forms | `done` | 07 |
| Lookup index | DDL、SHOW、planner integration | `done` | 07 |
| Range index | DDL、SHOW、seek/ordering semantics | `done` | 07 |
| Text index | DDL、SHOW、text seek semantics | `done` | 07 |
| Point index | DDL、SHOW、spatial seek semantics | `done` | 07 |
| Full-text index | DDL、原生 FTS5 analyzer specification、queryNodes/queryRelationships、query-time analyzer、score/options、历史/cache/connection 与失败原子性 | `done` | 08、12 |
| Vector index | DDL、dimension/similarity/quantization/filter properties、SHOW | `done` | 08 |
| SEARCH | MATCH/OPTIONAL MATCH vector ANN subclause、filter、LIMIT、SCORE | `done` | 08 |
| LOAD CSV | headers、field parsing、URI source、periodic transaction composition | `done` | 08 |
| IN TRANSACTIONS | batch size、error behavior、status output、native transaction boundaries | `done` | 08, 10 |
| IN CONCURRENT TRANSACTIONS | concurrent batching、DISJOINT BY semantics、SQLite serialized commit | `done` | 08, 10 |
| EXPLAIN | semantic validation + plan without execution | `done` | 04, 10 |
| PROFILE | execution + operator runtime counters | `done` | 04, 10 |
| SHOW current graph surfaces | functions/procedures/indexes/constraints/current graph type | `done` | 06–08 |
| Error compatibility | syntax position、semantic/type/constraint failures、transaction errors | `done` | 03–10 |

### Phase 12 FTS5 provider supplemental inventory

**Supplemental inventory 已全部 `done`；Phase 12 开发与验收已闭合。** 以下 backend binding 验收不向冻结 Cypher 语言 coverage 分母混入 SQLite 专属 tokenizer。Phase 08 的历史 evidence 继续保留；Phase 12 已用原生 FTS5 query tokenizer 委托替换旧的词集合近似，并闭合 QUERY mode、短语/布尔组合、历史/cache、connection lifecycle 与失败原子性。

| 验收面 | Acceptance 映射 | 状态 | Owner |
| --- | --- | --- | --- |
| 原生 specification、默认值、取消两个名称的特殊映射 | FT12-01–04 | `done` | 12.1–12.2 |
| 构造验证、失败原子性、planning/no-op 边界 | FT12-05–06 | `done` | 12.2、12.5 |
| Node/Relationship、完整定义、cache/history/connection | FT12-07–11 | `done` | 12.3、12.5 |
| Query-time analyzer、表达式/flags/synonym、score/pagination/lifetime | FT12-12–17 | `done` | 12.4 |
| 真实 SQLite 双 extension、Native/staged/SQL parity | FT12-18 | `done` | 12.5 |
| Cypher/Vector/version regression、完整 gates 与文档同步 | FT12-19–20 | `done` | 12.6 |

映射的完整场景及证据要求见 [Phase 12 acceptance](phases/12-fulltext-tokenizer.md#4-acceptance-matrix)。Cypher grammar、scope、type/null、Graph View-before-pagination 和事务语义按原有语言/执行规则验证；tokenizer/参数以直接 SQLite FTS5 + synthetic tokenizer 作 oracle。不得把 `porter unicode61` 称作 Neo4j `english` 等价实现，也不得为了使新 binding 通过而静默改写 Neo4j 语言 fixture。固定 v0.1.0 的两个 analyzer 测试属于旧 binding 证据；新 binding 的旧名称失败/原生行为由 FT12-01 明确替代并记录，而不是未解释 skip。

Phase 12 当前完成证据绑定 `463fb7c5f372c097d7dae776d271936f8b25b68a` 基础上的未提交 worktree：`lithograph-fts5` unit tests 4/4、Phase 12 Core targeted tests 11/11、真实双 extension `lithograph-phase12` probe、SQLite 3.45.0/3.53.4 smoke 与 Native ABI 全部通过；`cargo make quality` exit 0，coverage regions/functions/lines 83.26% / 84.35% / 85.26%；最终 `scripts/ci.sh` exit 0。executable inherited openCypher TCK 继续为 3,777/3,777 applicable scenarios 通过、0 failure，120 个非 applicable scenario 的 machine-readable exclusion reason 未变化。该结果是开发验收，不表示新版本已经发布或重新跑过 hosted Release Matrix。

### Phase 03 frontend evidence

Phase 03 的 inherited openCypher frontend regression 固定为：4,224 个合法 query/query-precondition parser inputs 全部 parse；3,312 个非 compile-error `executing query` 全部通过 frontend validation；585 个 compile-time-error `executing query` 中，Phase 06 已关闭 function inventory 与 aggregation/DISTINCT `ORDER BY` visibility/grouping gap。当前仅剩 7 个 procedure catalog/signature scenario 由后续 procedure owner 接管；另有 1 个旧 openCypher “同一 pattern 重用 relationship 必须失败”scenario 已被 Cypher 25 Match Mode 语义取代，因而显式列为 superseded，而不是 deferred 或静默 skip。`phase03_parser_tck` 锁定这两个精确集合，不能通过等量替换掩盖 regression。

`tests/fixtures/cypher25` 当前包含 18 个 frozen Profile fixtures；其中 3 个显式 parser-positive fixture 由 `phase03_parser_tck` 直接执行。compatibility runner 对 18 个 enabled fixtures 执行真实 core engine，结果为 18 passed、0 failed、0 planned。Graph Type 已包含 empty graph 与 populated graph 两个真实 execution fixture，SEARCH fixture 已由 Phase 08 进入真实执行。

这些证据只证明 parser/semantic/type foundation，不把尚未执行的 result semantics 标为 `done`。

### Phase 04 read execution evidence

Phase 04 的 targeted suite 覆盖真实 parser -> planner -> executor -> version-aware storage 路径，包括 multi-pattern、directed/incoming/undirected expansion、OPTIONAL null preservation、property access、basic `count()` / DISTINCT、ordering/skip/limit、Path/Node tagged value materialization、historical Snapshot、Graph View execution boundary、typed adjacency seek、external sort spill 与 missing-derived-statistics conservative planning。SQL Bridge `.load` probe 同时验证 scalar envelope 与 `lithograph_rows` row encoding 一致，Native smoke 验证 `COLUMNS -> ROW* -> SUMMARY` 和 callback cancel / `SQLITE_INTERRUPT`。

这些证据在 Phase 04 当时只把上述 family 提升为 `partial`；Phase 06 已用后续 semantic/result/composition suite 闭合 `RETURN *`、WITH/LET、grouped aggregation、current-query built-ins、variable/quantified pattern、match/path mode 与 subquery/composition。

### Phase 05 mutation execution evidence

Phase 05 的 67 个 targeted scenarios 覆盖真实 parser -> planner -> executor -> version-aware storage 路径，包括 Node/Relationship CREATE/INSERT、property/label SET/REMOVE、DELETE/DETACH DELETE、MERGE match/create 与 `ON MATCH` / `ON CREATE` foundation、multi-row clause barrier、Graph View write boundary、canonical net delta、empty-delta Commit、Branch CAS、historical read、stale writer、caller-owned transaction/savepoint、host interrupt 与全 PropertyValue family 的持久化边界。它们同时验证 write projection 的 DISTINCT/grouping/order/expression lowering、aggregate ORDER BY validation、已支持 function 的 argument/null semantics、zero-row unsupported function rejection、mutating EXPLAIN 与实际 lowering 一致，以及未拥有语义不会被静默近似执行。SQL Bridge `.load` probe 验证 invocation savepoint、scalar length failure、rows read-only、Graph View violation 与 reopen history；Native smoke 验证 mutating summary、callback cancel 和 fault-injection rollback。

这些证据在 Phase 05 当时把四个 mutation family 提升为 `partial`；Phase 06 已闭合 SET map merge/replace、FOREACH、advanced CREATE/INSERT、correlated/multi-row MERGE、DELETE/NODETACH edge cases、相邻 query composition 与 IANA named-zone timezone/DST。

### Phase 06 query-completeness evidence

Phase 06 的 54 个 targeted scenarios 覆盖统一 row/scope 执行模型下的 WITH/LET、UNWIND/FOR、UNION/WHEN/NEXT、CALL 与 expression subquery、implicit/explicit grouping、完整 aggregate inventory、advanced/quantified path、Match/Path Mode、selector/shortest、current-query function inventory、current-graph procedures/SHOW、dynamic name/property mutation、FOREACH、NODETACH/DETACH、Graph View composition、temporal clock/IANA DST、numeric overflow/NaN、Unicode、UUID、VECTOR、Point 与 string interpolation；Point regression 额外锁定输入 null propagation、`crs`/`srid` 与坐标/第三维别名冲突、CRS/维度相关 component access、direct inequality 的 `null` 结果、geographic antimeridian bounding box 与 WGS-84 3D average-height distance。Phase 04/05 regression 26/26 与 67/67 同时通过。

统一 registry 已关闭未知 function 静默执行，并支撑当前 query functions、aggregates、`db.labels` / `db.propertyKeys` / `db.relationshipTypes` 与 `SHOW FUNCTIONS` / `SHOW PROCEDURES`。Phase 08 已补齐 LOAD CSV 上下文函数 `file()` / `linenumber()`；Phase 09 进一步注册并执行 Branch/Tag/Commit/Log/Diff/Patch/Merge/Rebase/Squash/Reset/Revert/GC Version Procedures，因此 frozen built-in Procedure CALL / YIELD / scope-result surface 已闭合。Phase 10 仍负责最终 applicable TCK、PROFILE/error 与 release-scale compatibility closure。

### Phase 07 Schema/standard-index execution evidence

Phase 07 的 36 个 targeted scenarios 覆盖 canonical versioned Schema、Graph Type SET/ADD/ALTER/DROP 与 identifying/implied endpoint semantics、standalone/dependent Constraint、existing-data/write-time validation、DDL conflict/IF EXISTS/IF NOT EXISTS、Branch/time-travel Schema isolation、persistent property type lowering、`SHOW CURRENT GRAPH TYPE` canonical round-trip/virtual graph、`SHOW CONSTRAINTS` type filter/classification，以及 node/relationship lookup/range/text/point standard index。review regression 进一步锁定 Property Type union 的 canonical normalization、non-null round-trip、同一 schema 上冲突 Property Type Constraint 的拒绝、Vector coordinate alias canonicalization 与 `VECTOR<TYPE>(DIMENSION)` SHOW round-trip；KEY/UNIQUE 按完整 Cypher property-value equality 处理 Integer/Float、List/复合 key、NaN non-reflexive equality、Point/Vector signed zero 与 `2^53` 精度边界。Range Index derived cache 覆盖全部 Property Value family，exact equality / `IN` 共用 Cypher equality key；Vector exact seek 只有在目标 Commit 的 Property Type Constraint 精确证明 coordinate type + dimension 时启用，否则回退 scan 以保留 Type error。ordered Range、String predicate 与 Point spatial predicate 同样要求目标 Commit 的 versioned Property Type proof，避免 typed derived index 把异构属性上的 `TYPE_ERROR` 提前过滤掉；历史 Snapshot 使用历史 Commit 的 proof，不借用 current-head Schema。planner regression 还锁定 composite Range 实际选择复合 index 而不是被任意 `IndexSeek` 掩盖。Point Index `=` / `IN` 保持 `+0.0` / `-0.0` scan/seek 等价，spatial regression 继续覆盖 Cartesian/WGS-84 bbox/distance、antimeridian 与 Graph View hidden-candidate filtering。Schema DDL 在 canonical Commit/ref 已进入 invocation savepoint、derived index cache 尚未完成的 interrupt 窗口也会完整回滚；derived cache 删除后仍可从 canonical Snapshot 重建。

两个 Graph Type frozen fixtures（empty/populated graph）已进入真实 compatibility executor；Phase 07 完成当时的 18 个 CY25 fixtures 为 17 passed、1 planned、0 failed，唯一 planned family 是 Phase 08 SEARCH。SQL Bridge 的 Phase 07 probe 同时验证 schema summary/Commit、SHOW dependent constraints、constraint violation rollback、standard IndexSeek + Graph View 与 DDL+`graphView` `INVALID_ARGUMENT` rollback；该 probe 在 SQLite 3.51.0 与最低支持 SQLite 3.45.0 均通过。当前状态以 Phase 08 evidence 段记录的 18/18 enabled execution 为准。

Phase 06 闭合时的历史报告是 17 个 frozen fixtures 中 15 passed、2 planned、0 failed，当时两个 planned family 为 Graph Type 与 SEARCH；Phase 07 已按上面的当前报告关闭 Graph Type。Phase 06 的 composition/path/function/mutation oracle 结合 frozen official Manual 与 Neo4j 2026.07.1 实例复核；最新独立 review 的 Point null、constructor invalid forms、component access、inequality、WGS-84 3D distance 与 antimeridian bounding box 由 frozen official Manual/GQL status contract 再次复核；2026.08 delta 以 frozen Manual 为准。复合执行器的 release-scale streaming/spill 与 10M/100M memory gate 仍由 Phase 10 明确验收，Phase 06 不把 targeted semantic evidence 描述为规模证明。

### Phase 08 Search/Ingestion execution evidence

Phase 08 的 27 个 targeted scenarios 覆盖 Full-text Node/Relationship DDL/query、multi-label/multi-property schema、query analyzer override、phrase/boolean/property query syntax、Graph View-before-pagination、derived FTS5 rebuild；Vector Index 的 dimension/similarity/quantization/HNSW metadata、`SHOW VECTOR INDEXES`、Node/Relationship `SEARCH`、additional-property filter、`SCORE`、`LIMIT`、null query、OPTIONAL null preservation、hidden-candidate top-k、historical Snapshot 与 exact fallback；`LOAD CSV` 的 file/HTTP/HTTPS、headers/no headers、custom/Unicode delimiter、multiline/doubled/backslash-escaped quotes、`file()` / `linenumber()`、跨 `WITH`/scoped `CALL` 的 CSV context、用户变量命名空间隔离、多个/嵌套 `LOAD CSV` 的最近执行 context、`RETURN *` 隔离、clause barrier/global ORDER BY/aggregation、graph-element binding spill、5000-row streaming mutation 与 malformed/I/O rollback；以及 ordered/concurrent `IN TRANSACTIONS`、batch Commit cardinality、global prefix-before-batching、FAIL/CONTINUE/BREAK/RETRY/status、`DISJOINT BY`、Graph View repin 与 SQL Bridge/rows adapter transaction/I/O rejection。

Full-text/Vector definition 与 derived cache 都按目标 Commit 解析，DROP 后 historical Snapshot 仍可 rebuild/query；HNSW cache 删除或缺失不会改变结果语义。Native C ABI smoke 证明 transaction-owning query 仅在 autocommit Native path 执行、每个 mutating batch 形成独立 Commit，并在 caller-owned transaction 内返回 `TRANSACTION_BOUNDARY_REQUIRED`；Phase 08 real-extension probe 同时验证 Full-text/SEARCH、SQL Bridge transaction boundary、`lithograph_rows` fail-closed external-I/O/transaction contract 与 scalar `LOAD CSV`。

当前 18 个 CY25 frozen fixtures 均为 enabled execution；`phase06_compatibility` 的真实 core executor 结果为 18 passed、0 failed、0 planned。`lithograph-compat inventory` 仅生成 metadata inventory、不执行 fixture，因此 standalone inventory report 按设计显示为 planned，不能与 execution report 混读。Phase 08 不宣称完成 Phase 10 的 100% applicable TCK/error/release-scale closure；10M/100M scale、cross-platform release 与最终 PROFILE/error compatibility 仍由 Phase 10 验收。

### Phase 09 version-procedure execution evidence

Phase 09 的 46 个 targeted version scenarios 覆盖 Branch/Tag/Commit Data/explicit Commit、bounded DAG log、query-start version/target-head pinning、Version Procedure summary commit semantics、Diff/Patch、Patch provenance/slot-shape/identity atomic validation、typed before/after shape、Relationship Patch round-trip、同名 Index definition replacement 的单一 `SetIndex` logical slot、durable Merge Session、criss-cross virtual base、same-value auto-merge、delete-vs-modify、Relationship endpoint dependency、Schema/Constraint conflict、dormant resolution、candidate inspection、revision/head CAS、WAL concurrent resolve、Rebase shared three-way/derived conflict、criss-cross first-parent replay boundary、resolution validation 与 completed-replay rollback、Squash/Reset/Revert/GC，以及 Merge Commit mainline/rebase flatten。真实 extension probe 另覆盖 format `1 -> 2` migration、migration failure rollback 与 Merge Session restart/adapters；Native C ABI smoke 覆盖 SQLite connection-client-data scoped explicit transaction、staged Schema/Constraint visibility、LOAD CSV / transaction-owning query rejection、per-execution option boundary 与 fail-closed cleanup。`SHOW PROCEDURES` inventory 已包含 Version Procedures。

这些证据关闭 Phase 09 自己拥有的 Procedure CALL implementation surface；Phase 10 仍负责 100% applicable TCK、PROFILE/error taxonomy、SQL Bridge/Native parity、scale/recovery/cross-platform release gate。

### Phase 10 compatibility/release evidence

Phase 10 的 release-level compatibility evidence 已闭合三个最终 family：mutating query 与 Version Procedure 的 `EXPLAIN` 都执行 semantic validation + logical/physical planning 而不产生 graph/ref side effect；`PROFILE` 与普通 execution 返回相同 rows，并通过 public serializer 暴露 query-level 与 per-operator `rows` / `dbHits` / time counters，targeted regression 同时校验 operator totals 与 query metrics 一致；parse/semantic/type error 保留 source line/column、constraint failure 保持 `CONSTRAINT_ERROR`，SQLite `BUSY/LOCKED` 在 Core public `QueryErrorKind` 与 Extension public category 都映射为 `BUSY` 并保留 primary SQLite code，adapter 继续隐藏底层 storage detail。

Phase 10 完成时，最终 executable inherited openCypher TCK runner 在当时 worktree 上执行 3,897 个 scenarios：3,777 个 applicable scenarios 全部通过、0 failure；其余 120 个全部由显式 machine-readable reason 分类为产品面之外或已被 Cypher 25 语义取代，没有 unexplained skip。Phase 10 targeted compatibility suite 4/4、hardening 6/6、recovery 10/10 同时通过。因此 `EXPLAIN`、`PROFILE` 与 Error compatibility 三行当时由 `partial` 提升为 `done`，当时 matrix 已无 partial family。该历史报告不覆盖后续发现；当前 Full-text family 的待修复项和新 provider 验收见 Phase 12 supplemental inventory。

## 5. 2025.06+ Cypher 25 Delta Inventory

除 frozen Cypher 5/openCypher inheritance 外，Profile 必须显式覆盖 2025.06 之后加入 Cypher 25 的新/改变能力。当前 inventory 至少包含：

- Cypher 25 query version prefix；
- `WHEN`、`NEXT`；
- GQL-aligned syntax/aliases introduced through Cypher 25；
- path/match mode updates including `ACYCLIC` and later selector combinations；
- temporal `format()` and constructor format patterns；
- `VECTOR` value/type/functions；
- vector index evolution；
- `SEARCH` ANN subclause and filtering extensions；
- Graph Types / element types / property type constraints；
- `FOR`；
- composable `SHOW` updates applicable to current graph；
- `GROUP BY` and aggregation visibility updates；
- current string functions introduced through 2026.05；
- `PROPERTY_EXISTS`；
- native `UUID`；
- string interpolation；
- `IN CONCURRENT TRANSACTIONS` / `DISJOINT BY` current semantics。

实现阶段必须从 frozen official Manual 自动/半自动生成精确 function/procedure/clause inventory，不能把本节的人工摘要当作 exhaustive machine-readable list。

## 6. Test Requirements per Capability

一个 capability family 只有同时具备以下证据才可 `done`：

1. parser positive fixtures；
2. parser negative + line/column fixtures；
3. semantic/type error fixtures；
4. result semantics，包括 null、duplicates、ordering 和 edge values；
5. 与至少两个相邻 clause/operator 的 composition fixtures；
6. write capability 额外覆盖 rollback、multi-row 与 version Commit；
7. schema/index capability 额外覆盖 populated graph validation 和 historical Snapshot；
8. oracle comparison 结果记录；
9. regression test 进入默认 CI suite。

## 7. Completion Gate

Profile 完成必须满足：

```text
applicable openCypher TCK failures = 0
CY25-2026.08 matrix unresolved/partial = 0
unexplained skipped compatibility fixtures = 0
semantic oracle mismatches = 0
```

性能差异不算语言不兼容，但执行结果、error category、type/value、transaction visibility 或 path cardinality 差异必须解决。

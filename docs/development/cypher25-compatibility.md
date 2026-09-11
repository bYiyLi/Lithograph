# Cypher 25 Compatibility Matrix

## 1. Profile

**Profile ID:** `CY25-2026.08`

**Frozen evidence date:** `2026-09-09`
**Product contract:** `docs/design.md#3-cypher-25-兼容合同`

Profile 由 Neo4j Cypher 25 current-graph Manual + 2026.08 已公开 Cypher additions 冻结。未来 Cypher 25 新功能建立新的 Profile，不回写本 Profile 的 acceptance meaning。

## 2. Status Semantics

| 状态 | 含义 |
| --- | --- |
| `planned` | Profile 已要求，尚无实现证据 |
| `partial` | 部分 fixtures 通过，但该 capability family 未闭合 |
| `done` | inventory 全部存在，positive/negative/composition fixtures 全部通过 |

Phase 03 已交付 parser/AST/scope/type/value frontend foundation；Phase 04 已交付第一条真实 read execution vertical slice；Phase 05 已交付 version-aware mutation/transaction vertical slice，包括 CREATE/INSERT、SET/REMOVE、DELETE/DETACH DELETE、MERGE foundation、Graph View write boundary、immutable Commit 与 rollback/concurrency。对应 capability family 因此已有自动化执行证据并标为 `partial`；`partial` 仍不代表该 family 的 frozen Profile inventory 已闭合。完整 aggregation/grouping、built-in function/procedure、advanced path/query composition、mutation completeness、schema/index/search 等继续由后续 owning Phase 完成。

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
| Lexical / identifiers / parameters | keywords、escaped/unescaped identifiers、Unicode、parameter naming、comments、query options | `partial` | 03, 06 |
| Literals / operators | boolean、numeric、string、list、map、temporal/spatial/vector/UUID literals/constructors、operator precedence | `partial` | 03, 06 |
| Null / equality / ordering | three-valued logic、comparison、ordering、NaN、type ordering where specified | `partial` | 03, 06 |
| MATCH | node/relationship pattern、labels/types/property predicates、multiple patterns | `partial` | 04, 06 |
| OPTIONAL MATCH | outer/null preservation、predicate placement/composition | `partial` | 04, 06 |
| FILTER / WHERE | predicate expressions、pattern predicates、scope | `partial` | 04, 06 |
| RETURN / WITH / LET | projection、alias/scope、star、aggregation boundary、ordering | `partial` | 04, 06 |
| UNWIND / FOR | list-to-row expansion、null/empty semantics、scope | `planned` | 04, 06 |
| Aggregation / GROUP BY | implicit/explicit grouping、aggregates、distinct、ordering interaction | `planned` | 06 |
| ORDER BY / SKIP / LIMIT | expression visibility、parameters、aggregation composition | `partial` | 04, 06 |
| UNION | column contract、ALL/DISTINCT、type reconciliation、subquery composition | `planned` | 06 |
| WHEN | conditional composed queries、scope/result compatibility | `planned` | 06 |
| NEXT | sequential query composition and row handoff | `planned` | 06 |
| CALL subqueries | imported variables、scope isolation、nested subqueries、UNION | `planned` | 06 |
| EXISTS/COUNT/COLLECT subquery expressions | correlation、aggregation、null/empty semantics | `planned` | 06 |
| CREATE / INSERT | node/relationship creation、properties、multi-row behavior | `partial` | 05, 06 |
| SET / REMOVE | property/label mutation、map update、null-removes-property | `partial` | 05, 06 |
| DELETE / DETACH DELETE | entity deletion、relationship integrity、row behavior | `partial` | 05, 06 |
| MERGE | match/create branches、ON MATCH/CREATE semantics、locking/concurrency | `partial` | 05, 06 |
| FOREACH | update-only iteration and variable scope | `planned` | 06 |
| Pattern semantics | directions、anonymous elements、label expressions、relationship type expressions | `partial` | 04, 06 |
| Quantified / variable-length patterns | group variables、bounds、predicates、result cardinality | `planned` | 06 |
| Match modes | repeatable/different relationship semantics | `planned` | 06 |
| Path modes | walk/trail/acyclic/simple constraints as Profile specifies | `planned` | 06 |
| Path selectors / shortest | ANY/ALL shortest、k/group selectors、mix with path modes | `planned` | 06 |
| Runtime values | Boolean/Integer/Float/String/List/Map/Node/Relationship/Path | `partial` | 03, 06 |
| Temporal / Duration | constructors、arithmetic、formatting、timezone/DST semantics | `partial` | 03, 06 |
| Spatial Point | CRS、constructors、comparison/functions、point index integration | `partial` | 03, 06, 07 |
| VECTOR | coordinate types、dimension、storage、conversion、similarity/distance | `partial` | 03, 06, 08 |
| UUID | native type、constructors、bit helpers、storage/equality | `partial` | 03, 06 |
| String interpolation | `s"...{expr}..."` / `S"..."` semantics | `partial` | 03, 06 |
| Built-in functions | complete current-graph function inventory from frozen Manual | `planned` | 06 |
| Aggregating functions | complete frozen Manual inventory and null/group semantics | `planned` | 06 |
| Procedure CALL | built-in current-graph procedures、YIELD、scope/result contract | `planned` | 06–09 |
| Graph Type | SET/EXTEND/ALTER/SHOW/DROP element semantics、open schema | `planned` | 07 |
| Constraints | key/unique/existence/type and Profile-defined forms | `planned` | 07 |
| Lookup index | DDL、SHOW、planner integration | `planned` | 07 |
| Range index | DDL、SHOW、seek/ordering semantics | `planned` | 07 |
| Text index | DDL、SHOW、text seek semantics | `planned` | 07 |
| Point index | DDL、SHOW、spatial seek semantics | `planned` | 07 |
| Full-text index | DDL、analyzer/config、queryNodes/queryRelationships、score/options | `planned` | 08 |
| Vector index | DDL、dimension/similarity/quantization/filter properties、SHOW | `planned` | 08 |
| SEARCH | MATCH/OPTIONAL MATCH vector ANN subclause、filter、LIMIT、SCORE | `planned` | 08 |
| LOAD CSV | headers、field parsing、URI source、periodic transaction composition | `planned` | 08 |
| IN TRANSACTIONS | batch size、error behavior、status output、native transaction boundaries | `planned` | 08, 10 |
| IN CONCURRENT TRANSACTIONS | concurrent batching、DISJOINT BY semantics、SQLite serialized commit | `planned` | 08, 10 |
| EXPLAIN | semantic validation + plan without execution | `partial` | 04, 10 |
| PROFILE | execution + operator runtime counters | `partial` | 04, 10 |
| SHOW current graph surfaces | functions/procedures/indexes/constraints/current graph type | `planned` | 06–08 |
| Error compatibility | syntax position、semantic/type/constraint failures、transaction errors | `partial` | 03–10 |

### Phase 03 frontend evidence

Phase 03 的 inherited openCypher frontend regression 固定为：4,224 个合法 query/query-precondition parser inputs 全部 parse；3,312 个非 compile-error `executing query` 全部通过 frontend validation；585 个 compile-time-error `executing query` 中，当前仍有 16 个由后续 owner 明确接管（7 个 procedure signature、1 个完整 built-in function inventory、8 个 aggregation/DISTINCT `ORDER BY` visibility/grouping）。`phase03_parser_tck` 锁定这 16 个 scenario 的精确集合，而不是“最多 16 个”的数量门槛；后续只能显式收缩，不能通过等量替换掩盖 regression。

`tests/fixtures/cypher25` 当前包含 15 个 frozen Profile fixtures，其中 12 个 parser-positive fixture 由 `phase03_parser_tck` 直接执行；新增覆盖 query preamble/options、braced conditional `UNION`、Match Mode + numeric-start parameter，以及 Cypher 25 `GROUP BY` / `RETURN ALL`。Fixture 的 `execution: planned` 仍表示完整 result semantics 尚由后续 owning Phase 完成，不与 Phase 03 parser evidence 混淆。

这些证据只证明 parser/semantic/type foundation，不把尚未执行的 result semantics 标为 `done`。

### Phase 04 read execution evidence

Phase 04 的 targeted suite 覆盖真实 parser -> planner -> executor -> version-aware storage 路径，包括 multi-pattern、directed/incoming/undirected expansion、OPTIONAL null preservation、property access、basic `count()` / DISTINCT、ordering/skip/limit、Path/Node tagged value materialization、historical Snapshot、Graph View execution boundary、typed adjacency seek、external sort spill 与 missing-derived-statistics conservative planning。SQL Bridge `.load` probe 同时验证 scalar envelope 与 `lithograph_rows` row encoding 一致，Native smoke 验证 `COLUMNS -> ROW* -> SUMMARY` 和 callback cancel / `SQLITE_INTERRUPT`。

这些证据只把上述 family 提升为 `partial`。例如 `RETURN *`、WITH/LET、grouped aggregation、完整 built-in function inventory、variable/quantified pattern、match/path mode、subquery/composition 仍由 Phase 06 / 10 的 frozen Profile acceptance 闭合。

### Phase 05 mutation execution evidence

Phase 05 的 67 个 targeted scenarios 覆盖真实 parser -> planner -> executor -> version-aware storage 路径，包括 Node/Relationship CREATE/INSERT、property/label SET/REMOVE、DELETE/DETACH DELETE、MERGE match/create 与 `ON MATCH` / `ON CREATE` foundation、multi-row clause barrier、Graph View write boundary、canonical net delta、empty-delta Commit、Branch CAS、historical read、stale writer、caller-owned transaction/savepoint、host interrupt 与全 PropertyValue family 的持久化边界。它们同时验证 write projection 的 DISTINCT/grouping/order/expression lowering、aggregate ORDER BY validation、已支持 function 的 argument/null semantics、zero-row unsupported function rejection、mutating EXPLAIN 与实际 lowering 一致，以及未拥有语义不会被静默近似执行。SQL Bridge `.load` probe 验证 invocation savepoint、scalar length failure、rows read-only、Graph View violation 与 reopen history；Native smoke 验证 mutating summary、callback cancel 和 fault-injection rollback。

这些证据把四个 mutation family 提升为 `partial`，但不提前完成 Phase 06 的 SET map merge/replace 完整语义、FOREACH、advanced CREATE/INSERT variants、correlated/multi-row MERGE edge cases 或相邻完整 query-composition inventory。Fixed-offset ZonedDateTime 已持久化 round-trip；IANA named-zone 的 timezone/DST 规则仍由 Phase 06 闭合，当前 write 会明确拒绝而不会静默改写值。

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

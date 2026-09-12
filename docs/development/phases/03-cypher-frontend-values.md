# Phase 03：Cypher Frontend and Value Semantics

**状态：`done`**

## 1. 目标

建立 `CY25-2026.08` 的 lexer/parser/AST/scope/type/value foundation，使后续 executor 不再从字符串或 SQL alias 猜测 Cypher semantics。

## 2. 依赖

- Phase 00–02 `done`。

## 3. Design Inputs

- `docs/design.md` 第 3、5–7、13 节；
- `docs/development/cypher25-compatibility.md`。

## 4. Features

### Feature 03.1 Lexer/parser

覆盖 frozen Profile grammar，包括：

- clauses / composed queries；
- pattern / path syntax；
- Graph Type / index DDL；
- `SEARCH`；
- vector/UUID/string interpolation；
- query prefix；
- error line/column/span。

Parser implementation 可以依法参考 GraphQLite/openCypher grammar，但输出必须进入 Lithograph-owned AST。

### Feature 03.2 Scope and semantic analyzer

- variable binding/import/export；
- subquery isolation；
- alias visibility；
- pattern variable categories；
- aggregation grouping rules；
- write/read clause legality；
- schema/index command validation。

### Feature 03.3 Value model

实现 runtime `Value` / persistent `PropertyValue` 分离，覆盖：

- null/bool/int64/float/string；
- list/map；
- node/relationship/path refs；
- temporal/duration/point；
- vector coordinate type/dimension；
- UUID。

### Feature 03.4 Type semantics

- type inference；
- type predicate/cast；
- nullability；
- numeric coercion；
- comparison/equality/order；
- property-type validation；
- function signature resolution。

### Feature 03.5 Parameters and Lithograph JSON

- JSON params -> exact Cypher values；
- typed tagged JSON result encoding；
- INTEGER64、NaN/Infinity、temporal timezone、vector、UUID round-trip 不丢失。

## 5. Acceptance

- [x] compatibility matrix lexical/parser families 有完整 inventory；
- [x] openCypher parser fixtures 与 Cypher 25 grammar fixtures 运行；
- [x] invalid syntax 返回 stable line/column；
- [x] scope isolation/correlation negative fixtures 通过；
- [x] value/type/null comparison fixture 通过；
- [x] int64/vector/temporal/UUID params/result round-trip；
- [x] parser/semantic layer 不访问 SQLite graph rows；
- [x] AST 不携带 GraphQLite/Cypher-to-SQL implementation-specific node。
- [x] `lithograph_validate()` 接入真实 parser + semantic/type/schema validation，success JSON 为 `{valid: true, cypherProfile}`，不执行 query。

## 6. Review

重点检查 grammar coverage 是否通过 special-case 拼接、scope/type 是否推迟给 executor 猜测、JSON adapter 是否改变 Cypher type。

Phase-level review 已闭环：

- parser 输出 Lithograph-owned AST；semantic/type 层只依赖 AST/query text，不访问 SQLite graph rows，也没有把 GraphQLite/Cypher-to-SQL implementation node 暴露给后续 planner；
- frozen grammar review 补齐并锁定了 Cypher query preamble/options、`EXPLAIN` / `PROFILE` 组合顺序、`RETURN/WITH ALL`（包括 parenthesized expression）、精确 Match Mode、数字开头 parameter、braced conditional `UNION` 与 `GROUP BY` parser surface；空 `WHEN ... THEN` / `ELSE` 和非法 Match Mode 组合会在 parser boundary 拒绝；
- downstream 会改变行为的 frontend discriminator 不再在 lowering 时丢失：query options、`ALL/DISTINCT`、Match Mode、`ORDER BY` direction、`MERGE ON CREATE/ON MATCH`、`SET =/+=`、transaction retry fallback、`LOAD CSV` modifiers、Label/Relationship Type expression、Schema/Index/Graph Type name/operation 等均保留为 Lithograph-owned typed AST；multi-token discriminator 由 parse-tree rule 判断，不依赖空白/comment spelling；Label/Relationship Type 连续 negation 也按显式 parser operator node 计数，因此 `!` 之间存在合法 layout/comment 时不会丢失 negation 层数；braced/conditional subquery 的 scope/output traversal 同步修正；
- openCypher inherited parser corpus 固定覆盖 4,224 个合法 query/query-precondition；3,312 个非 compile-error `executing query` 全部通过 frontend validation；585 个 compile-time error 中 Phase 03 可静态判定的场景全部拒绝；
- Phase 06 已关闭当时 16 个 deferred scenario 中的完整 built-in function inventory 与 8 个 aggregation/DISTINCT `ORDER BY` visibility/grouping gap；当前只剩 7 个 procedure catalog/signature scenario 由后续 procedure owner 接管，并显式记录 1 个被 Cypher 25 Match Mode 新语义取代的旧 relationship-reuse scenario；`phase03_parser_tck` 锁定精确集合，不能用等量替换掩盖 regression；
- CALL import、LOAD CSV binding、relationship direction、YIELD alias、escaped identifier、grouping reference、unary integer boundary 与 expression-subquery kind 均由 AST 结构驱动；raw query text 不再承担 scope/type/legality 判断，interpolation fragment error span 会映射回原始 query 的全局 line/column；
- scope/correlation、graph-element category、pattern predicate、write/read clause legality、property type、literal range、numeric/null/type comparison、parameter legality均已有 negative fixtures；comparison 会对左右 operand 对称执行静态类型验证，`CASE` 通过显式 alternative AST 节点提取 `THEN`/`ELSE` 结果类型，不把 simple `CASE` operand 误当作结果；
- 40 层 nested List TCK query 曾触发小栈 stack overflow；AST traversal 改为 iterative DFS，type inference 增加 transparent-expression peeling 后，在 2 MiB thread stack 与正式 TCK suite 中均通过；
- runtime `Value` 与 persistent `PropertyValue` 分层；Property legality 不反向改变 format 1 LCE1 physical encoding，Phase 02 frozen golden hash 保持 `cea822c1c96dd7c456beae36aa4e9e89d8d3cad8658aa450b3ede2887ade4440`；
- direct comparison 与 `ORDER BY` total ordering 使用独立 value contract；Cypher equality 对不同 value family 返回 `false`（保留 numeric cross-type、null three-valued logic 与同-family 特殊规则），与 vendored openCypher equality corpus 一致；同-family Map/Node/Relationship/List/Path/Vector/Point/Temporal/numeric/NaN/null ordering 均有行为测试，公开可构造的 malformed `PathValue` 在 ordering boundary 返回 `ValueError` 而不是 panic，UUID ordering 在 frozen public evidence 未定义前不臆造；
- Lithograph JSON 对 INTEGER64、非有限 Float、reserved `$type` Map、Node/Relationship/Path、Temporal、Point、全部 Vector coordinate types 与 UUID 做 exact round-trip/error validation；Duration 输出由 component value 重新生成 canonical text，而不是保留任意输入 spelling；
- semantic/type/value/parser lowering 实现按职责拆分，production Rust 文件均满足 650 行 budget；repository jscpd duplicated lines 为 0.86%。

## 7. 验证证据

- `cargo test --locked -p lithograph-core --test phase03_frontend --test phase03_ast --test phase03_value_order`：22/22 + 4/4 + 4/4，共 30 个 Phase 03 core integration tests 通过；
- `cargo test --locked -p lithograph-test-support --test phase03_parser_tck`：4/4 通过；除锁定 4,224 parser corpus、3,312 frontend-success corpus、585 compile-error corpus、精确 7 个 deferred 与 1 个 Cypher 25 superseded scenario 外，还直接执行 17 个 `CY25-2026.08` fixture inventory 中的 5 个显式 parser-positive fixtures；
- Phase 02 regression：`phase02_storage` 13/13、`phase02_checkpoint_integrity` 8/8 通过；
- `cargo test --locked -p lithograph-extension --all-features`：8/8 通过；真实 SQLite `lithograph_validate()` 与 Native ABI validation/error-category surface 通过；
- `cargo make quality`：vendor/LCE1 golden、file budget、format、workspace Clippy、Rustdoc、complexity、duplicate、unused dependency、supply-chain、coverage 全部通过；coverage 为 regions 82.53%、functions 84.53%、lines 84.24%，`value_order.rs` line coverage 为 88.76%；
- `scripts/ci.sh`：SQLite 3.51.0 与最低 SQLite 3.45.0 的真实 `.load`、Phase 01/02/03 probes、artifact inspection、Native ABI、compatibility harness/inventory 全部通过。Harness self-check 中 1 个 intentional failure 是 harness 自检预期，不是产品失败。

## 8. 完成条件

Frontend/value foundation 已满足 read planner 前置合同；Phase 03 保持 `done`。Phase 06 已闭合 aggregation/order/current-query function semantics；后续 procedure catalog、Schema/Search 与 release closure 仍按 compatibility matrix 的 owning Phase 执行，不能把 frontend foundation 单独误标为完整 Cypher execution。

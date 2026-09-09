# Phase 03：Cypher Frontend and Value Semantics

**状态：`planned`**

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

- [ ] compatibility matrix lexical/parser families 有完整 inventory；
- [ ] openCypher parser fixtures 与 Cypher 25 grammar fixtures 运行；
- [ ] invalid syntax 返回 stable line/column；
- [ ] scope isolation/correlation negative fixtures 通过；
- [ ] value/type/null comparison fixture 通过；
- [ ] int64/vector/temporal/UUID params/result round-trip；
- [ ] parser/semantic layer 不访问 SQLite graph rows；
- [ ] AST 不携带 GraphQLite/Cypher-to-SQL implementation-specific node。
- [ ] `lithograph_validate()` 接入真实 parser + semantic/type/schema validation，success JSON 为 `{valid: true, cypherProfile}`，不执行 query。

## 6. Review

重点检查 grammar coverage 是否通过 special-case 拼接、scope/type 是否推迟给 executor 猜测、JSON adapter 是否改变 Cypher type。

## 7. 完成条件

Frontend/value foundation 可支撑 read planner；Phase 04 转 `ready`，compatibility matrix 对已完成 family 更新真实状态。

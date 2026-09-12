# Phase 07：Schema, Constraint and Standard Indexes

**状态：`done`**

## 1. 目标

完整实现 Cypher 25 Graph Type、Constraint 和 lookup/range/text/point index，并把定义纳入 versioned Schema history。

## 2. 依赖

- Phase 05–06 `done`。

## 3. Design Inputs

- `docs/design.md` 第 4.4、7.7、8、10–11 节；
- `docs/development/cypher25-compatibility.md`。

## 4. Features

### Feature 07.1 Canonical Schema object

- Graph Type canonical representation；
- element type / property type；
- constraints；
- index definitions；
- canonical schema hash；
- Commit -> schema hash。

### Feature 07.2 Graph Type commands

完整实现 frozen Profile current graph commands：

- set/extend/alter/drop element semantics；
- open graph type；
- identifying/implied labels/types；
- `SHOW CURRENT GRAPH TYPE` string/virtual-graph surface。

### Feature 07.3 Constraints

- KEY；
- UNIQUE；
- NOT NULL/existence；
- property type；
- node/relationship variants；
- existing-data validation；
- write-time validation。

Graph View 不形成 per-view Schema 或 per-view Constraint。Graph Type、KEY、UNIQUE、existence 与 property type validation 始终针对目标 Commit 的完整 versioned Schema 和 mutation candidate canonical graph state；view 外 element 可以导致正常 `SCHEMA_ERROR` / `CONSTRAINT_ERROR`。`MERGE` matching 仍只观察 view 内 graph data，不能把 hidden match 偷偷提升为可见来规避 constraint conflict。

### Feature 07.4 Lookup/range/text/point indexes

- DDL / IF EXISTS/IF NOT EXISTS semantics；
- versioned definitions；
- derived tables；
- build/drop/rebuild；
- `SHOW INDEXES`；
- planner seeks；
- historical snapshot fallback/cache。

Index candidate 可以来自完整 Snapshot 的 derived index，但进入 Cypher-visible row 前必须执行 Phase 04 Graph View visibility check；不能让 hidden candidate 影响 result cardinality。Graph View 不创建 per-view index definition 或第二份 versioned Schema。

### Feature 07.5 Schema merge hooks

为 Phase 09 暴露 schema logical slots、schema diff 和 post-merge validation API。

## 5. Acceptance

- [x] empty graph 和 populated graph Graph Type fixtures 通过；
- [x] invalid existing data 设置 constraint 原子失败；
- [x] write violation 不生成 Commit；
- [x] Graph View 隐藏一个已占用 KEY/UNIQUE value 的 element 时，view 内 CREATE/MERGE 仍由完整图 constraint 检测冲突并返回 `CONSTRAINT_ERROR`，不产生 view-local uniqueness；
- [x] Schema change 自身生成 Commit；
- [x] time-travel 到旧 Commit 返回旧 Graph Type/index definitions；
- [x] Branch 上独立 Schema 变化互不污染；
- [x] lookup/range/text/point planner seek 与 scan result 相同；
- [x] ordered/String/spatial predicate 在 Property Type 未证明兼容时不会通过 derived index 吞掉 scan 路径本应产生的 `TYPE_ERROR`；有 versioned Property Type proof 时对应 range/text/point seek 仍真实进入具体 index；
- [x] Vector coordinate type alias 在 Schema 中 canonicalize，runtime constraint validation 与 `SHOW CURRENT GRAPH TYPE` 使用同一 canonical Vector type；
- [x] Schema DDL 在 Commit/ref 已写入 invocation savepoint、derived index 构建阶段发生 interrupt 时完整 rollback，不留下 Branch move 或半成品 Schema；
- [x] 同一 Graph View 下 lookup/range/text/point index seek 与 view-aware scan result 相同，hidden index candidate 不泄漏；
- [x] Graph Type / Constraint / Index DDL 与 `graphView` 同时提交时返回 `INVALID_ARGUMENT`，不产生 Schema Commit；
- [x] 删除 derived index cache 后查询仍正确并可重建；
- [x] SHOW surfaces 与 frozen Profile 对齐；
- [x] compatibility matrix Schema/standard-index families 全部 `done`。

## 6. Review

检查 schema 是否被放成 database-global mutable state、Graph View 是否错误投影成 per-view Schema/Constraint、hidden UNIQUE/KEY conflict 是否被绕过、KEY/UNIQUE 是否使用完整 Cypher value equality（包括 Integer/Float、复合/List key、NaN 与 Point/Vector signed zero）、同一 schema 上冲突 Property Type Constraint 是否被拒绝、Property Type union 是否 canonical normalization 并保持 frozen nullability contract、`SHOW CURRENT GRAPH TYPE` 是否输出 parser 可重用的 canonical type syntax（包括 Vector）、Range cache 是否覆盖全部 Property Value family 且 exact seek 与 scan 等价、ordered/String/spatial seek 是否只有在目标 Commit 的 Property Type Constraint 足以证明类型兼容时才过滤其它类型、Text/Point typed index 是否在缺少类型保证时错误用于通用 existence predicate、composite Range 是否真的选择复合 index 而不是被单列 index 的通用 `IndexSeek` 断言掩盖、index definition 是否脱离 Commit、constraint validation 是否有 TOCTOU、historical query 是否误用 current index definition。

## 7. 完成条件

Schema/Constraint/standard index compatibility closure；Phase 08 转 `ready`。

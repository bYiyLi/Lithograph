# Phase 07：Schema, Constraint and Standard Indexes

**状态：`planned`**

## 1. 目标

完整实现 Cypher 25 Graph Type、Constraint 和 lookup/range/text/point index，并把定义纳入 versioned Schema history。

## 2. 依赖

- Phase 05–06 `done`。

## 3. Design Inputs

- `docs/design.md` 第 8、10–11 节；
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

### Feature 07.4 Lookup/range/text/point indexes

- DDL / IF EXISTS/IF NOT EXISTS semantics；
- versioned definitions；
- derived tables；
- build/drop/rebuild；
- `SHOW INDEXES`；
- planner seeks；
- historical snapshot fallback/cache。

### Feature 07.5 Schema merge hooks

为 Phase 09 暴露 schema logical slots、schema diff 和 post-merge validation API。

## 5. Acceptance

- [ ] empty graph 和 populated graph Graph Type fixtures 通过；
- [ ] invalid existing data 设置 constraint 原子失败；
- [ ] write violation 不生成 Commit；
- [ ] Schema change 自身生成 Commit；
- [ ] time-travel 到旧 Commit 返回旧 Graph Type/index definitions；
- [ ] Branch 上独立 Schema 变化互不污染；
- [ ] lookup/range/text/point planner seek 与 scan result 相同；
- [ ] 删除 derived index cache 后查询仍正确并可重建；
- [ ] SHOW surfaces 与 frozen Profile 对齐；
- [ ] compatibility matrix Schema/standard-index families 全部 `done`。

## 6. Review

检查 schema 是否被放成 database-global mutable state、index definition 是否脱离 Commit、constraint validation 是否有 TOCTOU、historical query 是否误用 current index definition。

## 7. 完成条件

Schema/Constraint/standard index compatibility closure；Phase 08 转 `ready`。

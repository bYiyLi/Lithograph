# Phase 05：Mutation, Transaction and Commit

**状态：`planned`**

## 1. 目标

把 Cypher graph mutation、SQLite transaction 和 immutable Commit history 连成一个原子 write path。

## 2. 依赖

- Phase 02–04 `done`。

## 3. Design Inputs

- `docs/design.md` 第 4.4、7–10、13–14 节。

## 4. Features

### Feature 05.1 Mutating operators

实现 foundation semantics：

- `CREATE` / `INSERT` Node/Relationship；
- `SET` property/labels；
- `REMOVE` property/labels；
- `DELETE` / `DETACH DELETE`；
- `MERGE` foundation；
- multi-row write pipeline。

所有 graph-data write 继承 Phase 04 Graph View boundary。Selector 在整个 query 内固定，但 visibility 按当前 clause 的 input graph state 计算，因此后序 clause 必须观察前序 clause 已完成的 writes。不可见既有 element 不能被 mutation 定位；每个 mutating clause 的完整 effect 结束后，新建或修改且仍存在的 Node 必须位于 view 内，Relationship 两端必须可见；Label mutation 或 delete/detach 若让 element 越界或要求修改不可见 Relationship，返回 `GRAPH_VIEW_VIOLATION` 并回滚整个 top-level write。

### Feature 05.2 Delta builder

- 把 executor mutation 归一化为 logical slots；
- 同一 query 对同一 slot 多次修改 canonicalize 到最终 delta；
- Node delete 显式包含 Relationship delete；
- before-state 保留用于 patch/conflict/inverse。

### Feature 05.3 Commit writer

每个成功 mutating query：

- pin branch head；
- execute；
- canonicalize Layer；
- hash Layer / Commit；
- write commit；
- compare-and-move branch；
- output commit id/counters。

No-op mutating query 仍创建 Commit。

### Feature 05.4 Transaction semantics

覆盖：

- auto-commit；
- caller-owned `BEGIN/COMMIT/ROLLBACK`；
- SQLite savepoint interaction；
- query failure rollback；
- extension error/panic conversion；
- stale branch head -> `BRANCH_HEAD_MOVED`。
- SQL Bridge per-invocation SAVEPOINT 与 caller-owned transaction composition。

### Feature 05.5 Write/read barriers

实现 Cypher read-after-write / write-after-read 所需 Eager/materialization barrier，防止同一 query 因 streaming mutation 看到自己刚写的数据而产生错误 cardinality。Graph View visibility 使用 barrier 所对应的 Cypher graph state：不能固定为 query-start membership，也不能提前看到后序 clause 的 write。

## 5. Acceptance

- [ ] CREATE Node/Relationship 后当前 head 前进一个 Commit；
- [ ] Graph View 内 MATCH 到的隐藏 element 不可被 SET/REMOVE/DELETE/MERGE 修改；
- [ ] `CREATE` / `INSERT` clause 完成后新 Node 不满足当前 Graph View，或 Relationship endpoint 不可见时返回 `GRAPH_VIEW_VIOLATION`，不生成 Commit；
- [ ] 在 required Label view 中，`CREATE (n:Required)` 后续 read clause 能观察该 staged Node；`CREATE (n) SET n:Required` 在前一个 `CREATE` clause boundary 返回 `GRAPH_VIEW_VIOLATION`；
- [ ] `SET` / `REMOVE` Label 使可见 Node 离开当前 view 时原子返回 `GRAPH_VIEW_VIOLATION`；
- [ ] delete/detach 需要隐式删除 view 外 Relationship 时原子拒绝，不发生跨 view mutation；
- [ ] SET/REMOVE/DELETE/DETACH 在历史 Commit 可正确 time-travel；
- [ ] no-op mutating query 产生 empty-delta Commit；
- [ ] multi-row write 只产生一个 top-level Commit；
- [ ] query 中途失败 Branch head / Commit / Layer 全部 rollback；
- [ ] SQL Bridge mutating invocation 发生 parser-after-write/fault-injection/error 时回滚到 invocation savepoint；
- [ ] autocommit 下两个 scalar write invocation 前一成功、后一失败时保持两个独立 invocation；caller-owned outer rollback 能撤销其中全部 graph invocation；
- [ ] `lithograph_rows` 对 mutating query 返回 `READ_ONLY_ADAPTER`，virtual-table rescan 不执行任何 write；
- [ ] Native callback cancel 发生在尚未 durable 的 write 时返回 `SQLITE_INTERRUPT` 并回滚整个 write；
- [ ] outer SQLite rollback 移除其中全部 graph Commits；
- [ ] outer SQLite commit 后其它 connection 一次看到完整 commit chain；
- [ ] two writers 基于同一 branch base 时一个成功、另一个准确 `BRANCH_HEAD_MOVED`；
- [ ] read/write barrier regression fixtures 证明每个 clause 看到全部前序 write、看不到后序 write，且 Graph View membership 基于同一个 clause graph state；
- [ ] reopen DB 后 history 与 current head 一致。

## 6. Review

重点检查 Branch move 是否可能脱离 Layer transaction、Graph View 是否能通过 direct element reference/MERGE/DETACH 绕过、violation rollback 是否残留 identity/history、multi-row write 是否错误地产生 per-row Commit、MERGE 是否依赖非原子先查后写。

## 7. 完成条件

Versioned write vertical slice 完整通过；Phase 06 转 `ready`。

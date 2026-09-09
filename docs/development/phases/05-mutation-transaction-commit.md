# Phase 05：Mutation, Transaction and Commit

**状态：`planned`**

## 1. 目标

把 Cypher graph mutation、SQLite transaction 和 immutable Commit history 连成一个原子 write path。

## 2. 依赖

- Phase 02–04 `done`。

## 3. Design Inputs

- `docs/design.md` 第 7–10、13–14 节。

## 4. Features

### Feature 05.1 Mutating operators

实现 foundation semantics：

- `CREATE` / `INSERT` Node/Relationship；
- `SET` property/labels；
- `REMOVE` property/labels；
- `DELETE` / `DETACH DELETE`；
- `MERGE` foundation；
- multi-row write pipeline。

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

实现 Cypher read-after-write / write-after-read 所需 Eager/materialization barrier，防止同一 query 因 streaming mutation 看到自己刚写的数据而产生错误 cardinality。

## 5. Acceptance

- [ ] CREATE Node/Relationship 后当前 head 前进一个 Commit；
- [ ] SET/REMOVE/DELETE/DETACH 在历史 Commit 可正确 time-travel；
- [ ] no-op mutating query 产生 empty-delta Commit；
- [ ] multi-row write 只产生一个 top-level Commit；
- [ ] query 中途失败 Branch head / Commit / Layer 全部 rollback；
- [ ] SQL Bridge mutating invocation 发生 parser-after-write/fault-injection/error 时回滚到 invocation savepoint；
- [ ] outer SQLite rollback 移除其中全部 graph Commits；
- [ ] outer SQLite commit 后其它 connection 一次看到完整 commit chain；
- [ ] two writers 基于同一 branch base 时一个成功、另一个准确 `BRANCH_HEAD_MOVED`；
- [ ] read/write barrier regression fixtures 通过；
- [ ] reopen DB 后 history 与 current head 一致。

## 6. Review

重点检查 Branch move 是否可能脱离 Layer transaction、rollback 是否残留 identity/history、multi-row write 是否错误地产生 per-row Commit、MERGE 是否依赖非原子先查后写。

## 7. 完成条件

Versioned write vertical slice 完整通过；Phase 06 转 `ready`。

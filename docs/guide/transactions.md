# 事务与并发

当前开发基线区分三种边界：

| 使用方式 | 成功后的版本语义 |
| --- | --- |
| 普通 top-level mutation | 一次 execution 一个 Commit |
| caller-owned SQLite `BEGIN...COMMIT` 包含多次普通 mutation | durability 可整体 rollback，但历史中仍是每次 execution 各一个 Commit |
| Lithograph explicit transaction | 多次标准 execution 最终最多一个 Commit |
| `CALL { ... } IN TRANSACTIONS` | 每个 mutating inner batch 独立 Commit |

## 多 execution 一个 Commit

同一 SQLite connection：

```sql
SELECT lithograph_init();
SELECT lithograph_tx_begin('{"author":"app","message":"create two people"}');

SELECT lithograph('CREATE (:Person {name:''张三''}) FINISH');
SELECT lithograph('CREATE (:Person {name:''李四''}) FINISH');

SELECT lithograph(
  'MATCH (p:Person) RETURN p.name ORDER BY p.name'
);

SELECT lithograph_tx_commit();
```

active transaction 内的普通 `lithograph()` / `lithograph_rows()` 自动使用同一 staged graph/Schema/Index state，前一条成功 execution 的 staged writes 对后一条可见。每条 staged execution 的 `summary.commit` 为 `null`；最终 Commit 只由 `lithograph_tx_commit()` 返回。

取消：

```sql
SELECT lithograph_tx_begin('{}');
SELECT lithograph('CREATE (:Discarded) FINISH');
SELECT lithograph_tx_abort();
```

`tx_begin` options：`branch`、`expectedHead`、`author`、`message`。active transaction 内 query options 不能切换 branch/at/author/message/version context；`graphView` 仍可按 execution 指定。

任一 execution parse/type/constraint/I/O/interrupt/error，或 `lithograph_rows()` 在 success summary 前被关闭，都会 fail-closed abort 整个 explicit transaction。之后必须重新 begin。

## Explicit transaction 中的边界

普通 `LOAD CSV`、Managed Semantic query 等 external I/O 可以执行；它们可能延长 writer hold。拥有独立 committed-target/version lifecycle 的 operation（例如 committed-target rebuild、ref/version mutation）继续拒绝 active explicit transaction。

`CALL { ... } IN TRANSACTIONS` / `IN CONCURRENT TRANSACTIONS` 自己拥有 inner transaction boundary，因此只能在 SQLite autocommit、且没有 active Lithograph explicit transaction 时运行。已经成功 commit 的 earlier batch 不会因为 later batch error 或 outer rows early-close 被撤销。

## Caller-owned SQLite transaction

```sql
BEGIN;
SELECT lithograph('CREATE (:Event {name:''one''}) FINISH');
SELECT lithograph('CREATE (:Event {name:''two''}) FINISH');
ROLLBACK;
```

outer rollback 会撤销这两个 Commit/ref 的 SQLite durability，但它们并不会被“合并成一个 Commit”。需要 version atomicity 时使用 Lithograph explicit transaction。

## 并发与 cursor

SQLite single-writer 仍是最终写锁边界。side-effecting `lithograph_rows()` 在 terminal summary 前保持可 rollback boundary，因此慢 consumer 会延长 writer hold；及时消费或 close cursor。BUSY 可以有限重试，但写失败后先确认实际 history/state，不盲目 replay。

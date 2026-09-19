# 事务与并发

适用 v0.2.1；普通 query / caller-owned SQLite transaction 语义仍与 v0.1.0 一致，SQL explicit transaction 从 v0.2.1 起可用。首先区分“哪些数据一起落盘”和“历史中形成几个 Commit”。它们不是同一个问题。

| 使用方式 | 失败边界 | 成功后的 Commit 数 |
| --- | --- | --- |
| 普通 top-level graph / Schema / Index 写查询 | 整次查询 | 1，即使 effective delta 为空 |
| 外层 SQLite BEGIN 包含多次普通写查询 | 整个外层事务可回滚 | 每次写查询各 1 个，不合并 |
| SQL / Native explicit transaction | 任一 execution 失败使整个事务 abort | 有成功 mutation 时恰好 1；只有读取时 0 |
| Native Cypher `IN TRANSACTIONS` | 每个 batch 独立，前面成功 batch 不被后续错误回滚 | 每个 mutating batch 1 个 |

## SQL 外层事务

以下在独立空库中依次执行：

```sql
SELECT lithograph_init();
BEGIN;
SELECT lithograph('CREATE (:Event {name:''one''}) FINISH');
SELECT lithograph('CREATE (:Event {name:''two''}) FINISH');
COMMIT;
SELECT lithograph('CALL lithograph.log(''branch/main'',10)');
```

历史中有 Root 加两次写入，共三个 Commit。Commit Descriptor 在外层 COMMIT 前尚未获得持久性；外层 rollback 后不能继续把它当作已经保存的版本。

```sql
BEGIN;
SELECT lithograph('CREATE (:Event {name:''discarded''}) FINISH');
ROLLBACK;
SELECT lithograph('MATCH (e:Event) RETURN e.name AS name ORDER BY name');
```

最后只有 `one`、`two`。普通查询失败会撤销该 invocation 的写入；外层事务后续提交还是回滚由宿主决定，不能把普通 SQL Bridge invocation 的单次失败等同于 SQL / Native explicit transaction 的整体自动 abort。

使用 Python 时明确设置事务策略。示例使用 `isolation_level=None`，SQL BEGIN/COMMIT 由应用显式控制。不要依赖不同 binding 的默认 implicit transaction 行为。

## 多次执行只产生一个 Commit

普通 SQLite driver 直接使用 SQL 封装：

```sql
SELECT lithograph_tx_begin('{"author":"app","message":"create two people"}');
SELECT lithograph_tx_execute('CREATE (:Person {name: ''张三''}) FINISH');
SELECT lithograph_tx_execute('CREATE (:Person {name: ''李四''}) FINISH');
SELECT lithograph_tx_execute('MATCH (p:Person) RETURN p.name ORDER BY p.name');
SELECT lithograph_tx_commit();
```

四条 lifecycle statement 必须使用同一 SQLite connection。不要在外面再执行 `BEGIN/COMMIT`；`lithograph_tx_begin` 自己取得 writer，commit/abort 自己结束 SQLite transaction。使用 connection pool 时，在 terminal operation 完成前不得归还或换用 connection。需要取消时：

```sql
SELECT lithograph_tx_abort();
```

已有 C binding 的应用可继续使用同一生命周期：

```text
lithograph_v1_tx_begin(db, {branch,expectedHead,author,message})
  → lithograph_v1_tx_execute(db, query A)
  → lithograph_v1_tx_execute(db, query B)
  → lithograph_v1_tx_commit(db)
```

真实 C 函数接收 UTF-8 指针、长度及输出指针，完整签名见 [Native API](../reference/native-api.md)。SQL 和 Native 只是同一 connection-local transaction core 的两个 adapter；不存在在普通 SQL 事务末尾“自动 squash”的配置。

begin 必须在 SQLite autocommit、没有 active Lithograph explicit transaction 时运行；`expectedHead` 可要求当前 Branch head 等于已知 Commit。begin 取得 single-writer ownership，后续 execution 看到此前 staged writes。Constraint 按 statement 即时验证，不自动延迟到最后。

tx_execute 的 `summary.commit` 为 `null`，新分配 ID 是 provisional。只有 tx_commit 成功返回的最终 Commit 才是这组操作的版本身份；最终 counters 描述 base→final 的净变化，不简单累加每条 statement 的 counters。

任一 execution 的解析、参数、约束、I/O、取消或 callback 错误会 fail-closed abort；不能 catch 后继续执行或 commit 原事务。只有 read 的 transaction 返回原 base，不创建 Commit。有 mutation 即使最终数据改回原值也生成一个 empty-delta Commit。

## Explicit transaction 中不能做什么

禁止 LOAD CSV、Version Procedures、transaction-owning subquery；不要调用普通 SQL Bridge / Native execute / validate / init / integrity 绕过 staged state。纯 `lithograph_version()` 信息查询例外。tx_execute options 只允许自己的 `graphView`，目标 Branch 和 metadata 在 begin 时确定。

事务持有数据库 writer，不在其中等待网络请求、用户审查或无限长外部工作。合并冲突使用 Merge Session，不用一个长事务等待解决。

显式取消使用 tx_abort。没有 active transaction 时调用 execute/commit/abort 返回 `INVALID_ARGUMENT` + `SQLITE_MISUSE`。任一 execute 的参数、query、result/callback 错误都已自动 abort，不能修正参数后在原 transaction 上继续。清理失败为 `INTERNAL_ERROR` 时关闭并丢弃 connection，不把它放回连接池。

## 并发、游标与重试

不同 Branch 不等于不同写锁。SQLite 是单文件写入串行化的最终边界；WAL、synchronous 和 busy timeout 由宿主选择，Lithograph 不静默替应用修改。

读取固定 immutable Snapshot，不会读到半写的 Layer。流式读取必须迭代完成或显式关闭 cursor；不要在同一 connection 的未关闭读取游标之间插入写入，读资源可能仍受保护。

`BUSY` 可采用有上限的等待/退避，但先判断调用是否已经成功提交，不能对任意写入盲目重试。`BRANCH_HEAD_MOVED` 需要重新读取最新 head 并重做业务决策，不会自动 merge。跨线程共享 connection 遵循宿主 SQLite threading mode；连接池不能把一个 active explicit transaction 分给其他请求。

Native callback 非零返回值或宿主 `sqlite3_interrupt()` 可请求取消。中间 ROW 不代表写入已经成功；必须等待 API 返回成功，并在 caller-owned SQL transaction 场景下等待外层 COMMIT。批量独立 transaction 的已提交部分需要另行记录。

可执行示例见 [sql_transaction.py](examples/sql_transaction.py) 和 [native_transaction.c](examples/native_transaction.c)。

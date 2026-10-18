# 事务与 Execution 生命周期参考

适用版本：**v0.3.2** · [产品手册](../guide/README.md) · [接口参考](README.md)

本页定义宿主调用的生命周期；[事务指南](../guide/transactions.md) 提供任务流程，Cypher 的 batch 语法与部分提交规则见 [Transaction Subqueries](../cypher/transaction-subqueries.md)。所有生命周期调用都必须使用同一 SQLite connection，不能在连接池的不同连接间传递事务。

<a id="boundaries"></a>

## 四种事务边界

| 方式 | 版本与 durability | 失败范围 |
| --- | --- | --- |
| 普通 auto-commit execution | 一次 mutating execution 一个 Commit；ref/派生操作按自身合同处理 | 撤销该次未发布写入 |
| caller-owned SQLite `BEGIN...COMMIT` | 多次普通 mutation 各自产生 Commit，SQLite durability 由外层统一提交 | 宿主 ROLLBACK 可撤销全部未提交 Commit/ref |
| `lithograph_tx_begin/commit/abort` | 多次标准 execution 共享 staged state，最终只发布一个 graph Commit | 尚未 finalize-success 的 staged execution 失败或取消时自动 abort 整体 |
| Cypher `IN TRANSACTIONS` | 每个 mutating inner batch 独立 Commit | earlier durable batches 保留，后续失败不整体撤销 |

外层 SQLite transaction 不会自动合并 graph Commit。需要“一次业务变更多条查询一个版本”时选择 Lithograph explicit transaction。SQLite single-writer 边界仍然存在；Branch 不提供独立 writer。

<a id="begin"></a>

## `lithograph_tx_begin(options_json)`

恰好一个 JSON object TEXT；不省略参数，无配置时传 `'{}'`。字段见 [begin options](execution-options.md#transaction-begin-options)。成功返回：

```json
{"baseCommit":"commit/<64-hex-id>"}
```

前置条件是 graph 已初始化、connection healthy、SQLite autocommit、没有 active Lithograph transaction 或尚未关闭的 execution cursor。begin 使用 `BEGIN IMMEDIATE` 取得 writer ownership；纯读事务也会持有 writer，因此需长时间只读时优先使用普通 pinned read。

`expectedHead` 只接受 resolved Commit Descriptor。获取 writer 后不匹配返回 `BRANCH_HEAD_MOVED`，不会留下 active transaction。busy、错误 options、未知 Branch 等也不表示事务已开始；以成功返回为准。不要再包一层 SQLite `BEGIN`，nested explicit transaction 返回 `TRANSACTION_BOUNDARY_REQUIRED`。

<a id="active-execution"></a>

## Active 状态中的 execution

使用普通 `lithograph()` / `lithograph_rows()`；它们共享当前 staged graph/Schema/Index state，前一条成功的写入对后一条可见。每个 execution 都返回正常 columns/rows/counters，但 `summary.commit=null`，不能将其作为已发布版本。

v0.3.1 execution options 只接受 `graphView` 与 `readOnlyNoExternalIO`；v0.3.2 还接受每次独立指定的 `readAccess` / `writeAccess`；begin 确定的 Branch、base 和 metadata 固定。每个安全读取都必须独立传 `readOnlyNoExternalIO:true`；一次成功不会自动把后续 execution 变成安全读取。

Graph/Schema/Index 查询与写入可组合。普通 LOAD CSV、Managed Semantic query 可执行并占用当前 writer；它们失败时整体 abort。Version/ref/checkout/GC、需要 committed target 的维护操作、transaction-owning subquery 等不能嵌套，返回 `TRANSACTION_BOUNDARY_REQUIRED`。具体 Procedure 的限制见各自条目。

execution 在 finalize-success 前发生 parse、semantic、type、constraint、options、I/O、编码、interrupt 或安全拒绝失败时，整个 explicit transaction fail closed：先前成功的 staged writes 一起 rollback。之后 `tx_commit()` / `tx_abort()` 返回 no-active-transaction 的 `INVALID_ARGUMENT`，不能靠重试 commit 恢复已回滚内容。已经 finalize-success 的 statement 不因后续 SQLite summary handoff 失败而重新 abort，详见下文 cursor 生命周期。

<a id="access-controls"></a>

## 每次 execution 的访问规则（v0.3.2）

`readAccess` / `writeAccess` 加在普通 `lithograph()` / `lithograph_rows()` execution 上；`tx_begin` 的 options shape 不变。每次可选择新规则，以本次开始时的 staged state 为保护属性写入的基线，不追溯否定此前可信 execution 的写入。未填写规则不会自动继承上一次策略；宿主需要每次构造并传入。

查询属性隐藏作用于当前 staged read；result 只过滤本次公开图对象。明确拒绝为 `ACCESS_DENIED` + `SQLITE_AUTH`，仍自动 abort 整个 active explicit transaction，之前 staged writes 一并回滚。普通 caller-owned SQLite transaction 的 invocation 失败按原有 SAVEPOINT/outer transaction 合同处理，不擅自提交外层事务。

`IN TRANSACTIONS` / concurrent variant 在原有合法的 autocommit context 中继承整个 execution 的策略。当前失败 batch 不发布；`ON ERROR CONTINUE/BREAK/FAIL` 等保持 Cypher 原有语义，earlier durable batches 不被倒退。不能因为后来 ACCESS_DENIED、取消或结果交付失败，把导入报告成“整体没有副作用”。Version/maintenance Procedure 在 explicit transaction 内原有独立边界限制仍然生效，不因 allowVersionChanges/allowMaintenance 为 true 解除。

完整调用与失败后 Branch head/数据断言见 [访问控制指南](../guide/access-controls.md) 和 [access_controls.py](../guide/examples/access_controls.py)。

<a id="commit"></a>

## `lithograph_tx_commit()`

无参数。先关闭或完整消费全部 execution cursor，再提交。成功形状：

```json
{"commit":"commit/<64-hex-id>","counters":{"nodesCreated":0,"nodesDeleted":0,"relationshipsCreated":0,"relationshipsDeleted":0,"propertiesSet":0,"propertiesRemoved":0,"labelsAdded":0,"labelsRemoved":0,"constraintsAdded":0,"constraintsRemoved":0,"indexesAdded":0,"indexesRemoved":0}}
```

最终 Commit 的 first parent 是 `baseCommit`，metadata 取自 begin。纯读事务返回 base Commit，不创建新 Commit；只要执行过 mutating query，即使净变化为空也创建 empty-delta Commit。这里的 counters 按 base 到最终状态的差异统计，不保证等于各 staged execution counter 的逐项和。

Engine 在提交前发现编码/长度错误，或 SQLite COMMIT 失败时，会尝试整体 rollback；清理失败的 connection 被 quarantine，必须关闭丢弃。仍有 cursor 的 lifecycle 调用先被边界检查拒绝，不等于数据已经提交。宿主必须保留最终返回码，不能把收到一条 staged row 当成 commit 成功。

<a id="abort"></a>

## `lithograph_tx_abort()`

无参数；成功返回 `{"aborted":true}`，rollback staged writes 并退出 active 状态。它不是任意时刻都成功的幂等 no-op：没有 active transaction 时返回 `INVALID_ARGUMENT`。关闭带有未提交事务的 connection 由 SQLite rollback，应用仍应显式关闭 cursor 并 abort，以便发现清理错误。

不要直接执行 SQL `COMMIT` / `ROLLBACK` 操作 Engine-owned transaction；不要在 active 状态修改内部表。宿主若同时维护业务表事务，应明确选择 caller-owned SQLite transaction 或将工作组织到已确认的边界内。

<a id="stream-lifecycle"></a>

## `lithograph_rows()` cursor

visible columns 是 `ordinal,event,data`，事件顺序见 [SQL API](sql-api.md#rows-execution-stream)。推荐完整读取 `columns`、所有 `row` 和一个 `summary`，检查外层 SQLite statement 正常返回 `SQLITE_DONE`（Python 等 binding 正常结束迭代），最后 finalize/close，以便观察整个 SQL statement 的最终状态并释放资源。

成功边界的顺序是：**terminal-ready 时预检 summary 编码与长度 → execution finalize-success → 向 SQLite 暴露 summary event**。因此收到 summary 已证明这次 Lithograph execution 成功，不需要再额外推进一次 `xNext` 到 EOF 才成立；外层 SQL 的最终返回码仍应检查。对于 active explicit transaction，这只表示 staged statement 成功，最终发布仍需 `lithograph_tx_commit()`；caller-owned SQLite transaction 也仍由宿主提交。

`row` 是增量产生的候选结果，不能证明后续行或 terminal-ready 检查会成功。只有内部尚未 finalize-success 的 execution，在 reset/finalize/close 时才属于 incomplete：它被取消，普通未发布写入 rollback，explicit transaction 则整体 abort。若内部已经 finalize-success，后续 close 只回收 adapter/read/spill 资源，不再次取消或回滚；是否 incomplete 不能只按应用是否拿到 summary bytes 判断。

| 消费方式或失败阶段 | 结果 |
| --- | --- |
| 收到成功 summary | execution 已 finalize-success；active transaction 仍需 tx_commit |
| 完整 drain 后正常 `SQLITE_DONE` | 同时确认外层 SQL statement 正常结束；随后 finalize/close 释放资源 |
| 内部尚未 finalize-success 时提前 close | 取消 incomplete execution；普通未发布写入 rollback，active transaction 整体 abort |
| `WHERE event='row'` / 外层 `LIMIT` | 仅影响宿主消费，可能提前关闭或隐藏 summary；不能替代 Cypher WHERE/LIMIT |
| finalize-success 前编码/长度预检失败 | 当前 invocation 报错，撤销未发布写入 |
| finalize-success 后的 SQLite `xColumn` / host handoff 失败 | SQL 可能报错，但 execution 已成功；不能依此推断已发布结果被撤销，也不将已成功 staged statement 改成自动 abort |
| transaction-subquery 在 earlier batch 后失败/关闭 | 已 durable batch 保留；只取消尚未发布状态 |

SQLite 交付 summary 时的分配或宿主处理失败，可能使应用没有拿到 summary，却已经产生成功的 execution。遇到这类不确定结果，关闭 cursor 后核查 Branch head、业务键和 history；在 explicit transaction 内确认 staged state 与 transaction 状态，再决定 commit、abort 或重做。不要因为没有收到 summary 就盲目重试写入或假定自动 rollback。

同一连接有 active reader 时，新的 side-effecting execution、checkout 和 lifecycle 操作可能因边界冲突被拒绝。不要并行复用一个连接承载多个调用；先 close 当前 cursor，再开始下一条语句。query 的 `ORDER BY`、aggregation、DISTINCT 等自身 barrier 仍可能需要 TEMP/spill，streaming 不等于所有 operator 都常量内存。

<a id="cancellation"></a>

## 取消、重试与连接池

取消使用宿主的 `sqlite3_interrupt()` 或 binding 的对应 API。错误 category 为 `INTERRUPTED`，SQLite code 为 `SQLITE_INTERRUPT`；消费线程应检查真实返回码并 finalize。取消与完成可能竞争，收到取消请求并不证明 execution 尚未完成，先检查实际结果和版本状态。

把连接归还池前必须证明：

1. 所有 scalar statement 和 rows cursor 已 finalize/close。
2. 没有 active Lithograph transaction；若本调用仍拥有且未被自动 abort，则显式 abort。
3. `sqlite3_get_autocommit()` 为 true；caller-owned transaction 按宿主策略清理。
4. active Branch 已恢复池约定的基线（通常 `main`）；autocommit 本身不恢复 checkout。
5. 清理无错误、连接未 quarantine；无法证明时关闭丢弃。

`BUSY` 可按宿主有限退避重试；write/transaction-subquery/版本操作失败后先核对实际 head、结果和 durable batches，不能把 SQL error 一概当成“未执行”而重放。可运行的 stream 提前关闭与失败清理示例见 [Integration](../guide/integration.md) 和 [Streaming 示例](../guide/examples/streaming.py)。

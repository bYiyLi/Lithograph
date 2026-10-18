# 让不可信 Cypher 只读并禁止外部 I/O

[产品手册](README.md) · [Cypher 语言手册](../cypher/index.md) · [接口参考](../reference/README.md)

本文适用 **v0.3.3**（`CY25-2026.08`，storage format `3`）。目标：宿主允许调用方提交 Cypher query，但由宿主强制禁止本次 execution 的写入、文件读取、HTTP 和 Provider 调用。需要先具备 [应用集成](integration.md) 的 SQLite connection；以下 SQL 在新的演示库依次执行。

<a id="force-options"></a>

## 1. 由宿主构造 options

`readOnlyNoExternalIO` 是每次 query 的 Boolean option，默认省略或 `false`。服务端必须自己设置为 `true`，不能让请求体中的 options 覆盖它，也不能把原始 SQL execution 权限一并开放给调用方。

以下 Python 只接受 query 和 params，options 由宿主生成。`db` 是已经加载扩展、初始化完成的 connection：

```python
import json

def read_query(db, cypher, params):
    options = {"readOnlyNoExternalIO": True}
    cursor = db.execute(
        "SELECT lithograph(?, ?, ?)",
        (cypher, json.dumps(params, allow_nan=False), json.dumps(options)),
    )
    try:
        return json.loads(cursor.fetchone()[0])
    finally:
        cursor.close()
```

如果宿主还要固定 Branch 或 Graph View，在这个受控构造步骤加入选项；不要把任意客户端 options 合并进来。值 `"true"`、`1`、`null` 均返回 `INVALID_ARGUMENT`，不等价于 Boolean `true`。

<a id="allowed-queries"></a>

## 2. 运行允许的查询

演示数据由可信初始化步骤创建，随后在安全选项下读取：

```sql
SELECT lithograph_init();
SELECT lithograph('CREATE (:SafeDoc {id:''guide'', value:7}) FINISH');
SELECT lithograph(
  'MATCH (d:SafeDoc) CALL (d) { RETURN d.value AS value } RETURN value',
  '{}', '{"readOnlyNoExternalIO":true}'
);
SELECT ordinal,event,data FROM lithograph_rows(
  'MATCH (d:SafeDoc) RETURN d.id AS id',
  '{}', '{"readOnlyNoExternalIO":true}'
);
SELECT lithograph(
  'CALL lithograph.branch.list() YIELD name RETURN name',
  '{}', '{"readOnlyNoExternalIO":true}'
);
```

前三项查询结果分别是值 `7`、包含 `guide` 的完整 `columns → row → summary` stream，以及 Branch `main`。这些读取不创建 Commit。普通 graph read、聚合、只读组合/子查询、历史查询、Schema SHOW 和 Raw Vector SEARCH 均可使用；已明确分类的只读 Version Procedure 也可使用。

<a id="rejected-queries"></a>

## 3. 理解会被拒绝的操作

| 操作 | 为什么不允许 |
| --- | --- |
| CREATE/MERGE/SET/DELETE、Schema/Index DDL | 修改 graph 或 versioned definition |
| Branch/Tag/Commit Data mutation、checkout、Merge resolve/finalize、GC、rebuild | 修改版本引用、连接状态或维护状态 |
| LOAD CSV，包括参数 source 和本地文件 | 可读取宿主文件或发起 HTTP(S) |
| Managed Semantic create/query/rebuild | 可调用 Embedding Provider；即使预期 cache hit 也拒绝 |
| Full-text query Procedure | 可调用宿主注册的 tokenizer；不按某次 tokenizer 配置豁免 |
| CALL … IN TRANSACTIONS / IN CONCURRENT TRANSACTIONS | 拥有独立 transaction lifecycle；只读 inner body 也拒绝 |

这些语法正确的查询会在 prepare 阶段返回 `INVALID_ARGUMENT`，错误消息包含 `options.readOnlyNoExternalIO forbids`。检查遍历完整 AST，零行输入、`WHEN false` 分支、嵌套 CALL、`EXPLAIN` 和 `PROFILE` 都不能绕过。例如 `EXPLAIN CREATE (:Blocked)`、`UNWIND [] AS x CREATE (:Blocked)` 仍被拒绝，文件/Provider 不会先运行再撤销。

不应先用字符串寻找 `CREATE` 或 `LOAD CSV` 来替代该选项；关键字可以出现在注释、字符串、嵌套和组合查询中。静态 `lithograph_validate()` 也不能替代一次 execution 的安全选项。

<a id="explicit-transaction"></a>

## 4. 在 explicit transaction 中使用

选项加在本次 execution 上，不加在 transaction begin 上：

```sql
SELECT lithograph_tx_begin('{"message":"trusted multi-step work"}');
SELECT lithograph('CREATE (:StagedSafe {value:9}) FINISH');
SELECT lithograph(
  'MATCH (d:StagedSafe) RETURN d.value AS value',
  '{}', '{"readOnlyNoExternalIO":true}'
);
SELECT lithograph_tx_abort();
SELECT lithograph('MATCH (d:StagedSafe) RETURN count(d) AS count');
```

受限 read 能看到此前可信 execution 的 staged 数据，得到 `9`；abort 后 count 为 `0`。安全选项不把整个 transaction 变成只读，也不会阻止其它可信 execution 写入。

如果安全选项拒绝了 active transaction 内的 execution，适用普通失败规则：整个 transaction 自动 abort，此前 staged writes 一并回滚。应重新确认状态、重新 begin；不能继续使用已经失败的 transaction。可执行负向例子和 Branch head 不变断言在 [streaming.py](examples/streaming.py)。

<a id="host-boundary"></a>

## 5. 明确宿主仍负责的边界

该选项限制本次 Lithograph Cypher execution；SQLite 读取当前数据库、TEMP/spill 和可重建 derived-state 工作仍允许。它不隔离任意 SQL、本机扩展、OS 文件权限、其它 connection，也不约束单独的 init/transaction lifecycle SQL。`readOnlyNoExternalIO` 本身没有 CPU、耗时、结果行数或内存配额；宿主仍需适当的查询/结果预算、取消和并发管理。

v0.3.3 / Phase 19 增加独立的 `memoryLimitBytes`，可与本页安全选项组合，用来限制受控 execution 工作内存。v0.3.2 发行库不接受该字段；新构建的设置方式、资源错误和不能覆盖的进程内存边界见 [应用集成](integration.md#execution-memory-budget)。只读和限制返回行数不会自动限制表达式中间值的开销。

Graph View 限制可见图，`readOnlyNoExternalIO` 限制本次操作能力；需要二者时由宿主同时强制设置。完整 option 合同及允许的 Procedure 名称见 [Execution Options](../reference/execution-options.md)，连接清理见 [Transactions Reference](../reference/transactions.md)。

v0.3.2 还提供字段级查询/返回/写入限制和可组合操作开关，见 [访问控制指南](access-controls.md)。它们省略时不自动收紧权限，不能把空 readAccess/writeAccess 当作此页的整体只读策略。

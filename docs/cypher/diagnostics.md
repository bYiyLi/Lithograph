# EXPLAIN、PROFILE 与 SHOW

适用 Lithograph v0.3.1、`CY25-2026.08`。[Cypher 手册](index.md) · [兼容范围](../reference/cypher-compatibility.md) · [SQL API](../reference/sql-api.md)

EXPLAIN 查看计划，PROFILE 执行并观察计划计数，SHOW 读取当前图的语言和 Schema 目录。它们通过相同的 `lithograph()` / `lithograph_rows()` surface 执行，JSON 结果和 summary 的结构见 [SQL API](../reference/sql-api.md)。

<a id="explain"></a>

## EXPLAIN

```text
EXPLAIN [CYPHER 25 [<preamble-options>]] <query>
CYPHER 25 [<preamble-options>] EXPLAIN <query>
```

EXPLAIN 解析并执行 semantic validation，生成计划，但不执行查询数据路径。它不创建节点、修改 Schema 或移动 Branch；也不读 LOAD CSV source、不调用 Embedding Provider，不构造 Full-text tokenizer。它只能证明静态检查成功，不能保证参数-dependent runtime type、数据约束、I/O、tokenizer 或 Provider 运行成功。

<!-- example: schema-diagnostics-explain -->
```cypher
EXPLAIN RETURN 1 AS value
```

结果为一列 `plan`、一行多行文本；上例的计划文本为 `00 Project`。计划文本服务人和诊断，operator 名称、编号与格式可随优化变化，应用不应以完整字符串作为稳定的机器协议。

写入也可 EXPLAIN：

<!-- example: schema-diagnostics-explain-write -->
```cypher
EXPLAIN CREATE (:ExplainOnly) FINISH
```

<!-- example: schema-diagnostics-explain-check -->
```cypher
MATCH (n:ExplainOnly) RETURN count(n) AS count
```

前例不写数据，后例返回 `0`。语法/静态语义错误仍失败；静态检查不会因为数据为空跳过。`readOnlyNoExternalIO:true` 同样拒绝 EXPLAIN 包裹的禁止操作，不能把它作为绕过 prepare policy 的入口。

<a id="profile"></a>

## PROFILE

```text
PROFILE [CYPHER 25 [<preamble-options>]] <query>
CYPHER 25 [<preamble-options>] PROFILE <query>
```

PROFILE 真正执行查询，返回正常 columns/rows，并在成功 summary 中增加 `profile.operators`。每项包含 `id`、`operator`、`rows`、`dbHits`；整体 `summary.metrics` 包含 `rows`、`dbHits`、`elapsedMicros`。Operator 的 rows/dbHits 描述该次执行观察到的产出和访问，不应把所有 operator 的 rows 相加当成最终行数。

<!-- example: schema-diagnostics-profile -->
```cypher
PROFILE UNWIND [1,2,3] AS n RETURN n * n AS square ORDER BY square
```

正常结果为 `1,4,9` 三行；profile 位于 summary，不能用 `RETURN profile` 读取。`lithograph_rows()` 通过最终 summary event 返回同一信息；提前关闭 cursor 得不到完整计数或成功证明。

PROFILE write 具有普通 write 的全部副作用：

<!-- example: schema-diagnostics-profile-write -->
```cypher
PROFILE CREATE (:Profiled {value:1}) FINISH
```

<!-- example: schema-diagnostics-profile-check -->
```cypher
MATCH (n:Profiled) RETURN count(n) AS count
```

依次执行后返回 `1`。普通写入仍只在成功 terminal summary/result 完成后提交，出错或 early-close 按原生命周期回滚；batch query 已完成的独立 Commit 遵守 [IN TRANSACTIONS](transaction-subqueries.md) 例外。不要将带写入的 PROFILE 当作只读诊断。

<a id="show"></a>

## current-graph SHOW 形式

```text
SHOW {FUNCTION | FUNCTIONS}
SHOW {PROCEDURE | PROCEDURES}
SHOW [ALL | <index-kind>] {INDEX | INDEXES}
SHOW [ALL | <constraint-filter>] {CONSTRAINT | CONSTRAINTS}
SHOW CURRENT GRAPH TYPE [AS GRAPH]
```

这些目录属于当前数据库/目标 Snapshot；不提供 Neo4j server 的用户、角色、数据库集群、事务监控或 DBMS 管理目录。标准 Schema filter 与完整列分别见 [Index SHOW](indexes.md#show)、[Constraint SHOW](constraints.md#show)、[Graph Type SHOW](graph-types.md#show)。

FUNCTIONS 默认列为 `name,category,description`；PROCEDURES 默认列为 `name,description,mode,worksOnSystem`。使用 YIELD * 获取签名和 metadata；角色/admin/system metadata 不创建 Lithograph 自身的认证授权系统。

| 目录 | 可额外 YIELD 的列 |
| --- | --- |
| FUNCTIONS | `signature,isBuiltIn,argumentDescription,returnDescription,aggregating,rolesExecution,rolesBoostedExecution,isDeprecated,deprecatedBy` |
| PROCEDURES | `signature,argumentDescription,returnDescription,admin,rolesExecution,rolesBoostedExecution,isDeprecated,deprecatedBy,option` |

FUNCTIONS 的每个 overload 是独立目录行，所以函数名称数与签名行数不同。具体语义见[函数索引](../reference/functions.md)、[Procedure 索引](../reference/procedures.md)。

<a id="yield"></a>

## YIELD、WHERE、排序与投影

```text
SHOW <target>
[YIELD {column [AS alias] [, ...] | *}
  [ORDER BY ...] [SKIP <integer>] [LIMIT <integer>]]
[WHERE <predicate>]
[RETURN <projection> [ORDER BY ...] [SKIP ...] [LIMIT ...]]
```

YIELD 只选择目录字段和 aliases；表达式计算放在 RETURN。WHERE 使用已经输出的字段；YIELD 改名后使用 alias。ORDER BY/SKIP/LIMIT 控制目录行，RETURN 可以聚合。`YIELD *` 暴露所有列，不只默认列。

<!-- example: schema-diagnostics-functions -->
```cypher
SHOW FUNCTIONS YIELD name, aggregating
WHERE name IN ['abs','count']
RETURN name, aggregating ORDER BY name
```

返回 `['abs',false]` 与 `['count',true]`。

<!-- example: schema-diagnostics-procedures -->
```cypher
SHOW PROCEDURES YIELD name
WHERE name = 'db.labels'
RETURN name
```

返回 `'db.labels'`。SHOW 可以与已产生输入行的 clause 组合，输出按每条输入行展开；外部变量仍遵守[查询作用域](query-model.md)，不能与目录列产生不明确的同名冲突。

<!-- example: schema-diagnostics-show-error -->
```cypher
SHOW INDEXES YIELD missingColumn RETURN missingColumn
```

返回 `SEMANTIC_ERROR`。任意 expression 直接放进 YIELD、未定义字段、变量冲突同样不能通过零输入行逃过语义检查。

<a id="use"></a>

## 使用计划与目录排查问题

Index definition 存在只说明 Planner 可以选择该访问路径；Type proof、谓词形状、statistics 和目标 Snapshot 都会影响实际计划。先 SHOW 确认目标版本，再 EXPLAIN 看访问路径，最后对合适的查询 PROFILE 测量实际代价。`dbHits` 不是磁盘 I/O 次数，`elapsedMicros` 不是跨机器稳定性能指标。

历史 Snapshot 的 SHOW 返回历史 Schema；缺少 Provider/tokenizer 不阻止纯目录读取。错误分类、JSON 编码及 stream cleanup 见[错误参考](../reference/errors.md)、[值编码](../reference/values.md)、[事务生命周期](../reference/transactions.md)。

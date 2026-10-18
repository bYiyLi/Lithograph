# LOAD CSV

适用版本：Lithograph **v0.3.3**，Cypher Profile **`CY25-2026.08`**。

[Cypher 手册](index.md) · [兼容范围](../reference/cypher-compatibility.md) · [导入指南](../guide/import.md)

`LOAD CSV` 把外部 CSV 的记录展开为查询行。它可以只读取和转换数据，也可以把记录交给 `CREATE`、`MERGE` 等写入 clause。CSV 文件中的数字、日期和 Boolean 文本均先作为 `String` 读取；需要保存为其它类型时显式转换。

<a id="syntax"></a>

## 语法与结果

```text
LOAD CSV [WITH HEADERS] FROM sourceExpression AS row
  [FIELDTERMINATOR 'delimiter']
```

| 部分 | 含义 |
| --- | --- |
| `sourceExpression` | 求值为 `String` 的 URI；可以是字面量、参数，或当前输入行中可见变量构成的表达式 |
| `WITH HEADERS` | 第一条 CSV 记录作为字段名，不作为数据行输出；`row` 为 `Map` |
| 不写 `WITH HEADERS` | 包括第一条记录在内的所有记录都是数据；`row` 为 `List<String>` |
| `AS row` | 为当前 CSV 记录绑定变量；之前的输入变量继续可见 |
| `FIELDTERMINATOR` | 单字符字符串字面量，默认 `','`；可以使用字符串 Unicode 转义，例如 `'\u003B'` 表示分号 |

分隔符不能是空字符串、多个字符、双引号或换行符。`FIELDTERMINATOR` 的位置在 `AS` 之后，它接受字面量，不能替换为 `$delimiter` 参数。

每个上游输入行会求值一次 source，然后展开该 source 的记录。没有上游 clause 时，查询从一个空输入行开始；空文件不输出记录。读取多份文件可以先用 `UNWIND $sources AS source` 展开 URI，再执行 `LOAD CSV FROM source AS row`。这是按输入逐份加载，不会把多个文件隐式拼成一个表头或一个 CSV 上下文。

`LOAD CSV` 自身不返回最终结果；使用 `RETURN` 投影需要的字段，或用 `FINISH` 完成无结果导入。后续变量作用域、过滤、聚合和排序遵守[查询模型](query-model.md)与[读取 clause](reading.md)。

<a id="sources"></a>

## URI、宿主权限与 I/O

支持 `file://`、`http://`、`https://`：

| 来源 | Lithograph 行为 |
| --- | --- |
| `file:///absolute/path/people.csv` | 访问宿主进程可读取的本地文件；文件路径按 URI 百分号编码解码并解析为真实绝对路径 |
| `file://localhost/absolute/path/people.csv` | 同一机器的本地文件；其它 file URI 主机名被拒绝 |
| `http://…`、`https://…` | 由宿主 extension runtime 发起 HTTP GET，按 CSV 流读取响应，并使用临时文件维护 source 上下文 |

文件访问使用实际宿主路径。Neo4j 的 import 目录、服务器配置和 `LOAD` privilege 不适用于嵌入式 SQLite 扩展；Lithograph 不添加隐藏凭据。FTP、云存储专用 scheme 与 ZIP/gzip 文件解压不属于本版本公开 source 合同；提供普通 UTF-8 CSV 内容及上述 URI。

HTTP 响应读取失败、文件不存在、没有读取权限和临时文件创建失败都属于查询失败。外部读取已经发生的网络费用或访问本身无法通过 graph rollback 撤销。要处理不可信查询，宿主可以强制设置 [`readOnlyNoExternalIO`](../reference/execution-options.md)，它会在 prepare 阶段拒绝任何 `LOAD CSV`，包括未进入的分支和参数 URI。

<a id="fields"></a>

## CSV 字段、空值与表头

CSV 以 UTF-8 文本读取；第一条记录开头的 UTF-8 BOM 会被去除。支持 LF、CRLF、CR 记录结束符、双引号包围的字段、引号内换行，以及 `""` 或 `\"` 表示字段中的双引号。闭合引号后只能是分隔符、记录结束或 EOF；未闭合引号、未引用字段中的意外引号等格式错误使查询失败。

`WITH HEADERS` 要求每个字段名非空且不重复。字段名区分大小写；名称含空格或特殊字符时使用 `row['field name']` 或转义属性名访问。

| CSV 情况 | `WITH HEADERS` | 无表头 |
| --- | --- | --- |
| 正常字段 | 对应 key 的 `String` 值 | 对应位置的 `String` 值 |
| 逗号之间的空字段，包括 `""` | 空字符串 `''` | 空字符串 `''` |
| 记录列数少于表头 | 缺失的 header key 对应 `null` | List 只有实际读取的字段；越界访问得到 `null` |
| 记录列数多于表头 | 超出表头数量的字段不进入 Map | List 保留所有字段 |

文本 `null` 仍然是四个字符组成的字符串。CSV 不推断数值、Boolean、日期，也不把空字符串自动转换为 `null`。可使用 `toInteger()`、`date()`、`trim()`、`nullIf()`、`coalesce()` 等明确转换；把属性设置为 `null` 会移除属性，详见[写入](writing.md)。

<a id="header-example"></a>

## 读取有表头与无表头的数据

本页使用以下教学文件 `people.csv`，`p3` 记录没有第三列；调用方把文件的实际 `file:///…/people.csv` URI 绑定到参数 `$source`。

<!-- fixture: people.csv -->
```csv
id,name,age
p1,Ada,36
p2,Bo,
p3,Chen
```

<!-- example: csv-headers -->
```cypher
LOAD CSV WITH HEADERS FROM $source AS row
RETURN row.name AS name, row.age AS age, linenumber() AS record
ORDER BY name
```

结果为 `['Ada','36',2]`、`['Bo','',3]`、`['Chen',null,4]`；`age` 仍是字符串或缺失值。

<!-- example: csv-no-headers -->
```cypher
LOAD CSV FROM $source AS row FIELDTERMINATOR ','
RETURN row[0] AS id, row[2] AS age
ORDER BY id
```

结果为 `['id','age']`、`['p1','36']`、`['p2','']`、`['p3',null]`。第一条记录没有被当作表头。

<a id="context-functions"></a>

## file() 与 linenumber()

`file()` 返回最近执行的 `LOAD CSV` 对应的本地绝对文件路径。对于 HTTP/HTTPS，它是保存该 source 上下文的临时文件路径，不能把它当成原始 URL 或可长期引用的资产。要保留原始 URI，应保留自己的 source 参数或变量。

`linenumber()` 返回从 1 开始的逻辑 CSV 记录号；`WITH HEADERS` 的 header 占第 1 条记录。一个含换行的 quoted 字段仍属于一条记录，因此它不是文本编辑器中的物理行号。

CSV 上下文可以随行穿过 `WITH` 和 scoped `CALL`；多个或嵌套 `LOAD CSV` 以最近实际执行的 clause 更新上下文。上下文不会作为用户变量出现在 `RETURN *` 中，也不会被用户取同名变量覆盖。

<!-- example: csv-context-outside -->
```cypher
RETURN file() AS source, linenumber() AS record
```

没有 CSV 上下文时两者都返回 `null`。

<a id="import"></a>

## 转换并写入图

以下例子使用同一 `people.csv`，将 `age` 显式转成 Integer；空字符串和缺失值经 `toInteger()` 得到 `null`，不会形成 `age` 属性。

<!-- example: csv-import -->
```cypher
LOAD CSV WITH HEADERS FROM $source AS row
CREATE (person:CsvPerson {id:row.id, name:row.name, age:toInteger(row.age)})
RETURN person.name AS name, person.age AS age
ORDER BY name
```

结果为 `['Ada',36]`、`['Bo',null]`、`['Chen',null]`。在导入后执行：

<!-- example: csv-import-count -->
```cypher
MATCH (person:CsvPerson)
RETURN count(person) AS people, count(person.age) AS knownAges
```

结果为 `[3,1]`。对需要重复导入的数据，应以业务 key 建立约束并选择 `MERGE`；单纯改用 `MERGE` 不能替代数据清洗和约束检查。

`WITH count(row)`、`ORDER BY`、`DISTINCT` 等 clause 仍对相应 clause 的全部输入生效。不能把它们理解成仅处理 CSV reader 当前缓冲区。需要全局输入时会等待必要读取并使用临时存储；否则可逐记录推进。

<a id="atomicity"></a>

## 事务与失败

普通 `LOAD CSV` 的全部图写入属于当前 execution 的事务边界。晚到的 CSV 格式错误、读取失败、类型或约束错误会撤销这次普通 execution 的写入。它可以在 active Lithograph explicit transaction 中运行；此时失败会使整个 explicit transaction abort，详见[事务参考](../reference/transactions.md)。

下面的教学文件 `malformed.csv` 在两条合法记录后留有未闭合引号：

<!-- fixture: malformed.csv -->
```csv
name
Ada
Bo
"unterminated
```

将它的 URI 绑定到 `$source`：

<!-- example: csv-ordinary-malformed -->
```cypher
LOAD CSV WITH HEADERS FROM $source AS row
CREATE (:OrdinaryCsv {name:row.name})
FINISH
```

查询返回 `RESOURCE_ERROR`。在同一数据库检查：

<!-- example: csv-ordinary-rollback -->
```cypher
MATCH (person:OrdinaryCsv)
RETURN count(person) AS remaining
```

结果为 `[0]`。若改用 [`CALL … IN TRANSACTIONS`](transaction-subqueries.md#csv-composition)，之前成功的 batch 会保留，因此错误后的恢复策略不同。

`lithograph_rows()` 可以执行 CSV 读取和写入。读取纯 metadata 的 `columns` event 不触发 I/O；继续推进 cursor 才开始 runtime work。普通写入需要消费到成功 `summary` 才完成自己的提交边界；提前关闭 cursor 会回滚尚未提交的普通 execution。

<a id="errors"></a>

## 类型、参数和关键错误

| 情况 | 错误或结果 |
| --- | --- |
| source 求值为 `null`、Integer 或其它非 String | `TYPE_ERROR`；不会把 `null` 当作空文件 |
| 不支持的 URI scheme、非本机 file host、错误百分号编码 | `INVALID_ARGUMENT` |
| 分隔符为空、多字符、双引号或换行 | `INVALID_ARGUMENT` |
| 非法 UTF-8、文件或 HTTP 读取失败 | `IO_ERROR` |
| CSV 格式错误、空或重复 header | `RESOURCE_ERROR` |
| 未定义 source 变量、source 中使用聚合 | `SEMANTIC_ERROR` |
| Graph View 之外的写入、属性类型或约束失败 | 相应 `GRAPH_VIEW_VIOLATION`、`TYPE_ERROR` 或 `CONSTRAINT_ERROR`，遵守当前事务回滚边界 |
| `readOnlyNoExternalIO=true` | prepare 阶段 `INVALID_ARGUMENT` |
| Merge candidate execution 中的 `LOAD CSV` | `TRANSACTION_BOUNDARY_REQUIRED` |

<!-- example: csv-null-source -->
```cypher
LOAD CSV FROM null AS row
RETURN row
```

预期 `TYPE_ERROR`。

<!-- example: csv-invalid-delimiter -->
```cypher
LOAD CSV FROM $source AS row FIELDTERMINATOR '||'
RETURN row
```

预期 `INVALID_ARGUMENT`，不会开始读取 source。

语言用法另与 [Neo4j Cypher 25 LOAD CSV](https://neo4j.com/docs/cypher-manual/25/clauses/load-csv/) 核对。这里的 URI、宿主权限、错误和事务说明以 Lithograph v0.3.3 为准；线上手册的服务器管理能力不扩展本页支持范围。

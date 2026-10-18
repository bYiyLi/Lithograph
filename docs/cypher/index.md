# Lithograph Cypher 25 使用手册

[产品手册](../guide/README.md) · [接口参考](../reference/README.md) · [版本与兼容范围](../reference/cypher-compatibility.md)

本手册适用于 **Lithograph v0.3.3**，语言基线为冻结的 **`CY25-2026.08`**。它说明本产品公开的 Cypher 语法与可观察行为。Cypher 通过 SQLite 的 `lithograph()` 或 `lithograph_rows()` 执行；第一次接入请先完成 [快速入门](../guide/getting-started.md)。

## 阅读与查找

学习顺序：**语法基础 → 查询模型 → 类型/表达式 → 读取 → Pattern/Path → 修改 → Schema/搜索**。已熟悉 Cypher 的开发者可按下表直接查阅。

| 主题 / keyword | 页面 |
| --- | --- |
| 标识符、注释、参数、字面量、`CYPHER 25` | [语法基础](syntax.md) |
| 行、作用域、结果、重复与顺序 | [查询模型](query-model.md) |
| Property/runtime types、Temporal、Point、Vector、UUID | [类型与值](types.md) |
| `null`、相等、比较、排序、NaN | [Null 与排序](null-and-ordering.md) |
| 运算符、CASE、List/Map、comprehension、动态访问、插值 | [表达式](expressions.md) |
| MATCH、OPTIONAL MATCH、WHERE/FILTER、WITH/LET/RETURN、UNWIND/FOR、分页 | [读取与投影](reading.md) |
| DISTINCT、GROUP BY、空输入与分组 | [聚合](aggregation.md) |
| 节点/关系、方向、Label/Type expression、属性谓词 | [Pattern](patterns.md) |
| variable-length、quantified patterns、group variables | [变长与量化路径](quantified-paths.md) |
| match/path modes、ANY/ALL、shortest、k/group selectors | [路径选择](path-selection.md) |
| UNION、WHEN、NEXT | [查询组合](composition.md) |
| CALL 子查询、EXISTS/COUNT/COLLECT 子查询 | [子查询](subqueries.md) |
| 命名 CALL、YIELD、别名和过滤 | [Procedure 调用语法](procedure-calls.md) |
| CREATE/INSERT、MERGE、SET/REMOVE、DELETE、FOREACH | [数据修改](writing.md) |
| Graph Type 的 SET/ADD/ALTER/SHOW/DROP | [Graph Type](graph-types.md) |
| KEY、UNIQUE、existence、type constraints | [约束](constraints.md) |
| LOOKUP/RANGE/TEXT/POINT/FULLTEXT/VECTOR、DDL OPTIONS、SHOW/DROP | [索引](indexes.md) |
| Vector SEARCH、过滤、LIMIT、SCORE | [搜索](search.md) |
| LOAD CSV、字段、URI 和导入组合 | [CSV](load-csv.md) |
| IN TRANSACTIONS、CONCURRENT、DISJOINT BY、ON ERROR、REPORT STATUS | [事务子查询](transaction-subqueries.md) |
| EXPLAIN、PROFILE、SHOW | [诊断与发现](diagnostics.md) |
| 全部函数、overload 与分类 | [函数索引](../reference/functions.md) |
| Full-text、Semantic、版本和维护 Procedure 参数 | [Procedure Reference](../reference/procedures.md) |

## 语法与示例约定

语法模板使用 `text` 代码块：尖括号表示需要替换的语法组成部分，方括号表示可选项，花括号和竖线的具体含义以所在模板说明为准。不能把模板标记直接粘贴为查询。实际 Cypher 示例使用 `cypher` 代码块，正文说明前置数据与结果；关联的验证清单使用隔离数据库执行这些原文。

语言示例里的 `$name` 是 Cypher 参数；SQLite 参数绑定、JSON tagged value 和 execution options 分别见 [SQL API](../reference/sql-api.md)、[Values](../reference/values.md)、[Execution Options](../reference/execution-options.md)。Branch/Commit/Tag 选择、Graph View 和安全执行属于宿主 execution context，使用 [版本指南](../guide/versioning.md)、[Graph View](../guide/graph-views.md) 与 [安全执行](../guide/safe-execution.md)。

## 适用边界

本手册覆盖冻结 Profile 的单个 current graph 查询与数据库语义。Neo4j Server 的多数据库、账号权限、集群、Bolt/HTTP driver 和外部 Procedure 库不因支持 Cypher 而进入产品范围；准确边界和后端差异见 [兼容范围](../reference/cypher-compatibility.md)。Full-text query string 是传入 Procedure 的检索表达式，Managed Semantic 是标准 CALL 调用的产品 Procedure，二者的精确输入由对应参考拥有。

上游 [Neo4j Cypher 25 Manual](https://neo4j.com/docs/cypher-manual/25/) 用于标准语义核对；本手册独立编写，按 Lithograph 冻结 Profile 与运行结果验收。上游最新内容不自动扩展本版本承诺。

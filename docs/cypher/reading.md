# 读取、过滤、投影与分页

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

本文适用于 Lithograph v0.3.1、`CY25-2026.08`。基础行规则见 [查询模型](query-model.md)。以下建图块只需在空图执行一次；本页涉及人物的示例均从该数据开始，各示例互相独立。

<!-- example: foundation-reading-data -->
```cypher
CREATE (a:ReaderPerson {name:'阿青',age:30}),
       (b:ReaderPerson {name:'白川',age:20}),
       (c:ReaderPerson {name:'陈禾'}),
       (a)-[:RECOMMENDS {rating:5}]->(b)
RETURN count(*) AS created
```

返回 `created = 1`，表示一条 CREATE 输入行；图中创建了三个节点和一条关系。

<a id="match"></a>

## MATCH

```text
MATCH [DIFFERENT RELATIONSHIPS | REPEATABLE ELEMENTS] <pattern>, ...
      [SEARCH ...] [WHERE <predicate>]
```

`MATCH` 对每条输入行找出所有符合 Pattern 的变量绑定。属性 map 是相等条件，Label/Type 与方向进一步限制匹配；多 Pattern 之间共享变量时按该绑定连接。无匹配则不输出该输入行。完整 Pattern、重复关系及 Path 语法见 [Pattern](patterns.md)、[量化路径](quantified-paths.md) 和 [路径选择](path-selection.md)。`SEARCH` 见 [搜索](search.md)。

<!-- example: foundation-reading-match -->
```cypher
MATCH (p:ReaderPerson)-[r:RECOMMENDS]->(friend:ReaderPerson)
WHERE r.rating >= 4
RETURN p.name AS person, friend.name AS recommendation
```

返回 `["阿青","白川"]`。WHERE 是该 MATCH 的谓词，不是一个可独立放置的 clause。

<a id="optional-match"></a>

## OPTIONAL MATCH 与谓词位置

```text
OPTIONAL MATCH [<match-mode>] <pattern>, ... [SEARCH ...] [WHERE <predicate>]
```

对每条输入行，存在完整匹配时输出全部匹配；不存在时保留一行，所有新变量为 null，之前的变量保持原值。Pattern 与紧随的 WHERE 共同决定“存在匹配”；不是先用 OPTIONAL MATCH 产生 null 行，再由该 WHERE 过滤。

<!-- example: foundation-reading-optional -->
```cypher
MATCH (p:ReaderPerson)
OPTIONAL MATCH (p)-[r:RECOMMENDS]->(friend)
WHERE r.rating >= 4
RETURN p.name AS person, friend.name AS recommendation
ORDER BY person
```

返回 `["白川",null]`、`["阿青","白川"]`、`["陈禾",null]`（按 Unicode 字符串顺序）。未找到关系不会丢失人物。

<!-- example: foundation-reading-optional-filter -->
```cypher
MATCH (p:ReaderPerson)
OPTIONAL MATCH (p)-[r:RECOMMENDS]->(friend)
FILTER r.rating >= 4
RETURN p.name AS person, friend.name AS recommendation
```

这里只返回 `["阿青","白川"]`，因为独立 FILTER 看到 null 关系的 predicate 为 null，删除对应行。把 Optional 的筛选从 WHERE 移到 FILTER 会改变结果。

<a id="filter"></a>

## WHERE 与 FILTER

```text
MATCH ... WHERE <predicate>
OPTIONAL MATCH ... WHERE <predicate>
WITH ... [ORDER BY ...] [SKIP ...] [LIMIT ...] WHERE <predicate>
CALL <procedure>(...) YIELD ... WHERE <predicate>
FILTER <predicate>
```

WHERE 附属于各自 clause；FILTER 是独立行过滤 clause，可用于具有输入行的 pipeline。谓词需为 Boolean 或 null；true 保留，false/null 丢弃，其它值为 `TYPE_ERROR`。不会把数字、非空字符串或容器当作“真值”。谓词可以使用当前作用域的表达式、Pattern predicate、EXISTS 子查询等，见 [表达式](expressions.md)。

<!-- example: foundation-reading-filter -->
```cypher
UNWIND [true, false, null] AS allowed
FILTER allowed
RETURN allowed
```

只返回 `[true]`。`WHERE x = null` 不会查出 null 值；使用 `x IS NULL`。

<a id="return-with"></a>

## RETURN 与 WITH

```text
RETURN [ALL | DISTINCT] <expression> [AS <alias>], ...
       [GROUP BY ALL | GROUP BY () | GROUP BY <expression>, ...]
       [ORDER BY <expression> [ASC[ENDING] | DESC[ENDING]], ...]
       [SKIP | OFFSET <expression>] [LIMIT <expression>]
WITH   [ALL | DISTINCT] <expression> [AS <alias>], ...
       [GROUP BY ...] [ORDER BY ...] [SKIP | OFFSET <expression>]
       [LIMIT <expression>] [WHERE <predicate>]
```

`ALL` 是默认，保留重复行；`DISTINCT` 对整条投影行去重。`RETURN` 决定最终列与顺序，`WITH` 将投影传给下一个 clause。投影项也可以是 `*`，表示保留作用域内的变量。计算结果用 `AS` 命名，避免依赖自动生成的列名。聚合语法与分组规则见 [聚合](aggregation.md)。

同一投影的表达式根据输入作用域求值；不要在一个逗号列表后项引用前项刚创建的别名。用连续 WITH 或 LET 分两步。投影后的 WHERE/ORDER BY 可以引用该投影生成的别名。

<!-- example: foundation-reading-with -->
```cypher
MATCH (p:ReaderPerson)
WITH p.name AS name, coalesce(p.age, 0) AS age
WHERE age >= 20
RETURN name, age ORDER BY age DESC, name
```

返回 `["阿青",30]`、`["白川",20]`。`p` 不再传到 RETURN 作用域。非 DISTINCT、非聚合投影的 ORDER BY 可访问相应输入变量；DISTINCT/聚合后则受投影/分组边界限制，不能排序未保留的任意输入值。

<!-- example: foundation-reading-distinct -->
```cypher
UNWIND [2, 1, 2] AS x
RETURN DISTINCT x ORDER BY x
```

返回 `[1]`、`[2]`。

<a id="let"></a>

## LET

```text
LET <new-variable> = <expression>, ...
```

LET 为每条输入行增加变量，保留原有变量，不做聚合或去重。同一个 LET 的右侧只看到该 clause 输入作用域；要使用刚计算的值必须再写一个 LET。变量名必须是新的，不能重新绑定已有变量，也不能在同一 LET 重复定义。

<!-- example: foundation-reading-let -->
```cypher
LET price = 20, quantity = 3
LET subtotal = price * quantity
LET payable = subtotal + 5
RETURN price, quantity, subtotal, payable
```

返回 `[20,3,60,65]`。

<!-- example: foundation-reading-let-error -->
```cypher
LET first = 1, second = first + 1
RETURN second
```

返回 `SEMANTIC_ERROR`，因为 `first` 不在该 LET 输入作用域。`LET total = count(*)` 也不是合法 aggregation；使用 WITH/RETURN。

<a id="unwind-for"></a>

## UNWIND 与 FOR

```text
UNWIND <expression> AS <new-variable>
FOR <new-variable> IN <expression>
```

两种形式将列表逐元素展开，对每条输入行分别执行，保留已有变量，增加一个新变量。空列表和 null 产生零行；列表中的 null 仍产生一行。非 List 且非 null 的值按单个元素产生一行。新变量不能与当前绑定冲突。不保证展开后的结果顺序，最终需要 ORDER BY。

<!-- example: foundation-reading-unwind -->
```cypher
UNWIND [2, null, 1] AS x
RETURN x ORDER BY x
```

升序返回 `[1]`、`[2]`、`[null]`。

<!-- example: foundation-reading-for -->
```cypher
FOR row IN [[1,2],[3]]
FOR value IN row
RETURN value ORDER BY value
```

返回 `[1]`、`[2]`、`[3]`。展开只处理一层；两层嵌套列表需要两次展开。

<!-- example: foundation-reading-unwind-scalar -->
```cypher
UNWIND 5 AS x
FOR y IN x + 1
RETURN x, y
```

返回 `[5,6]`。

<a id="order-pagination"></a>

## ORDER BY、SKIP/OFFSET 与 LIMIT

ORDER BY 按列出的表达式从左至右比较；ASC/ASCENDING 为默认升序，DESC/DESCENDING 为降序。null 在升序最后、降序最前。跨类型排序、NaN 和 List 的顺序见 [null 与排序](null-and-ordering.md)。并列行不保证相对顺序，要稳定分页必须增加唯一业务键或 `elementId()`。

SKIP 与 OFFSET 同义，跳过排序/投影后的指定行数；LIMIT 最多保留指定行数，0 保留零行。它们接受非负 Integer 表达式或参数，不接受 Float、负数或 null；不能依赖当前行变量决定每行不同的限制。顺序为投影/去重或聚合、排序、跳过、限制。`WITH` 后的 WHERE 作用于其投影结果；涉及分页时，建议显式拆成多个 WITH，使先过滤还是先取页清楚可见。

<!-- example: foundation-reading-pagination -->
```cypher
UNWIND [5, 2, 4, 1, 3] AS x
RETURN x ORDER BY x OFFSET $offset LIMIT $limit
```

传入 `{"offset":1,"limit":2}`，返回 `[2]`、`[3]`。LIMIT 不表示“之前的写入只做这些行”；限制写入目标应在写 clause 之前用 WITH ... LIMIT 选择目标。

<a id="finish"></a>

## FINISH

```text
<clause> ... FINISH
```

FINISH 消费前序 pipeline 并结束查询，输出零列零行；前面的读、写、调用和错误仍发生。它适用于执行检查、只需要副作用或不需要业务结果的查询。执行是否成功以 SQL scalar 的 summary 或 stream 终端 summary 为准。

<!-- example: foundation-reading-finish -->
```cypher
CREATE (:FinishExample {ready:true})
FINISH
```

在空图创建一个节点，返回零列零行。修改 clause 的详细语义见 [写入](writing.md)，消费/取消行为见 [查询模型](query-model.md#termination)。

相关主题：[Pattern](patterns.md) · [表达式](expressions.md) · [聚合](aggregation.md) · [子查询](subqueries.md)。

<a id="return-star"></a>

## 展开当前作用域的全部列

`RETURN *` 展开当前作用域内的绑定；需要固定业务列顺序时优先明确列出名称。下面先用 WITH 将作用域收敛成一个变量，再完整输出。

<!-- example: foundation-reading-return-star -->
```cypher
UNWIND [2, 1] AS x
WITH x AS number
RETURN * ORDER BY number
```

返回列 `number`，行依次为 `[1]`、`[2]`。

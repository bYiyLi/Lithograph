# CALL 子查询与子查询表达式

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

适用于 Lithograph v0.3.1，Cypher Profile `CY25-2026.08`。命名 Procedure 的调用见 [Procedure CALL](procedure-calls.md)，子查询的独立 batch 事务见 [Transaction Subqueries](transaction-subqueries.md)。本章的普通 CALL 不创建独立事务。

<a id="call-subquery"></a>

## CALL scope clause

```text
CALL () { query }
CALL (variable [, variable ...]) { query }
CALL (*) { query }
CALL { [WITH importedVariables | WITH *] query }
```

| 写法 | 能访问的外部变量 |
| --- | --- |
| `CALL ()` | 不导入任何变量 |
| `CALL (x,y)` | 仅显式列出的变量，变量必须已存在 |
| `CALL (*)` | 导入该处全部外部变量 |
| `CALL { WITH x ... }`、`CALL { WITH * ... }` | 旧式 importing WITH 指定的变量，或 * 导入全部变量 |
| `CALL { ... }` | 未导入的外部变量不可见；可声明同名的局部变量 |

推荐使用 scope clause；传入的参数 `$name` 不属于行变量，不需要通过 scope clause 导入。CALL 会针对每个输入行执行一次；没有前序 clause 时从一个空输入行开始。外部行多次传入同一个值，子查询仍可能执行多次。

<!-- example: path-subquery-correlated -->
```cypher
UNWIND [1,2] AS x
CALL (x) { UNWIND [10,20] AS multiplier RETURN x * multiplier AS product }
RETURN x, product ORDER BY x, product
```

结果：`[1,10]`、`[1,20]`、`[2,20]`、`[2,40]`。每个外部行和该次子查询的每个结果行组合；未 RETURN 的内部变量不会导出。

<a id="subquery-scope"></a>

## 导入变量与局部变量

scope clause 导入的变量在整个子查询中保持可见，内部 WITH 不会把它们移除。不能用 WITH、LET、UNWIND、FOR 等重定义同名导入变量；子查询导出列也不能与已存在的外部变量冲突。对普通局部变量，WITH 仍可缩小作用域。

<!-- example: path-subquery-global-import -->
```cypher
WITH 11 AS x
CALL (x) {
  UNWIND [2,3] AS factor
  WITH factor
  RETURN x * factor AS product
}
RETURN x, product ORDER BY product
```

结果：`[11,22]`、`[11,33]`。内部 WITH 只列出 factor，导入的 x 仍可见。

<!-- example: path-subquery-empty-scope-error -->
```cypher
WITH 1 AS x
CALL () { RETURN x AS y }
RETURN y
```

结果：`SEMANTIC_ERROR`，没有导入 x。

<a id="importing-with"></a>

## 旧式 importing WITH

`CALL { WITH x,y ... }` 中第一个 WITH 只负责导入：直接列出外部变量，不能在该导入 WITH 中加 alias、表达式、DISTINCT、WHERE、排序或分页。需要变换或过滤时，再写第二个普通 WITH。`WITH *` 导入全部外部变量，不能再混入额外 projection items。UNION 各分支的 importing WITH 分别指定导入变量。旧式 importing WITH 不能与同一子查询组合层的 NEXT 一起使用；需要 NEXT 时改用 scope clause。导入后仍按旧式局部 WITH 作用域处理，后续 WITH 可以移除这些绑定；不要把它与 scope clause 的全子查询可见性混为一谈。

<!-- example: path-subquery-legacy-import -->
```cypher
UNWIND [1,2] AS x
CALL {
  WITH x
  WITH x WHERE x = 2
  RETURN x * 10 AS value
}
RETURN x, value
```

结果：仅 `[2,20]`，因为 x=1 的子查询返回零行。

<a id="returning-unit-subquery"></a>

## Returning 子查询与 Unit 子查询

返回结果的 CALL 使用 RETURN 明确导出列。每个输入行可被放大为多行，也可因零个子查询结果而消失；需要保留行时可使用无 grouping 的聚合，使空输入产生一个聚合行。

没有 RETURN、以写入或 FINISH 结束的 unit subquery 只执行副作用，不增加新列，不移除外层输入行。Unit subquery 内变量不导出。带返回列的 CALL 不能作为完整 top-level query 的唯一终结；调用后接外层 RETURN。Unit CALL 可以作为 query 终点。

<!-- example: path-subquery-unit-write -->
```cypher
UNWIND [1,2] AS value
CALL (value) { CREATE (:Recorded {value:value}) }
RETURN value ORDER BY value
```

结果：`1`、`2`，图中增加两个 Recorded 节点。各 invocation 依次观察前序成功 invocation 的 staged 写入；它们合并为本次普通 execution 的一个逻辑 Commit。

<!-- example: path-subquery-count-empty -->
```cypher
WITH 7 AS outerValue
CALL () { MATCH (n:Missing) RETURN count(n) AS total }
RETURN outerValue, total
```

在空图上结果：`[7,0]`。如果改为 `RETURN n`，子查询将返回零行，外层也无行。

<a id="subquery-union-nesting"></a>

## 组合与嵌套

子查询内部可以使用 UNION、WHEN、NEXT 和更深的 CALL，返回值仍按每个子查询的独立 scope 和列合同检查。scope clause 的导入明确传到当前子查询；更深一层仍需显式导入。

<!-- example: path-subquery-union -->
```cypher
WITH 3 AS n
CALL (n) {
  RETURN n AS value
  UNION ALL
  RETURN n + 1 AS value
}
CALL (value) { RETURN value * 2 AS doubled }
RETURN value, doubled ORDER BY value
```

结果：`[3,6]`、`[4,8]`。子查询中的 RETURN ORDER BY/LIMIT 是每个外部输入行各自的排序和分页，适合每个节点的 top-k；外层 LIMIT 是对组合后的行限制。

<a id="exists-count-collect"></a>

## EXISTS、COUNT、COLLECT 子查询表达式

```text
EXISTS { query }
COUNT { query }
COLLECT { queryReturningOneColumn }
```

三者产生值，不向外部行引入新变量；都可以直接引用可见外部变量，作为相关查询使用，无需 importing WITH。导入的外部变量在表达式子查询中保持可见，不能用局部同名定义遮蔽。表达式子查询是只读语境，不允许将写入藏在表达式中。

| 表达式 | 结果 | 空输入 | RETURN 要求 |
| --- | --- | --- | --- |
| `EXISTS { ... }` | Boolean，是否至少有一个结果行 | `false` | 普通 MATCH 型可省略 RETURN |
| `COUNT { ... }` | Integer，结果行数，包含重复行 | `0` | 普通 MATCH 型可省略 RETURN |
| `COLLECT { ... }` | 单列结果的 List，保留该列的 null 和重复值 | `[]` | 必须返回恰好一列 |

COUNT 子查询计行数，`COUNT { MATCH (n) RETURN n.property }` 的 null 投影行也计数；它不同于忽略 null 的 `count(n.property)`。COLLECT 子查询保留 null，区别于聚合函数 `collect(value)`。需要去重就在子查询里 RETURN DISTINCT；需要稳定 List 顺序就在其 RETURN 中 ORDER BY。

<!-- example: path-subquery-expression -->
```cypher
UNWIND [1,2] AS x
RETURN x,
       EXISTS { WITH x WHERE x = 2 RETURN x } AS selected,
       COUNT { UNWIND range(1,x) AS n RETURN n } AS total,
       COLLECT { UNWIND range(1,x) AS n RETURN n * 10 AS value ORDER BY value } AS values
ORDER BY x
```

结果：`[1,false,1,[10]]`、`[2,true,2,[10,20]]`。

<!-- example: path-subquery-null-collect -->
```cypher
RETURN COLLECT { UNWIND [null,1] AS value RETURN value } AS values,
       COUNT { UNWIND [null,1] AS value RETURN value } AS total,
       EXISTS { MATCH (n:Absent) } AS anyNode
```

在空图上结果：`[[null,1],2,false]`。

<a id="subquery-errors"></a>

## 错误与事务边界

未导入变量、导出名称冲突、重复导入、导入变量重定义、不合法 importing WITH、COLLECT 多列返回均为 `SEMANTIC_ERROR`。静态错误在零行输入时同样检查。计算中的类型错误按表达式规则传播，并使普通 mutating execution 全部回滚；active explicit transaction 的失败处理见 [事务参考](../reference/transactions.md)。

所有子查询继承同一 execution 的 Snapshot、Graph View 和安全 options；不能用 CALL 或 EXISTS 绕开历史只读、View 可见性或 `readOnlyNoExternalIO`。需要独立 batch 事务时显式使用 [IN TRANSACTIONS](transaction-subqueries.md)，其平台入口、部分提交与错误策略另有合同。

标准语义参考：[Neo4j CALL Subqueries](https://neo4j.com/docs/cypher-manual/25/subqueries/call-subquery/) 与 [Subquery Expressions](https://neo4j.com/docs/cypher-manual/25/subqueries/)。

<a id="nested-call"></a>

## 显式嵌套两层 CALL

每一层显式导入自己的输入变量；内层输出先成为外层子查询的局部变量，再由外层 RETURN 传回主查询。

<!-- example: foundation-subquery-nested-call -->
```cypher
WITH 3 AS n
CALL (n) {
  CALL (n) { RETURN n + 1 AS innerValue }
  RETURN innerValue * 2 AS result
}
RETURN result
```

返回列 `result`、行 `[8]`。

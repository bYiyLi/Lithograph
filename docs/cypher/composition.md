# UNION、WHEN 与 NEXT

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

适用于 Lithograph v0.3.2，Cypher Profile `CY25-2026.08`。本章说明多个 query 如何合并结果、按条件选择，或把结果行传给后续 query。普通 clause 的变量传递见 [查询模型](query-model.md)，相关调用见 [Subqueries](subqueries.md)。

<a id="union"></a>

## UNION：合并相同列合同的结果

```text
query UNION query
query UNION DISTINCT query
query UNION ALL query
{ query } UNION ALL { query }
```

`UNION` 与 `UNION DISTINCT` 去除整个输出行相同的重复项；`UNION ALL` 保留重复。每个分支从该组合段的相同输入作用域开始执行，不能引用前一个分支中新引入的变量。分支必须暴露相同列名，不同值类型可出现在同一结果列中；合并不把所有值转成 String。

一组没有 NEXT 分隔的 UNION 不能混用 ALL 与 DISTINCT。用花括号明确嵌套或分组；花括号本身不建立事务。每个分支自己的 ORDER BY 排序本分支；需要对合并后的整体排序、过滤、聚合时，把组合放入 CALL 子查询，再接外层操作。

<!-- example: path-union-distinct -->
```cypher
CALL () {
  RETURN 1 AS value
  UNION
  RETURN 1 AS value
  UNION DISTINCT
  RETURN 2 AS value
}
RETURN value ORDER BY value
```

结果：`1`、`2`。

<!-- example: path-union-all -->
```cypher
CALL () {
  RETURN 1 AS value
  UNION ALL
  RETURN 1 AS value
}
RETURN value, count(*) AS copies GROUP BY value
```

结果：`[1,2]`。

<!-- example: path-union-column-error -->
```cypher
RETURN 1 AS leftValue UNION RETURN 2 AS rightValue
```

结果：`SEMANTIC_ERROR`；两边列名不一致。即使某个分支实际上返回零行，也必须通过静态列检查。

<!-- example: path-union-flavor-error -->
```cypher
RETURN 1 AS value UNION ALL RETURN 1 AS value UNION RETURN 2 AS value
```

结果：`SEMANTIC_ERROR`。将 UNION 改为统一 flavor，或使用独立组合层次。

<a id="when"></a>

## WHEN：执行第一个成立的分支

```text
WHEN condition THEN { query }
[WHEN condition THEN { query } ...]
[ELSE { query }]
```

也支持不带花括号的分支 query；包含复合 query 时建议使用花括号显示边界。按书写顺序检查条件，执行第一个求值为 `true` 的分支；后续分支不执行。false 或 null 不命中；没有 ELSE 且均未命中时产生零行。条件是 Boolean 表达式，类型不符返回 `TYPE_ERROR`。这是一项 query composition，与只产生一个表达式值的 CASE 不同。

所有分支必须有一致的输出列名，且未被选中的分支仍经过 parse、scope 和语义检查。因此不能在 `WHEN false` 分支中隐藏未知函数、非法变量或被 execution option 禁止的写入。

<!-- example: path-when-first -->
```cypher
WHEN null THEN { RETURN 'null' AS route }
WHEN 2 > 1 THEN { RETURN 'chosen' AS route }
WHEN true THEN { RETURN 'later' AS route }
ELSE { RETURN 'fallback' AS route }
```

结果：`route="chosen"`。

<!-- example: path-when-no-branch -->
```cypher
WHEN false THEN { RETURN 1 AS value }
```

结果：列 `value`，零行。结果 schema 不因未选中分支而丢失。

在逐输入行条件处理中，把 WHEN 放在相关 CALL 中：

<!-- example: path-when-correlated -->
```cypher
UNWIND [0,1] AS n
CALL (n) {
  WHEN n = 0 THEN { RETURN 'zero' AS kind }
  ELSE { RETURN 'nonzero' AS kind }
}
RETURN n, kind ORDER BY n
```

结果：`[0,"zero"]`、`[1,"nonzero"]`。

<a id="next"></a>

## NEXT：把显式结果传给下一段

```text
query NEXT query
{ query UNION [ALL | DISTINCT] query } NEXT query
query NEXT { WHEN condition THEN { query } ELSE { query } }
```

NEXT 开始一个新的 query segment，前一段的显式结果列成为下一段的输入变量。非结果变量在边界处移除。NEXT 前的 RETURN 表达式应有显式 `AS` 别名；直接变量和 `RETURN *` 已具有名称。NEXT 不会自动提交，一次普通写 execution 内的所有段仍属于同一个原子写单元。

<!-- example: path-next-rows -->
```cypher
UNWIND [1,2] AS seed RETURN seed * 10 AS amount
NEXT
RETURN amount + 1 AS result ORDER BY result
```

结果：`11`、`21`。前段变量 `seed` 在后段不可见。

<!-- example: path-next-union -->
```cypher
{ RETURN 1 AS x } UNION ALL { RETURN 2 AS x }
NEXT
RETURN x + 1 AS result ORDER BY result
```

结果：`2`、`3`。UNION 的 flavor 要求以 NEXT 分段检查，不必强制所有 NEXT 段使用同一种 flavor。

<a id="next-unit-query"></a>

## 无结果 query 与零行结果的区别

显式返回零行的 query 把零行交给 NEXT，下一段不会凭空恢复一行。FINISH 或不返回结果的写 query 没有公开结果表；NEXT 后从相应的 unit-query 边界继续，内部变量不泄漏。常规无结果写段/FINISH 使用一次新的空绑定启动后段；作为输入管线终结的 unit CALL 或 YIELD Procedure 仍有其各输入/产出调用的行基数，不能把这些行为混为 RETURN。

<!-- example: path-next-empty-return -->
```cypher
UNWIND [] AS n RETURN n
NEXT
RETURN 1 AS value
```

结果：列 `value`，零行。

<!-- example: path-next-finish -->
```cypher
UNWIND [] AS n FINISH
NEXT
RETURN 1 AS value
```

结果：一行 `value=1`。FINISH 丢弃输出，表达完成 query 的意图。

<!-- example: path-next-scope-error -->
```cypher
CREATE (node:Internal)
NEXT
RETURN node
```

结果：`SEMANTIC_ERROR`，`node` 未作为显式结果交接；语义失败不会创建节点。

<a id="composition-write"></a>

## 与写入、事务和 Graph View 组合

后序 clause、子查询 invocation 或组合分支观察前序已完成的 staged 写入；组合语法不会创建额外的独立 Snapshot 或 SQLite connection。普通 top-level mutating execution 失败时全部回滚；分批事务的部分提交规则仅由 [Transaction Subqueries](transaction-subqueries.md) 指定。

Graph View 对每段与分支持续生效，并基于已完成写入重新计算可见性。历史 Commit 上不能通过 UNION、WHEN、NEXT 隐藏写入。`readOnlyNoExternalIO` 检查整个 AST；未选中的分支也不能包含被禁止的操作。

常见错误：输出列不一致、ALL/DISTINCT 混用、NEXT 后引用未交接变量、条件类型错误以及不完整 query。请以 [Errors](../reference/errors.md) 的 code 分类处理，不依赖错误消息文本。

标准语义参考：[Neo4j Composed Queries](https://neo4j.com/docs/cypher-manual/25/queries/composed-queries/)。

# Variable-length 与 Quantified Path

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

适用于 Lithograph v0.3.3，Cypher Profile `CY25-2026.08`。本章说明一段关系或一个子模式重复零次或多次时的匹配、变量和结果；固定元素语法见 [Patterns](patterns.md)。

<a id="quantified-forms"></a>

## 三种重复形式

```text
// Legacy variable-length relationship：长度在关系详情内
(a)-[r:R*1..3]->(b)

// Quantified relationship：次数跟在箭头后
(a)-[r:R]->{1,3}(b)

// Quantified path pattern：整个括号中的片段重复
(a) ((x)-[r:R]->(y) WHERE predicate){1,3} (b)
```

三种形式都按实际关系数/重复次数匹配，不代表跳过中间节点。`r` 在普通单段固定关系中是 Relationship，在 variable-length 或 quantified relationship 中是 Relationship List。Quantified path pattern 中内部的节点、关系变量在重复片段外变成 List，称为 group variables。

<a id="quantified-bounds"></a>

## 上下界

| Quantified 写法 | 重复次数 | Legacy 对应写法 |
| --- | --- | --- |
| `{m}` | 恰好 m 次 | `*m` |
| `{m,n}` | 从 m 到 n，含两端 | `*m..n` |
| `{m,}` | 至少 m 次 | `*m..` |
| `{,n}` | 从零到 n | `*0..n` |
| `+` | 一次或更多 | `*` 或 `*1..` |
| `*` | 零次或更多 | `*0..` |

Quantified 上下界是非负整数字面量，不能写参数或一般表达式；上界不能小于下界。同一关系不能同时使用 `*1..3` 和外部 `{1,3}`。零次重复绑定同一个起终节点，内部 group variables 为 `[]`，路径长度为零。独立完整路径必须仍包含至少一个节点。

默认关系唯一性使关系不能在同一 MATCH 中反复使用；这不等于节点不能重复。`REPEATABLE ELEMENTS` 允许重复关系，因此必须给每个重复片段提供有限上界，见 [模式组合限制](path-selection.md#path-mode-combinations)。即使查询带 `LIMIT`，它也不能让无限上界变成合法写法。

<a id="quantified-data"></a>

## 教学图

以下 setup 使用四个城市和五条单向路线。A→D 为直达；A→B→D、A→C→D 是两条中转路线。

<!-- example: path-quantified-setup -->
```cypher
CREATE (a:City {name:'A'}), (b:City {name:'B'}),
       (c:City {name:'C'}), (d:City {name:'D'}),
       (a)-[:ROAD {cost:1}]->(b), (b)-[:ROAD {cost:1}]->(d),
       (a)-[:ROAD {cost:2}]->(c), (c)-[:ROAD {cost:2}]->(d),
       (a)-[:ROAD {cost:9}]->(d)
RETURN 5 AS roads
```

结果：`roads=5`。本章后续成功例子使用这个 setup。

<a id="legacy-variable-length"></a>

## Legacy variable-length relationship

<!-- example: path-legacy-variable-length -->
```cypher
MATCH p = (a:City {name:'A'})-[r:ROAD*1..2]->(d:City {name:'D'})
RETURN length(p) AS hops, size(r) AS relationships
ORDER BY hops
```

结果：`[1,1]`、`[2,2]`、`[2,2]`。这里返回三条路径，不是一个可达性 Boolean；端点相同也不会自动合并。Legacy `*` 缺省最小次数为 1，而新式箭头后 `*` 的最小次数为 0。

Legacy 的 `*..n` 和 `*..` 省略下界时，下界也是 1；例如 `*..2` 是 1 到 2 跳，不是零到两跳。需要零跳必须写 `*0..n`。

Legacy 的空区间保留既有 Cypher 语义：`*2..1`、`*1..0` 和 `*..0` 都不匹配任何路径，普通 MATCH 返回零行，不报错。这与新式 quantified bounds 不同：`{3,1}` 等上界小于下界的形式返回 `SEMANTIC_ERROR`。

Legacy 写法保留在当前 Profile，但不能与显式 `WALK`、`TRAIL`、`ACYCLIC` path mode 组合；选择现代 quantified relationship 或 quantified path pattern，便于表达模式和内联谓词。

<a id="quantified-relationship"></a>

## Quantified relationship 与 group variable

<!-- example: path-quantified-relationship -->
```cypher
MATCH p = (a:City {name:'A'})-[r:ROAD]->{1,2}(d:City {name:'D'})
RETURN length(p) AS hops, reduce(total=0, edge IN r | total + edge.cost) AS cost
ORDER BY hops, cost
```

结果：`[1,9]`、`[2,2]`、`[2,4]`。`r.cost` 不是逐元素属性访问；应先 `UNWIND r`，或像上面用 List comprehension / `reduce()` 操作每条关系。

<!-- example: path-quantified-zero -->
```cypher
MATCH p = (a:City {name:'A'})-[r:ROAD]->{0,1}(b:City)
RETURN b.name AS name, length(p) AS hops, size(r) AS edges
ORDER BY hops, name
```

结果：`["A",0,0]`、`["B",1,1]`、`["C",1,1]`、`["D",1,1]`。零次不会凭空产生节点；它使用已匹配的 A 作为终点，并继续检查终点的 Label/属性条件。

<a id="quantified-group-pattern"></a>

## 重复整个 Path Pattern

完整片段可以包含多条关系，次数作用于整个片段。重复片段的末节点和下一次的首节点必须相同，相邻外部节点模式也连接到片段边界；例如 `(start) ((x)-->(y)){2} (end)` 是两次首尾相连的单跳。

<!-- example: path-quantified-group -->
```cypher
MATCH (a:City {name:'A'})
      ((x)-[r:ROAD]->(y) WHERE r.cost < 5){2}
      (d:City {name:'D'})
RETURN [n IN x | n.name] AS starts,
       [n IN y | n.name] AS ends,
       [edge IN r | edge.cost] AS costs
ORDER BY starts[1]
```

结果：`[["A","B"],["B","D"],[1,1]]` 和 `[["A","C"],["C","D"],[2,2]]`。

`x`、`y`、`r` 在片段内部是当前 repetition 的 singleton，内部 `WHERE r.cost < 5` 在每次 repetition 过滤；片段外是按书写遍历方向排列的 List。即使 planner 改变访问路径，List 的语义顺序也不改变。已经从外部作用域导入的变量仍是原绑定，不能借 group variable 重定义它。

<a id="quantified-predicates"></a>

## 谓词、null 与结果数量

内联 predicate 在每步产生候选时参与匹配，可以尽早排除无效路线；只保留 `true`。属性缺失导致的 `null` 不被当作 `true`。片段末尾 `WHERE` 可使用片段内变量，MATCH 末尾 WHERE 可以使用生成的 group List 或整条路径。对于 shortest/selective query，过滤位置还会影响哪些候选有资格参与选择，见 [Path Selection](path-selection.md#path-filter-placement)。

下例先枚举有限路径，再按 group List 的累计成本过滤：

<!-- example: path-quantified-filter -->
```cypher
MATCH p = (a:City {name:'A'})-[r:ROAD]->{1,2}(d:City {name:'D'})
WHERE all(edge IN r WHERE edge.cost <= 2)
RETURN length(p) AS hops, [edge IN r | edge.cost] AS costs
ORDER BY costs[0]
```

结果：`[2,[1,1]]`、`[2,[2,2]]`。如果只关心存在性，`EXISTS { MATCH ... }` 表达的意图更清楚；若需要计数，应明确计的是路径还是不同终点。

<a id="quantified-errors"></a>

## 错误与使用边界

- 新式 quantified bounds 的上界小于下界时返回 `SEMANTIC_ERROR`；legacy variable-length 的空区间返回零个匹配，不作为错误。关系同时使用两种量词返回 `SEMANTIC_ERROR`；不合法语法返回 `PARSE_ERROR`。
- `REPEATABLE ELEMENTS` 下无上界的 `+`、`*`、`{m,}` 被拒绝。
- group variable 在片段外是 List；把它当作 Node/Relationship 访问或写入会违反类型要求。
- 不能把整个量化片段用作 CREATE/INSERT/MERGE 的构造模板；写入使用有限且明确的节点和关系 Pattern。
- 不指定排序的多个路径结果无稳定顺序；路径总数可能远大于节点数量。优先给出业务合理上界、Label/Type 和局部条件，再选择需要的路径。

<!-- example: path-quantified-invalid-repeatable -->
```cypher
MATCH REPEATABLE ELEMENTS (a)-[:ROAD]->+(b)
RETURN a
```

结果：`SEMANTIC_ERROR`，因为重复模式无有限上界；即使空图也在语义分析阶段拒绝。

相关主题：[Path Selection](path-selection.md) · [Subqueries](subqueries.md) · [List Functions](functions/list.md)。标准语义参考：[Neo4j Variable-length Patterns](https://neo4j.com/docs/cypher-manual/25/patterns/variable-length-paths/)。

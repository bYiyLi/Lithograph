# Match Mode、Path Mode 与路径选择

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

适用于 Lithograph v0.3.2，Cypher Profile `CY25-2026.08`。Match mode 决定一个 MATCH 内是否允许重复关系，path mode 决定一条路径内能否重复元素，selector 决定每对起终点保留哪些路径。三者在语法和语义上分开。

<a id="path-selection-syntax"></a>

## 写法与作用范围

```text
MATCH [DIFFERENT RELATIONSHIPS | REPEATABLE ELEMENTS]
  [p =] [selector] [WALK | TRAIL | ACYCLIC] [PATH | PATHS] pattern
[WHERE predicate]
```

`PATH` / `PATHS` 是可读性关键词；可以跟在 selector 或 mode 的相应位置，不改变数量。例如 `p = ANY SHORTEST ACYCLIC PATH (a)-->(b)`。`MATCH` 后先写 match mode，`p =` 后先写 selector 再写 path mode。

<a id="match-mode"></a>

## Match mode

| Mode | 允许关系重用 | 节点重用 | 范围 |
| --- | --- | --- | --- |
| 省略或 `DIFFERENT RELATIONSHIPS` | 不允许 | 允许 | 同一个 MATCH 的所有逗号 Pattern |
| `REPEATABLE ELEMENTS` | 允许 | 允许 | 同一个 MATCH，配合有限上界和 WALK |

默认情况下，即使两个不同关系变量或两个逗号 Pattern 指向同一关系，也不能在同一次 MATCH 的匹配中重复使用它。拆开为两个 MATCH 会建立新的关系唯一性范围；这会改变可匹配结果。

<!-- example: path-mode-setup -->
```cypher
CREATE (a:Stop {name:'A'}), (b:Stop {name:'B'}),
       (a)-[:LINK]->(b), (b)-[:LINK]->(a)
RETURN 2 AS links
```

以下例子使用此 setup。默认模式下两条不同关系允许回到 A；ACYCLIC 排除这种重复节点。

<!-- example: path-mode-default -->
```cypher
MATCH p = (a:Stop {name:'A'})-[:LINK]->{1,3}(b)
RETURN b.name AS target, length(p) AS hops ORDER BY hops
```

结果：`["B",1]`、`["A",2]`。

<!-- example: path-mode-acyclic -->
```cypher
MATCH p = ACYCLIC (a:Stop {name:'A'})-[:LINK]->{1,3}(b)
RETURN b.name AS target, length(p) AS hops
```

结果仅 `["B",1]`。

<!-- example: path-mode-repeatable -->
```cypher
MATCH REPEATABLE ELEMENTS p = WALK (a:Stop {name:'A'})-[:LINK]->{3}(b)
RETURN b.name AS target, length(p) AS hops
```

结果：`["B",3]`，第一条 LINK 被再遍历一次。

<a id="path-mode"></a>

## Path mode

| Mode | 一条路径中的限制 |
| --- | --- |
| 省略或 `WALK` | 不额外限制，仍受外层 match mode 约束 |
| `TRAIL` | 关系不重复，节点可以重复 |
| `ACYCLIC` | 节点不重复，因此关系也不能重复 |

ACYCLIC 也排除回到起点的环和 self-loop；零长度路径只有一个节点，满足节点不重复。`TRAIL` 配默认 DIFFERENT RELATIONSHIPS 不会放宽外层唯一性。`SIMPLE` 不属于此 Profile 的 path mode。

<a id="path-mode-combinations"></a>

## 合法组合

- 同一 MATCH 的所有逗号 Pattern 必须使用同一个有效 path mode；一个省略 mode 的 Pattern 按 WALK 判断。
- `REPEATABLE ELEMENTS` 只能配 WALK；每个量化片段必须有有限上界。
- 显式 path mode 不能与 legacy `[:R*1..3]` 组合；改用 `[:R]->{1,3}`。
- 默认 DIFFERENT RELATIONSHIPS 下，只要使用一个非 ALL 的 selective selector，该 MATCH 就必须只有一个 path pattern。需要连接其它条件时先用前序 MATCH 绑定起点，再写单 Pattern 的 MATCH。
- 这些约束在空输入和空图时也检查，不能靠 WHERE 或 LIMIT 规避。

<!-- example: path-mode-invalid-combination -->
```cypher
MATCH REPEATABLE ELEMENTS TRAIL (a)-[:LINK]->{1,2}(b)
RETURN a
```

结果：`SEMANTIC_ERROR`。

<a id="path-selectors"></a>

## Selector 一览

选择以当前输入行和每对起终点为单位，不是整个查询的全局 LIMIT。Shortest 按关系跳数计算；关系上的 `cost`、距离、权重不会自动改变最短定义。

| Selector | 对每对起终点的输出 |
| --- | --- |
| 省略、`ALL`、`ALL PATHS` | 所有满足模式的路径 |
| `ANY`、`ANY PATH` | 任意一条，不保证业务上的最优 |
| `ANY k`、`ANY k PATHS` | 最多 k 条，不保证业务上的最优 |
| `ANY SHORTEST` | 最少跳数的一条；并列时不承诺选中哪一条 |
| `ALL SHORTEST` | 全部并列的最少跳数路径 |
| `SHORTEST k`、`SHORTEST k PATHS` | 按跳数选择最多 k 条；边界并列可能只取其中部分 |
| `SHORTEST k GROUPS`、`SHORTEST k PATH GROUPS` | 最小的 k 种不同跳数，对这些长度保留全部路径 |
| `SHORTEST GROUP` | `SHORTEST 1 GROUP`，即最小跳数的整个组 |

k 可以是正整数字面量或 Integer 参数；0、负数、null、String、Float 均不合法。k 大于可用数量时只返回实际存在的路径。没有路径时普通 MATCH 不产生行；OPTIONAL MATCH 按其 null 扩展规则保留输入。

<a id="shortest-examples"></a>

## Shortest 与 Group 的区别

下列例子使用 [Quantified Path 教学图](quantified-paths.md#quantified-data)：A→D 一条单跳，两条双跳。

<!-- example: path-selector-shortest -->
```cypher
MATCH p = ALL SHORTEST (a:City {name:'A'})-[:ROAD]->{1,2}(d:City {name:'D'})
RETURN length(p) AS hops
```

结果：一行 `1`。

<!-- example: path-selector-k -->
```cypher
MATCH p = SHORTEST 2 PATHS (a:City {name:'A'})-[:ROAD]->{1,2}(d:City {name:'D'})
RETURN length(p) AS hops ORDER BY hops
```

结果：`1`、`2`；双跳的两条路径只取其中一条。

<!-- example: path-selector-groups -->
```cypher
MATCH p = SHORTEST 2 GROUPS (a:City {name:'A'})-[:ROAD]->{1,2}(d:City {name:'D'})
RETURN length(p) AS hops ORDER BY hops
```

结果：`1`、`2`、`2`。GROUPS 限制的是不同长度的数量。

<!-- example: path-selector-parameter -->
```cypher
MATCH p = ANY $count PATHS (a:City {name:'A'})-[:ROAD]->{1,2}(d:City {name:'D'})
RETURN count(p) AS selected
```

参数 `{"count":2}`；结果 `selected=2`。不要依赖 ANY 选到具体哪两条。

<a id="path-filter-placement"></a>

## Predicate 的位置改变选择集合

Pattern 内的节点/关系谓词、quantified fragment 的谓词参与构造合法候选；路径选出后，MATCH 末尾 WHERE 再过滤绑定结果。因此“选最短后拒绝”可能返回空结果，“先排除某些关系再选最短”仍可能找到较长路径。

<!-- example: path-selector-post-filter -->
```cypher
MATCH p = ANY SHORTEST (a:City {name:'A'})-[r:ROAD]->{1,2}(d:City {name:'D'})
WHERE all(edge IN r WHERE edge.cost < 5)
RETURN length(p) AS hops
```

结果无行：先选到 cost=9 的单跳，再被 WHERE 过滤。

<!-- example: path-selector-pre-filter -->
```cypher
MATCH p = ALL SHORTEST (a:City {name:'A'})
      ((x)-[r:ROAD]->(y) WHERE r.cost < 5){1,2}
      (d:City {name:'D'})
RETURN length(p) AS hops
```

结果：`2`、`2`。候选先排除 cost=9 的关系，所以最短合法路径是双跳。过滤属性为 null 的候选同样不会保留。

<a id="path-selection-errors"></a>

## 错误与稳定性

非法 mode 组合、缺少有限上界、k=0（字面量或 Integer 参数）返回 `SEMANTIC_ERROR`；负 Integer 参数和非 Integer 参数（含 null）返回 `TYPE_ERROR`；负数字面量不符合 selector 的语法，返回 `PARSE_ERROR`；未知关键词/错序可能返回 `PARSE_ERROR`。需要稳定的结果展示时在 RETURN 中加 ORDER BY；它只能排列已经选中的路径，不能改变 ANY 或 SHORTEST k 对并列项的选择。若必须保留全部并列，应使用 ALL SHORTEST 或 GROUPS。

相关主题：[Quantified Path](quantified-paths.md) · [Patterns](patterns.md) · [Null 与排序](null-and-ordering.md)。标准语义参考：[Neo4j Shortest Paths](https://neo4j.com/docs/cypher-manual/25/patterns/shortest-paths/)。

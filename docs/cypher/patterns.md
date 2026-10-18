# 节点、关系与固定长度 Pattern

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

适用于 Lithograph v0.3.2，Cypher Profile `CY25-2026.08`。本章说明如何描述图结构；行和变量规则见 [查询模型](query-model.md)，重复路径见 [Quantified Path](quantified-paths.md)，重复元素和最短选择见 [Path Selection](path-selection.md)。

<a id="pattern-syntax"></a>

## Pattern 的组成

```text
MATCH [DIFFERENT RELATIONSHIPS | REPEATABLE ELEMENTS]
  [path =] [selector] [pathMode] nodePattern relationshipChain ...
  [, anotherPattern ...]
[WHERE predicate]

nodePattern         = ([variable] [labelExpression] [{key: expression, ...}] [WHERE predicate])
relationshipChain   = -[[variable] [:typeExpression] [{key: expression, ...}] [WHERE predicate]]-> nodePattern
```

方括号在此模板中表示可选项；实际的关系详情使用一对 `[`、`]`。匿名关系可以简写为 `-->`、`<--` 或 `--`。Pattern 描述期望的结构，匹配结果是满足它的实际节点、关系和路径；`MATCH` 不会创建数据。

| 元素 | 含义 | 绑定后的值 |
| --- | --- | --- |
| `(n)`、`()` | 任意节点，分别命名或匿名 | `n` 为 Node |
| `(n:Person)` | 有 `Person` Label 的节点 | Node |
| `(n {id:$id})` | 对列出的每个属性执行相等匹配 | Node |
| `(a)-[r:KNOWS]->(b)` | 从 `a` 指向 `b` 的关系 | `r` 为 Relationship |
| `(a)<-[r:KNOWS]-(b)` | 从 `b` 指向 `a` 的关系 | Relationship |
| `(a)-[r:KNOWS]-(b)` | 接受任意方向，不改变关系本身的端点 | Relationship |
| `p = (a)-->(b)-->(c)` | 绑定整个按遍历顺序排列的路径 | Path |

一个节点可有零到多个 Label；一个关系恰好有一个 Type。允许 self-loop 与端点、Type 相同的多条关系，因此不能把相同属性或相同端点当作 identity。节点和关系的 identity 用 `elementId()` 查看。

<a id="pattern-data"></a>

## 本章教学数据

在空的已初始化图中执行以下 setup。后面的成功例子以此图为起点；不依赖其它手册章节。

<!-- example: path-pattern-setup -->
```cypher
CREATE (a:Person:Reader {name:'An', age:30}),
       (b:Person {name:'Bo', age:20}),
       (c:Team {name:'Core'}),
       (a)-[:KNOWS {since:2024}]->(b),
       (a)-[:MEMBER_OF]->(c)
RETURN 2 AS people
```

结果：列 `people`，一行 `2`。

<a id="pattern-labels-types"></a>

## Label 与 Type expression

匹配支持以下名称表达式；`&` 比 `|` 优先，使用括号可明确分组，`!` 取反。

| 表达式 | 节点语义 | 关系语义 |
| --- | --- | --- |
| `:A` | 含 Label A | Type 是 A |
| `:A&B` 或 `:A:B` | 同时含 A、B | 因为关系仅有一个 Type，不同 A、B 的合取不匹配 |
| `:A\|B` | 含 A 或 B | Type 是 A 或 B |
| `:!A` | 不含 A | Type 不是 A |
| `:%` | 至少有一个 Label | 任意 Type |
| `:(A\|B)&!C` | 分组后的布尔条件 | 同样按名称逻辑求值 |
| `IS A` | `:A` 的 Label expression 写法 | 关系详情继续使用冒号 |
| `:$(expression)` | expression 产生 String 或 String List，List 中名称全部必须满足 | 必须产生一个 Type 名称；多 Type 合取不能匹配 |

动态名称值必须是非 null String，或满足该位置规则的非 null String List；不能把任意 Cypher 片段放入名称参数。参数使用 `:$(...)` 而不是 `:$label`。名称和值区分大小写。

<!-- example: path-pattern-label-expression -->
```cypher
MATCH (p:(Person|Team)&!Reader)
RETURN p.name AS name ORDER BY name
```

结果：`Bo`、`Core`。`!Reader` 还会接受完全没有 Label 的节点；若必须有至少一个 Label，可组合 `%`。

<!-- example: path-pattern-dynamic-label -->
```cypher
MATCH (p:$($label))
RETURN p.name AS name ORDER BY name
```

参数 `{"label":"Person"}`；结果：`An`、`Bo`。

<a id="pattern-properties-predicates"></a>

## 属性与 Pattern 内谓词

`{name:$name, age:30}` 是各项相等比较的合取。缺失属性读取为 `null`，`null` 相等比较不会得到 `true`，因此 `{name:null}` 不会匹配缺失属性；使用 `WHERE n.name IS NULL`。属性 Map 的值可以是表达式，但 Pattern 的 key 仍是名称；动态属性访问写在谓词中。

内联 `WHERE` 附在对应节点或关系上，适合表达局部过滤；它仍能使用该处已经可见的变量。谓词只保留值为 `true` 的候选，`false`、`null` 不匹配。普通 String/数值不能代替 Boolean 谓词。

<!-- example: path-pattern-inline-predicate -->
```cypher
MATCH (a:Person WHERE a.age >= 30)-[r:KNOWS WHERE r.since >= 2020]->(b)
RETURN a.name AS source, b.name AS target, r.since AS since
```

结果：`["An", "Bo", 2024]`。Clause 末尾的 `WHERE` 可以表达跨整个 Pattern 的条件；`OPTIONAL MATCH` 时附着位置会影响保留 null 的行为，见 [读取](reading.md)。

<a id="pattern-bindings"></a>

## 变量复用、连接与结果基数

同一变量再次出现要求是同一图元素，不是相同属性。例如 `(a)-->(b)-->(a)` 要求回到起点。已经由前序 clause 绑定的节点可以直接作为起点；绑定为 `null` 不会匹配任何实际节点。对新变量绑定和旧变量约束遵守相同作用域规则。

同一个 `MATCH` 中逗号连接的 Pattern 共同构成图模式：共享变量是 identity join；没有共享变量则产生笛卡尔组合。默认 `DIFFERENT RELATIONSHIPS` 还要求整个 `MATCH` 内关系不重复，拆成两个 `MATCH` 会建立两个独立的关系唯一性范围。匹配不会自动去重，多条关系或多条可满足路径可产生重复的投影值；需要时显式 `DISTINCT`。

<!-- example: path-pattern-connected -->
```cypher
MATCH (a:Person)-[:KNOWS]->(b), (a)-[:MEMBER_OF]->(t:Team)
RETURN a.name AS person, b.name AS friend, t.name AS team
```

结果：`["An", "Bo", "Core"]`。

<!-- example: path-pattern-named-path -->
```cypher
MATCH p = (a:Person {name:'An'})-[:KNOWS]->(b)
RETURN length(p) AS hops, [n IN nodes(p) | n.name] AS names,
       [r IN relationships(p) | type(r)] AS types
```

结果：`[1, ["An", "Bo"], ["KNOWS"]]`。`nodes()` 与 `relationships()` 遵循路径的遍历顺序；无 `ORDER BY` 时不同结果行之间的顺序不保证。

<a id="pattern-expression"></a>

## 在表达式中使用 Pattern

存在性条件可写成 `exists((a)-->(b))` 或 [EXISTS 子查询](subqueries.md#exists-count-collect)。Pattern comprehension 生成 List，支持可选路径绑定、过滤和投影：

```text
[(a)-->(b) [WHERE predicate] | expression]
[p = (a)-->(b) [WHERE predicate] | expression]
```

<!-- example: path-pattern-comprehension -->
```cypher
MATCH (a:Person {name:'An'})
RETURN exists((a)-[:KNOWS]->()) AS hasFriend,
       [(a)-[:KNOWS]->(b) WHERE b.age >= 18 | b.name] AS friends
```

结果：`[true, ["Bo"]]`。Comprehension 的新变量局限于其内部；需稳定 List 顺序时，使用含 `ORDER BY` 的 `COLLECT { ... }` 子查询。

<a id="pattern-errors"></a>

## 错误与组合边界

- 关系变量不能在需要 Node 的位置使用，未知变量或冲突绑定返回 `SEMANTIC_ERROR`。
- 动态名称的 null、类型或数量非法返回 `TYPE_ERROR` 或具体语义错误；名称转义规则见 [Syntax](syntax.md)。
- Pattern 内不能放聚合；先 MATCH，在 WITH/RETURN 中聚合。
- MATCH 使用当前 execution 的 Snapshot 和 Graph View；不可见关系不会成为可遍历候选，路径函数不能绕过该边界。
- CREATE/MERGE 的 Pattern 是构造或匹配后构造规则，不能把读取支持的取反、任意长度、选择器等直接当作写入形式，见 [Writing](writing.md)。

相关主题：[量化路径](quantified-paths.md) · [路径选择](path-selection.md) · [表达式](expressions.md)。标准语义参考：[Neo4j Cypher 25 Patterns](https://neo4j.com/docs/cypher-manual/25/patterns/)；Lithograph 支持范围以本手册的冻结 Profile 为准。

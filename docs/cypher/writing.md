# CREATE、MERGE、SET 与 DELETE

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

适用于 Lithograph v0.3.3，Cypher Profile `CY25-2026.08`。本章说明 graph-data mutation；Schema/Index DDL 见各自章节，Branch/Commit/Merge 等版本接口见 [Procedure Reference](../reference/procedures.md)。所有例子在隔离教学图运行。

<a id="create-insert"></a>

## CREATE 与 INSERT

```text
CREATE (n[:Label[:OtherLabel]] [{key:expression, ...}])
       [, morePatterns ...]
CREATE (a)-[r:TYPE [{key:expression, ...}]]->(b)
INSERT (n:Label&OtherLabel {key:expression})
```

对每个输入行创建 Pattern 中尚未绑定的节点和关系；已经绑定的节点作为端点复用。属性表达式可以用参数或前序 clause 变量。新元素的变量可用于后续 clause；不要在同一 CREATE 的属性表达式中依赖它尚未完成的其它新元素属性，先拆成前序 CREATE/WITH。

关系必须恰好有一个 Type，且创建时有明确方向；不能创建任意方向、Type 取反或任意长度关系。CREATE 不会去重：相同 Label/属性的两次创建得到两个不同 identity。

<!-- example: path-writing-create -->
```cypher
CREATE (a:Person {name:'An'}), (b:Person {name:'Bo'}),
       (a)-[r:KNOWS {since:2025}]->(b)
RETURN a.name AS source, b.name AS target, type(r) AS type
```

结果：`["An","Bo","KNOWS"]`。

CREATE 支持多 Label 用冒号或 `&` 表达，但同一个 CREATE clause 不能混用这两套多 Label 分隔风格。INSERT 使用 `&`，不支持冒号分隔多 Label，也不支持动态 Label/Type。二者均不支持匹配时的 Label 析取、取反或 wildcard 来决定要创建的名称。

<!-- example: path-writing-insert -->
```cypher
INSERT (n:Person&Reader {name:'Chen'})
RETURN n.name AS name, n:Person AS person, n:Reader AS reader
```

结果：`["Chen",true,true]`。

<a id="dynamic-create"></a>

## 动态 Label 与 Relationship Type

CREATE 的 `:$(expression)` 在每行求值。节点可接受 String 或 String List，产生相应 Labels；关系只接受一个 Type。名称不能为 null，List 中也不能含 null 或非 String。

<!-- example: path-writing-dynamic-create -->
```cypher
WITH ['Person','Author'] AS labelsToAdd, 'WROTE' AS relationshipType
CREATE (a:$(labelsToAdd) {name:'Dai'})-[r:$(relationshipType)]->(b:Book {title:'Graph'})
RETURN a.name AS author, a:Author AS hasLabel, type(r) AS type, b.title AS title
```

结果：`["Dai",true,"WROTE","Graph"]`。动态名称只选择名称，不解析任意语法；属性值继续通过参数绑定。

<a id="merge"></a>

## MERGE：整段匹配或创建

```text
MERGE patternPart
  [ON CREATE SET ...]
  [ON MATCH SET ...]
```

MERGE 先匹配整个 Pattern：找到匹配时使用现有元素，没有完整匹配时创建需要的整段。它不会自动复用部分未绑定 Pattern；若要“复用已有节点，仅确保关系存在”，先 MATCH/MERGE 绑定端点，再 MERGE 关系。MERGE 可能产生多个匹配行，并不承诺唯一；需要唯一性应建立 [KEY/UNIQUE constraint](constraints.md)。

`ON CREATE SET` 仅在创建分支执行，`ON MATCH SET` 仅在已匹配分支执行；其 SET 语法与普通 SET 一致。多个输入行按写入语义观察前序已完成的结果，重复 key 可命中先前创建元素。

<!-- example: path-writing-merge -->
```cypher
UNWIND [1,1,2] AS identity
MERGE (n:Counter {id:identity})
ON CREATE SET n.hits = 1
ON MATCH SET n.hits = n.hits + 1
WITH count(*) AS processed
MATCH (n:Counter)
RETURN n.id AS id, n.hits AS hits, processed ORDER BY id
```

结果：`[1,2,3]`、`[2,1,3]`。`processed` 是输入行数，最终只有两个节点。

MERGE 也支持无向关系 `(a)-[r:TYPE]-(b)`：匹配任一方向，创建分支按 Pattern 的左端到右端创建关系。它不同于要求明确方向的 CREATE/INSERT。

MERGE 的匹配属性不能为 null。创建关系时若端点变量为 null，也不能创建缺失端点。唯一约束冲突返回 `CONSTRAINT_ERROR` 并回滚，不会隐式覆盖其它节点；并发写入遵循 SQLite 与 Branch CAS，不保证靠无约束 MERGE 形成跨连接唯一业务键。

<!-- example: path-writing-merge-null-error -->
```cypher
MERGE (n:Person {id:null}) RETURN n
```

结果：`SEMANTIC_ERROR`。

<a id="set-properties"></a>

## SET 属性与 Map

```text
SET n.key = expression [, ...]
SET n[dynamicKeyExpression] = expression
SET n = mapExpression
SET n += mapExpression
```

| 写法 | 效果 |
| --- | --- |
| `SET n.key = value` | 新增或替换单个属性 |
| `SET n.key = null` | 删除该属性 |
| `SET n[key] = value` | 每行计算 String key，随后按普通属性写入 |
| `SET n = map` | 替换全部属性；Map 未出现的旧 key 被删除 |
| `SET n += map` | 合并 Map；未出现的旧 key 保留，Map 中 null 删除对应属性 |

`SET n += null` 不改变已有属性；`SET n = null` 清空全部属性。这些形式也适用于 Relationship 属性；Map 替换和合并不改变 Label、Type、端点或 identity。Property value 必须属于可持久化类型，不能把 Map、Node、Relationship、Path 或含 null/异构元素的非法 property List 作为属性保存。具体类型见 [Types](types.md)。

<!-- example: path-writing-set-map -->
```cypher
CREATE (n:Example {a:1,b:2})
SET n += {b:null,c:3}
RETURN properties(n) AS properties
```

结果：`{"a":1,"c":3}`。

<!-- example: path-writing-replace-map -->
```cypher
CREATE (n:Example {a:1,b:2})
SET n = {c:4}
RETURN properties(n) AS properties, n.a AS removed
```

结果：`[{"c":4},null]`。

<!-- example: path-writing-dynamic-key -->
```cypher
CREATE (n:Example {kept:1})
WITH n, 'score' AS key
SET n[key] = 7
REMOVE n[key]
RETURN n.score AS removed, n.kept AS kept
```

结果：`[null,1]`。Dynamic key 必须是非空 String；null/Integer 返回 `TYPE_ERROR`，空字符串返回 `SEMANTIC_ERROR`，Integer 不会隐式转成文本。

<a id="set-remove-labels"></a>

## SET / REMOVE Label 与 REMOVE 属性

```text
SET n:Label:OtherLabel
SET n:$(labelExpression)
REMOVE n.key [, n.otherKey ...]
REMOVE n[dynamicKeyExpression]
REMOVE n:Label:OtherLabel
REMOVE n:$(labelExpression)
```

SET 增加已存在 Label、REMOVE 删除不存在 Label/属性都是幂等操作。动态 Label 同 CREATE 一样逐行求值；节点可以拥有多个 Label，Relationship 没有可变 Label，其 Type 和端点不能用 SET 改写。要改变关系 Type，应显式创建新关系并删除旧关系，这会产生新的 identity。

<!-- example: path-writing-labels -->
```cypher
CREATE (n:Person:Draft {name:'An'})
SET n:Published
REMOVE n:Draft, n.name
RETURN n:Person AS person, n:Draft AS draft,
       n:Published AS published, n.name AS name
```

结果：`[true,false,true,null]`。对可选匹配得到的 null target，正常的属性/Label 更新不创建元素；不能借 SET 把 null 变成节点。

<a id="delete"></a>

## DELETE、NODETACH DELETE 与 DETACH DELETE

```text
DELETE expression [, expression ...]
NODETACH DELETE expression [, expression ...]
DETACH DELETE expression [, expression ...]
```

DELETE 与 NODETACH DELETE 同义：删除 Node 时若仍有未一并删除的 Relationship 引用它，操作失败并回滚。DETACH DELETE 先删除关联关系再删除目标节点。Relationship 可单独删除，Path 可作为删除目标，删除其组成元素；若路径节点还有路径外关系，普通 DELETE 仍受完整性约束。

删除目标表达式必须得到 Node、Relationship、Path 或 null；null 表示没有目标。不能传整数 identity、String、Map 或 List 代替图元素；删除 List 需要 FOREACH 或 UNWIND。已删除元素不应用于后续属性读取，需先保留必要标量，或只返回 counters/聚合结果。

<!-- example: path-writing-delete-setup -->
```cypher
CREATE (:Parent {name:'A'})-[:OWNS]->(:Child {name:'B'})
RETURN 1 AS relation
```

<!-- example: path-writing-delete-error -->
```cypher
MATCH (p:Parent) NODETACH DELETE p FINISH
```

以上错误例子使用 setup，结果为 `CONSTRAINT_ERROR`；父节点和关系都保留。

<!-- example: path-writing-detach-delete -->
```cypher
MATCH (p:Parent) DETACH DELETE p
WITH count(*) AS deletedParents
MATCH (child:Child)
RETURN deletedParents, count(child) AS remainingChildren
```

使用相同 setup，结果：`[1,1]`。子节点仍在，关联关系被删除。

<a id="foreach"></a>

## FOREACH：只执行更新的循环

```text
FOREACH (variable IN listExpression |
  updateClause [updateClause ...]
)
```

内部允许 CREATE、INSERT、MERGE、SET、REMOVE、DELETE/NODETACH/DETACH DELETE 与嵌套 FOREACH；不能在内部放 MATCH、WITH、RETURN 或一般 CALL。需要每项查询时用 UNWIND 加 CALL 子查询。

循环变量及内部新变量不向外导出，外部已有变量可以作为更新目标。FOREACH 不扩增或过滤外部行；空 List 和 null 执行零次。非 List 的非法输入按类型规则拒绝。循环中的一次失败使普通 execution 全部回滚，前几轮更新不会部分保留。

<!-- example: path-writing-foreach -->
```cypher
WITH [1,2,3] AS values
FOREACH (value IN values | CREATE (:Item {value:value}))
WITH size(values) AS requested
MATCH (n:Item)
RETURN requested, count(n) AS created
```

结果：`[3,3]`。

<a id="write-atomicity"></a>

## 多行、Clause 可见性与原子性

普通 top-level graph/schema/index write 是一个逻辑写单元。前一 clause 的全部 effects 对后序 clause 可见；前序 clause 不观察后序写入。CALL、FOREACH、UNION、WHEN、NEXT 不自行拆分 Commit。结果行数、创建元素数和 Commit 数是不同概念。

成功写入进入 immutable history，Branch head 与 graph/schema write 在同一 SQLite transaction 移动。未知变量、类型错误、约束、Graph View、冲突、取消或执行错误会回滚本次普通写入；streaming 写 query 也需要消费至 terminal summary 才能确认成功，提前关闭按接口规则回滚。多个 execution 合成一个 Commit 使用 [Lithograph explicit transaction](../guide/transactions.md)；`IN TRANSACTIONS` 则按 [batch 规则](transaction-subqueries.md)独立提交。

Graph View 在每个 mutation clause 边界校验：新节点必须在创建 clause 完成时可见；不能靠后续 SET 才补足必需 Label。SET/REMOVE 不能把节点移出已选 View；DETACH DELETE 也不能越过 View 删除隐藏关系。这些失败返回 `GRAPH_VIEW_VIOLATION`。对历史 Commit 的写入返回历史只读错误；`readOnlyNoExternalIO=true` 在 prepare 阶段拒绝整个写 AST。

<!-- example: path-writing-dynamic-key-error -->
```cypher
CREATE (n:Rollback)
SET n[42] = 1
RETURN n
```

结果：`TYPE_ERROR`，新节点不保留。应用应按 [Errors](../reference/errors.md) 的稳定 code 处理失败；宿主显式事务的失败会影响整个 active transaction，见 [事务参考](../reference/transactions.md)。

标准语义参考：[Neo4j Updating Clauses](https://neo4j.com/docs/cypher-manual/25/clauses/) 与 [MERGE](https://neo4j.com/docs/cypher-manual/25/clauses/merge/)。

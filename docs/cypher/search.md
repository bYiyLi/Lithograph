# Vector SEARCH

适用 Lithograph v0.3.1、`CY25-2026.08`。[Cypher 手册](index.md) · [兼容范围](../reference/cypher-compatibility.md) · [Search 操作指南](../guide/search.md)

SEARCH 是 MATCH/OPTIONAL MATCH 的 Vector 检索子句。它从指定标准 VECTOR Index 选择与 query vector 最相似的元素，再继续 Cypher 行处理。调用方负责向量值；文本自动 Embedding 使用独立的 [Managed Semantic Procedure](../reference/procedures/semantic.md)，全文检索使用 [Full-text Procedure](../reference/procedures/fulltext.md)。

<a id="syntax"></a>

## 语法与输入

```text
MATCH <pattern>
SEARCH variable IN (
  VECTOR INDEX indexName FOR <vector-expression>
  [WHERE <additional-property-predicate>]
  LIMIT <integer-expression>
)
[SCORE AS scoreVariable]
[WHERE <match-predicate>]
RETURN ...
```

`MATCH` 可替换为 `OPTIONAL MATCH`。indexName 是 Schema 标识符；`FOR` 是 Vector/List expression，允许参数和之前已绑定的变量。query vector、filter value 不能依赖本次 SEARCH 要绑定的 variable。LIMIT 必须提供，支持参数，接受 INTEGER 0–2147483647；负数、FLOAT、null 或越界值返回 `SEMANTIC_ERROR`。

Index 必须是目标 Snapshot 中存在的标准 VECTOR Index，实体类型必须匹配。不存在、错误 kind 或 Node/Relationship mismatch 返回 `SEMANTIC_ERROR`。DDL 与索引配置见 [VECTOR Index](indexes.md#vector)。

query vector 接受非空 VECTOR 或全部成员为 Integer/Float 的 List；维度必须符合显式 index dimensions。未固定 dimensions 时只检索 query vector 对应维度。数值进入检索时按有限 FLOAT32 coordinates 参与比较；cosine 不接受零范数。实际进入候选检索时，错误类型、null 元素、非有限值、维度不符或 cosine 零向量失败；不会转换为零结果。

query expression 本身为 null 是特定的无匹配情况：MATCH 返回零行，OPTIONAL MATCH 保留输入行并填充 null；LIMIT 0 同样不产生候选，且不进入 query vector 的类型/维度/范数检验。Index、pattern 和 filter 合法性仍须成立，表达式求值本身产生的错误也不会被短路吞掉。

<a id="patterns"></a>

## SEARCH 可使用的 Pattern

SEARCH 的 MATCH 必须只有一个 pattern part，最多一个固定 relationship hop；不能使用 variable length/quantified pattern、命名 Path 或非 ALL path selector。本次 Pattern 只能命名 SEARCH variable；其它节点/关系保持匿名，不能在这些未绑定元素上添加 Label/Type、属性或内联 WHERE 限制。

典型形式为节点 `(d:Doc)`、关系 `()-[r:SIMILAR]->()`。关系 index 可以通过 endpoints 检查可见性；需要复杂邻接查询时先通过 SEARCH 产生结果，再用后续 MATCH 绑定其它元素：

```text
MATCH (d:Doc) SEARCH d IN (VECTOR INDEX doc_embedding FOR $query LIMIT $k)
WITH d
MATCH (d)-[:AUTHORED_BY]->(a:Author)
RETURN d, a
```

SEARCH variable 之前已有绑定时，候选还必须与该绑定相同。每个输入行独立计算 query/filter/LIMIT，执行检索，再与后续 clause 组合。

<a id="filters"></a>

## SEARCH WHERE 与 MATCH WHERE

括号内 SEARCH WHERE 在 top-k/limit 之前筛选候选。只允许使用 Index `WITH [...]` 声明的 additional properties，支持：

| 形式 | 示例 |
| --- | --- |
| equality | `d.lang = 'zh'`，也可交换 property/value 两侧 |
| range | `d.year >= 2024 AND d.year < 2027` |
| IN | `d.lang IN $languages` |
| Boolean property | `d.published` 或 `NOT d.published` |
| null predicate | `d.lang IS NULL`、`d.lang IS NOT NULL` |
| conjunction | 不同条件用 `AND` 组合 |

不支持 OR、任意函数包裹字段、两个 SEARCH property 相互比较、未登记字段，或查询依赖本次 variable 的 value expression。同一个字段不能有两条同方向 range boundary，也不能把 equality 与 range 混用；允许一条下界加一条上界。

Comparison value 可以是 null、Boolean、Integer、Float、String、Temporal 或 Duration；Point/UUID/Vector、容器和图元素不作为 filter scalar。IN 的右侧必须是 scalar List；仅由 null/numeric/String 构成的列表不额外限制长度，其它支持类型最多 256 个值，超限返回 `INVALID_ARGUMENT`。错误类型的 runtime filter value 返回 `TYPE_ERROR`，不被当成 false。合法 predicate 只在结果为 true 时选中，false/null 不选中。

括号外 `WHERE` 是普通 MATCH predicate，在 SEARCH 选出 top-k 后再筛选；它可能使最终结果少于 k。若目标是“某种语言中最相似的 k 个”，应将 lang 声明为 additional property 并放进 SEARCH WHERE；后置 WHERE 无法补回被丢弃的名额。

<a id="score"></a>

## Score、排序与结果行

SCORE AS 绑定 FLOAT score，数值越大越相似。cosine score 为 `(1 + cosine) / 2`，euclidean score 为 `1 / (1 + squared-distance)`，范围为 0–1。它不是概率，也不应直接与 Full-text score 比较。

内部候选按 score 降序，同分按 element identity 稳定排列。需要对外保证整体查询顺序时显式 `ORDER BY score DESC, ...`；后续 clause、聚合或组合可以改变原顺序。SEARCH 采用 HNSW ANN，quantization 和 expansion 影响近似召回；不能把每次 top-k 的成员承诺为完整精确扫描的全局最优集。重建/fallback 仍使用相同值和过滤规则。

Graph View 在候选计数和 top-k 前生效。隐藏 Node、Relationship 或隐藏 endpoint 不消耗 limit 名额。Historical search 使用历史 Graph/Index definition；cache 缺失时重建或正确 fallback。索引不是结果的独立真源。

<a id="examples"></a>

## 完整节点示例

先执行 [Index 定义示例](indexes.md#vector)中的 `doc_embedding`，再写入三个应用生成的向量：

<!-- example: schema-search-data -->
```cypher
CREATE (:Doc {name:'Graph', lang:'en', published:true, embedding:[1.0,0.0]}),
       (:Doc {name:'Nearby', lang:'fr', published:true, embedding:[0.8,0.2]}),
       (:Doc {name:'History', lang:'en', published:false, embedding:[0.0,1.0]})
FINISH
```

<!-- example: schema-search-top -->
```cypher
MATCH (d:Doc)
SEARCH d IN (VECTOR INDEX doc_embedding FOR [1.0,0.0]
             WHERE d.lang = 'en' LIMIT 2)
SCORE AS score
RETURN d.name AS name, score ORDER BY score DESC, name
```

返回 `['Graph',1.0]`、`['History',0.5]`；Nearby 在 top-k 前因 lang 被排除。

<!-- example: schema-search-null -->
```cypher
WITH null AS query
OPTIONAL MATCH (d:Doc)
SEARCH d IN (VECTOR INDEX doc_embedding FOR query LIMIT 2)
SCORE AS score
RETURN d.name AS name, score
```

同一 Index 已建立时返回 `[null,null]`；OPTIONAL MATCH 的输入行被保留。

<!-- example: schema-search-zero -->
```cypher
MATCH (d:Doc)
SEARCH d IN (VECTOR INDEX doc_embedding FOR [1.0,0.0] LIMIT 0)
RETURN d.name AS name
```

返回零行。

<!-- example: schema-search-invalid-filter -->
```cypher
MATCH (d:Doc)
SEARCH d IN (VECTOR INDEX doc_embedding FOR [1.0,0.0]
             WHERE d.name = 'Graph' LIMIT 1)
RETURN d.name AS name
```

返回 `SEMANTIC_ERROR`，因为 name 没有在 WITH 声明中。

<!-- example: schema-search-invalid-limit -->
```cypher
MATCH (d:Doc)
SEARCH d IN (VECTOR INDEX doc_embedding FOR [1.0,0.0] LIMIT null)
RETURN d.name AS name
```

返回 `SEMANTIC_ERROR`。

<a id="relationships"></a>

## Relationship 检索

<!-- example: schema-search-rel-index -->
```cypher
CREATE VECTOR INDEX similar_embedding
FOR ()-[r:SIMILAR]-() ON (r.embedding)
OPTIONS {indexConfig: {`vector.dimensions`:2}}
```

<!-- example: schema-search-rel-data -->
```cypher
CREATE (a:Item), (b:Item),
       (a)-[:SIMILAR {name:'pair', embedding:[1.0,0.0]}]->(b)
FINISH
```

<!-- example: schema-search-relationship -->
```cypher
MATCH ()-[r:SIMILAR]->()
SEARCH r IN (VECTOR INDEX similar_embedding FOR [1.0,0.0] LIMIT 1)
RETURN r.name AS name
```

依次执行后返回 `'pair'`。后续查询可在 WITH r 后调用 startNode/endNode 或继续匹配实际端点。

<a id="contexts"></a>

## 执行上下文与失败

SEARCH 可以在普通读写 pipeline、历史只读 Snapshot、Graph View 和 Lithograph explicit transaction 中使用，事务内读取当前 staged graph/index state。`readOnlyNoExternalIO:true` 允许 Raw Vector SEARCH；它不调用 Provider。

普通错误终止整次 execution；streaming 结果必须消费到 summary 才表示完成，先读到一条命中不能证明后续查询成功。SEARCH 本身不拥有独立 batch transaction。复杂搜索与调用组合见[子查询](subqueries.md)、[事务接口](../reference/transactions.md)、[错误参考](../reference/errors.md)。

# Full-text Procedure 与查询字符串

适用于 Lithograph v0.3.3 · [产品手册](../../guide/README.md) · [Procedure 总入口](../procedures.md) · [全文工作流](../../guide/search.md)

Full-text 使用当前 SQLite runtime 的 FTS5。索引创建语法、DDL options 与版本化定义见 [Index 语言参考](../../cypher/indexes.md)；本页拥有查询参数、字符串内的检索表达式和结果语义。

<a id="proc-db-index-fulltext-querynodes"></a>

## `db.index.fulltext.queryNodes`

```text
CALL db.index.fulltext.queryNodes(indexName, queryString [, options])
YIELD node, score
```

<a id="proc-db-index-fulltext-queryrelationships"></a>

## `db.index.fulltext.queryRelationships`

```text
CALL db.index.fulltext.queryRelationships(indexName, queryString [, options])
YIELD relationship, score
```

两项均为 `READ`。`indexName :: STRING` 指向选定 Snapshot 中存在的 FULLTEXT Index；Node/Relationship family 必须与所用 Procedure 一致。`queryString :: STRING` 是全文查询表达式，可以使用参数或当前行的表达式。`options :: MAP` 可省略，默认 `{}`，不接受 `null`。

| options key | 类型、默认及含义 |
| --- | --- |
| `skip` | 非负 `INTEGER`；默认 `0`；跳过可见且已按相关性排序的命中 |
| `limit` | 非负 `INTEGER`；省略时不额外限制命中数；`0` 返回零行 |
| `analyzer` | 非空、无 NUL 的 `STRING`，即完整 FTS5 tokenizer specification；省略使用历史 IndexDefinition 保存的值 |

未知 key、浮点分页数、负数和显式 `null` 均失败。返回 `node :: NODE, score :: FLOAT` 或 `relationship :: RELATIONSHIP, score :: FLOAT`，每个命中元素一行；无命中为零行。score 非负、数值越大相关性越高，同分按元素 identity 稳定排序。score 来自 FTS5 BM25 的映射，不是概率，不保证与 Neo4j/Lucene 相等，也不适合跨查询或 analyzer 比较。需要业务稳定展示顺序时在 `RETURN` 后增加自己的 `ORDER BY`。

索引对目标集合使用“任一 Label/Type 命中”规则。每个被索引 Property 接受 String 或全为 String 的 List；String List 以空格连接交给 tokenizer，其它类型、缺失及 null 不贡献文本。它与 Managed Semantic 的单 String source 规则不同。

<a id="query-string"></a>

## queryString 的写法

字符串中的表达式由全文检索处理，外层仍是普通 Cypher STRING。参数绑定能够避免外层引号混淆，不能把 queryString 变成任意 Cypher clause。当前可使用以下形式：

| 写法 | 含义 |
| --- | --- |
| `graph` | 单个词项，按索引 tokenizer 匹配 |
| `graph database` | 平坦的多个未修饰词项按 OR 组合 |
| `"graph database"` | 有顺序与相邻位置要求的短语 |
| `graph*` | 词项前缀 |
| `graph AND sqlite`、`graph OR sqlite` | 显式布尔组合 |
| `graph NOT cloud` | 左侧命中并排除右侧 |
| `(graph OR sqlite) AND embedded` | 用括号组合 FTS5 布尔表达式 |
| `title:graph` | 限定到索引包含的 `title` Property |
| `+graph sqlite -cloud` | 平坦 required/optional/prohibited 词项；必需 graph，排除 cloud；无 required 时 optional 词项使用 OR |
| `NEAR(graph sqlite, 5)` | FTS5 proximity group；由实际 tokenizer 产生的位置决定距离 |

显式布尔组合与平坦 `+`/`-` 修饰是两种写法，不应混用并假定所有 Lucene Query Parser 语法均可移植。拼写、转义和复杂表达式遵循实际 FTS5 行为；纯 prohibited 查询没有正向候选，不能拿它表示全图补集。属性限定必须引用当前索引中的 Property。包含特殊字符的词项可在 queryString 内用双引号包围；外层 Cypher 字符串再使用单引号或参数。

教学数据与索引，后续例子均各自先执行这两个 setup：

<!-- example: proc-ft-data -->
```cypher
CREATE (:Doc {id:'a', title:'Graph guide', text:'graph database sqlite'}),
       (:Doc {id:'b', title:'Cloud guide', text:'graph cloud'}),
       (:Doc {id:'c', title:'SQLite guide', text:'embedded sqlite'})
RETURN count(*) AS created
```

<!-- example: proc-ft-index -->
```cypher
CREATE FULLTEXT INDEX doc_text
FOR (d:Doc) ON EACH [d.title, d.text]
OPTIONS {indexConfig: {`fulltext.analyzer`:'unicode61'}}
```

<!-- example: proc-ft-phrase -->
```cypher
CALL db.index.fulltext.queryNodes('doc_text', '"graph database"', {limit:10})
YIELD node, score RETURN node.id AS id, score > 0 AS ranked
```

返回 `['a', true]`。

<!-- example: proc-ft-flat -->
```cypher
CALL db.index.fulltext.queryNodes('doc_text', '+graph -cloud')
YIELD node RETURN node.id AS id ORDER BY id
```

返回 `a`。

<!-- example: proc-ft-field -->
```cypher
CALL db.index.fulltext.queryNodes('doc_text', 'title:SQLite')
YIELD node RETURN node.id AS id
```

返回 `c`。

<!-- example: proc-ft-prefix -->
```cypher
CALL db.index.fulltext.queryNodes('doc_text', 'embed*')
YIELD node RETURN node.id AS id
```

返回 `c`。

<!-- example: proc-ft-boolean -->
```cypher
CALL db.index.fulltext.queryNodes('doc_text', 'graph AND sqlite')
YIELD node RETURN node.id AS id
```

返回 `a`。

使用原生 proximity group 的例子返回 `a`：

<!-- example: proc-ft-near -->
```cypher
CALL db.index.fulltext.queryNodes('doc_text', 'NEAR(graph sqlite, 5)')
YIELD node RETURN node.id AS id
```

纯 prohibited 查询不产生正向候选；以下返回零行：

<!-- example: proc-ft-pure-prohibited -->
```cypher
CALL db.index.fulltext.queryNodes('doc_text', '-cloud')
YIELD node RETURN node.id AS id
```

<a id="analyzer"></a>

## Analyzer 与宿主 tokenizer

默认 `unicode61`；词干化可使用 `porter unicode61`；带参数的例子是 `unicode61 remove_diacritics 0`。`analyzer` 覆盖只改变本次查询文本的分词，索引文档始终按 IndexDefinition 的 specification 分词；不会修改 Schema 或创建 Commit。一个无法命中已索引 token 的 override 可以合法返回零行。

<!-- example: proc-ft-analyzer -->
```cypher
CALL db.index.fulltext.queryNodes('doc_text', 'graph',
  {skip:0, limit:1, analyzer:'unicode61'})
YIELD node, score RETURN score > 0 AS ranked
```

返回一行 `true`。

第三方 tokenizer 由宿主在每个实际 SQLite connection 注册；可用名及参数由插件决定，Lithograph 不安装或自动加载插件。`english` / `standard-no-stop-words` 没有内置 alias。历史定义保存名字和参数，不保存插件二进制或词典；要复现历史结果，宿主还需固定 runtime、插件与外部资源。

<a id="relationship-example"></a>

## Relationship 查询

<!-- example: proc-ft-rel-data -->
```cypher
CREATE (a:Person {name:'Ada'}), (b:Person {name:'Bo'}),
       (a)-[:KNOWS {note:'graph friends'}]->(b)
RETURN count(*) AS created
```

<!-- example: proc-ft-rel-index -->
```cypher
CREATE FULLTEXT INDEX relation_text
FOR ()-[r:KNOWS]-() ON EACH [r.note]
```

<!-- example: proc-ft-rel-query -->
```cypher
CALL db.index.fulltext.queryRelationships('relation_text', 'friends', {limit:5})
YIELD relationship, score
RETURN relationship.note AS note, score > 0 AS ranked
```

返回 `['graph friends', true]`。

<a id="context-errors"></a>

## Snapshot、事务、Graph View 与失败

普通查询使用 pinned Snapshot；`options.at` 使用那个 Commit 的 definition 和文本。active Lithograph explicit transaction 能检索前序执行已完成的 staged graph/Index state。Graph View 先检查 Node，或 Relationship 及两端 Node，再计算 skip/limit，隐藏命中不占分页名额。缓存是 connection-local TEMP derived data；缺失时从当前目标状态重建，不写 main、不新建 Commit。

两个 SQL execution surface 均可调用。`readOnlyNoExternalIO:true` 会在 prepare 阶段返回 `INVALID_ARGUMENT`，因为实际 tokenizer 可能执行宿主代码或访问外部资源；`EXPLAIN` / 零 limit 不能豁免安全检查。普通 EXPLAIN 不构造 tokenizer，也不证明查询所需 tokenizer 可用。

| 失败 | category |
| --- | --- |
| 参数数量、String/Map 类型、未知 key、负/错误类型分页参数、非法查询表达式、analyzer 不可用、索引缺失或 target family 不匹配 | `SEMANTIC_ERROR` |
| tokenizer / SQLite I/O、锁、资源或中断 | 对应 `IO_ERROR` / `BUSY` / `RESOURCE_ERROR` / `INTERRUPTED` |
| 缓存清理或内部不变量失败 | `INTERNAL_ERROR` 或更具体错误 |

有效的空 queryString 或 limit 0 返回零行，但实际执行时仍验证配置与 tokenizer 可用性。失败不返回部分结果，不静默换回默认 tokenizer。SQLite rollback 不撤销第三方 native tokenizer 自身的外部副作用。

<!-- example: proc-ft-invalid-options -->
```cypher
CALL db.index.fulltext.queryNodes('doc_text', 'graph', {limit:null})
YIELD node RETURN node.id AS id
```

该调用返回 `SEMANTIC_ERROR`。

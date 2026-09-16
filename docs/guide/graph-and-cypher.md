# Graph 与 Cypher

适用 v0.1.0。以下 SQL 示例在独立空库、已加载扩展的 connection 中依次运行。

## 数据模型

Node 有独立身份、零到多个 Label 和 Property。Relationship 有独立身份、起点、终点、恰好一个 Type，也可以拥有 Property。允许 self-loop 和相同端点、相同 Type 的多条关系；业务唯一性需要显式约束，而不是依靠端点组合猜测。

```sql
SELECT lithograph_init();
SELECT lithograph(
  'CREATE (a:Person:Employee {id: ''alice'', name: ''Alice'', age: 30}),
          (b:Person {id: ''bob'', name: ''Bob'', age: 40}),
          (a)-[:KNOWS {since: 2020}]->(b)
   RETURN elementId(a) AS alice, elementId(b) AS bob'
);
```

`elementId()` 返回 `n:<id>` / `r:<id>` 字符串，在同一数据库不同 Branch 和历史状态中保持身份一致。它不是业务主键，也不是跨数据库全局 ID；跨数据库引用还需要 `databaseId`。不要根据 ID 连续性推断记录数。

## MATCH、过滤与投影

```sql
SELECT lithograph(
  'MATCH (p:Person)
   WHERE p.age >= $minimum
   RETURN p.id AS id, p.name AS name
   ORDER BY id LIMIT 10',
  '{"minimum":35}'
);

SELECT lithograph(
  'MATCH (a:Person)-[r:KNOWS]->(b:Person)
   RETURN a.name AS source, b.name AS target, r.since AS since'
);

SELECT lithograph(
  'MATCH p = (:Person {id: ''alice''})-[:KNOWS*1..3]->(:Person)
   RETURN length(p) AS hops'
);
```

分别返回 Bob、Alice 到 Bob 的关系和路径长度 `1`。按需投影字段比总是返回完整 Node/Path 更适合较大结果。没有 `ORDER BY` 时，不依赖偶然返回顺序；排序列相同时，用业务唯一键提供稳定 tie-breaker。

## 聚合、批量输入和修改

```sql
SELECT lithograph('MATCH (p:Person) RETURN count(p) AS people');

SELECT lithograph(
  'UNWIND $people AS item
   MERGE (p:Person {id: item.id})
   SET p.name = item.name
   RETURN p.id AS id ORDER BY id',
  '{"people":[{"id":"carol","name":"Carol"},{"id":"dave","name":"Dave"}]}'
);

SELECT lithograph(
  'MATCH (p:Person {id: ''alice''})
   SET p.age = 31, p.nickname = ''A''
   REMOVE p:Employee
   RETURN p.age AS age, labels(p) AS labels'
);

SELECT lithograph(
  'MATCH (p:Person {id: ''alice''}) SET p.nickname = null
   RETURN p.nickname AS nickname'
);
```

`SET property = null` 删除 Property，读取缺失 Property 返回 `null`。`MERGE` 按模式匹配，不替代 Schema 中的唯一性/存在性约束；并发应用应显式定义业务键，见 [Schema](schema-and-indexes.md)。一条含 `UNWIND` 的普通写查询只形成一个 Commit，而不是每个对象一个。

## 删除

```sql
SELECT lithograph(
  'MATCH (p:Person {id: ''dave''}) DELETE p FINISH'
);

SELECT lithograph(
  'MATCH (p:Person {id: ''bob''}) DETACH DELETE p FINISH'
);

SELECT lithograph('MATCH (p:Person) RETURN p.id AS id ORDER BY id');
```

最后返回 `alice`、`carol`。`DELETE` 不能遗留指向不存在节点的关系；`DETACH DELETE` 同时删除相连关系。在 Graph View 中涉及不可见关系时会失败，而不是越界删除。

删除会生成新 Commit，不抹掉已有历史。需要恢复旧图时使用版本 API，不直接编辑内部表。

## Cypher 与宿主 SQL 的分工

SQL 负责加载、调用接口、事务和解析 JSON；Cypher 负责图查询、图写入、Schema、检索和 procedures。一次 `lithograph()` 输入是一个 Cypher query，不是用分号拼接的脚本。

`lithograph_validate()` 可检查静态合法性，但不能证明给定参数、当前数据、权限或约束一定允许执行。`EXPLAIN` 只查看计划；`PROFILE` **真的执行**查询，包含写入时会改变图。

完整语言范围与内置函数见 [Compatibility](../reference/cypher-compatibility.md)、[Functions](../reference/functions.md)。复杂类型与参数保真见 [Values](../reference/values.md)。

# Schema、Constraint 与 Index

适用 v0.1.0 基础接口与 v0.2.0 Semantic Index 增量。目标：在一个新的演示库中定义业务键、字段类型与索引，并确认这些定义随图一起进入历史。已加载扩展后依次执行本页 SQL。

## 从 schema-free 开始

空库允许 schema-free 图。Graph Type、独立 Constraint 和 Index definition 共同组成版本化 Schema；不需要先设计所有领域对象才能创建节点。

```sql
SELECT lithograph_init();
SELECT lithograph(
  'CREATE CONSTRAINT person_id FOR (p:Person) REQUIRE p.id IS NODE KEY'
);
SELECT lithograph(
  'CREATE CONSTRAINT person_age_type FOR (p:Person) REQUIRE p.age IS :: INTEGER'
);
SELECT lithograph(
  'CREATE (:Person {id:''alice'', age:30}), (:Person {id:''bob'', age:40}) FINISH'
);
```

KEY 同时要求存在与唯一；UNIQUE 不等于 NOT NULL。单独的 Property Type Constraint 约束有值时的类型，不自动使字段必填。需要必填时另加存在性约束。

```sql
SELECT lithograph(
  'CREATE CONSTRAINT person_age_required FOR (p:Person) REQUIRE p.age IS NOT NULL'
);
SELECT lithograph('SHOW CONSTRAINTS YIELD name, type RETURN name, type ORDER BY name');
```

之后重复创建相同 `id`、缺失必填字段或写入错误类型都会使对应查询失败，旧 Branch head 不变。创建新约束也会验证已有数据；不能先成功创建一个与当前数据矛盾的约束。

## 创建标准索引

```sql
SELECT lithograph('CREATE RANGE INDEX person_age FOR (p:Person) ON (p.age)');
SELECT lithograph('CREATE TEXT INDEX person_name FOR (p:Person) ON (p.name)');
SELECT lithograph('CREATE POINT INDEX person_location FOR (p:Person) ON (p.location)');
SELECT lithograph('CREATE LOOKUP INDEX relation_types FOR ()-[r]-() ON EACH type(r)');
SELECT lithograph('SHOW INDEXES YIELD name, type RETURN name, type ORDER BY name');
SELECT lithograph('MATCH (p:Person) WHERE p.age >= 35 RETURN p.id AS id');
```

最后返回 `bob`。索引是 planner 可以选择的访问路径，不要求每个 MATCH 显式指定索引。UNIQUE / KEY 可能拥有自己的 backing index，不要为相同 Schema 随意重复创建同类索引。

| Index family | 常见用途 | DDL 形式 |
| --- | --- | --- |
| LOOKUP | Label / Relationship Type 访问 | `FOR (n) ON EACH labels(n)` 或 relationship `type(r)` |
| RANGE | 等值、范围、复合 Property | `FOR (n:Label) ON (n.a, n.b)` |
| TEXT | String predicate | `FOR (n:Label) ON (n.text)` |
| POINT | 空间条件 | `FOR (n:Label) ON (n.location)` |
| FULLTEXT | 文本相关性检索 | `ON EACH [n.title, n.content]` |
| VECTOR | 向量相似度检索 | `ON (n.embedding)`，可声明 filtering properties |
| SEMANTIC | v0.2.0：String Property → Provider-derived Vector | 通过 `db.index.semantic.createNodeIndex/createRelationshipIndex` 创建 |

全文、向量索引的完整例子见 [Search](search.md)。Relationship 也可以拥有 Property index，例如 `FOR ()-[r:ROUTE]-() ON (r.distance)`。

Semantic Index 不重载 `CREATE VECTOR INDEX`。它仍属于公共 Index namespace、Schema hash 与历史：`SHOW ALL INDEXES` 的 type 为 `SEMANTIC`，`DROP INDEX name` 使用通用 DDL；provider/config/source 变化在 Diff/Patch/Merge 中表现为同一个 `index/<name>` logical slot 更新。创建 Semantic definition 时当前 connection 必须加载对应 Embedding Provider 并通过本地 `validate`；纯历史 SHOW 与 DROP 不要求 Provider 当前存在。完整用法见 [Search：Managed Semantic](search.md#managed-semantic-text-search)。

## 查看计划，而不是猜测索引是否被使用

```sql
SELECT lithograph('EXPLAIN MATCH (p:Person) WHERE p.age = 40 RETURN p.id');
SELECT lithograph('PROFILE MATCH (p:Person) WHERE p.age = 40 RETURN p.id');
```

Property Type Constraint 可为安全的 indexed access 提供类型证明。只有索引而没有足够类型信息时，planner 可能保留 scan/filter 来维护 Cypher 的跨类型错误和比较语义；这不代表索引失效。

`PROFILE` 会执行查询。查看其 `summary.metrics` 和 `summary.profile.operators`，不要用带写入的 PROFILE 当只读诊断。

## 验证定义随历史变化

```sql
SELECT lithograph('CALL lithograph.tag.create(''with-age-index'', ''branch/main'')');
SELECT lithograph('DROP INDEX person_age');
SELECT lithograph('SHOW INDEXES YIELD name RETURN name ORDER BY name');
SELECT lithograph(
  'SHOW INDEXES YIELD name RETURN name ORDER BY name',
  '{}', '{"at":"tag/with-age-index"}'
);
```

当前 Schema 不再有 `person_age`，历史 Schema 仍然有。索引定义属于历史真源，物理索引内容是可重建数据；历史查询缺少缓存时走正确的 fallback，而不是套用最新 Schema。

## 使用 Graph Type

以下定义使用新的 Label，不改变上面的 Person 教程数据：

```sql
SELECT lithograph(
  'ALTER CURRENT GRAPH TYPE ADD {
    (:Account => {name :: STRING}),
    (:Account)-[:FOLLOWS => {since :: INTEGER}]->(:Account)
  }'
);
SELECT lithograph('SHOW CURRENT GRAPH TYPE');
```

Graph Type 描述它声明覆盖的元素，保持 open graph type 语义；不是把整个数据库变成封闭的应用 ontology。`SET` 替换 Graph Type、`ADD` 增加声明、`DROP` 删除指定声明，执行前先检查已有定义与影响域。关系端点和 identifying declaration 属于 Schema 本身，不能随意省略后声称等价。

使用 `SHOW CURRENT GRAPH TYPE` 保存返回 specification，并配合版本 Branch 试验迁移；业务迁移成功后再合并。Schema 指令不能与非空 Graph View 混用，不会形成 view-local 唯一性域。

显式索引重建方法见 [维护](operations.md)，语义依据 [Schema、Constraint 与 Index Model](../design/schema-and-indexes.md)、[Full-text](../design/full-text.md)、[Vector](../design/vector.md)。

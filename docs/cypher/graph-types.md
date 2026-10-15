# Graph Type

适用 Lithograph v0.3.1、`CY25-2026.08`。[Cypher 手册](index.md) · [兼容范围](../reference/cypher-compatibility.md) · [Schema 操作指南](../guide/schema-and-indexes.md)

Graph Type 声明节点和关系的类型、属性类型、必填字段、隐含 Label、端点要求及键。它是 open Schema：只约束声明覆盖的元素，不禁止其它 Label、Relationship Type 或未声明的属性。空库的 Graph Type 为 `{}`。

<a id="syntax"></a>

## 命令与声明形式

```text
ALTER CURRENT GRAPH TYPE { SET | ADD | ALTER | DROP } {
  <element-or-constraint> [, ...]
}
SHOW CURRENT GRAPH TYPE [AS GRAPH]

<node-element> :=
  ([alias]:IdentifyingLabel => [:ImpliedLabel [& OtherLabel ...]]
    [{property :: <property-type> [NOT NULL] [IS [NODE] {KEY | UNIQUE}], ...}])
  [REQUIRE (alias.property [, alias.property ...]) IS [NODE] {KEY | UNIQUE} ...]

<relationship-element> :=
  (<endpoint>)-[[alias]:TYPE => [{property :: <property-type>, ...}]]->(<endpoint>)
  [REQUIRE (alias.property [, alias.property ...]) IS [RELATIONSHIP] {KEY | UNIQUE} ...]

<endpoint> := <empty> | :Label | :IdentifyingLabel =>
<independent-constraint> :=
  CONSTRAINT [name] FOR <node-or-relationship-pattern> REQUIRE <requirement>
```

方括号在以上说明中表示可选部分；关系本身仍使用一对 `[...]`。`:Label` 可写为 `IS Label`，`=>` 可写为 `IMPLIES`，`:: type` 可写为 `IS :: type` 或 `IS TYPED type`。延伸当前 Graph Type 的实际命令是 `ADD`。

节点的 identifying Label 位于箭头左侧；右侧 Label 是所有该类节点必须同时具有的 Label。关系由 Relationship Type 标识，方向约束从左端点到右端点。`()` 不限制端点 Label，`(:Label)` 要求端点具有该 Label，`(:Label =>)` 还引用已声明的 Node Element Type；后者会阻止直接删除被引用的定义。

`alias` 只供该定义的 `REQUIRE` 使用，不绑定查询变量。REQUIRE 中的变量必须与声明别名一致；元组表达复合键，检查的是全部字段组成的值组合。单字段的 `IS KEY` 同时要求非空和唯一；`IS UNIQUE` 允许缺失字段。

非 DROP 的 Node Element Type 至少声明一个隐含 Label 或属性；Relationship Element Type 至少约束一个端点或属性。只写 `(:Person =>)` 是 DROP 的识别形式，不能作为空 Node Element Type 创建。

<a id="properties"></a>

## 属性类型、存在与唯一性

Property type 采用[类型参考](types.md)中的持久属性类型：Boolean、Integer、Float、String、Temporal、Duration、Point、UUID、Vector、允许的 List 和 Union。例如 `LIST<INTEGER NOT NULL>` 约束列表元素，`VECTOR<FLOAT32>(3)` 约束 coordinate type 和维度；List 的内部类型必须符合持久属性规则，不能以任意嵌套 Map/List 绕过限制。Graph Type 可用 `ANY NOT NULL` 表示任意已存在的合法持久属性值，裸 `ANY` 不作为 nullable property declaration；`ANY<STRING | INTEGER>` 是显式列出成员的 closed dynamic union。

属性默认可缺失。`name :: STRING` 表示有值时必须是 String；`name :: STRING NOT NULL` 进一步要求每个覆盖节点都有该属性。`null` 表示没有属性，不能满足 NOT NULL 或 KEY。Graph Type 声明不自动填写默认值，也不自动为节点添加隐含 Label。

以下例子先建立 Schema，再写入满足定义的数据：

<!-- example: schema-graph-set -->
```cypher
ALTER CURRENT GRAPH TYPE SET {
  (:Account => :Entity {id :: INTEGER IS KEY, name :: STRING NOT NULL}),
  (:Account =>)-[:FOLLOWS => {since :: INTEGER}]->(:Account =>)
}
```

结果没有列和行；成功 summary 的 `queryType` 为 `schema`。

<!-- example: schema-graph-create -->
```cypher
CREATE (a:Account:Entity {id:1, name:'Ada'}),
       (b:Account:Entity {id:2, name:'Lin'}),
       (a)-[:FOLLOWS {since:2026}]->(b)
RETURN a.name AS source, b.name AS target
```

在前例之后返回 `['Ada', 'Lin']`。省略 `Entity`、缺少 `name` 或重复 `id` 都会使整个写入失败。

<!-- example: schema-graph-invalid -->
```cypher
CREATE (:Account {id:3, name:'No implied label'}) FINISH
```

在 Graph Type 建立后返回 `CONSTRAINT_ERROR`。失败不会给图留下这个节点。

<a id="changes"></a>

## SET、ADD、ALTER 与 DROP

| 操作 | 语义 |
| --- | --- |
| `SET {...}` | 替换整个 Graph Type，包括 element declarations 和独立 constraints；`SET {}` 清空它们。已有独立 Index definitions 保留，约束拥有的 backing index 随约束调整。 |
| `ADD {...}` | 增加此前不存在的 element declarations 或独立 constraints；同名或等价冲突失败。 |
| `ALTER {...}` | 按 identifying Label/Relationship Type 替换已存在元素的属性、隐含 Label 和端点规则。它是完整替换，不是仅追加列出的字段。不能在 ALTER 声明中增加或修改 KEY/UNIQUE，也不能用它修改独立 constraint。已有 KEY/UNIQUE 保留。 |
| `DROP {...}` | 按 identifying element、完整定义或 constraint name 删除。若提供完整定义，它必须与当前定义精确匹配。 |

SET/ADD 可在同一个 `{...}` 中声明多个节点、关系和独立 constraints。identifying endpoint 必须能在最终 Graph Type 中解析；Node Element Type 的 identifying Label 不能同时被其它 Node Element Type 用作 implied Label。已有 Node Element Type 的 type/existence 规则应通过 ALTER 修改，不能再对同一 identifying Label 叠加独立 type/existence constraints。

<!-- example: schema-graph-add -->
```cypher
ALTER CURRENT GRAPH TYPE ADD {
  (:Project => {code :: STRING}),
  CONSTRAINT project_code FOR (p:Project) REQUIRE p.code IS UNIQUE
}
```

<!-- example: schema-graph-alter -->
```cypher
ALTER CURRENT GRAPH TYPE ALTER {
  (:Project => {code :: STRING, active :: BOOLEAN})
}
```

后例在 ADD 后执行；新定义仍须写出 `code`，`project_code` 的 UNIQUE 保留。

<!-- example: schema-graph-drop -->
```cypher
ALTER CURRENT GRAPH TYPE DROP { (:Project =>) }
```

DROP 不删除已有图元素。元素的 type/existence/implied-Label/endpoint 规则随定义移除；KEY/UNIQUE 保留并成为独立 constraints，可用 `SHOW CONSTRAINTS` 查出名称，再使用 `DROP CONSTRAINT name` 或 `ALTER CURRENT GRAPH TYPE DROP { CONSTRAINT name }` 删除。

先删除引用某 Node Element Type 的 identifying relationship declaration，才能删除该 Node Element Type。Graph Type 派生的 type/existence constraint 不能单独 DROP；应修改或删除拥有它的 element declaration。

<a id="show"></a>

## SHOW 的结果

<!-- example: schema-graph-show-empty -->
```cypher
SHOW CURRENT GRAPH TYPE
```

空库返回列 `specification`、一行字符串 `'{}'`。非空时返回可供 `ALTER CURRENT GRAPH TYPE SET` 再用的规范 specification；它包含 standalone constraints，保留 endpoint 的 identifying 身份。标识符会被规范转义，文本不必与输入逐字符相同。

`SHOW CURRENT GRAPH TYPE AS GRAPH` 返回一行 `nodes, relationships`，均为 List。它们是描述 Schema 的虚拟图值：Node Labels 包含 `NodeElementType`、`NodeLabel`、`AnyLabel`，Relationship Types 包含 `IMPLIES` 和 `RELATIONSHIP_ELEMENT_TYPE`。这些虚拟元素的负数 element ID 不能用于修改真实图，也不是业务节点 ID。可通过 YIELD/RETURN 读取属性和结构：

<!-- example: schema-graph-show-virtual -->
```cypher
SHOW CURRENT GRAPH TYPE AS GRAPH
YIELD nodes, relationships
RETURN size(nodes) AS nodes, size(relationships) AS relationships
```

空库返回 `[0, 0]`。

<a id="atomicity"></a>

## 版本、事务与错误

Graph Type 与 graph/constraints/indexes 一起版本化。普通成功 DDL 建立 Schema Commit；SQL explicit transaction 内先 staged，最终与其余写入共同提交一次。新增/变更规则先验证当前数据，违反时返回 `CONSTRAINT_ERROR`，图、旧 Schema 和 Branch head 保持原状。读取 `options.at` 的历史 Snapshot 能看到当时的 Graph Type。

DDL 不能在历史只读 Snapshot 执行，也不能与非空 `options.graphView` 混用；Schema 是整个图的定义，唯一性域不会缩成可见子图。`readOnlyNoExternalIO:true` 拒绝 Schema mutation。语法错误为 `PARSE_ERROR`；未知类型或属性类型不合法可能为 `TYPE_ERROR`；定义冲突、缺失引用、错误的 ALTER/DROP 为 `SCHEMA_ERROR`。完整错误 envelope 见[错误参考](../reference/errors.md)。

相关：[独立约束](constraints.md) · [Index](indexes.md) · [事务接口](../reference/transactions.md)。

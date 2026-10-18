# Constraint

适用 Lithograph v0.3.3、`CY25-2026.08`。[Cypher 手册](index.md) · [兼容范围](../reference/cypher-compatibility.md) · [Schema 操作指南](../guide/schema-and-indexes.md)

Constraint 在写入或 Schema 变更时验证图数据。它可以单独创建，也可以由 [Graph Type](graph-types.md) 声明产生；规则和定义随 Commit 版本化。

<a id="create"></a>

## 创建形式

```text
CREATE CONSTRAINT [name] [IF NOT EXISTS]
FOR (n:Label) REQUIRE <requirement>

CREATE CONSTRAINT [name] [IF NOT EXISTS]
FOR ()-[r:TYPE]-() REQUIRE <requirement>

<requirement> :=
    variable.property IS UNIQUE
  | (variable.property [, variable.property ...]) IS UNIQUE
  | variable.property IS {NODE | RELATIONSHIP} KEY
  | (variable.property [, variable.property ...]) IS {NODE | RELATIONSHIP} KEY
  | variable.property IS NOT NULL
  | variable.property IS :: <property-type>

DROP CONSTRAINT name [IF EXISTS]
```

`name` 是 Schema 名称；省略时分配自动名称，后续使用 SHOW 获取。名称是标识符，特殊字符用反引号；名称位置不是 `$parameter`。`FOR` 中的变量只用于 REQUIRE，其后所有属性引用必须使用同一个变量。关系约束不以方向区分唯一性域。

KEY/UNIQUE 可写可选实体前缀 `NODE`、`REL`、`RELATIONSHIP`，前缀必须与目标一致；省略 KEY 的前缀时仍根据目标识别。Property Type 的等价写法为 `p IS :: type`、`p :: type`、`p IS TYPED type`。推荐使用上面的显式形式。

| 规则 | 约束内容 | 缺失属性与 null |
| --- | --- | --- |
| UNIQUE | 同一 Label 或 Relationship Type 内，所声明字段组合不能重复 | 允许缺失；不作为相同的完整组合冲突 |
| KEY | 组合唯一，且每个组成字段都存在 | 缺失或 null 失败 |
| NOT NULL | 单个属性必须存在 | 缺失或 null 失败 |
| Property Type | 已存在属性必须符合所声明类型 | 可缺失；必填需另建 NOT NULL 或 KEY 约束 |

复合 KEY/UNIQUE 检查完整 tuple，不分别要求每个字段单独唯一。单独的 NOT NULL 和 Property Type 只接受一个 Property；错误的 tuple 返回 `SCHEMA_ERROR`。独立 Property Type Constraint 不允许在最外层类型后追加 `NOT NULL`；例如 `IS :: INTEGER NOT NULL` 返回 `SCHEMA_ERROR`，应另建存在性约束。List 的元素非空声明不受此项限制。属性类型范围、Union、List 和 Vector 维度见[类型](types.md)。

<a id="examples"></a>

## 业务键、类型与失败

<!-- example: schema-constraint-key -->
```cypher
CREATE CONSTRAINT account_key
FOR (a:Account) REQUIRE (a.tenant, a.id) IS NODE KEY
```

<!-- example: schema-constraint-type -->
```cypher
CREATE CONSTRAINT account_age
FOR (a:Account) REQUIRE a.age IS :: INTEGER
```

<!-- example: schema-constraint-data -->
```cypher
CREATE (:Account {tenant:'east', id:1, age:30}),
       (:Account {tenant:'west', id:1})
FINISH
```

在两个约束之后执行，成功且没有结果行。两个节点的 id 相同而 tenant 不同，满足复合键；第二个节点没有 age，仍满足 nullable type constraint。

<!-- example: schema-constraint-duplicate -->
```cypher
CREATE (:Account {tenant:'east', id:1}) FINISH
```

在前述数据之后返回 `CONSTRAINT_ERROR`，本次节点不会提交。

<!-- example: schema-constraint-wrong-type -->
```cypher
CREATE (:Account {age:'30'}) FINISH
```

只建立 `account_age` 约束后执行，返回 `CONSTRAINT_ERROR`；Property Type 不进行 String 到 Integer 的隐式转换。

<!-- example: schema-constraint-existence -->
```cypher
CREATE CONSTRAINT required_email
FOR (a:Subscriber) REQUIRE a.email IS NOT NULL
```

<!-- example: schema-constraint-relationship -->
```cypher
CREATE CONSTRAINT follows_key
FOR ()-[r:FOLLOWS]-() REQUIRE r.id IS RELATIONSHIP KEY
```

创建规则也验证已有数据，而非只约束未来写入：已有重复值、错误类型或缺失必填字段都会使 DDL 失败并保留旧 Schema 和 Branch head。`SET p.x = null` 会删除属性，因此也可能触发存在性或键约束。

<a id="conflicts"></a>

## 名称、重复创建与删除

Constraint 与 Index 共用 Schema 名称空间。KEY/UNIQUE 拥有 RANGE backing index，`SHOW INDEXES` 可见，`owningConstraint` 指向 constraint name；删除约束同时删除 backing index。不能单独 DROP 这个索引。其它显式 Index 不因删除 type/existence constraint 被删除。

普通 CREATE 遇到已存在名称或等价约束返回 `SCHEMA_ERROR`。`IF NOT EXISTS` 遇到同名 constraint 或等价 constraint 时成功不重新建立定义；它不是“把旧定义改成新定义”。与 Index 名称冲突，或 KEY/UNIQUE 与已有同 Schema RANGE index 冲突，仍返回 `SCHEMA_ERROR`。应 SHOW 检查实际 definition，不能只依据 CREATE 成功推断定义已变更。

<!-- example: schema-constraint-idempotent -->
```cypher
CREATE CONSTRAINT account_age IF NOT EXISTS
FOR (a:Account) REQUIRE a.age IS :: INTEGER
```

在已建立 `account_age` 后成功，仍只有原约束。成功 Schema command 的 write intent 仍可能进入 Commit，不能用 `IF NOT EXISTS` 推断没有创建历史记录。

<!-- example: schema-constraint-drop -->
```cypher
DROP CONSTRAINT account_age
```

不存在时普通 DROP 报 `SCHEMA_ERROR`；`IF EXISTS` 成功。删除规则不删除节点、关系或属性。Graph Type 的 dependent type/existence constraints 必须通过所属 element 的 ALTER/DROP 调整；KEY/UNIQUE 可以独立管理，参见 [Graph Type 变更](graph-types.md#changes)。

<a id="show"></a>

## SHOW CONSTRAINTS

```text
SHOW [ALL | <filter>] {CONSTRAINT | CONSTRAINTS}
  [YIELD column [AS alias] [, ...] | YIELD *]
  [ORDER BY ...] [SKIP ...] [LIMIT ...]
  [WHERE predicate]
  [RETURN ...]
```

Filter 支持 UNIQUE/UNIQUENESS、KEY、EXIST/EXISTENCE、PROPERTY TYPE；可按 NODE 或 REL/RELATIONSHIP 过滤，UNIQUE/EXISTENCE 可加 PROPERTY。例：`SHOW NODE KEY CONSTRAINTS`、`SHOW REL PROPERTY TYPE CONSTRAINTS`、`SHOW PROPERTY EXISTENCE CONSTRAINTS`。singular/plural 是等价形式。

默认列为 `id, name, type, entityType, labelsOrTypes, properties, enforcedLabel, ownedIndex, propertyType`。`YIELD *` 还暴露 `classification, options, createStatement`。

| 列 | 含义 |
| --- | --- |
| `id` / `name` | 当前 SHOW 中的序号与可用于 DROP 的名称；序号不是跨版本稳定身份 |
| `type` | 如 `NODE_KEY`、`RELATIONSHIP_UNIQUENESS`、`NODE_PROPERTY_EXISTENCE`、`NODE_PROPERTY_TYPE`，以及 Graph Type 派生规则类型 |
| `entityType` / `labelsOrTypes` | `NODE`/`RELATIONSHIP` 与约束目标 |
| `properties` | 属性名 List；不依赖属性的规则可能为 null |
| `enforcedLabel` | implied Label 规则要求的 Label，其它规则为 null |
| `ownedIndex` | KEY/UNIQUE backing index name，否则 null |
| `propertyType` | Property Type 文本，其它规则为 null |
| `classification` | 独立规则或 Graph Type 规则的分类信息 |
| `options` / `createStatement` | 可用的配置与重建声明；不适用时为 null |

<!-- example: schema-constraint-show -->
```cypher
SHOW NODE KEY CONSTRAINTS
YIELD name, type, properties
RETURN name, type, properties ORDER BY name
```

在 `account_key` 建立后返回 `['account_key','NODE_KEY',['tenant','id']]`。YIELD 只能引用目录列；使用 RETURN 计算派生表达式。

<a id="contexts"></a>

## 事务、历史与错误边界

DDL 独立提交或参与当前 Lithograph explicit transaction，失败遵守整次执行/事务回滚；`options.at` 历史 Snapshot 只读。非空 Graph View 下拒绝 Schema DDL，已有约束仍在整个目标图上检查，隐藏节点也能造成 UNIQUE 冲突。`readOnlyNoExternalIO:true` 拒绝修改但允许 SHOW。

语法结构错误返回 `PARSE_ERROR`；无效类型返回 `TYPE_ERROR`；Schema 名称/目标/重复或非法删除返回 `SCHEMA_ERROR`；数据不满足规则返回 `CONSTRAINT_ERROR`。参见[错误参考](../reference/errors.md)、[事务接口](../reference/transactions.md)、[Index](indexes.md)。

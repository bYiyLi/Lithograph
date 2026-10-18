# 图元信息 Procedure

适用于 Lithograph v0.3.3 · [产品手册](../../guide/README.md) · [Procedure 总入口](../procedures.md) · [CALL / YIELD](../../cypher/procedure-calls.md)

这些 Procedure 枚举当前 execution 可见图中**实际使用**的名字。它们不枚举内部 dictionary、历史上曾出现但当前已删除的名字或尚未用于数据的 Schema 声明；结果去重。三个接口无位置参数、无 Procedure options，Mode 均为 `READ`，不写 graph / Schema，不创建 Commit。

<a id="proc-db-labels"></a>

## `db.labels()`

```text
CALL db.labels() YIELD label
```

每个可见 Node 的每个 Label 贡献一个名字；返回 `label :: STRING`，每个不同名字一行。没有带 Label 的可见 Node 时返回零行。Graph View 用 selector 决定可见 Node；某个 Node 可见后，它实际携带的 Label 都属于结果，不会只返回 selector 中写出的名字。

以下教学图也供本页另外两个例子使用；每个例子在隔离数据库中先执行该 setup：

<!-- example: proc-graph-data -->
```cypher
CREATE (a:Person:Visible {name:'Ada'}),
       (b:Person:Visible {name:'Bo'}),
       (a)-[:KNOWS {since:2020}]->(b)
RETURN count(*) AS created
```

<!-- example: proc-graph-labels -->
```cypher
CALL db.labels() YIELD label
RETURN label ORDER BY label
```

结果为 `Person`、`Visible` 两行。

<a id="proc-db-propertykeys"></a>

## `db.propertyKeys()`

```text
CALL db.propertyKeys() YIELD propertyKey
```

返回 `propertyKey :: STRING`，汇总可见 Node 和可见 Relationship 当前实际存在的 Property key。相同 key 在不同元素上出现仍只返回一行。赋值为 `null` 等价于删除 Property，因此不会单独制造一个 key。

<!-- example: proc-graph-propertykeys -->
```cypher
CALL db.propertyKeys() YIELD propertyKey
RETURN propertyKey ORDER BY propertyKey
```

结果为 `name`、`since` 两行。

<a id="proc-db-relationshiptypes"></a>

## `db.relationshipTypes()`

```text
CALL db.relationshipTypes() YIELD relationshipType
```

返回 `relationshipType :: STRING`。只有 Relationship 本身与两端 Node 都满足 Graph View 可见性时，其 Type 才参与枚举。

<!-- example: proc-graph-relationshiptypes -->
```cypher
CALL db.relationshipTypes() YIELD relationshipType
RETURN relationshipType ORDER BY relationshipType
```

结果一行 `KNOWS`。

<a id="context-errors"></a>

## Snapshot、事务与错误

普通调用读取本次 pin 的 Commit；`options.at` 可选择历史 Commit。active Lithograph explicit transaction 内读取 staged graph，包括先前 execution 已完成的修改。`options.graphView` 在枚举之前生效，不能从隐藏元素泄漏 Property key 或 Type。Merge candidate 的普通只读上下文可以调用这些接口。`readOnlyNoExternalIO:true` 允许这三个已分类的纯读取 Procedure。

返回顺序应通过 `ORDER BY` 明确指定；不要把 metadata 枚举用作数据库授权检查。位置参数数量错误、未知 Procedure 或 `YIELD` 不存在的字段返回 `SEMANTIC_ERROR`；错误不以空结果替代。SQLite 锁、资源、中断等遵循[错误参考](../errors.md)。

<!-- example: proc-graph-bad-argument -->
```cypher
CALL db.labels('Person') YIELD label RETURN label
```

该调用返回 `SEMANTIC_ERROR`。

在同一教学图上，使用 SQL options `{"graphView":{"requireAllLabels":["Hidden"]}}` 调用以下例子，返回零行；隐藏节点和关系的 key 不参与 metadata 枚举。

<!-- example: proc-graph-view-hidden -->
```cypher
CALL db.propertyKeys() YIELD propertyKey RETURN propertyKey
```

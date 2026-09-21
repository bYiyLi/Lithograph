# 快速入门：创建图并查看历史

适用当前 Phase 15 开发基线。目标：创建两个节点和一条关系，参数化查询、修改数据，并查看修改前的状态。使用安装章节确认可加载的扩展，打开新的演示数据库后执行 `.load`。不要在已有业务库直接运行本教程。

## 初始化

```sql
SELECT lithograph_init();
```

数据库现在有空图的 Root Commit 和 `main` Branch。

## 创建节点和关系

```sql
SELECT lithograph(
  'CREATE (p:Person {name: $person}),
          (c:Company {name: $company}),
          (p)-[:WORKS_AT {since: 2026}]->(c)
   RETURN p.name AS person, c.name AS company',
  '{"person":"Alice","company":"Acme"}',
  '{"message":"Create the first graph"}'
);
```

返回 envelope 的 `columns` 为 `["person","company"]`，`rows` 为 `[["Alice","Acme"]]`。`summary.counters.nodesCreated` 为 `2`，`relationshipsCreated` 为 `1`；`summary.commit` 是新 Commit Descriptor。**这次写入已经生成 Commit，不需要再调用 commit.create。**

SQL 字符串中的单引号需要写成两个单引号；JSON 参数可以减少嵌套转义。应用中还应使用 SQL 参数绑定，见 [应用集成](integration.md)。

## 查询关系

```sql
SELECT ordinal, event, data
FROM lithograph_rows(
  'MATCH (p:Person)-[r:WORKS_AT]->(c:Company)
   RETURN p.name AS person, c.name AS company, r.since AS since'
);
```

得到三个 event：`ordinal=0,event='columns'` 的 `data` 是列名数组；随后 `event='row'` 的 `data` 为 `["Alice","Acme",2026]`；最后是 `event='summary'`。读取不会创建 Commit。

## 为当前状态命名，再修改

```sql
SELECT lithograph(
  'CALL lithograph.tag.create(''before-rename'', ''branch/main'')'
);

SELECT lithograph(
  'MATCH (p:Person {name: $old}) SET p.name = $new RETURN p.name AS name',
  '{"old":"Alice","new":"Alicia"}',
  '{"message":"Rename Alice"}'
);
```

第二个结果的 `rows` 为 `[["Alicia"]]`，并返回另一个 Commit。Tag 不随这次写入前进。

## 对比当前与历史

```sql
SELECT lithograph('MATCH (p:Person) RETURN p.name AS name');

SELECT lithograph(
  'MATCH (p:Person) RETURN p.name AS name',
  '{}',
  '{"at":"tag/before-rename"}'
);

SELECT lithograph(
  'CALL lithograph.log(''branch/main'', 10)
   YIELD commit, message RETURN commit, message'
);
```

当前查询返回 `Alicia`，历史查询仍返回 `Alice`。`at` 指定的图只读。`log` 包含初始化 Root 和两次图写入的 Commit；创建 Tag 不产生 Commit。

## 从历史分出新 Branch

```sql
SELECT lithograph(
  'CALL lithograph.branch.create(''experiment'', ''tag/before-rename'')'
);

SELECT lithograph(
  'MATCH (p:Person) SET p.name = $name RETURN p.name AS name',
  '{"name":"Ally"}',
  '{"branch":"experiment"}'
);

SELECT lithograph('CALL lithograph.branch.list()');
SELECT lithograph('MATCH (p:Person) RETURN p.name AS name');
```

`experiment` 中是 `Ally`；最后不带 options 的查询仍在 `main`，返回 `Alicia`。`options.branch` 只影响本次执行，不改变 connection checkout。

## 确认结果与清理

```sql
SELECT lithograph_integrity_check();
```

应返回 `ok: true`。退出 CLI 后，演示数据保存在该文件中。删除演示文件前关闭所有连接，不要把这种清理方式用于业务库。

接下来阅读 [Graph 与 Cypher](graph-and-cypher.md)、[版本管理](versioning.md) 或运行 [Python 等价示例](examples/python_quickstart.py)。结果结构见 [SQL API](../reference/sql-api.md)。

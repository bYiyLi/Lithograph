# Graph View：选择执行可见子图

适用 v0.1.0。Graph View 是一次 execution 的 Label selector，不是另一张图、另一份数据库或授权系统。以下在独立空库中运行。

## 选择可见节点

```sql
SELECT lithograph_init();
SELECT lithograph(
  'CREATE (a:Item:TenantA {id:''a''}),
          (b:Item:TenantB {id:''b''}),
          (a)-[:LINKS_TO]->(b) FINISH'
);
SELECT lithograph(
  'MATCH (n:Item) RETURN n.id AS id ORDER BY id',
  '{}', '{"graphView":{"requireAllLabels":["TenantA"],"excludeAnyLabels":[]}}'
);
SELECT lithograph(
  'MATCH ()-[r]->() RETURN count(r) AS relationships',
  '{}', '{"graphView":{"requireAllLabels":["TenantA"]}}'
);
```

只看到 `a`，关系数为 `0`，因为两个端点必须都可见。所有 required labels 都要满足，命中任一 excluded label 就不可见。空 selector 等价于完整图。

Graph View 在 MATCH、路径、aggregation、subquery、索引和 Search 内生效；不是查询后过滤。可见 Node 的 Property 不被遮罩，Schema 也不投影成 view-local Schema。

## 写入必须保持可见

```sql
SELECT lithograph(
  'CREATE (:Item:TenantA {id:''new''}) FINISH',
  '{}', '{"graphView":{"requireAllLabels":["TenantA"]}}'
);
SELECT lithograph(
  'MATCH (n:Item) RETURN count(n) AS items',
  '{}', '{"graphView":{"requireAllLabels":["TenantA"]}}'
);
```

返回 `2`。selector 在一次 execution 内固定，但后续 clause 能看到前序写入；不是在查询起点计算一个永远不变的 ID 列表。

在要求 TenantA 的 view 下，`CREATE (n) SET n:TenantA` 会失败：CREATE clause 结束时新节点还不可见。应直接 `CREATE (n:TenantA)`。REMOVE 所需 Label、给节点加上 excluded Label，或 DETACH DELETE 必须连带删除不可见关系，都会使该写查询以 `GRAPH_VIEW_VIOLATION` 回滚。

## Schema 与安全边界

Graph Type、KEY / UNIQUE / existence 等仍针对完整 canonical 图校验。视图外相同业务键仍可能引发 UNIQUE 冲突；不能把 selector 当成独立的租户约束域。Schema / version mutation 不接受非空 graphView。

原始 API 调用方可以省略 selector。需要强制隔离的服务应控制入口与 options 构造，必要时使用不同数据库；不能让不可信客户端自由提交第三个参数后声称实现访问控制。

Selector 的 JSON 类型、互斥项和错误详见 [Execution Options](../reference/execution-options.md)。

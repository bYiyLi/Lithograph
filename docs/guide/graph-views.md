# Graph View：选择执行可见子图

[产品手册](README.md) · [Cypher 语言手册](../cypher/index.md) · [接口参考](../reference/README.md)

本文适用于 **v0.3.4**（`CY25-2026.08`，storage format `3`）。`requireAnyLabels` 从本版本开始支持；v0.3.3 及更早发行二进制不接受该参数。

Graph View 是一次 execution 的 Label selector，不是另一张图、另一份数据库或授权系统。以下在独立空库中运行。

<a id="visible-nodes"></a>

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

<a id="label-union"></a>

## 多 Label 并集（v0.3.4）

在上述独立测试库中，`requireAnyLabels` 选择 TenantA **或** TenantB；其中任何节点只返回一次，两个端点都在范围内的 `LINKS_TO` 关系也可见。它不等于把多个 Label 放进 `requireAllLabels` 的 AND 条件：

```sql
SELECT lithograph(
  'MATCH (n:Item) RETURN n.id AS id ORDER BY id',
  '{}', '{"graphView":{"requireAnyLabels":["TenantA","TenantB"]}}'
);
SELECT lithograph(
  'MATCH ()-[r:LINKS_TO]->() RETURN count(r) AS relationships',
  '{}', '{"graphView":{"requireAnyLabels":["TenantA","TenantB"]}}'
);
```

两条查询分别返回 `a`、`b` 和关系数 `1`。可以同时使用 `requireAllLabels`（与 OR 求交）及 `excludeAnyLabels`（过滤命中的节点），三个数组全空仍为完整图。Graph View 在 Full-text、Raw Vector 和 Managed Semantic 的候选排序及 Top-K 之前应用，不把全图 Top-K 结果取回后再过滤；它只选 Label 子图，不能直接表达任意 Relationship 路径或每个实例的业务领域归属。

<a id="visible-writes"></a>

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

<a id="schema-and-security"></a>

## Schema 与安全边界

Graph Type、KEY / UNIQUE / existence 等仍针对完整 canonical 图校验。视图外相同业务键仍可能引发 UNIQUE 冲突；不能把 selector 当成独立的租户约束域。Schema / version mutation 不接受非空 graphView。

原始 API 调用方可以省略 selector。需要强制隔离的服务应控制入口与 options 构造，必要时使用不同数据库；不能让不可信客户端自由提交第三个参数后声称实现访问控制。

Selector 的 JSON 类型、互斥项和错误详见 [Execution Options](../reference/execution-options.md)。

<a id="combined-options"></a>

## 与历史、安全执行和事务组合

先选择版本，再应用同一次 execution 的 selector。下面的两个 read 在刚才创建的 `main` 上返回相同的 `a`、`new`；第二条还禁止本次 Cypher write 和外部 I/O：

```sql
SELECT lithograph(
  'MATCH (n:Item) RETURN n.id AS id ORDER BY id', '{}',
  '{"at":"branch/main","graphView":{"requireAllLabels":["TenantA"]}}'
);
SELECT lithograph(
  'MATCH (n:Item) RETURN n.id AS id ORDER BY id', '{}',
  '{"readOnlyNoExternalIO":true,"graphView":{"requireAllLabels":["TenantA"]}}'
);
```

多个读取必须固定同一历史状态时，先取得 Commit Descriptor，再让每次 `at` 使用该 Descriptor；反复解析可移动的 `branch/main` 不能保证跨调用 Snapshot 一致。active explicit transaction 内 selector 仍可按 execution 指定，读取会看到此前成功 staged writes；selector 不会改变最终 Commit 的目标 Branch。

selector 的 Label 名区分大小写，不做 Unicode normalization。不存在的 required Label 使现有节点不可见，不会创建字典条目；不存在的 excluded Label 当前没有过滤效果。排障时先去掉业务过滤、核对目标版本和 selector，再判断是没有匹配数据还是 View 选择错误。View 的受限读写失败不会自动修改 selector 来重试。

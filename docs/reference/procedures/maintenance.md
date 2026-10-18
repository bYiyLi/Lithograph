# 索引维护与历史 GC Procedure

适用于 Lithograph v0.3.2 · [产品手册](../../guide/README.md) · [Procedure 总入口](../procedures.md) · [运维工作流](../../guide/operations.md)

<a id="proc-lithograph-index-rebuild"></a>

## `lithograph.index.rebuild`

```text
CALL lithograph.index.rebuild(name, version)
YIELD name, commit, indexedEntities
```

两个参数均为必需、非空 `STRING`；`name` 是 Index name，`version` 是完整[Descriptor](../versioning.md#descriptors)。没有 Procedure options。支持 RANGE、TEXT、POINT 和 Relationship LOOKUP 的 persistent Standard Index generation；Node LOOKUP、FULLTEXT、VECTOR、SEMANTIC 不支持。Managed Semantic 的 rebuild 使用[独立 Procedure](semantic.md#proc-db-index-semantic-rebuild)。

| 返回列 | 类型和含义 |
| --- | --- |
| `name` | `STRING`，已重建的 Index 名称 |
| `commit` | `STRING`，实际 anchor 的 `commit/<id>` |
| `indexedEntities` | `INTEGER`，该 Index generation 索引的元素数 |

成功返回一行，重建 persisted derived generation，不创建 Commit、不移动 Branch、不中断 canonical history。它属于 `WRITE` maintenance，`summary.queryType="version"`、`summary.commit=null`，graph/Schema mutation counters 为 0；结果列 commit 才是实际 anchor。旧 generation 缺失时普通查询仍必须有正确 fallback，显式 rebuild 是恢复性能的手段。

它可以在 SQLite autocommit 或 caller-owned SQLite transaction 中调用，后者的 derived generation 随外层事务提交/回滚；不得放入 active Lithograph explicit transaction。不能放入独立 transaction subquery、Merge candidate，不能在一条 query 中与其它 Version Procedure 混用。Graph View、options.at、options.branch、author/message 不适用；历史目标写在 version 参数中。两个 SQL execution surface 均支持，rows 需要完整消费到 summary 结束。`readOnlyNoExternalIO:true` 拒绝此 maintenance。

<!-- example: proc-maint-data -->
```cypher
CREATE (:Item {sku:'A'}), (:Item {sku:'B'}) RETURN count(*) AS created
```

<!-- example: proc-maint-index -->
```cypher
CREATE RANGE INDEX item_sku FOR (n:Item) ON (n.sku)
```

<!-- example: proc-maint-rebuild -->
```cypher
CALL lithograph.index.rebuild('item_sku', 'branch/main')
YIELD name, indexedEntities RETURN name, indexedEntities
```

执行本页两个 setup 后返回 `['item_sku',2]`。

参数类型/空值、目标 Index 缺失或不支持的 family 返回 `INVALID_ARGUMENT`；未知 Descriptor 使用相应版本错误；不兼容 transaction context 为 `TRANSACTION_BOUNDARY_REQUIRED`，历史 execution write context 为 `READ_ONLY_SNAPSHOT`。SQLite 锁、I/O、资源或中断继续保留对应类别，不发布可读的部分 generation。

<a id="proc-lithograph-gc"></a>

## `lithograph.gc()`

```text
CALL lithograph.gc()
YIELD commits, layers, schemas, checkpoints, commitData
```

无参数、无 Procedure options；Mode `WRITE`。逐项返回删除的对象数，五列全部为非负 `INTEGER`，成功始终一行：

| 列 | 删除的对象 |
| --- | --- |
| `commits` | 不可从任何保留 root 到达的 Commit |
| `layers` | 不再被保留 Commit 使用的 Layer |
| `schemas` | 不再被保留历史使用的 Schema object |
| `checkpoints` | 被回收的 derived checkpoint |
| `commitData` | 被回收 Commit 的 Data sidecar |

保留 root 是所有 Branch、Tag 和 open Merge Session pin 的历史。Commit descriptor 字符串、业务表中保存的 hash、已经关闭的 query cursor 都不能代替保留 root。Dictionary 与已提交 NodeId/RelationshipId 不回收复用。GC 不创建 graph Commit，不移动 Branch/Tag，不清理 Provider 自己的 embedding cache；删除数量也不表示 SQLite 文件已经缩小。

**GC 删除不可逆。** 执行前应使用 Tag/Branch 保留要保留的历史、完成 open Session 的管理，并取得一致性备份。需要恢复已经删除的 Commit 时必须使用备份，reset 无法复活不存在的 Commit。

以下只验证计数 shape 的例子使用隔离临时数据库；空数据库不应删除可达 Root：

<!-- example: proc-maint-gc -->
```cypher
CALL lithograph.gc()
YIELD commits, layers, schemas, checkpoints, commitData
RETURN commits = 0 AS keptHistory,
       layers >= 0 AND schemas >= 0 AND checkpoints >= 0 AND commitData >= 0 AS validCounts
```

返回 `[true,true]`。

GC 不接受 Graph View、options.at、options.branch、author/message，不得在 active Lithograph explicit transaction 或 Merge candidate 内执行。caller-owned SQLite transaction 中允许时，其 durable visibility 仍由宿主 COMMIT/ROLLBACK 决定；运维推荐独立、完整消费的一次调用。`readOnlyNoExternalIO:true` 返回 `INVALID_ARGUMENT`。

错误参数数目为 `SEMANTIC_ERROR`；不合法 execution option 为 `INVALID_ARGUMENT` 或历史写入的 `READ_ONLY_SNAPSHOT`；busy、资源、存储及 I/O 错误按[错误参考](../errors.md)传播。失败不能把“可能未执行”当作“已回收”，应先查当前状态。

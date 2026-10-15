# Procedure Reference

适用于 Lithograph v0.3.1 · [产品手册](../guide/README.md) · [接口参考](README.md) · [运行时签名清单](procedure-inventory.md)

本入口按职责连接全部公开 Procedure。每个专题给出真实输入类型、默认值、输出、关键失败和版本/事务影响。[CALL 与 YIELD](../cypher/procedure-calls.md) 解释语言调用规则；[SQL API](sql-api.md) 解释如何在 SQLite 内执行。[版本共享对象](versioning.md) 拥有 Descriptor、Commit Data、Patch 与冲突对象的字段说明。

<a id="calling-conventions"></a>

## 公共调用规则

`CALL` 名称保留 `db.` / `lithograph.` 前缀。本文签名中的方括号表示可省略的位置参数，不是 Cypher 字符；可选参数通过省略取得默认值，不能假定 null 或 `{}` 等于省略。后置 `YIELD` 引用真实输出列，投影别名使用普通 Cypher 规则。

```text
CALL db.labels() YIELD label RETURN label
CALL lithograph.log('branch/main', 100) YIELD commit, parents RETURN commit, parents
```

Procedure 在所属 clause 收到的每一行上调用，结果与该输入行组合。写操作不能放在可能重复的 UNWIND/MATCH 后再假定只执行一次；需要多次调用时明确处理重复和原子边界。返回零行、空输出与一行 null 不等价。

`READ`/`WRITE` 是执行分类，不能单靠它推断是否创建 Commit：ref/Data/Session write、GC 和 index rebuild 都可能是 WRITE，却不创建 graph Commit。`db.index.semantic.rebuild` 为 READ，但拥有明确 committed-target maintenance lifecycle，并可能调用外部 Provider。

<a id="图元信息与全文检索"></a>

## 图元信息与全文检索

| Procedure | 精确参考 | 返回列 |
| --- | --- | --- |
| `db.labels()` | [可见 Label](procedures/graph.md#proc-db-labels) | `label :: STRING` |
| `db.propertyKeys()` | [可见 Property key](procedures/graph.md#proc-db-propertykeys) | `propertyKey :: STRING` |
| `db.relationshipTypes()` | [可见 Type](procedures/graph.md#proc-db-relationshiptypes) | `relationshipType :: STRING` |
| `db.index.fulltext.queryNodes(...)` | [全文 Node 查询](procedures/fulltext.md#proc-db-index-fulltext-querynodes) | `node :: NODE, score :: FLOAT` |
| `db.index.fulltext.queryRelationships(...)` | [全文 Relationship 查询](procedures/fulltext.md#proc-db-index-fulltext-queryrelationships) | `relationship :: RELATIONSHIP, score :: FLOAT` |

[全文查询专题](procedures/fulltext.md) 同时拥有 queryString、分页、analyzer override、FTS5/tokenizer 与 score 的行为。它们不创建 Commit；Full-text 仍可能调用宿主 native tokenizer，安全只读选项会拒绝。

<a id="managed-semantic-procedures"></a>
<a id="managed-semantic"></a>

## Managed Semantic

| Procedure | 精确参考 |
| --- | --- |
| `db.index.semantic.createNodeIndex(...)` | [创建 Node Semantic Index](procedures/semantic.md#proc-db-index-semantic-createnodeindex) |
| `db.index.semantic.createRelationshipIndex(...)` | [创建 Relationship Semantic Index](procedures/semantic.md#proc-db-index-semantic-createrelationshipindex) |
| `db.index.semantic.queryNodes(...)` | [文本检索 Node](procedures/semantic.md#proc-db-index-semantic-querynodes) |
| `db.index.semantic.queryRelationships(...)` | [文本检索 Relationship](procedures/semantic.md#proc-db-index-semantic-queryrelationships) |
| `db.index.semantic.rebuild(...)` | [重建 connection-local Semantic/HNSW](procedures/semantic.md#proc-db-index-semantic-rebuild) |

完整参数、providerConfig 持久化、staged-state 可见性、Graph View、失败与 cache 边界统一见 [Semantic 专题](procedures/semantic.md)。不提供 `db.index.semantic.cache.*` Kernel Procedure；具体 Provider 的 cache 属于 [Provider 配置参考](openai-compatible-provider.md)。

<a id="branch-与-tag"></a>

## Branch 与 Tag

| Procedure | 精确参考 |
| --- | --- |
| `lithograph.branch.create` | [创建 Branch](procedures/versioning.md#proc-lithograph-branch-create) |
| `lithograph.branch.checkout` | [切换 connection active Branch](procedures/versioning.md#proc-lithograph-branch-checkout) |
| `lithograph.branch.list` | [列出 Branch](procedures/versioning.md#proc-lithograph-branch-list) |
| `lithograph.branch.delete` | [删除 Branch ref](procedures/versioning.md#proc-lithograph-branch-delete) |
| `lithograph.tag.create` | [创建 Tag](procedures/versioning.md#proc-lithograph-tag-create) |
| `lithograph.tag.list` | [列出 Tag](procedures/versioning.md#proc-lithograph-tag-list) |
| `lithograph.tag.move` | [移动 Tag](procedures/versioning.md#proc-lithograph-tag-move) |
| `lithograph.tag.delete` | [删除 Tag](procedures/versioning.md#proc-lithograph-tag-delete) |

这些操作改变 ref/connection state，不为 ref 变化创建 graph Commit。name 与 Descriptor 是不同输入类型；完整合法名称与版本解析见 [共享对象](versioning.md#descriptors)。

<a id="commit-与可修改-data"></a>

## Commit 与可修改 Data

| Procedure | 精确参考 |
| --- | --- |
| `lithograph.commit.get` | [读取 immutable metadata 与 Data](procedures/versioning.md#proc-lithograph-commit-get) |
| `lithograph.commit.create` | [显式 empty-delta Commit](procedures/versioning.md#proc-lithograph-commit-create) |
| `lithograph.commit.data.set` | [设置完整 Data](procedures/versioning.md#proc-lithograph-commit-data-set) |
| `lithograph.commit.data.clear` | [清除 Data](procedures/versioning.md#proc-lithograph-commit-data-clear) |

Data 是可修改 sidecar，`hasData=false` 与显式 JSON null 不等价；不能作为 immutable audit。精确字段见 [Commit 与 Data](versioning.md)。

<a id="history-与-structural-patch"></a>

## History 与 Structural Patch

| Procedure | 精确参考 |
| --- | --- |
| `lithograph.log` | [固定起点的 DAG 分页](procedures/versioning.md#proc-lithograph-log) |
| `lithograph.diff` | [Snapshot 差异](procedures/versioning.md#proc-lithograph-diff) |
| `lithograph.patch.apply` | [同库原子 Patch 应用](procedures/versioning.md#proc-lithograph-patch-apply) |

Patch 属于结构化逻辑变化，完整 `format/databaseId/from/to/operations`、slot 与 typed before/after 见 [Patch 对象](versioning.md)。不要把结果 JSON stringify 后再丢失 tagged value 类型。

<a id="merge-session"></a>

## Merge Session

| Procedure | 精确参考 |
| --- | --- |
| `lithograph.merge.start` | [pin inputs 并建立 Session](procedures/merge.md#proc-lithograph-merge-start) |
| `lithograph.merge.get` | [读取当前 revision/status](procedures/merge.md#proc-lithograph-merge-get) |
| `lithograph.merge.list` | [分页枚举 open Session](procedures/merge.md#proc-lithograph-merge-list) |
| `lithograph.merge.conflicts` | [分页读取 conflict inventory](procedures/merge.md#proc-lithograph-merge-conflicts) |
| `lithograph.merge.resolve` | [revision CAS 保存 resolution](procedures/merge.md#proc-lithograph-merge-resolve) |
| `lithograph.merge.finalize` | [验证 target head 并发布](procedures/merge.md#proc-lithograph-merge-finalize) |
| `lithograph.merge.abort` | [revision CAS 放弃 Session](procedures/merge.md#proc-lithograph-merge-abort) |

candidate 检查通过普通只读 query 和 `options.mergeSession` 完成。[Merge 专题](procedures/merge.md) 说明完整状态机；[Merge 指南](../guide/merge.md) 按任务组织恢复与冲突解决。

<a id="历史改写与撤销"></a>

## 历史改写与撤销

| Procedure | 精确参考 |
| --- | --- |
| `lithograph.rebase` | [first-parent sequence replay](procedures/versioning.md#proc-lithograph-rebase) |
| `lithograph.squash` | [压缩为新的 delta](procedures/versioning.md#proc-lithograph-squash) |
| `lithograph.reset` | [移动目标 Branch head](procedures/versioning.md#proc-lithograph-reset) |
| `lithograph.revert` | [反向变化形成新 Commit](procedures/versioning.md#proc-lithograph-revert) |

操作前对需要保留的旧 head 建立 Tag/Branch。旧 Commit 只有在不可达并执行 GC 后才删除；单纯保存字符串不保护它。

<a id="返回值类型与-introspection-注意事项"></a>

## 返回值类型与 introspection 注意事项

运行时签名索引见 [Inventory](procedure-inventory.md)，本手册各条目中的实际类型表用于解码判断。部分 metadata 仍把 Procedure 的 `returnDescription[*].type` 统一报为 STRING，`argumentDescription` 也不保证是完整参数清单；不应把这个 metadata 缺口解释为“真实结果都是 String”或“Procedure 无参数”。实际结果必须按 [Values](values.md) 解码，并按各接口正文处理 nullable 字段、Map、List、Node 与 Relationship。

INTEGER 包括 revision/计数和 UNIX epoch **微秒** 时间；大整数遵守 tagged wrapper。Commit Data 可以是任意 JSON 值；Patch/conflict/resolution 保留其结构，不能强制转换成 String。

<a id="索引维护与-gc"></a>

## 索引维护与 GC

| Procedure | 精确参考 |
| --- | --- |
| `lithograph.index.rebuild` | [persistent Standard Index generation](procedures/maintenance.md#proc-lithograph-index-rebuild) |
| `lithograph.gc` | [不可达历史回收](procedures/maintenance.md#proc-lithograph-gc) |

GC 是不可逆的历史删除，执行前建立保留引用与一致性备份。rebuild 只恢复 derived data；两者的上下文约束不同，见[维护专题](procedures/maintenance.md)。

<a id="context-summary"></a>

## 上下文速查

| family | Commit/状态影响 | `graphView` / `at` / active Lithograph tx |
| --- | --- | --- |
| 图元信息 | 纯读取 | 支持可见图、历史、staged state |
| Full-text query | 纯 graph read，TEMP derived build | 支持可见图、历史、staged state |
| Semantic create/query | create 为 Schema write；query 可调用 Provider | create 按 Schema write 限制；query 支持可见图、历史、staged state |
| Semantic rebuild | committed-target TEMP maintenance | 不接受 graphView/at；拒绝 active tx/candidate |
| Version Procedure（含 Merge、GC） | 依各条目改变 Commit/ref/Data/Session | 不接受 graphView/at；active tx/candidate 拒绝 |
| Standard Index rebuild | persisted derived generation | 支持 caller-owned SQLite transaction；拒绝 active tx/candidate |

`readOnlyNoExternalIO` 的完整 allowlist/拒绝行为见 [Execution Options](execution-options.md)，不能把所有 READ mode 一概当作无外部 I/O。没有实际执行成功的返回不能当作已提交；尤其 rows interface 需要读到 summary 并完成外层 SQL statement。

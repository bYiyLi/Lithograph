# Procedure Inventory — v0.1.0

来自 v0.1.0 `SHOW PROCEDURES YIELD *` 的全部 33 个公开 Procedure。
参数的可选方括号是签名说明，不是实际 Cypher 调用字符。返回列可由 YIELD / RETURN 投影。

**登记存在不等于每种 transaction context 都能执行。** 每个参数与 lifecycle 限制见 [Procedures](procedures.md)。
WRITE mode 也可能只修改 ref / sidecar，并不总是产生 Commit。

v0.1.0 的 SHOW returnDescription.type 全部标为 STRING，argumentDescription 也为空；
这些 metadata 不能用于生成参数/结果解码器。本表只采用真实列名，值类型见 [Procedure Reference](procedures.md)。
相关复现见 [DOC-V010-03](known-issues.md)。

| 签名 | Mode | 输出列 |
| --- | --- | --- |
| `db.labels() :: (label :: STRING)` | READ | `label` |
| `db.propertyKeys() :: (propertyKey :: STRING)` | READ | `propertyKey` |
| `db.relationshipTypes() :: (relationshipType :: STRING)` | READ | `relationshipType` |
| `db.index.fulltext.queryNodes(indexName :: STRING, queryString :: STRING, options = {} :: MAP) :: (node :: NODE, score :: FLOAT)` | READ | `node, score` |
| `db.index.fulltext.queryRelationships(indexName :: STRING, queryString :: STRING, options = {} :: MAP) :: (relationship :: RELATIONSHIP, score :: FLOAT)` | READ | `relationship, score` |
| `lithograph.branch.create(name :: STRING [, from :: STRING])` | WRITE | `name, commit` |
| `lithograph.branch.checkout(name :: STRING)` | WRITE | `name, commit` |
| `lithograph.branch.list()` | READ | `name, commit, active` |
| `lithograph.branch.delete(name :: STRING)` | WRITE | `name, previousCommit` |
| `lithograph.commit.get(version :: STRING)` | READ | `commit, parents, author, message, committedAt, hasData, data` |
| `lithograph.commit.create([data :: ANY])` | WRITE | `commit` |
| `lithograph.commit.data.set(version :: STRING, data :: ANY)` | WRITE | `commit, data` |
| `lithograph.commit.data.clear(version :: STRING)` | WRITE | `commit` |
| `lithograph.tag.create(name :: STRING, target :: STRING)` | WRITE | `name, commit` |
| `lithograph.tag.list()` | READ | `name, commit` |
| `lithograph.tag.move(name :: STRING, target :: STRING)` | WRITE | `name, previousCommit, commit` |
| `lithograph.tag.delete(name :: STRING)` | WRITE | `name, previousCommit` |
| `lithograph.log([version :: STRING [, limit :: INTEGER [, cursor :: STRING]]])` | READ | `commit, parents, author, message, committedAt, cursor` |
| `lithograph.diff(before :: STRING, after :: STRING)` | READ | `patch` |
| `lithograph.patch.apply(patch :: MAP)` | WRITE | `commit` |
| `lithograph.merge.start(source :: STRING [, expectedHead :: STRING])` | WRITE | `session, targetBranch, ours, theirs, revision, status, unresolved` |
| `lithograph.merge.get(session :: STRING)` | READ | `session, targetBranch, ours, theirs, revision, status, unresolved` |
| `lithograph.merge.list([limit :: INTEGER [, cursor :: STRING]])` | READ | `session, targetBranch, ours, theirs, revision, createdAt, cursor` |
| `lithograph.merge.conflicts(session :: STRING [, limit :: INTEGER [, cursor :: STRING]])` | READ | `session, revision, conflictId, slot, base, ours, theirs, resolution, cursor` |
| `lithograph.merge.resolve(session :: STRING, expectedRevision :: INTEGER, resolutions :: LIST<MAP>)` | WRITE | `session, revision, status, unresolved` |
| `lithograph.merge.finalize(session :: STRING, expectedRevision :: INTEGER)` | WRITE | `status, commit` |
| `lithograph.merge.abort(session :: STRING, expectedRevision :: INTEGER)` | WRITE | `session` |
| `lithograph.rebase(onto :: STRING [, options :: MAP])` | WRITE | `status, commit, rewritten, conflicts` |
| `lithograph.squash(since :: STRING)` | WRITE | `from, previousHead, commit` |
| `lithograph.reset(target :: STRING)` | WRITE | `from, to` |
| `lithograph.revert(commit :: STRING [, options :: MAP])` | WRITE | `commit` |
| `lithograph.index.rebuild(name :: STRING, version :: STRING) :: (name :: STRING, commit :: STRING, indexedEntities :: INTEGER)` | WRITE | `name, commit, indexedEntities` |
| `lithograph.gc()` | WRITE | `commits, layers, schemas, checkpoints, commitData` |

依据：[注册表](../../crates/lithograph-core/src/query/registry.rs)、发布制品 introspection。
`rolesExecution` 等兼容字段不表示 Lithograph 有独立账号、RBAC 或 system database。

## 当前 Managed Semantic supplemental

v0.3.0 在历史 inventory 之外保留 5 个 `db.index.semantic.*` procedure；当前真实 `SHOW PROCEDURES` 总数为 38：

| 签名 | Mode | 输出列 |
| --- | --- | --- |
| `db.index.semantic.createNodeIndex(indexName :: STRING, labels :: LIST<STRING>, sourceProperty :: STRING, options :: MAP)` | WRITE | 无 |
| `db.index.semantic.createRelationshipIndex(indexName :: STRING, relationshipTypes :: LIST<STRING>, sourceProperty :: STRING, options :: MAP)` | WRITE | 无 |
| `db.index.semantic.queryNodes(indexName :: STRING, queryString :: STRING, options :: MAP) :: (node :: NODE, score :: FLOAT)` | READ | `node, score` |
| `db.index.semantic.queryRelationships(indexName :: STRING, queryString :: STRING, options :: MAP) :: (relationship :: RELATIONSHIP, score :: FLOAT)` | READ | `relationship, score` |
| `db.index.semantic.rebuild(name :: STRING, version :: STRING) :: (name :: STRING, commit :: STRING, indexedEntities :: INTEGER, embeddedTexts :: INTEGER)` | READ | `name, commit, indexedEntities, embeddedTexts` |

这些 procedure 是 Lithograph-specific Managed Semantic surface，不属于冻结 Cypher 25 built-in procedure denominator。Provider-owned persistent cache 不增加 Lithograph procedure；其配置位于 versioned `providerConfig.cache`。transaction / external-I/O 边界见 [Procedure Reference](procedures.md#managed-semantic-procedures)。

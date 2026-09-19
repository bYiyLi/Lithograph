# Procedure Inventory — v0.1.0

来自 v0.1.0 `SHOW PROCEDURES YIELD *` 的全部 33 个公开 Procedure。
参数的可选方括号是签名说明，不是实际 Cypher 调用字符。返回列可由 YIELD / RETURN 投影。

**登记存在不等于每种 adapter 都能执行。** checkout 的发布问题见 [Known Issues](known-issues.md)，
SQL rows 只读限制、Native transaction 限制及每个参数的语义见 [Procedures](procedures.md)。
WRITE mode 也可能仅修改 ref / sidecar / cache，并不总是产生 Commit。

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

## Unreleased Phase 13 supplemental

当前 `main` 在上面的 v0.1.0/v0.1.1 历史 inventory 之外新增 8 个 `db.index.semantic.*` procedure；当前真实 `SHOW PROCEDURES` 总数为 41。它们尚未进入新的正式 Release：

| 签名 | Mode | 输出列 |
| --- | --- | --- |
| `db.index.semantic.createNodeIndex(indexName :: STRING, labels :: LIST<STRING>, sourceProperty :: STRING, options :: MAP)` | WRITE | 无 |
| `db.index.semantic.createRelationshipIndex(indexName :: STRING, relationshipTypes :: LIST<STRING>, sourceProperty :: STRING, options :: MAP)` | WRITE | 无 |
| `db.index.semantic.queryNodes(indexName :: STRING, queryString :: STRING, options :: MAP) :: (node :: NODE, score :: FLOAT)` | READ | `node, score` |
| `db.index.semantic.queryRelationships(indexName :: STRING, queryString :: STRING, options :: MAP) :: (relationship :: RELATIONSHIP, score :: FLOAT)` | READ | `relationship, score` |
| `db.index.semantic.cache.configure(options :: MAP) :: (enabled :: BOOLEAN, maxBytes :: INTEGER)` | WRITE | `enabled, maxBytes` |
| `db.index.semantic.cache.stats() :: (enabled :: BOOLEAN, maxBytes :: INTEGER, usedBytes :: INTEGER, entries :: INTEGER, spaces :: INTEGER)` | READ | `enabled, maxBytes, usedBytes, entries, spaces` |
| `db.index.semantic.cache.clear() :: (deletedEntries :: INTEGER, releasedPayloadBytes :: INTEGER)` | WRITE | `deletedEntries, releasedPayloadBytes` |
| `db.index.semantic.rebuild(name :: STRING, version :: STRING) :: (name :: STRING, commit :: STRING, indexedEntities :: INTEGER, embeddedTexts :: INTEGER, cacheHits :: INTEGER)` | WRITE | `name, commit, indexedEntities, embeddedTexts, cacheHits` |

这些 procedure 是 Lithograph-specific Managed Semantic surface，不属于冻结 Cypher 25 built-in procedure denominator。adapter / transaction / external-I/O 边界见 [Procedure Reference](procedures.md#unreleasedmanaged-semantic)。

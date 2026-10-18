# Procedure 清单

适用版本：Lithograph v0.3.3 · [产品手册](../guide/README.md) · [Procedure Reference](procedures.md)

本表由目标扩展的 `SHOW PROCEDURES YIELD *` 生成，共 38 个公开 Procedure。参数的可选方括号是签名记号；返回列可由 `YIELD` 投影。

Mode 表示登记分类。`WRITE` 操作可能只修改 ref/sidecar；是否产生 Commit、事务与历史 Snapshot 限制见每行的完整说明。登记存在不表示所有 execution context 都允许调用。

表中输出列名来自 `returnDescription`。introspection 的 argument/return 类型 metadata 不用于生成解码器：完整值类型由对应 Procedure Reference 规定；metadata 的已知差异见 [Known Issues](known-issues.md)。

<!-- generated: procedures -->
| 签名 | Mode | 输出列 | 完整说明 |
| --- | --- | --- | --- |
| `db.index.fulltext.queryNodes(indexName :: STRING, queryString :: STRING, options = {} :: MAP) :: (node :: NODE, score :: FLOAT)` | READ | `node, score` | [db.index.fulltext.queryNodes](procedures/fulltext.md#proc-db-index-fulltext-querynodes) |
| `db.index.fulltext.queryRelationships(indexName :: STRING, queryString :: STRING, options = {} :: MAP) :: (relationship :: RELATIONSHIP, score :: FLOAT)` | READ | `relationship, score` | [db.index.fulltext.queryRelationships](procedures/fulltext.md#proc-db-index-fulltext-queryrelationships) |
| `db.index.semantic.createNodeIndex(indexName :: STRING, labels :: LIST<STRING>, sourceProperty :: STRING, options :: MAP)` | WRITE | 无 | [db.index.semantic.createNodeIndex](procedures/semantic.md#proc-db-index-semantic-createnodeindex) |
| `db.index.semantic.createRelationshipIndex(indexName :: STRING, relationshipTypes :: LIST<STRING>, sourceProperty :: STRING, options :: MAP)` | WRITE | 无 | [db.index.semantic.createRelationshipIndex](procedures/semantic.md#proc-db-index-semantic-createrelationshipindex) |
| `db.index.semantic.queryNodes(indexName :: STRING, queryString :: STRING, options :: MAP) :: (node :: NODE, score :: FLOAT)` | READ | `node, score` | [db.index.semantic.queryNodes](procedures/semantic.md#proc-db-index-semantic-querynodes) |
| `db.index.semantic.queryRelationships(indexName :: STRING, queryString :: STRING, options :: MAP) :: (relationship :: RELATIONSHIP, score :: FLOAT)` | READ | `relationship, score` | [db.index.semantic.queryRelationships](procedures/semantic.md#proc-db-index-semantic-queryrelationships) |
| `db.index.semantic.rebuild(name :: STRING, version :: STRING) :: (name :: STRING, commit :: STRING, indexedEntities :: INTEGER, embeddedTexts :: INTEGER)` | READ | `name, commit, indexedEntities, embeddedTexts` | [db.index.semantic.rebuild](procedures/semantic.md#proc-db-index-semantic-rebuild) |
| `db.labels() :: (label :: STRING)` | READ | `label` | [db.labels](procedures/graph.md#proc-db-labels) |
| `db.propertyKeys() :: (propertyKey :: STRING)` | READ | `propertyKey` | [db.propertyKeys](procedures/graph.md#proc-db-propertykeys) |
| `db.relationshipTypes() :: (relationshipType :: STRING)` | READ | `relationshipType` | [db.relationshipTypes](procedures/graph.md#proc-db-relationshiptypes) |
| `lithograph.branch.checkout(name :: STRING)` | WRITE | `name, commit` | [lithograph.branch.checkout](procedures/versioning.md#proc-lithograph-branch-checkout) |
| `lithograph.branch.create(name :: STRING [, from :: STRING])` | WRITE | `name, commit` | [lithograph.branch.create](procedures/versioning.md#proc-lithograph-branch-create) |
| `lithograph.branch.delete(name :: STRING)` | WRITE | `name, previousCommit` | [lithograph.branch.delete](procedures/versioning.md#proc-lithograph-branch-delete) |
| `lithograph.branch.list()` | READ | `name, commit, active` | [lithograph.branch.list](procedures/versioning.md#proc-lithograph-branch-list) |
| `lithograph.commit.create([data :: ANY])` | WRITE | `commit` | [lithograph.commit.create](procedures/versioning.md#proc-lithograph-commit-create) |
| `lithograph.commit.data.clear(version :: STRING)` | WRITE | `commit` | [lithograph.commit.data.clear](procedures/versioning.md#proc-lithograph-commit-data-clear) |
| `lithograph.commit.data.set(version :: STRING, data :: ANY)` | WRITE | `commit, data` | [lithograph.commit.data.set](procedures/versioning.md#proc-lithograph-commit-data-set) |
| `lithograph.commit.get(version :: STRING)` | READ | `commit, parents, author, message, committedAt, hasData, data` | [lithograph.commit.get](procedures/versioning.md#proc-lithograph-commit-get) |
| `lithograph.diff(before :: STRING, after :: STRING)` | READ | `patch` | [lithograph.diff](procedures/versioning.md#proc-lithograph-diff) |
| `lithograph.gc()` | WRITE | `commits, layers, schemas, checkpoints, commitData` | [lithograph.gc](procedures/maintenance.md#proc-lithograph-gc) |
| `lithograph.index.rebuild(name :: STRING, version :: STRING) :: (name :: STRING, commit :: STRING, indexedEntities :: INTEGER)` | WRITE | `name, commit, indexedEntities` | [lithograph.index.rebuild](procedures/maintenance.md#proc-lithograph-index-rebuild) |
| `lithograph.log([version :: STRING [, limit :: INTEGER [, cursor :: STRING]]])` | READ | `commit, parents, author, message, committedAt, cursor` | [lithograph.log](procedures/versioning.md#proc-lithograph-log) |
| `lithograph.merge.abort(session :: STRING, expectedRevision :: INTEGER)` | WRITE | `session` | [lithograph.merge.abort](procedures/merge.md#proc-lithograph-merge-abort) |
| `lithograph.merge.conflicts(session :: STRING [, limit :: INTEGER [, cursor :: STRING]])` | READ | `session, revision, conflictId, slot, base, ours, theirs, resolution, cursor` | [lithograph.merge.conflicts](procedures/merge.md#proc-lithograph-merge-conflicts) |
| `lithograph.merge.finalize(session :: STRING, expectedRevision :: INTEGER)` | WRITE | `status, commit` | [lithograph.merge.finalize](procedures/merge.md#proc-lithograph-merge-finalize) |
| `lithograph.merge.get(session :: STRING)` | READ | `session, targetBranch, ours, theirs, revision, status, unresolved` | [lithograph.merge.get](procedures/merge.md#proc-lithograph-merge-get) |
| `lithograph.merge.list([limit :: INTEGER [, cursor :: STRING]])` | READ | `session, targetBranch, ours, theirs, revision, createdAt, cursor` | [lithograph.merge.list](procedures/merge.md#proc-lithograph-merge-list) |
| `lithograph.merge.resolve(session :: STRING, expectedRevision :: INTEGER, resolutions :: LIST<MAP>)` | WRITE | `session, revision, status, unresolved` | [lithograph.merge.resolve](procedures/merge.md#proc-lithograph-merge-resolve) |
| `lithograph.merge.start(source :: STRING [, expectedHead :: STRING])` | WRITE | `session, targetBranch, ours, theirs, revision, status, unresolved` | [lithograph.merge.start](procedures/merge.md#proc-lithograph-merge-start) |
| `lithograph.patch.apply(patch :: MAP)` | WRITE | `commit` | [lithograph.patch.apply](procedures/versioning.md#proc-lithograph-patch-apply) |
| `lithograph.rebase(onto :: STRING [, options :: MAP])` | WRITE | `status, commit, rewritten, conflicts` | [lithograph.rebase](procedures/versioning.md#proc-lithograph-rebase) |
| `lithograph.reset(target :: STRING)` | WRITE | `from, to` | [lithograph.reset](procedures/versioning.md#proc-lithograph-reset) |
| `lithograph.revert(commit :: STRING [, options :: MAP])` | WRITE | `commit` | [lithograph.revert](procedures/versioning.md#proc-lithograph-revert) |
| `lithograph.squash(since :: STRING)` | WRITE | `from, previousHead, commit` | [lithograph.squash](procedures/versioning.md#proc-lithograph-squash) |
| `lithograph.tag.create(name :: STRING, target :: STRING)` | WRITE | `name, commit` | [lithograph.tag.create](procedures/versioning.md#proc-lithograph-tag-create) |
| `lithograph.tag.delete(name :: STRING)` | WRITE | `name, previousCommit` | [lithograph.tag.delete](procedures/versioning.md#proc-lithograph-tag-delete) |
| `lithograph.tag.list()` | READ | `name, commit` | [lithograph.tag.list](procedures/versioning.md#proc-lithograph-tag-list) |
| `lithograph.tag.move(name :: STRING, target :: STRING)` | WRITE | `name, previousCommit, commit` | [lithograph.tag.move](procedures/versioning.md#proc-lithograph-tag-move) |
<!-- /generated: procedures -->

`rolesExecution` 等兼容字段不表示 Lithograph 提供独立账号、RBAC 或 system database。更新方式见 [示例验证说明](../guide/examples/README.md#reference-generation)。

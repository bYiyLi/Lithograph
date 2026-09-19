# 部署、备份与维护

适用 v0.1.0/v0.1.1 发布基线，并补充当前 Unreleased `main` 的 format 4 / Managed Semantic 运维。Lithograph 运行在应用的 SQLite connection 中，没有独立管理 Server。以下操作涉及已有数据时，先确认文件路径、扩展版本、目标 Branch 与保留策略；不要直接修改 `_lithograph_*` 表。

## 部署前检查

固定 v0.1.0 制品及 SHA-256；在实际应用进程内检查 SQLite 3.45.0+、FTS5、extension loading、CPU 架构与动态库依赖。测试环境能加载不代表生产容器、沙箱或另一个语言 binding 能加载。新部署应先在一次性数据库中运行 [快速入门](getting-started.md)。

新库由 provisioning 步骤调用 `lithograph_init()`。已有库在启动时读取 `lithograph_version()` 并检查 `databaseId` / `storageFormat.current`；不要每个请求都执行 init 或 integrity scan。数据库文件、目录和备份位置的访问权限由宿主配置。

连接池中的每个连接都要加载扩展。用明确的 query `branch` / `at` 上下文，避免把应用的目标状态存成难以追踪的隐式连接状态。所有连接仍然受同一文件的 SQLite writer 限制。

## 一致性备份

备份对象是**完整 SQLite repository**，不是当前 Branch 的节点导出。完整备份保留数据库身份、所有可达历史、Schema、引用、Commit Data 和持久 Merge Session。Cypher 导出当前数据可以用于应用数据交换，但不能替代版本历史备份。

数据库正在写入时，不要只复制主 `.sqlite` 文件，更不要删除正在使用的 `-wal` / `-shm` 文件。使用 SQLite Online Backup API，或者关闭所有连接后按 SQLite 的一致性要求处理完整文件状态。

Python 的 backup 示例使用已经打开的 source connection `db`，并拒绝覆盖已有目标：

```python
from pathlib import Path
import sqlite3

destination = Path("backup-2026-09-16.sqlite").resolve()
with destination.open("xb"):
    pass  # Reserve a new path; FileExistsError prevents accidental overwrite.
backup = sqlite3.connect(str(destination))
try:
    db.backup(backup)
finally:
    backup.close()
```

失败时不要将目标提升为可用备份；保留失败信息，并用新路径重新执行。完整自动化备份、重开、比较 head 和完整性检查的示例见 [version_workflow.py](examples/version_workflow.py)。

SQLite CLI 也提供 `.backup`；它是 CLI 命令而不是 Cypher。无论使用哪种入口，都应在源端仍可用时用独立 connection 读取备份，核对版本、databaseId、关键 Branch head 和业务查询，并执行 `lithograph_integrity_check()`。备份策略还需要应用自己的异地保留、加密、访问控制与恢复演练。

## 从备份恢复

先把备份放在新的隔离路径，加载相容扩展验证。确认完整性、目标 Branch 和代表性业务数据之后，停止原库所有 reader/writer，再由应用的部署流程切换到恢复文件。不要覆盖仍被连接池打开的数据库，也不要把旧文件残留的 WAL sidecar 搬到另一份恢复副本旁边。

恢复保留相同 databaseId；它是原 repository 的副本，不是新的独立 identity space。两个副本分别继续写入后，不存在远程 fetch/push 或跨文件 branch merge API；应用不得把这种复制误当成支持多主同步。

## 存储格式升级

v0.1.0/v0.1.1 新建 format 3，支持读取 formats 1–3。当前 Unreleased `main` 新建 format **4**，并通过显式 `lithograph_init()` 支持 format 3 → 4；format 4 新增的 persistent Embedding Result Cache 是 derived data，不改变既有 Commit/Layer/Schema hash 或历史语义。加载扩展与普通读取不会自动升级文件。未来格式高于当前 binary 支持范围时停止使用旧扩展，不修改内部 version marker 来强行打开。

升级步骤：建立并验证一致性备份；在备份副本上验证新扩展与迁移；安排停止写入的维护窗口；运行 init 并检查结果；执行完整性和应用回归；再恢复业务流量。迁移可能扫描较大历史并占用显著时间、空间和 writer，不将幂等初始化描述为常数时间。

format 3/4 都没有自动降级接口。回退方案是相容的旧应用/扩展加升级前备份，不是在已升级文件上运行更旧 binary。尤其 format 4 文件不能交给只支持 format 3 的 v0.1.1 binary 继续写。pre-1.0 升级前还需审查 API / profile 变化，不能只比较 ABI 大版本数字。

## 完整性检查与损坏处理

`SELECT lithograph_integrity_check();` 返回 `ok,errors,checked`。这是显式的完整检查，不是每次读写都自动重算全部历史。SQL `PRAGMA integrity_check` 检查 SQLite 自身结构，不能替代 Lithograph 的版本化 graph invariants；反过来也不要用只读业务查询成功证明整个文件健康。

检查失败时停止 mutation，在原文件的安全副本上分析，保留脱敏错误、版本和环境。不要执行 DELETE、DROP、额外 trigger 或改 format marker 来“修复”内部表。缺失可重建缓存与 canonical 历史损坏是不同问题；后者需要有效备份或明确的恢复方案。

## 显式重建标准索引

下面是独立演示库中的完整 SQL，可安全验证维护入口；已有业务库只需对已经存在的目标索引调用最后一条：

```sql
SELECT lithograph_init();
SELECT lithograph('CREATE (:Metric {value:1}), (:Metric {value:2}) FINISH');
SELECT lithograph('CREATE RANGE INDEX metric_value FOR (m:Metric) ON (m.value)');
SELECT lithograph(
  'CALL lithograph.index.rebuild(''metric_value'', ''branch/main'')
   YIELD name,commit,indexedEntities RETURN name,commit,indexedEntities'
);
```

返回 name 为 `metric_value`、indexedEntities 为 `2`，commit 为实际 anchor。该操作不创建 Commit、不移动 Branch；`summary.commit` 为 null。只支持 Standard RANGE/TEXT/POINT 与 Relationship LOOKUP，不是 FULLTEXT/VECTOR 的统一 rebuild API。

重建在同一写事务中原子发布，失败不会暴露半成品。全量扫描/构建可能长时间占用 writer，安排维护窗口并测量真实空间和耗时。普通只读查询遇到标准索引缓存缺失时可以使用正确但更慢的 fallback；不要通过手工删除内部表来触发重建。

## Unreleased：Managed Semantic cache 维护

Phase 13 的 persistent Embedding Result Cache 是 database-local derived data。它不属于某个 Branch/Commit，也不进入 Schema hash；默认容量策略为 enabled、1 GiB。公开维护入口：

```cypher
CALL db.index.semantic.cache.stats()
CALL db.index.semantic.cache.configure({enabled:true, maxBytes:1073741824})
CALL db.index.semantic.cache.clear()
CALL db.index.semantic.rebuild('document_semantic', 'branch/main')
```

`stats` 只读；`configure`、`clear`、`rebuild` 是独立 maintenance write，只能从普通 `lithograph()` 或 autocommit Native execution 调用，不能放入 `lithograph_rows()`、Native explicit transaction、transaction-owning subquery 或 Merge candidate。它们成功时不创建 Commit、不移动 Branch/Tag。

`rebuild` 会 pin 指定 immutable Snapshot，在 SQLite single-writer ownership 之外调用 Provider 计算缺失 embedding；全部成功后才进入短 publish transaction，把完整结果写入 persistent cache。Provider I/O / cancel / payload validation 失败不会留下可复用半 entry。普通 semantic query 即使发生 Provider miss，也只写 connection-local TEMP/LRU，不把任意查询文本持久化到数据库。

需要使用 Managed Semantic 的每个真实 connection 都必须加载目标 Provider；persistent cache 已 warm 也不能绕过 Provider registration/validation。Provider 未加载时，普通 graph/history/SHOW 仍可用，但实际 semantic query/rebuild 明确失败。完整用法与 secret 边界见 [Search](search.md#unreleasedmanaged-semantic-文本检索)。

## 历史保留与 GC

Branch head、Tag 和未结束的 Merge Session 都保护相应可达历史。仅在应用日志里保存一个 `commit/...` 字符串不会阻止回收。需要长期留存的状态应建立明确的保留 Tag，同时维持外部备份。

`CALL lithograph.gc()` 是**显式、不可逆的历史回收**。执行前列出 Branch/Tag/open Session，确定哪些引用应删除、哪些应保留；先完成备份，再通过普通 scalar 或 Native execute 独立调用。它不是日常查询必需步骤，也不是自动按天过期的 retention 服务。

GC 不删除仍被保留引用保护的历史；物理文件也不保证立刻缩小。SQLite VACUUM / checkpoint 的时机、锁与额外磁盘需求由宿主维护，不把它们与逻辑 GC 或创建 graph Commit 混淆。

## 性能与资源管理

先用代表性的查询、数据规模和并发方式测量，再调整模型、索引和宿主参数。至少区分首次建索引、已有持久 generation 的 reopen、少量增量变化以及完全缺缓存的查询。它们不是同一个“冷启动”状态。

读取只投影需要的字段，用 Cypher 内的 WHERE / ORDER BY / LIMIT 表达语义；大量结果选择行适配器或 Native stream，同时及时关闭 cursor。排序、聚合、路径探索、全文/向量访问仍可能需要内存与 TEMP 空间，streaming API 不等于所有 query 都恒定内存。

EXPLAIN 无执行副作用；PROFILE 实际执行，写查询也会写入。出现索引未被选择时先检查类型约束与查询计划，而不是强制删建索引。迁移、GC 和 rebuild 与线上写请求共享资源，避免把人工审查或网络工作放进 Native explicit transaction。

WAL、synchronous、busy timeout、线程模式和 TEMP 位置由宿主 SQLite 决定，Lithograph 不替你静默修改。没有适合所有产品的默认吞吐/延迟承诺；最终优化验收见 [Phase 11](../development/phases/11-performance-optimization.md)，早期基线及其限制见 [Performance Evidence](../research/phase11-performance-evidence.md)。旧基线不是 v0.1.0 最终延迟数据，机器、规模和缓存状态必须一起阅读。

## 安全边界

扩展是本机代码，仅加载可信、校验过的文件，加载后关闭动态加载入口。SQL / Cypher 参数都使用绑定；query parser 不是应用认证、权限或沙箱。Graph View 不能阻止有原始 API 权限的调用方省略 selector。

LOAD CSV 与 remote Embedding Provider 都可能访问宿主文件/网络。不要给不可信请求无限制提交 SQL、Cypher、文件 URI、Provider endpoint/config 或网络 URL 的能力。应用负责请求取消、并发上限、敏感日志脱敏，以及数据与备份的加密策略。显式写入 Semantic `providerConfig.api_key` / secret header 会进入 versioned Schema/history/backup；需要避免持久化 credential 时使用 Provider 支持的 indirection（例如 `api_key_env`）。

备份机制依据 [SQLite Online Backup API](https://www.sqlite.org/backup.html) 与 [Python sqlite3](https://docs.python.org/3/library/sqlite3.html)。产品行为依据 [SQL API](../reference/sql-api.md)、[维护 Procedure](../reference/procedures.md) 与 [技术设计](../design.md)。

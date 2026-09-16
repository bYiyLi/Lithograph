# Phase 11 性能证据与采用边界

核对日期：2026-09-15。代码观察基线：`cccb760f5350806117bf290abf51f78fda4edff0`。本文保存观察、来源与证据限制，不定义产品合同；目标方案以 [技术设计](../design.md) 为准，实施/验收以 [Phase 11](../development/phases/11-performance-optimization.md) 为准。

## 1. 现有 benchmark 的范围

本轮重新读取本地 generated artifact `target/phase10-scale-release/baseline.json`。它来自 Phase 10 收尾期间的一次 release-mode run，使用已有 fixture；report 没有保存 Git tree digest，也没有 cold/warm 重复样本。不能把它当作 `cccb760` 的严谨重复测试或 P95。

| 字段 | 报告值/机器核对 |
| --- | --- |
| Profile | `release-10m-100m` |
| 图规模 | 10,000,000 Nodes；100,000,000 Relationships |
| 文件体积（report） | 28,022,696,688 bytes，约26.1 GiB |
| 机器 | Apple M2，8 CPU，16 GiB RAM，macOS arm64 |
| Rust / SQLite | release build；rustc 1.98.1；SQLite 3.51.0 |
| Search 样本 | 1000 documents，不是10M向量 |
| Fixture Commit | `79976d4f4a42535c14f27d70b430c7a449a2ca6e21376a389214cc4cb0429956` |

报告中的单次时间：

| Workload | 毫秒 | 结果范围 |
| --- | ---: | --- |
| Label scan | 392593 | 10M行，`MATCH (:ScaleNode) RETURN 1` |
| 第一次 indexed equality | 412396 | 1行；包含TEMP cache初始化路径，未拆出独立build时间 |
| indexed range | 1136 | 1000行；位于同一index首次查询之后 |
| low-degree one-hop | 130852 | 1行 |
| high-degree one-hop | 195346 | 1M行 |
| variable path | 20 | 4行 |
| full-text | 121 | 命中1000样本文档 |
| vector top-10 | 58 | 1000文档集合内的10结果 |
| historical read / Tag / Commit Data | 各0 | 整数毫秒被截断，不是零成本或普遍亚毫秒保证 |
| branch Diff | 49 | 300个operation |
| cursor History | 241 | 4页×64 Commit |
| write batch + one Commit | 66 | 1000 Node |

`reuse_existing_fixture` 与 `CREATE ... IF NOT EXISTS` 的 setup 耗时不是重新建立10M索引的时间。`lithograph-phase10-scale.rs::measure_streamed_rows_with_options` 在 prepare 之后计时，`measure_rows` 的 execute 则包含 prepare；不同项不能直接当作相同口径 latency。报告没有保存峰值 RSS、OS cache 清理、writer hold 或持续并发吞吐。新基线必须补齐这些信息。

另外，上一轮保留的 scale-extra 输出在 `target/phase10-scale/scale.sqlite` 上测试10000 conflicts、256/page：40页读取4509ms，40轮resolution 105533ms，candidate inspection 296ms，finalize整次调用245ms。它不是同一路径的10M/100M fixture，不把两者合成一个实验。该 `finalize_millis` **不是**独立writer持锁计时。Native scale smoke在大fixture证明两次staged write最终只增加一个Commit，但没有独立TPS/latency报告。

## 2. 代码与隔离诊断

### E1：邻接 SQL 的键序与索引不一致

`storage/schema.rs` 的 outgoing checkpoint index 是 `(commit_id, source_id, type_id, target_id, relationship_id)`，但 `storage/snapshot_scan.rs::base_adjacency_page` 按 `relationship_id > after ORDER BY relationship_id` 分页。

同日上一轮只读诊断，在大fixture的node2上执行 `EXPLAIN QUERY PLAN`，原始SQL选择：

```text
SEARCH _lithograph_cp_relationships USING PRIMARY KEY
  (commit_id=? AND relationship_id>?)
```

该有界诊断实际查询30秒超时。将同一单页诊断改为既有 `_lithograph_cp_rel_out`，按 `target_id, relationship_id` 排序后，覆盖索引返回1行约1.214ms。它说明存在可寻址物理路径，**不证明完整 Cypher one-hop 已达到1.214ms**；prototype未验证完整overlay、多页、Graph View或Cypher重复语义，本轮也未重跑这一诊断。目标实现必须迁移内部cursor并补齐这些证明，不能只复制一个hint。

### E2：重复 Snapshot resolution

`query/stream.rs::next_batch_with_interrupt` 的普通read路径每批调用 `prepared_snapshot`；`storage/snapshot.rs::resolve_with_checkpoint_skip` 会重新解析lineage/load Layer。输出batch256时，10M结果会有约39063次batch调用。`query/plan.rs` 的statistics collection还会单独resolve。调用关系是代码证据；各部分究竟占392.593秒中的多少，需要新instrumentation，而不是从次数直接推断全部时间。

上一轮原始 SQLite/Python covering Label index 流出10M NodeId约3.737秒；该实验不是同口径A/B，OS缓存未隔离，并且没有完整Cypher/adapter工作。它只作为拆分storage和executor成本的线索，不作为性能承诺或正式speedup。

### E3：Standard Index TEMP lifecycle

`query/schema/index/cache.rs::ensure_index_cache` 使用 `(snapshot.cache_identity(), index.name)` 查TEMP meta，miss后构建整个domain；connection关闭会失去该cache。`drop_cache_indexes` 会删除共享TEMP表的secondary indexes。Node property按页读取，但Relationship property cache仍通过全Relationship扫描再筛type。

因此“reopen复用”“小delta不全量重建”“不要构建一个index时破坏其它index”都有当前代码依据。412.396秒与冷materialization路径一致，但现有计时不能准确区分编码、插入、secondary index建立和其它开销。

### E4：已有能力与尚未证实的热点

- `query/plan.rs::optimize_node_scan` **已经**依据统计选择最小label cardinality；不能仅看 `lower_node` 初始化的第一个Label，就声称整个Planner没有选择能力。复杂path等其它路径是否需要对齐，只由新的plan/counter实验决定。
- `query/graph.rs::visible_relationship`、`stream.rs::match_relationship` 存在重复endpoint检查；`query/expression.rs::BindingRow` 使用map/set且匹配路径有clone。它们是候选热点，不是已获得独立火焰图证明的主因；不能以此直接授权整套executor重写。
- Core `Snapshot` 借用Connection，而VT cursor跨多次callback；简单将它塞进无lifetime的QueryCursor会产生ownership问题。复用设计必须包含read guard、cleanup、staged revision和GC并发，而非只增加一个缓存字段。
- format2的exact reserved-schema inventory拒绝未知table/index；将cache移入 `main` 必须明确format迁移，不能只把SQL中的 `temp` 改成 `main`。

## 3. SQLite 官方依据

下表来源于本轮读取的SQLite官方文档；采用的是公开能力，不增加私有API或运行时特殊编译要求。

| 来源 | 核对结论与采用限制 |
| --- | --- |
| [Row Values](https://www.sqlite.org/rowvalue.html)，§3.1 | 复合值比较可用于与索引顺序一致的keyset分页；不使用大OFFSET或不一致的排序键。 |
| [INDEXED BY](https://www.sqlite.org/lang_indexedby.html) | 它是索引使用的硬要求，不是hint；先修访问与键序，必要时才锁定已验证的内部access path。 |
| [Transaction](https://www.sqlite.org/lang_transaction.html) | 不同connection可以并发read，但只有一个writer；read view与transaction/statement生命周期有关。不能把反复独立SELECT等同于持续pin。 |
| [Isolation](https://www.sqlite.org/isolation.html) | WAL read snapshot与writer分离；同connection的修改不是另一个隔离reader。查询期间的same-connection mutation不能靠Commit ID缓存解决。 |
| [Statement counters](https://www.sqlite.org/c3ref/c_stmtstatus_counter.html) | FULLSCAN_STEP、SORT和VM_STEP观测不同成本。VM_STEP超出2147483647后结果未定义；须分段聚合，不能依赖溢出后的数值。 |
| [WAL](https://www.sqlite.org/wal.html) | 长reader可能限制WAL checkpoint推进；Snapshot reuse必须同时验证close/cancel后资源释放和WAL增长。 |
| [sqlite3_snapshot_open](https://www.sqlite.org/c3ref/snapshot_open.html) | 依赖 `SQLITE_ENABLE_SNAPSHOT`，不作为所有stock SQLite宿主必须提供的接口。 |

## 4. 对计划的约束

E1–E3足以驱动邻接keyset、query-owned state、持久derived index的设计；E4只允许按新profiling选择局部优化。原始SQL prototype、旧单次计时、Phase10逻辑plan名称都不替代Phase11端到端与物理工作量验收。

本文未引入新的benchmark结果，也未执行Phase11实现。原始大fixture、完整generated report和临时trace不进入Git；后续正式性能结果必须保存足以重现的fixture manifest、测试命令、Git tree identity与统计摘要，且标明哪些大型原始artifact在本地或CI留存。

## 5. 2026-09-16 Phase 11 当前工作树实测证据

本节追加Phase 11实现后的本地证据，不改写前文的优化前观察。测试机仍为Apple M2、8 CPU、16 GiB RAM；当前Git `HEAD` 为 `da9445f2552d6b98de7f1275f69521e9099b7653`，Phase 11改动仍在dirty worktree，尚未commit/push。大型数据库、JSON report、resource trace继续只保存在ignored `target/`，不进入Git。由于当前改动没有远端ref，本节**不**把Phase 10旧的hosted Release Matrix结果冒充为Phase 11当前代码的六平台结果。

### 5.1 10M/100M ready-read与Standard Index

当前format3 fixture固定为10,000,000 Nodes / 100,000,000 Relationships。最终Range报告来自 `target/phase11-range-direct-final/single.json`，1000-owner delta来自 `target/phase11-delta-1000-report/after.json`：

| Workload | 样本 | 结果 | P95 | 关键物理证据 |
| --- | ---: | ---: | ---: | --- |
| persistent Range equality | 20 | 1 row | 2.209 ms | `standardIndexBuilds=0` |
| persistent bounded Range | 20 | 1000 rows | 38.713 ms | `standardIndexBuilds=0`；直接走persistent range family |
| 1000-owner delta old equality | 20 | 0 row | 3.204 ms | ancestor generation + changed-owner overlay；无full rebuild |
| 1000-owner delta new Range | 20 | 1000 rows | 39.582 ms | ancestor generation + changed-owner/local overlay；无full rebuild |

这一结果关闭了两类本轮实测热点：普通read不再为验证derived payload执行10M `count(*)`；bounded numeric Range也不再通过TEMP UNION view把整个generation扫完。Canonical property recheck仍存在，Index payload没有升级为correctness source。

SQL rows / Native adapter的最终独立统计保存在 `target/phase11-adapter-final2/report.json`：5个warm connection、每connection 20次，另有100个independent reopen sample。rows/Native adjacency warm P95分别1.767/1.588 ms，indexed equality warm P95分别1.751/1.676 ms；reopen query P95：rows adjacency 1.643 ms、rows indexed equality 12.482 ms、Native adjacency 1.660 ms、Native indexed equality 12.429 ms。open/load另行计时，P95均低于0.4 ms；Native explicit transaction无failure，P95约11.582 ms。

Streaming最终报告 `target/phase11-stream-final/report.json` 各3轮均为0 failure：SQL rows / Native的10M Label scan median约6.72/4.75 s；1M Hub scan median约11.17/10.85 s。它们与短查询P95分开统计，不把大结果集吞吐误写成单row latency。

### 5.2 Search corpus、cold/warm与recall

最终Search报告 `target/phase11-search/report.json` 使用1,000,000全文/128维Vector文档，以及独立100,000×1536 Vector子集；coordinate type为FLOAT32、cosine、top-k=10，Graph View可见性约50%，runner保存deterministic seed、HNSW参数、文本长度与cold/warm run mode。

- 128维：new-connection cold build约279.7 s；same-connection warm samples约14.9–75.3 ms；historical约35.1 ms；Graph View过滤约14.9 ms。
- 1536维：new-connection cold build约142.7 s；same-connection warm约23.2–35.4 ms。
- 所有Vector样本tie-aware recall@10均为1.0，warm query不重建cache，并只point-load实际访问的HNSW entries（样本约153–683个entry），不再每次反序列化整个百万entry cache。
- 1M Full-text cold build约111.8 s；warm/historical samples约0.63–5.14 ms，结果与预期visible/historical语义一致。

FLOAT32 cosine corpus在midpoint存在大量exact cutoff同分项。原先按deterministic ID交集计算recall会把score完全等价的ANN top-k误报为0；Design §17.2已冻结tie-aware规则：高于cutoff的strict结果仍必须命中，等于cutoff的等分candidate可互换，同时继续检查返回score、去重和稳定score/identity顺序。这是benchmark oracle修正，不是降低ANN质量门槛。

### 5.3 Merge与30分钟mixed workload

10K conflict最终报告 `target/phase11-merge-final3/report.json`：10,000 conflicts，256/page，共40页/40轮resolution；merge start约85 ms、conflict scan约3.207 s、resolution约79.137 s、candidate inspection约171 ms。finalize整次约210 ms，其中prepare约195.429 ms、writer wait约42 µs、writer hold约2.050 ms；GC约39 ms且root preserved。

1/4/8 readers + 1 writer三档都各运行1800秒，并使用独立COW database：

| Readers | Reader ops | Reader P95 | Writer writes | Writer P95 | Writer wait P95 | failures/BUSY | peak WAL | final WAL |
| ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: |
| 1 | 59,413 | 103.425 ms | 3,282 | 279.459 ms | 304 µs | 0 / 0 | 1,293,572,912 B | 0 |
| 4 | 104,264 | 171.473 ms | 2,096 | 488.288 ms | 654 µs | 0 / 0 | 1,382,342,432 B | 0 |
| 8 | 78,853 | 477.272 ms | 1,479 | 763.206 ms | 999 µs | 0 / 0 | 1,341,546,192 B | 0 |

每档都包含Tag move、GC与persistent Index rebuild；最终WAL回收为0，未观察reader snapshot漂移、writer BUSY或failure。

### 5.4 Repository gate与未完成外部门禁

当前worktree最终 `cargo make quality` exit 0；`cargo make quality-coverage` exit 0，TOTAL coverage为regions 83.26%、functions 84.41%、lines 85.28%；applicable inherited openCypher TCK为3,777/3,777。最终 `scripts/ci.sh` exit 0，覆盖当前SQLite真实 `.load`、Phase 01–09 real-extension probes、artifact inspection、Native ABI smoke、compatibility harness/inventory与TCK inventory。Compatibility Profile与waiver本期未改变，因此 `docs/development/cypher25-compatibility.md` 不需要新增能力条目。

仍有两个必须明确的边界：

1. 在旧26 GiB format2大fixture上，`lithograph_init()` 的完整integrity migration曾超过1小时外层测试窗口并被终止；savepoint完整回滚，`storage_format`仍为2。Phase 11没有冻结大库migration latency目标，本期验证的是migration correctness/rollback与ready-query性能，**不声明**旧26 GiB数据库的migration已具备低延迟。
2. 当前Phase 11改动尚未commit/push，因此Linux x64/arm64、macOS x64/arm64、Windows x64/arm64六目标hosted Release Matrix还没有对应当前代码的远端ref与真实artifact结果。Phase状态必须继续为`in_progress`，直到该外部门禁真实通过。

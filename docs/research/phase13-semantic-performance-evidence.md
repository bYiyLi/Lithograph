# Phase 13 Managed Semantic 性能与资源证据

本文件记录 Phase 13 acceptance SV13-23 / SV13-26 的**机制级**量化证据。它不是跨机器 SLA，也不替代应用自己的 Provider/network/model benchmark。

## 1. 测量范围

测试入口：

```sh
scripts/phase13-semantic-performance.sh \
  <lithograph-extension> \
  <synthetic-embedding-provider>
```

配套 concurrency probe：

```sh
scripts/phase13-semantic-concurrency.sh \
  <lithograph-extension> \
  <synthetic-embedding-provider>
```

两者都使用 deterministic synthetic Provider，不访问公网。performance corpus 固定为 64 个 source owner、16 个 unique exact text；因此 duplicate rate 为 75%。当前 Managed Semantic 搜索路径标识为 `temp-hnsw-v1`：cold full-graph query 在 bounded exact fallback 得到完整 owner/vector 后构建 connection-local TEMP HNSW；同 connection 后续 query 复用 HNSW。restricted Graph View 的 cold fallback 不构建可被更宽 view 误用的 HNSW；已有完整 full-graph HNSW 可以在 candidate visibility/endpoint revalidation 后安全复用。

## 2. Provider call 与 cache 证据

在 Yi Mac（macOS arm64）当前 worktree 上，真实 SQLite runtime probe 观察到：

| 场景 | 结果 |
| --- | --- |
| owners / unique texts | 64 / 16 |
| cold query | 8 rows；16 provider inputs；2 provider calls；自动写入 persistent cache 16 entries；构建 1 个 TEMP HNSW / 64 entries |
| same-connection warm query | 0 provider inputs；复用 connection-local query/source cache + 同一 64-entry TEMP HNSW |
| new-connection/process-reopen warm query | 0 provider inputs；直接复用普通 query 写入的 16 个 persistent embeddings，并构建 1 个 64-entry TEMP HNSW |
| query 后 rebuild | indexed 64 owners；embedded 0；cacheHits 16；0 provider inputs；复用既有完整 HNSW |
| repeated rebuild | embedded 0；cacheHits 16；0 provider inputs |
| persistent cache | 16 entries；256 payload bytes；1 embedding space |
| TEMP HNSW materialization | 两个 connection 生命周期合计 2 次；每次 1 cache / 64 entries |

这组结果直接证明：

- exact source text 在同一 embedding space 内先去重，不因 owner 数量线性放大 Provider inputs；
- ordinary query miss 会把完整校验通过的 query/source embedding 自动写入 persistent cache；本 fixture 的 query `"0"` 与一个 source exact text 重合，因此 16 个 unique inputs 对应 16 个 entries；
- normal search 不依赖 explicit rebuild/prewarm，随后 rebuild 全部命中 cache；
- persistent cache 可跨 connection/process reopen 复用；
- warm rebuild 不重复调用 Provider。
- full-graph query/rebuild 会建立完整 connection-local TEMP HNSW；同 connection query/rebuild 不重复建立内容一致的 HNSW。

16 个 4-dimensional FLOAT32 vector 的 payload 为 `16 × 4 × 4 = 256` bytes，与 `cache.stats().usedBytes` 一致。

## 3. Writer lifetime

concurrency probe 让普通 Semantic query 的 synthetic Provider 在外部 embedding 阶段固定 sleep **1000 ms**，同时启动独立 graph writer。真实 SQLite 运行结果：

| SQLite runtime | provider sleep | writer wait | writer hold | non-autocommit provider calls |
| --- | ---: | ---: | ---: | ---: |
| 3.45.0 | 1000 ms | 0 ms | 4–14 ms（多次 probe 观测） | 0 |
| 3.53.4 | 1000 ms | 0–1 ms | 4–9 ms（多次 probe 观测） | 0 |

该结果证明当前 query Provider I/O **不持有 SQLite single-writer ownership**，且 Native explicit transaction 在到达 Provider callback 前已经拒绝外部 I/O path。并发 writer 在 Provider 期间提交后，原 query connection 的 WAL read snapshot 不能升级为 writer；实现通过同一 database file 的短生命周期 sibling connection 原子发布 query + 两个 source 共 3 个 cache entries，query 与 writer 都成功。它不意味着长 Provider 调用没有成本：目标 Snapshot 仍由 read guard pin 住，WAL/read-view lifetime 应按实际 workload 评估。

## 4. Runtime 与回归门禁

Phase 13 real-load 已在：

- SQLite 3.45.0 minimum runtime；
- SQLite 3.53.4 frozen/current runtime；

完成 Lithograph + synthetic Provider + OpenAI-compatible Provider dual-extension smoke、Native ABI、format migration、history/Graph View/cache/failure 与 concurrency/performance gates。

同一 worktree 的 repository gate：

- `scripts/ci.sh`：exit 0；
- `cargo make quality`：exit 0；
- quality coverage：regions **83.42%**、functions **84.22%**、lines **85.22%**；
- applicable inherited openCypher TCK：**3,777 / 3,777**。

六目标 hosted Release Matrix（Linux/macOS/Windows × x64/arm64）属于 [SV13-27](../development/phases/13-managed-semantic-vector.md#4-acceptance-matrix) 的独立发布制品证据，当前未在未提交 worktree 上执行，因此不由本文件声称完成。

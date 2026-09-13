# Phase 08：Search and Data Ingestion

**状态：`done`**

## 1. 目标

完整实现 Cypher 25 Full-text、Vector Index / `SEARCH` 与 `LOAD CSV`，同时保持 versioned snapshot correctness。

## 2. 依赖

- Phase 06–07 `done`。

## 3. Design Inputs

- `docs/design.md` 第 4.4、7.7、9、11–13 节；
- `docs/development/cypher25-compatibility.md`。

## 4. Features

### Feature 08.1 Full-text DDL and FTS5 lifecycle

- Cypher full-text index create/drop/show；
- node/relationship indexes；
- analyzer/config；
- FTS5 derived table build/rebuild；
- versioned definition + historical snapshot cache key。

### Feature 08.2 Full-text query procedures

实现 Profile procedures：

```text
db.index.fulltext.queryNodes
db.index.fulltext.queryRelationships
```

覆盖 score、skip、limit、analyzer/options 与 result ordering。

Full-text candidate 必须在 `skip` / `limit` 等 Cypher-visible结果语义前应用 Graph View visibility；不能先让 hidden result 消耗分页名额再 post-filter。

### Feature 08.3 Vector storage/index

- exact VECTOR coordinate type/dimension；
- vector index definition；
- HNSW build/persistence/cache；
- similarity functions；
- additional filter properties；
- deterministic metadata + rebuild。

### Feature 08.4 SEARCH

实现 frozen Profile：

- `MATCH` / `OPTIONAL MATCH` SEARCH；
- node/relationship binding；
- `VECTOR INDEX` name resolution；
- `FOR` query vector；
- supported `WHERE` filters；
- `LIMIT`；
- `SCORE AS`；
- planner operator + HNSW execution；
- exact-scan correctness fallback。

HNSW / exact-scan 都必须遵守 Graph View。ANN backend 可以遍历或产生 view 外候选作为内部搜索过程，但 hidden candidate 不得计入最终 top-k/`LIMIT`，也不得作为 graph element 返回；需要继续搜索足够的可见候选以满足 Profile 允许的 ANN 语义。

### Feature 08.5 LOAD CSV

- `file://`、HTTP、HTTPS；
- headers/no headers；
- parsing/encoding/error semantics；
- parameters/correlated writes；
- streaming rows，不读取完整 CSV 到内存。

LOAD CSV 内部发生的 graph-data mutation 继承 Phase 05 Graph View write boundary；外部 I/O 本身不因 Graph View 获得额外文件或网络权限。

### Feature 08.6 Transaction batching

Native API 实现：

- `CALL { ... } IN TRANSACTIONS`；
- `IN CONCURRENT TRANSACTIONS`；
- batch size / status / error semantics；
- `DISJOINT BY`；
- 每个成功 mutating batch 一个 Commit，read-only batch 不创建 Commit；
- branch commit coordinator 让内部 concurrent batch 从各自 transaction 的 latest head 开始；
- query-level Graph View selector 在 outer execution 解析一次并传播到所有 batch；每个 batch pin 自己的 base Commit 后重新计算 visibility，不能复用 outer query 开始时的 element membership；
- SQL Bridge 的 `TRANSACTION_BOUNDARY_REQUIRED` negative contract。

这里的 transaction batching 是 Cypher 25 `CALL { ... } IN TRANSACTIONS` 语义：每个 mutating batch 都是独立 transaction / Commit boundary。它**不**提供“多个外部 Cypher execution -> 一个 Commit”的 Native explicit transaction；后者属于 Phase 09，并建立在 Phase 05 的 staged write foundation 与 Phase 07/08 已完成的 Schema/Search surface 之上。

## 5. Acceptance

- [x] Full-text node/relationship query 与 frozen Profile oracle 对齐；
- [x] Full-text `skip`/`limit` 在 Graph View visibility 后计算，hidden result 不占用返回名额；
- [x] FTS5 cache 删除后可重建且结果一致；
- [x] vector property round-trip 保留 coordinate type/dimension；
- [x] SEARCH node/relationship/filter/score/limit fixtures 通过；
- [x] HNSW 与 exact fallback 在同一 Graph View 下都不返回 hidden element，hidden candidate 不截断可见 top-k；
- [x] HNSW result 满足 Profile ANN contract；
- [x] cache 缺失时 exact fallback 结果语义正确；
- [x] historical commit 的 full-text/vector query 不读取 current-head derived index；
- [x] LOAD CSV large streaming fixture 无全文件 materialization；
- [x] malformed CSV/network/file failure rollback 正确；
- [x] `lithograph_rows` 对 `LOAD CSV` 与 transaction-owning query 返回 `READ_ONLY_ADAPTER`，不触发 filesystem/network/transaction side effect；
- [x] Native API transaction batching 产生独立 durable Commits；
- [x] successful mutating batch 数与新增 Commit 数一致；read-only batch 不新增 Commit；
- [x] Graph View + ordered `IN TRANSACTIONS` 的后续 batch 按 Cypher 语义观察前一成功 batch 已 durable 的可见 graph changes；每个 batch 使用自身 pinned Snapshot 计算 visibility；
- [x] Graph View + `IN CONCURRENT TRANSACTIONS` 不复用 outer-query stale membership；每个 batch 的 visibility 与其实际 pinned branch base / coordinator execution state 一致；
- [x] internal concurrent batches 不因彼此 branch advance 产生伪 `BRANCH_HEAD_MOVED`，外部并发 writer 仍能触发真实 stale-head error；
- [x] SQL Bridge transaction-owning query 返回规定 error；
- [x] compatibility matrix Search/Ingestion families 全部 `done`。

## 6. Review

检查 derived index 是否成为 history truth、Full-text/Vector 是否在 limit/top-k 之后才错误过滤 Graph View、HNSW 是否把 approximate behavior 伪装成 deterministic exact order、historical index key 是否遗漏 commit、LOAD CSV 是否泄漏 credential/无限缓冲。

## 7. 完成条件

Search、Vector、Full-text、LOAD CSV 与 transaction batching compatibility closure；Phase 09 转 `ready`。

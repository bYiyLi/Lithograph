# Phase 08：Search and Data Ingestion

**状态：`planned`**

## 1. 目标

完整实现 Cypher 25 Full-text、Vector Index / `SEARCH` 与 `LOAD CSV`，同时保持 versioned snapshot correctness。

## 2. 依赖

- Phase 06–07 `done`。

## 3. Design Inputs

- `docs/design.md` 第 9、11–13 节；
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

### Feature 08.5 LOAD CSV

- `file://`、HTTP、HTTPS；
- headers/no headers；
- parsing/encoding/error semantics；
- parameters/correlated writes；
- streaming rows，不读取完整 CSV 到内存。

### Feature 08.6 Transaction batching

Native API 实现：

- `CALL { ... } IN TRANSACTIONS`；
- `IN CONCURRENT TRANSACTIONS`；
- batch size / status / error semantics；
- `DISJOINT BY`；
- 每个成功 mutating batch 一个 Commit，read-only batch 不创建 Commit；
- branch commit coordinator 让内部 concurrent batch 从各自 transaction 的 latest head 开始；
- SQL Bridge 的 `TRANSACTION_BOUNDARY_REQUIRED` negative contract。

## 5. Acceptance

- [ ] Full-text node/relationship query 与 frozen Profile oracle 对齐；
- [ ] FTS5 cache 删除后可重建且结果一致；
- [ ] vector property round-trip 保留 coordinate type/dimension；
- [ ] SEARCH node/relationship/filter/score/limit fixtures 通过；
- [ ] HNSW result 满足 Profile ANN contract；
- [ ] cache 缺失时 exact fallback 结果语义正确；
- [ ] historical commit 的 full-text/vector query 不读取 current-head derived index；
- [ ] LOAD CSV large streaming fixture 无全文件 materialization；
- [ ] malformed CSV/network/file failure rollback 正确；
- [ ] Native API transaction batching 产生独立 durable Commits；
- [ ] successful mutating batch 数与新增 Commit 数一致；read-only batch 不新增 Commit；
- [ ] internal concurrent batches 不因彼此 branch advance 产生伪 `BRANCH_HEAD_MOVED`，外部并发 writer 仍能触发真实 stale-head error；
- [ ] SQL Bridge transaction-owning query 返回规定 error；
- [ ] compatibility matrix Search/Ingestion families 全部 `done`。

## 6. Review

检查 derived index 是否成为 history truth、HNSW 是否把 approximate behavior 伪装成 deterministic exact order、historical index key 是否遗漏 commit、LOAD CSV 是否泄漏 credential/无限缓冲。

## 7. 完成条件

Search、Vector、Full-text、LOAD CSV 与 transaction batching compatibility closure；Phase 09 转 `ready`。

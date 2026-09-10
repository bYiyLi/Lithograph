# Phase 02：Version-aware Storage Core

**状态：`done`**

## 1. 目标

建立 Lithograph 最核心的版本化 Property Graph storage。此 Phase 完成后，后续任何 graph write 都不允许绕过 Commit/Layer history。

## 2. 依赖

- Phase 01 `done`。

## 3. Design Inputs

- `docs/design.md` 第 5、8–10、14、17 节。

## 4. Features

### Feature 02.1 Internal schema

创建 Design 定义的：

- sequences；
- label/type/property-key dictionaries；
- commits / branches / layers；
- node/label/relationship/property delta tables；
- schema object store；
- checkpoint tables。

所有 foreign/referential invariants 由 storage API + integrity tests 保证。

### Feature 02.2 Root and main

`lithograph_init()`：

- 生成并持久化稳定 `databaseId`；
- 写入 `storageFormat = 1`；
- 构造 deterministic empty Layer / empty Schema；
- 创建 Root Commit；
- `main` -> Root；
- 重复 init 不创建第二 Root。

Phase 02 接管 Phase 01 仅建立 format metadata 时暂为 `null` 的 init `root` / `branch` 字段；完成本 Feature 后，`lithograph_init()` 必须满足 Design 的最终 Root Commit / `main` Branch contract。

Phase 01 已产生的 canonical metadata-only development bootstrap 由本 Feature 在同一 `lithograph_init()` SAVEPOINT 内补齐 format 1 storage、Root 与 `main`，并保持既有 `databaseId`。只有“恰好为 Phase 01 canonical metadata-only 状态”才进入该迁移路径；partial / extra / corrupt format 1 storage 仍按 integrity contract 拒绝。

### Feature 02.3 Global identity

- NodeId / RelationshipId INTEGER64 allocator；
- label/type/key dictionary；
- committed identity 不复用；
- self-loop / parallel relationship storage fixtures。

### Feature 02.4 Canonical delta and hashing

- canonical slot ordering；
- tagged property encoding；
- BLAKE3 Layer/Schema/Commit hash；
- format version 进入 Commit hash；
- same canonical input 得到相同 hash；
- mutation of persisted bytes 被 integrity check 检出。

### Feature 02.5 Snapshot Resolver

实现：

```text
commit -> nearest checkpoint -> ordered first-parent layers -> overlay
```

Storage API 提供：

- node existence/labels/property；
- relationship existence/endpoints/type/property；
- outgoing/incoming adjacency；
- snapshot iterator。

### Feature 02.6 Checkpoint

- 从任意 Commit 生成 derived checkpoint；
- 删除 checkpoint 后结果不变；
- checkpoint 创建不产生 graph Commit；
- overlay cache 受 connection/query lifecycle 管理。

### Feature 02.7 Integrity checker

实现第 14.1 节 canonical invariants，并提供 intentional corruption fixtures。

## 5. Acceptance

- [x] fresh DB 只有一个 Root 与 `main`；
- [x] databaseId 在 reopen/migration fixture 中保持不变；
- [x] Node/Relationship/Label/Property delta 可写入 synthetic Layer 并从 Snapshot 正确读取；
- [x] forward/backward adjacency 均不执行 relationship full scan；
- [x] parallel relationships / self-loop 正确；
- [x] committed identity 稳定且不复用；
- [x] multiple layers resolution 正确；
- [x] merge-shaped two-parent commit 的 Snapshot 只依赖 first-parent + merge layer 重建；
- [x] checkpoint 建立/删除前后 snapshot hash 相同；
- [x] canonical hash deterministic；
- [x] `LCE1` golden vectors 跨进程重复编码得到相同 bytes/hash；
- [x] corruption fixtures 被 integrity checker 检出；
- [x] rollback 后 Layer/Commit/ref 不残留。

## 6. Review

重点检查是否偷偷建立 mutable-current-state source of truth、是否存在 O(|E|) adjacency、property encoding 是否丢 Cypher type、checkpoint 是否被 correctness 依赖。

Phase-level review 已闭环：

- 未建立 mutable current graph source of truth；canonical correctness 仅依赖 Commit / Layer / Schema history；
- checkpoint 与 overlay 只作为 derived/query-local state，删除 checkpoint 后 semantic hash 不变；
- outgoing / incoming checkpoint 与 delta path 均有定向 B-tree access path，Acceptance 使用 `EXPLAIN QUERY PLAN` 验证；
- property persistence 使用 tagged typed payload + LCE1，不经 JSON 降级；
- 修复 LCE1 Point/Vector count 必须使用 ULEB128、Relationship identity tuple stability、content-addressed Layer/Commit 被篡改后禁止静默复用、integrity restore/merge false-positive 等 findings；
- 生产 duplicate 为 0.62%，低于 1% gate；现有重复均为短 SQL/error-shape，不为消除局部重复增加新的抽象层。

## 7. 验证证据

- `cargo test -p lithograph-core`：16 个 Phase 02 storage/checkpoint/integrity integration tests 全部通过；
- `scripts/lce1-golden.sh`：两个独立进程产生相同 frozen LCE1 bytes/hash，hash 为 `cea822c1c96dd7c456beae36aa4e9e89d8d3cad8658aa450b3ede2887ade4440`；
- `cargo make quality`：format、clippy、Rustdoc、file/complexity/duplicate、unused dependency、supply-chain 与 coverage 全部通过；最终 coverage 为 regions 81.33%、functions 82.40%、lines 84.29%；
- `scripts/ci.sh`：SQLite 3.51.0 与最低 SQLite 3.45.0 的真实 `.load`、Phase 01 regression、Phase 02 storage probe、artifact inspection、Native ABI 与 compatibility inventory 全部通过；
- `docs/development/cypher25-compatibility.md` capability family 仍全部为 `planned`；Phase 02 没有把 storage foundation 误标为 Cypher semantics。

## 8. 完成条件

Storage vertical slice 与 integrity acceptance 通过；Phase 03 转 `ready`。

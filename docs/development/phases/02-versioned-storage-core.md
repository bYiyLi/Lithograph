# Phase 02：Version-aware Storage Core

**状态：`ready`**

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

- [ ] fresh DB 只有一个 Root 与 `main`；
- [ ] databaseId 在 reopen/migration fixture 中保持不变；
- [ ] Node/Relationship/Label/Property delta 可写入 synthetic Layer 并从 Snapshot 正确读取；
- [ ] forward/backward adjacency 均不执行 relationship full scan；
- [ ] parallel relationships / self-loop 正确；
- [ ] committed identity 稳定且不复用；
- [ ] multiple layers resolution 正确；
- [ ] merge-shaped two-parent commit 的 Snapshot 只依赖 first-parent + merge layer 重建；
- [ ] checkpoint 建立/删除前后 snapshot hash 相同；
- [ ] canonical hash deterministic；
- [ ] `LCE1` golden vectors 跨进程重复编码得到相同 bytes/hash；
- [ ] corruption fixtures 被 integrity checker 检出；
- [ ] rollback 后 Layer/Commit/ref 不残留。

## 6. Review

重点检查是否偷偷建立 mutable-current-state source of truth、是否存在 O(|E|) adjacency、property encoding 是否丢 Cypher type、checkpoint 是否被 correctness 依赖。

## 7. 完成条件

Storage vertical slice 与 integrity acceptance 通过；Phase 03 转 `ready`。

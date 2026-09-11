# Phase 05：Mutation, Transaction and Commit

**状态：`done`**

## 1. 目标

把 Cypher graph mutation、SQLite transaction 和 immutable Commit history 连成一个原子 write path。

## 2. 依赖

- Phase 02–04 `done`。

## 3. Design Inputs

- `docs/design.md` 第 4.4、7–10、13–14 节。

## 4. Features

### Feature 05.1 Mutating operators

实现 foundation semantics：

- `CREATE` / `INSERT` Node/Relationship；
- `SET` property/labels；
- `REMOVE` property/labels；
- `DELETE` / `DETACH DELETE`；
- `MERGE` foundation；
- multi-row write pipeline。

所有 graph-data write 继承 Phase 04 Graph View boundary。Selector 在整个 query 内固定，但 visibility 按当前 clause 的 input graph state 计算，因此后序 clause 必须观察前序 clause 已完成的 writes。不可见既有 element 不能被 mutation 定位；每个 mutating clause 的完整 effect 结束后，新建或修改且仍存在的 Node 必须位于 view 内，Relationship 两端必须可见；Label mutation 或 delete/detach 若让 element 越界或要求修改不可见 Relationship，返回 `GRAPH_VIEW_VIOLATION` 并回滚整个 top-level write。

### Feature 05.2 Delta builder

- 把 executor mutation 归一化为 logical slots；
- 同一 query 对同一 slot 多次修改 canonicalize 到最终 delta；
- Node delete 显式包含 Relationship delete；
- before-state 保留用于 patch/conflict/inverse。

### Feature 05.3 Commit writer

每个成功 mutating query：

- pin branch head；
- execute；
- canonicalize Layer；
- hash Layer / Commit；
- write commit；
- compare-and-move branch；
- output commit id/counters。

No-op mutating query 仍创建 Commit。

### Feature 05.4 Transaction semantics

覆盖：

- auto-commit；
- caller-owned `BEGIN/COMMIT/ROLLBACK`；
- SQLite savepoint interaction；
- query failure rollback；
- extension error/panic conversion；
- stale branch head -> `BRANCH_HEAD_MOVED`。
- SQL Bridge per-invocation SAVEPOINT 与 caller-owned transaction composition。

### Feature 05.5 Write/read barriers

实现 Cypher read-after-write / write-after-read 所需 Eager/materialization barrier，防止同一 query 因 streaming mutation 看到自己刚写的数据而产生错误 cardinality。Graph View visibility 使用 barrier 所对应的 Cypher graph state：不能固定为 query-start membership，也不能提前看到后序 clause 的 write。

## 5. Acceptance

- [x] CREATE Node/Relationship 后当前 head 前进一个 Commit；
- [x] Graph View 内 MATCH 到的隐藏 element 不可被 SET/REMOVE/DELETE/MERGE 修改；
- [x] `CREATE` / `INSERT` clause 完成后新 Node 不满足当前 Graph View，或 Relationship endpoint 不可见时返回 `GRAPH_VIEW_VIOLATION`，不生成 Commit；
- [x] 在 required Label view 中，`CREATE (n:Required)` 后续 read clause 能观察该 staged Node；`CREATE (n) SET n:Required` 在前一个 `CREATE` clause boundary 返回 `GRAPH_VIEW_VIOLATION`；
- [x] `SET` / `REMOVE` Label 使可见 Node 离开当前 view 时原子返回 `GRAPH_VIEW_VIOLATION`；
- [x] delete/detach 需要隐式删除 view 外 Relationship 时原子拒绝，不发生跨 view mutation；
- [x] SET/REMOVE/DELETE/DETACH 在历史 Commit 可正确 time-travel；
- [x] no-op mutating query 产生 empty-delta Commit；
- [x] multi-row write 只产生一个 top-level Commit；
- [x] query 中途失败 Branch head / Commit / Layer 全部 rollback；
- [x] SQL Bridge mutating invocation 发生 parser-after-write/fault-injection/error 时回滚到 invocation savepoint；
- [x] autocommit 下两个 scalar write invocation 前一成功、后一失败时保持两个独立 invocation；caller-owned outer rollback 能撤销其中全部 graph invocation；
- [x] `lithograph_rows` 对 mutating query 返回 `READ_ONLY_ADAPTER`，virtual-table rescan 不执行任何 write；
- [x] Native callback cancel 发生在尚未 durable 的 write 时返回 `SQLITE_INTERRUPT` 并回滚整个 write；
- [x] outer SQLite rollback 移除其中全部 graph Commits；
- [x] outer SQLite commit 后其它 connection 一次看到完整 commit chain；
- [x] two writers 基于同一 branch base 时一个成功、另一个准确 `BRANCH_HEAD_MOVED`；
- [x] read/write barrier regression fixtures 证明每个 clause 看到全部前序 write、看不到后序 write，且 Graph View membership 基于同一个 clause graph state；
- [x] reopen DB 后 history 与 current head 一致。

## 6. Review

重点检查 Branch move 是否可能脱离 Layer transaction、Graph View 是否能通过 direct element reference/MERGE/DETACH 绕过、violation rollback 是否残留 identity/history、multi-row write 是否错误地产生 per-row Commit、MERGE 是否依赖非原子先查后写。

Phase-level review 已闭环：

- mutating query 在一个 invocation savepoint 内 pin Branch head、执行全部 clause、canonicalize net delta、写入 immutable Layer/Commit 并 CAS 移动 Branch；parser/type/projection/Graph View/interrupt/adapter failure 均回滚 identity、dictionary、Layer、Commit 与 ref move；
- caller-owned transaction 与 savepoint 不被 extension 提前提交或回滚；autocommit invocation 彼此独立，outer commit 后其它 connection 一次观察完整 commit chain，outer rollback 不留下 graph history；
- Delta counters 直接来自最终 canonical slots；同一 query 内 create-then-delete、SET 后恢复原值等净零变化不会虚报 side effect，no-op mutating query 仍按设计创建 empty-delta Commit；
- mutating clause 使用 materialization barrier，后序 clause 观察完整前序 writes，前序 read 不追逐刚创建的 row；multi-row pipeline 只产生一个 top-level Commit；
- Graph View visibility 在每个 mutating clause boundary 按 staged graph state 重新验证；隐藏 element、不可见 Relationship endpoint、越界 label mutation 与跨 view DETACH 均不能绕过 write boundary；
- DELETE 对整条 clause 的所有 input row 先处理显式 Relationship，再处理 Node；非 DETACH connected-node delete 原子返回 constraint error，DETACH 的隐式 Relationship delete 同样经过 Graph View 校验；
- MERGE 的 match/create 与 `ON MATCH` / `ON CREATE` effect 位于同一 write transaction，并在每个独立 pattern match 重置 relationship uniqueness state；bound endpoint 不会被其它 candidate 覆盖，large `ON MATCH` apply loop 持续观察 host interrupt，null pattern property 在写入前拒绝；
- write `RETURN DISTINCT` 使用 canonical grouping key，null 与嵌套值按 grouping equality 去重；DISTINCT 在 ORDER BY 前执行，ORDER BY 与 read executor 共用 `cypher_order_compare` 总排序，未定义排序的 value family 返回 type error，并在 Commit 前回滚 mutation；aggregate write projection 同样在 SKIP/LIMIT 前验证 ORDER BY，只接受 projected count、alias 与 constant expression，不会因 aggregate shortcut 跳过错误；
- scalar、rows 与 Native adapter 共用 QueryCursor lifecycle；未消费完 row 的 cursor 不能提前 `complete()` 并提交，rows adapter 明确拒绝 mutation，scalar result 长度检查与 Native callback cancel 都发生在 savepoint release 之前；
- fixed-offset temporal value 与其余 13 个 PropertyValue family 经过 mutation/history round-trip；IANA named-zone 持久化在 Phase 06 timezone/DST 规则落地前明确返回 type error，不静默改写 offset；
- expression lowering 正确保留 function argument、nested parenthesized property access、nested `NOT` 与 chained comparison 语义；当前 executor 未支持的 function 即使 input 为零行也在 lowering 阶段拒绝，已支持的 `elementId` / `labels` / `type` / `size` 保留参数并传播 null；同一 CREATE 内只允许合法的 bare variable reuse，新建变量不能被同 clause property expression 提前引用；
- mutating EXPLAIN 构建与实际执行一致的 scan/filter/optional/projection/write logical plan，anonymous pattern placeholder 不会跨 pattern 形成错误 binding；
- Phase 06 才拥有的 map projection、list/pattern comprehension、dynamic/negative/disjunctive write name、inline pattern WHERE、path selector/mode/quantifier、computed SET target 与其它 unsupported postfix/mutation semantics 不做空集合或降级近似执行，而是明确拒绝；
- planner build、stream projection/write lifecycle、canonical delta 与 Phase 05 execution/history regression 按职责拆分为子模块；本次涉及的 Rust 文件全部低于 1000 行 soft file budget。

## 7. 验证证据

- `cargo test --locked -p lithograph-core -p lithograph-test-support`：Phase 02–05、frontend/TCK inventory 与 test-support regression 全部通过，其中 Phase 05 targeted mutation suite 67/67；
- `cargo test --locked -p lithograph-extension`：extension unit/doc tests 8/8；
- `lithograph-phase05` 真实 `.load` probe：write summary、失败/late projection/scalar length rollback、autocommit 隔离、outer transaction/savepoint composition、rows read-only、Graph View rollback 与 reopen history 10 项检查全部通过；
- `scripts/native-abi-smoke.sh`：Native mutating SUMMARY、ROW/SUMMARY callback cancel、`SQLITE_INTERRUPT` rollback 与 savepoint fault injection 通过；
- `cargo make quality` exit 0：无 file-budget warning，duplicated lines `0.98%`；coverage regions `83.25%`、functions `85.12%`、lines `85.13%`；fmt、Clippy、Rustdoc、production/support/C complexity、dependency policy 与 supply-chain gate 全部通过；
- `scripts/ci.sh` exit 0：当前 SQLite 3.51.0 与 SQLite 3.45.0 minimum fixture 的真实 `.load`、Phase 01–05 probes、artifact inspection、Native ABI、compatibility harness/inventory 全部通过；Windows smoke 已纳入 Phase 05 functional probe，真实跨平台 release matrix 仍由 Phase 10 验收。

## 8. 完成条件

Versioned write vertical slice 完整通过；Phase 06 转 `ready`。

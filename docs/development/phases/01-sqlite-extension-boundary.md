# Phase 01：SQLite Extension Boundary

**状态：`planned`**

## 1. 目标

交付可被 stock SQLite 真实加载的 Lithograph shared library，并固定 SQL Bridge、Native C ABI、初始化、内部 format metadata 与迁移 boundary。

## 2. 依赖

- Phase 00 `done`。

## 3. Design Inputs

- `docs/design.md` 第 4、13–16 节。

## 4. Features

### Feature 01.1 Loadable extension ABI

- `sqlite3_lithograph_init`；
- 使用 `sqlite3_api_routines` host API table 初始化 Rust SQLite boundary；
- `rusqlite::Connection::extension_init2` 或等价 `libsqlite3-sys` loadable-extension API 完成 connection wrapper；
- Extension artifact 不链接/打包私有 SQLite runtime；
- macOS/Linux/Windows shared-library build；
- 只使用 SQLite public extension API；
- `.load` 只注册功能，不创建内部表。

### Feature 01.2 Initialization and format metadata

实现：

```sql
SELECT lithograph_init();
SELECT lithograph_version();
```

- `_lithograph_meta`；
- storage format version；
- idempotent init；
- transactional migration scaffold；
- `FORMAT_TOO_NEW`；
- 不使用 `PRAGMA user_version`。

### Feature 01.3 SQL Bridge

注册：

```text
lithograph(...)
lithograph_rows(...)
lithograph_validate(...)
lithograph_integrity_check()
```

`lithograph_rows` 使用 eponymous-only virtual table + hidden input columns，真实支持 cursor streaming。

所有 side-effect scalar invocation 建立唯一内部 SAVEPOINT；失败必须 rollback 到 invocation 开始状态，再把 error 返回 SQLite。不得依赖宿主 SQL statement 自动回滚递归 UDF 写入。

### Feature 01.4 Native C ABI v1

实现 versioned symbols：

```text
lithograph_v1_execute
lithograph_v1_validate
lithograph_v1_free
```

- 接收 existing `sqlite3*`；
- callback streaming；
- structured error；
- ABI tests 验证 C caller。

### Feature 01.5 Safety flags

- mutating/executing entrypoints 使用 direct-only boundary；
- 无副作用 information function 才允许 innocuous；
- trigger/view 不得隐式触发 Cypher write/init/migration。

## 5. Acceptance

- [ ] macOS 当前开发机通过真实 `sqlite3 .load ./lithograph`；
- [ ] SQLite 3.45.0 minimum fixture 与当前开发 SQLite 都通过真实 `.load` + init smoke；
- [ ] artifact dependency/symbol inspection 证明没有 bundled/private SQLite runtime，SQLite calls 通过 extension host API table；
- [ ] `.load` 前后 user schema 不发生变化；
- [ ] 未初始化 database 调用 graph API 返回 `NOT_INITIALIZED`，`lithograph_version()` 仍可读取 Extension/format support metadata；
- [ ] `lithograph_init()` 创建 internal metadata 并幂等；
- [ ] init 中途故障 fixture rollback，无 half-schema；
- [ ] higher format fixture 返回 `FORMAT_TOO_NEW`；
- [ ] SQL scalar/rows/validate/version/integrity symbols 可调用；
- [ ] init/version/validate/integrity JSON shape 与 Design 固定 contract 一致；
- [ ] `lithograph_rows` 能在 synthetic generator query 上证明逐行 callback，不先构建完整 JSON array；
- [ ] `lithograph_rows` 对 mutating query 返回 `READ_ONLY_ADAPTER`，不会因 virtual-table rescan 重复执行 write；
- [ ] `lithograph_rows` 对 `LOAD CSV`、checkout、GC 等外部/connection side-effect query 返回 `READ_ONLY_ADAPTER`；
- [ ] 预先存在 `_lithograph_*` object 但无有效 magic marker 的 fixture 返回 `STORAGE_ERROR` 且不覆盖 user object；
- [ ] side-effect UDF 在写入一半后故障时，内部 SAVEPOINT 清除该 invocation 的全部变化；
- [ ] autocommit 下两个 scalar write invocation 前一成功、后一失败的 fixture 与 Design 的“独立 invocation”语义一致；显式 outer transaction rollback 能撤销其中全部 invocation；
- [ ] C ABI smoke 通过；
- [ ] Native event 顺序 `COLUMNS -> ROW* -> SUMMARY`、callback cancel、error allocation/free lifecycle 通过 C ABI test；
- [ ] trigger/view direct-only negative test 通过；
- [ ] Linux/Windows cross-build 通过。

## 6. Review

重点检查 ABI lifetime、SQLite allocator/API 使用、panic 跨 FFI、connection-local state cleanup、reentrancy、error ownership 和 migration atomicity。

## 7. 完成条件

Acceptance 全部通过且 review 闭环；Phase 02 转 `ready`。

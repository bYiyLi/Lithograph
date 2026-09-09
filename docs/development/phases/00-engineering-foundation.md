# Phase 00：Engineering Foundation

**状态：`ready`**

## 1. 目标

建立后续所有 Engine Phase 共享的 Rust、SQLite extension、compatibility 和 test 工程基线。Phase 00 不实现 graph product behavior。

## 2. Design Inputs

- `docs/design.md`：第 3、15、17 节；
- `docs/development/cypher25-compatibility.md`；
- `docs/research/reference-baseline.md`。

## 3. Features

### Feature 00.1 Rust workspace

- 通过官方 `rustup` 安装/提供 Rust toolchain；当前冻结 toolchain 为 **Rust 1.98.1**；
- Rust 2024 Edition workspace；
- 首个 SQLite boundary dependency 固定 `rusqlite 0.40.1`，启用 `loadable_extension`、function 与 virtual-table 所需 features；禁止 `bundled` SQLite feature；
- core crate、SQLite extension crate、test-support crate 的最小结构；
- `rust-toolchain.toml` 明确 pin `1.98.1`，CI 与本地都使用该版本；
- `Cargo.lock` 纳入仓库；
- 不建立没有当前 owner 的抽象 crate。

### Feature 00.2 Quality gates

建立 canonical commands：

```text
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

CI 默认执行同一 gate，避免本地/CI 两套标准。

### Feature 00.3 SQLite fixture harness

- ephemeral file database；
- in-memory database；
- extension-load fixture；
- disposable corruption/crash fixture；
- deterministic seed / cleanup；
- SQLite version/compile-option inspection helper。

### Feature 00.4 Compatibility harness

- vendor/pin Apache-2.0 openCypher TCK feature data 与 NOTICE，Lithograph 自己实现/适配 runner，不依赖 JVM runtime 才能执行默认 compatibility gate；
- 建立 Cypher 25 fixture format；
- fixture 可声明 query、params、initial graph、expected rows/error、profile source；
- 结果比较保留 row multiplicity、ordering requirement 和 typed values；
- 产生 machine-readable compatibility report。

### Feature 00.5 Reference fixtures

固定首批 oracle fixtures，覆盖：

- parse/error location；
- integer/null/string/list/map；
- basic `MATCH/RETURN`；
- Cypher 25 `WHEN/NEXT/FOR` parser fixtures；
- `VECTOR`、Graph Type、`SEARCH`、UUID、string interpolation parser fixtures。

这些 fixtures 允许未实现，但必须能被 harness 表达并报告 `planned/failing`，不能因未实现而从 inventory 消失。

## 4. Acceptance

- [ ] workspace 在 clean checkout 构建；
- [ ] `rustc --version` / `cargo --version` 与 pinned toolchain 一致；
- [ ] `cargo tree -e features` 证明 Extension dependency graph 未启用 bundled SQLite；
- [ ] format/clippy/test canonical commands 通过；
- [ ] fixture database 可重复创建/销毁；
- [ ] compatibility runner 可以执行至少一个 passing fixture 和一个 intentional failing fixture，并准确报告差异；
- [ ] openCypher TCK scenario 可以被 harness 读取；
- [ ] pinned TCK revision、LICENSE 与 NOTICE 可从仓库复现；
- [ ] `CY25-2026.08` fixture metadata 可以记录 source/version/family；
- [ ] CI 与本地 gate 使用同一 canonical scripts/commands；
- [ ] 没有 product behavior 被 mock 后声称实现。

## 5. Review

检查：

- workspace 是否为未来假设拆出多余 crate；
- test harness 是否丢失 duplicates/type/error-location；
- external TCK fixture 是否被修改成迎合实现；
- benchmark/corruption fixture 是否只使用 disposable data。

## 6. 完成条件

所有 Acceptance 通过，review finding 闭环，Phase 00 状态同步为 `done`，Phase 01 转为 `ready`。

# Phase 00：Engineering Foundation

**状态：`done`**

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
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace
```

CI 默认执行同一 gate，避免本地/CI 两套标准；除 `cargo fmt` 外的 Cargo build/test/run gate 使用 `--locked`，保证 `Cargo.toml` 与仓库 `Cargo.lock` 不一致时直接失败而不是静默改写 lockfile。Vendor integrity gate 使用 Python 3 标准库，不引入额外 Python package dependency。

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

- [x] workspace 在 clean checkout 构建；
- [x] `rustc --version` / `cargo --version` 与 pinned toolchain 一致；
- [x] `cargo tree -e features` 证明 Extension dependency graph 未启用 bundled SQLite；
- [x] format/clippy/test canonical commands 通过；
- [x] fixture database 可重复创建/销毁；
- [x] compatibility runner 可以执行至少一个 passing fixture 和一个 intentional failing fixture，并准确报告差异；
- [x] openCypher TCK scenario 可以被 harness 读取；
- [x] pinned TCK revision、LICENSE 与 NOTICE 可从仓库复现；
- [x] `CY25-2026.08` fixture metadata 可以记录 source/version/family；
- [x] CI 与本地 gate 使用同一 canonical scripts/commands；
- [x] 没有 product behavior 被 mock 后声称实现。

## 5. Review

检查：

- workspace 是否为未来假设拆出多余 crate；
- test harness 是否丢失 duplicates/type/error-location；
- external TCK fixture 是否被修改成迎合实现；
- benchmark/corruption fixture 是否只使用 disposable data。

## 6. 完成条件

所有 Acceptance 通过，review finding 闭环，Phase 00 状态同步为 `done`，Phase 01 转为 `ready`。

## 7. 完成证据

- Rust toolchain 由 `rust-toolchain.toml` 固定为 `1.98.1`，workspace 使用 Rust 2024 Edition，`Cargo.lock` 已纳入仓库；CI 显式执行 `rustup toolchain install 1.98.1`，不依赖 rustup 已弃用的 implicit auto-install；
- workspace 仅包含 `lithograph-core`、`lithograph-extension`、`lithograph-test-support` 三个当前有明确职责的 crate；
- Extension 使用 `rusqlite 0.40.1` 的 `loadable_extension`、`functions` 与 `vtab` feature，feature tree 已验证不存在 bundled SQLite，artifact dependency inspection 也未发现 private SQLite runtime linkage；
- `scripts/ci.sh` 是本地与 CI 共享 gate，并验证 SQLite `3.45.0+`、FTS5、thread-safe 与 loadable-extension runtime requirement；所有会解析 dependency graph 的 Cargo build/test/run gate 使用 `--locked`，已用 stale `Cargo.lock` 负向探针确认会直接失败而不会静默改写 lockfile；
- 当前开发机使用支持 `.load` 的 SQLite 3.51.0 对真实 `lithograph` shared library 完成 load smoke；系统自带 `OMIT_LOAD_EXTENSION` 的 SQLite 不作为合格 runtime；
- file / memory / corruption / crash fixture、runtime inspection、compatibility result comparator 与 machine-readable report 均有自动化测试；fixture/report boundary 会拒绝空 inventory、duplicate ID、mixed profile、row width mismatch、矛盾 parse expectation 与非法 error location，而当前 `CY25-2026.08` 11 个 fixture 已通过同一 validation；
- openCypher TCK 固定为 upstream `2024.3` / `677cbafabb8c3c5eed458fd3b1ec0daec8d67d23`，vendored `LICENSE`、`NOTICE`、feature 与 graph data 已与 upstream 逐字节核对；`.gitattributes` 禁止该目录的 Git text normalization，`MANIFEST.sha256` + `scripts/check-vendor.py` 在默认 gate 中同时验证固定 source/tag/commit/content/license revision metadata 和 227 个 upstream 文件；错误 commit metadata 的负向探针会被明确拒绝；
- Rust TCK adapter 对 pinned corpus 可读取 220 个 feature、1615 个 scenario definition、276 个 Scenario Outline、7104 个 step、2370 个 DocString、1687 个 DataTable 与 276 个 Examples block，并展开为 3897 个 executable scenario；Gherkin DataTable 的 `\\n` / `\\|` / `\\\\` escaping、Background、tag 与 outline substitution 均有 targeted tests；
- TCK 结构与展开结果使用临时安装的 Cucumber 官方 `gherkin-official` parser/compiler 做了独立交叉验证：全部 220 个 feature 的结构逐项一致，全部 3897 个 executable scenario 的 name/tag/step/DocString/DataTable 展开内容逐项一致，mismatch 为 0；该工具仅用于 review，不进入 Lithograph dependency；
- 首批 `CY25-2026.08` fixture inventory 已覆盖本 Phase 要求的 parser、value、`MATCH/RETURN` 与 Cypher 25 新能力入口；Graph Type positive fixture 已按官方 `ALTER CURRENT GRAPH TYPE SET { ... }` / element-type 语法校正，parse-error fixture 固定官方可观察的 line/column；全部 capability 仍保持 `planned`，没有伪造 Engine 实现状态；
- 最终验收使用当前工作树建立临时 Git commit，再从该 commit 真实 `git clone`；clone 工作区 clean、`target` 初始不存在、vendored CRLF 原始字节保持不变，并通过 `cargo build --workspace --locked` 与完整 `scripts/ci.sh` gate。

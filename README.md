# Lithograph

Lithograph 为 SQLite 提供版本化 Property Graph 数据库能力。

它以标准 SQLite loadable extension 的形式运行：保留 SQLite 的嵌入式、单文件部署体验，同时使用 Cypher 25 查询图数据，并使用 immutable Commit DAG、Branch 与 Tag 管理同一张图的状态演进。Git / TerminusDB 是版本机制参考，不限定上层如何解释这些状态。

## 核心能力

- **Property Graph**：Node、Relationship、Label、Relationship Type 与 Property。
- **Cypher 25**：图查询、数据修改、路径、Schema、Constraint、Index 与现代 Cypher 类型系统。
- **版本化状态管理**：immutable Commit、Branch、Tag、可修改 Commit Data、显式 empty-delta Commit、可分页历史查询、Time-travel、结构化 Diff/Patch、Merge、Rebase、Squash、Reset 与 Revert。
- **全文与向量搜索**：Full-text Index、Vector Index 与 Cypher 25 `SEARCH`。
- **SQLite 原生部署**：作为 loadable extension 使用同一个 SQLite database file，不需要独立数据库 Server。
- **大规模单机图**：版本感知存储、索引化邻接访问、流式查询执行与 checkpointed history 面向大规模本地图数据设计。

## 本地构建与 Quickstart

当前仓库尚未发布可用于生产的 Release；本地试用需要 Rust 1.98.1，以及支持 loadable extension 的 SQLite 3.45.0 或更高版本。

先构建 release extension：

```sh
cargo build --locked --release -p lithograph-extension
```

Cargo 生成的 shared library 位于 `target/release/`：Linux 为 `liblithograph.so`，macOS 为 `liblithograph.dylib`，Windows 为 `lithograph.dll`。下面以 SQLite CLI 为例，先打开一个 database：

```sh
sqlite3 lithograph-demo.sqlite
```

进入 SQLite CLI 后，`<extension>` 替换为当前平台的实际文件路径：

```text
.load <extension>
SELECT lithograph_init();
```

`.load` 只注册 Lithograph API；`lithograph_init()` 才会在当前 SQLite database 中初始化或迁移 Lithograph storage，并建立空图的 Root Commit 与默认 `main` Branch。

通过 `lithograph()` 执行会修改图状态的 Cypher：

```sql
SELECT lithograph('CREATE (:Person {name: ''Alice''}) FINISH');
```

只读结果较大时，使用流式的 `lithograph_rows()`：

```sql
SELECT row
FROM lithograph_rows('MATCH (p:Person) RETURN p.name');
```

上述流程已在当前 macOS arm64 release build 上以真实 SQLite CLI 验证；Phase 10 release gate 另外覆盖 SQLite 3.45.0 minimum 与 3.53.4 release-current runtime。其它 host application 需要在目标 SQLite connection 上启用 loadable extension，并通过 SQLite 官方 extension-loading API 加载同一个 shared library。

同一张图可以在不同 Commit 上查询，也可以在不同 Branch 上独立演化，而不需要复制 SQLite 数据库文件。

## 项目状态

Lithograph 当前已经完成 Phase 00–10 的基础功能与 release-hardening 验收，Phase 11 Performance Optimization 保持 `in_progress`。现有实现已具备标准 SQLite loadable-extension / Native ABI boundary、version-aware immutable storage、Graph View、Cypher 25 parser/type/value、真实 read planner/executor、mutation/Commit path、versioned Graph Type/Constraint、lookup/range/text/point/full-text/vector index、`SEARCH`、`LOAD CSV`、Cypher transaction batching，以及 Branch/Tag/Commit Data/Diff/Patch/Merge/Rebase/Squash/Reset/Revert/GC 等 Version operations。Phase 10 已完成 compatibility、recovery/migration、10M Node / 100M Relationship scale、architecture/security hardening，并通过 Linux x64/arm64、macOS x64/arm64、Windows x64/arm64 六目标 hosted release artifact acceptance。Phase 11 当前工作树已经实现 storage format 3、persistent Standard Index、物理邻接 keyset、query-owned resolved state/read guard、增量 index overlay 与本期性能优化；固定性能机的10M Node / 100M Relationship、1M/100K Search、10K-conflict Merge、1/4/8-reader 30分钟压力以及 repository-wide quality/coverage/真实 SQLite CI 均已通过。当前改动尚未 commit/push，因此无法对这份工作树执行新的六目标 hosted Release Matrix；该远端 acceptance 是 Phase 11 仍未标 `done` 的唯一剩余门禁。仓库尚未发布正式 Release。

- [技术设计](docs/design.md)
- [开发计划](docs/development/README.md)
- [Phase 11 性能优化计划](docs/development/phases/11-performance-optimization.md)
- 当前开发阶段：Phase 00–10 `done`；Phase 11 `in_progress`

## 许可

Lithograph 采用双许可模式：

- **AGPL-3.0-only**：本仓库代码默认依据 [GNU Affero General Public License v3.0](LICENSE) 提供。AGPL 允许商业使用，但使用者必须遵守其开源义务。
- **Commercial License**：无法或不希望按 AGPL 使用 Lithograph 的组织，可以申请独立商业许可；商业条款仅通过单独书面协议授予，详见 [COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md)。

第三方依赖、测试数据或 vendored material 可以保留其各自许可证与 NOTICE；对应目录或文件的明确许可证优先。

## 贡献

当前欢迎通过 GitHub Issues 提交 bug、设计反馈和兼容性问题。为保持 AGPL + Commercial License 双许可能力，在 contributor licensing policy 正式建立前暂不接受外部代码或文档 Pull Request，详见 [CONTRIBUTING.md](CONTRIBUTING.md)。

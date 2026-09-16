# Changelog

Lithograph 的用户可见版本变化记录在此文件。版本遵循 Semantic Versioning；在 `1.0.0` 之前，minor release 仍可能包含不兼容的公开接口或存储合同调整。

## 0.1.0 - 2026-09-16

Lithograph 首个公开版本。

### Added

- 标准 SQLite loadable extension 与 Native ABI boundary，无独立 Server/Daemon。
- Property Graph：Node、Relationship、Label、Relationship Type 与 Property。
- 冻结的 Cypher 25 compatibility profile；全部 applicable inherited openCypher TCK 3,777/3,777 通过。
- Version-aware immutable storage、Commit DAG、Branch、Tag、Commit Data、Time-travel、Diff/Patch、Merge/Rebase/Squash/Reset/Revert 与 GC。
- Versioned Graph Type、Constraint、Standard Index、Full-text Index、Vector Index、`SEARCH` 与 `LOAD CSV`。
- storage format 3、persistent Standard Index、邻接 keyset、query-owned resolved state/read guard 与增量 index overlay。
- Linux x64/arm64、macOS x64/arm64、Windows x64/arm64 六个平台预编译 Release Assets。

### Compatibility and scale evidence

- SQLite 3.45.0 minimum 与 3.53.4 release-current runtime gate。
- 10M Node / 100M Relationship scale workload。
- 1M×128 与 100K×1536 Vector、1M Full-text corpus。
- 10,000-conflict Merge Session。
- 1/4/8 readers + 1 writer 各 30 分钟 mixed stress。

### Known limitations

- v0.1.0 是 pre-1.0 release；后续 minor release 仍可能调整 API、ABI 或 storage compatibility contract。
- storage format 3 没有自动 downgrade；升级已有 database 前应保留完整备份。
- 旧 26 GiB format 2 大库的完整 integrity migration 在现有测试窗口中曾超过 1 小时；v0.1.0 不承诺大型旧库迁移的低延迟。

# Native / FFI Surface

v0.3.0 **不提供 application-facing Cypher execution、validation 或 explicit-transaction C API**。应用通过标准 SQLite API 加载 Lithograph，并使用 [SQL API](sql-api.md)。

保留的 FFI 边界只有：

1. SQLite loadable-extension entrypoint：`sqlite3_lithograph_init`。
2. `EmbeddingProviderV1` SPI：供独立 Embedding Provider extension 注册 `validate` / `embedBatch` 等能力；它不是 application query API。

Release artifact gate 会正向检查 Lithograph 不导出 legacy application query symbols，并继续验证 Provider SPI/dual-extension load。旧版本 C query ABI 的历史行为只在对应 release tag / release notes 中保留。

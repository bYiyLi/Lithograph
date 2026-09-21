# Lithograph Reference

本目录描述 **v0.3.0 正式发布接口**。旧版本历史接口请查对应 Release Notes/tag。

| 需要查找 | 页面 |
| --- | --- |
| SQL functions、execution event stream、result envelope | [SQL API](sql-api.md) |
| Execution options | [Execution Options](execution-options.md) |
| Value encoding | [Values](values.md) |
| Procedure 参数和返回 | [Procedures](procedures.md) |
| Procedure inventory | [Procedure Inventory](procedure-inventory.md) |
| Built-in functions | [Functions](functions.md) |
| Cypher compatibility | [Cypher Compatibility](cypher-compatibility.md) |
| Native / FFI 边界 | [Native Surface](native-api.md) |
| Error categories | [Errors](errors.md) |
| Limits | [Limits](limits.md) |

当前 application query surface 是 SQLite SQL-only；Native surface 只保留 SQLite loadable-extension entrypoint 与 Embedding Provider SPI。

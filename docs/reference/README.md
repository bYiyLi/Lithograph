# Lithograph v0.1.0 Reference

本目录用于查找准确接口，不替代 [入门与任务指南](../guide/README.md)。范围固定为 v0.1.0 / `CY25-2026.08` / Native ABI 1 / storage format 3；实现缺陷单独记录，不把缺陷改写成新的设计合同。

| 需要查找 | 页面 |
| --- | --- |
| SQL 函数、行适配器、结果 envelope、counters | [SQL API](sql-api.md) |
| Query / Native transaction 的 options 与互斥关系 | [Execution Options](execution-options.md) |
| JSON 参数和 Node / Relationship / Temporal / Vector 等值编码 | [Values](values.md) |
| Procedure 参数语义、返回值和操作限制 | [Procedures](procedures.md) |
| 完整公开 Procedure 签名与输出列（不信任错误的类型 metadata） | [Procedure Inventory](procedure-inventory.md) |
| 完整内置函数签名、重载、分类、deprecated 状态 | [Functions](functions.md) |
| Cypher profile 的支持范围与排除面 | [Cypher Compatibility](cypher-compatibility.md) |
| C ABI 签名、内存所有权、回调、事务 | [Native API](native-api.md) |
| 稳定错误类别与处理方法 | [Errors](errors.md) |
| 名称、类型、索引配置与运行限制 | [Limits](limits.md) |
| v0.1.0 发布制品与设计之间的已知差异 | [Known Issues](known-issues.md) |

Inventory 来自真实 v0.1.0 `SHOW FUNCTIONS YIELD *` / `SHOW PROCEDURES YIELD *`，不是根据 Neo4j 当前文档推测。重新生成方法见 [示例验证说明](../guide/examples/README.md)。这些 introspection 结果含兼容字段，不表示 Lithograph 实现了 Neo4j 的账号、角色、system database 或权限系统。

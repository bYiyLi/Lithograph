# Procedure CALL 与 YIELD

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

适用于 Lithograph v0.3.1，Cypher Profile `CY25-2026.08`。本章拥有调用语法、行组合和作用域。每个 Procedure 的完整参数、返回、错误与副作用由 [Procedure Reference](../reference/procedures.md) 维护；`CALL { ... }` 语言子查询见 [Subqueries](subqueries.md)。

<a id="procedure-call-syntax"></a>

## 完整调用形式

```text
CALL namespace.name([argumentExpression, ...])
  [YIELD outputName [AS localName] [, outputName [AS localName] ...]
   [WHERE predicate]]
[followingClauses ...]

// 独立调用可导出所有返回列
CALL namespace.name([argumentExpression, ...])
CALL namespace.name([argumentExpression, ...]) YIELD *
```

Procedure 名称是静态 qualified name；参数可以是字面量、参数 `$name`、外部变量或表达式。不能把 Procedure 名称自身作为 `$procedure` 参数。括号中按位置传参，调用方应按该 Procedure 的签名区分必选、可选参数及 options Map。

嵌入更长的 query 时，用显式 YIELD 声明需要的列。独立 Procedure 调用可以省略 YIELD 或使用 YIELD *；嵌入式 `YIELD *` 被拒绝，避免未来增加 Procedure 输出列时隐式改变调用方作用域。

<a id="procedure-yield"></a>

## YIELD、alias 与 WHERE

YIELD 左边必须是 Procedure 声明的输出列名，AS 右边是加入当前行的局部变量名。只导出列出的字段，不会把 Procedure 的其它输出偷偷放入作用域；同名冲突必须通过 AS 避免。YIELD 的 WHERE 在导出和重命名后求值，可用 alias 及原有外部变量。

<!-- example: path-procedure-setup -->
```cypher
CREATE (:Person), (:Project), (:Team)
RETURN 3 AS nodes
```

以下成功例子使用此 setup。

<!-- example: path-procedure-yield -->
```cypher
CALL db.labels() YIELD label AS name
WHERE name STARTS WITH 'P'
RETURN name ORDER BY name
```

结果：`"Person"`、`"Project"`。

<!-- example: path-procedure-standalone -->
```cypher
CALL db.labels()
```

结果列为 `label`，三行分别是 Person、Project、Team；行序不作为合同。

<a id="procedure-cardinality"></a>

## 输入行与输出行

Procedure 针对每个输入行调用一次。每次返回的每行与当前外部行组合；返回零行时，对应外部行消失。Procedure 并不天然只运行一次，尤其在 MATCH 或 UNWIND 后调用写 Procedure 时要注意调用次数。

<!-- example: path-procedure-correlated -->
```cypher
UNWIND ['P','T'] AS prefix
CALL db.labels() YIELD label
WHERE label STARTS WITH prefix
RETURN prefix, label ORDER BY prefix, label
```

结果：`["P","Person"]`、`["P","Project"]`、`["T","Team"]`。需要把 Procedure 结果聚合成一行或限制每次调用的输出，可使用 CALL 子查询加聚合或分页。

对结果分页的 placement 很重要：Procedure 自己的 options（例如检索的 limit）先按其接口执行；外层 ORDER BY/SKIP/LIMIT 再处理 Cypher 行。Graph View 内搜索必须按接口保证先过滤可见候选再分页；不应由应用在拿到结果后自行过滤来替代。

<a id="procedure-discovery"></a>

## 查询可用接口

<!-- example: path-procedure-show -->
```cypher
SHOW PROCEDURES YIELD name, mode
WHERE name = 'db.labels'
RETURN name, mode
```

结果：`["db.labels","READ"]`。SHOW inventory 可用于发现当前扩展的名称、签名与 metadata；完整行为仍应查阅对应版本的 Reference。`mode` 不足以判定外部 I/O，例如某些只读搜索可能调用 Provider 或宿主 tokenizer。

<a id="procedure-errors"></a>

## 参数、null 与错误

- 名称未知、必选参数数量错误、未知 YIELD 字段或冲突 alias 在语义阶段拒绝；code 为 `SEMANTIC_ERROR` 或所属接口的具体错误。
- 参数的 null、空字符串、负整数、options key/类型等由每个 Procedure 定义；不能假定所有 Procedure 都像某个 scalar function 一样传播 null。
- `WHERE` 仅保留 true，false/null 丢弃该输出行；不合法 predicate 类型按类型规则失败。
- 写 Procedure、版本操作、维护操作可能对 active transaction、历史 Snapshot、Graph View 或 streaming 有限制；调用前查所属接口。
- 只读安全选项检查具体 Procedure 及其外部调用能力。未执行的分支、零行上游和 EXPLAIN 都不能隐藏禁止调用。

<!-- example: path-procedure-unknown-yield -->
```cypher
CALL db.labels() YIELD missing RETURN missing
```

结果：`SEMANTIC_ERROR`，Procedure 没有 `missing` 输出列。

<!-- example: path-procedure-embedded-star-error -->
```cypher
CALL db.labels() YIELD * RETURN label
```

结果：`SEMANTIC_ERROR`；使用 `YIELD label` 显式声明。

返回数据中的 Node/Relationship/Temporal 等仍遵守 [Cypher 类型](types.md)，经 SQLite 返回时使用 [Values JSON 编码](../reference/values.md)。一次普通写 execution 的原子性与 Commit 边界见 [Writing](writing.md#write-atomicity)，具体版本管理与维护接口由 Procedure Reference 决定。

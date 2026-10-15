# 语法、名称与参数

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

本文适用于 Lithograph v0.3.1、`CY25-2026.08`。Cypher 文本交给 SQLite 的 `lithograph()` 或 `lithograph_rows()` 执行；SQL 字符串的转义由宿主语言和 SQLite 处理，本文中的字符串是完成 SQL 解码后的 Cypher 字符串。接入方式见 [SQL API](../reference/sql-api.md)。

<a id="notation"></a>

## 阅读语法格式

本手册的 `text` 代码块表示语法模板，不能直接执行。模板中的尖括号表示需要替换的部分，方括号表示可选部分，`...` 表示重复，竖线表示选择；它们只有在注明属于实际 Cypher 语法时才原样写入查询。例如：

```text
RETURN [ALL | DISTINCT] <expression> [AS <alias>], ...
       [ORDER BY <expression> [ASC | DESC], ...]
       [SKIP <integer-expression>] [LIMIT <integer-expression>]
```

`cypher` 代码块是实际示例。示例的参数、前置数据及预期结果在附近说明；需要初始化数据时先执行指定的建图块。一个 SQL 调用接受一个 Cypher query，结尾允许一个分号；不能用分号拼接多个独立 query。

<a id="preamble"></a>

## 版本与执行前缀

```text
[CYPHER 25 [<option>=<value> ...]] [EXPLAIN | PROFILE] <query>
[EXPLAIN | PROFILE] [CYPHER 25 [<option>=<value> ...]] <query>
CYPHER <option>=<value> ... [EXPLAIN | PROFILE] <query>
[EXPLAIN | PROFILE] CYPHER <option>=<value> ... <query>
```

省略版本时仍使用当前冻结的 Cypher 25 Profile。`CYPHER 5` 等其它版本不属于本 Profile。`EXPLAIN` 进行语义检查并返回执行计划；`PROFILE` 真正执行查询并收集统计，包括查询中的写入。两者的输出见 [诊断](diagnostics.md)。

Cypher preamble 可以解析 `runtime=slotted` 一类 query option，其值可以是名称、字符串或数字；这不代表 Lithograph 提供 Neo4j 的 runtime 或 planner 实现。Lithograph 的 Branch、Snapshot、Graph View、资源和安全行为通过宿主 [execution options](../reference/execution-options.md) 设置，不通过文本 preamble 设置。不要把 `CYPHER readOnlyNoExternalIO=true` 当作安全限制。

<!-- example: foundation-syntax-version -->
```cypher
CYPHER 25 RETURN 6 * 7 AS answer;
```

返回 `answer = 42`。

<a id="identifiers"></a>

## 标识符与关键字

变量、Label、Relationship Type、Property Key、索引名和别名都使用标识符。未转义名称以 Unicode identifier start 或 `_` 开始，后续可以包含 Unicode identifier continue、数字或 `_`。关键字不区分大小写，名称区分大小写；`person` 和 `Person` 是两个变量。

包含空格、标点、数字开头或容易与关键字混淆的名称，使用反引号包围。反引号内的一个反引号写成两个反引号。单双引号是字符串，不能代替反引号引用标识符。

<!-- example: foundation-syntax-names -->
```cypher
WITH 7 AS `order total`, '示例' AS 名称
RETURN `order total` AS total, 名称 AS name, {`a``b`: 3} AS object
```

返回 `[7, "示例", {"a`b":3}]`。采用动态输入名称时，优先使用语言支持的动态 Property 或 Label/Type 形式，见 [表达式](expressions.md#property-access) 和 [Pattern](patterns.md)。拼接未经处理的反引号内容仍可能改变查询结构。

<a id="parameters"></a>

## 参数

```text
$<parameter-name>
```

参数名称由一个或多个 Unicode identifier continue 或 `_` 组成，可以以数字开始，例如 `$0limit`。宿主 params 是对象，key 不包含 `$`；`$name` 对应 `{"name":"阿青"}`。相同名称的多次引用取得相同输入值。

<!-- example: foundation-syntax-params -->
```cypher
RETURN $name AS name, $0limit + 1 AS next, $nullable IS NULL AS missing
```

传入 `{"name":"阿青","0limit":2,"nullable":null}`，返回 `["阿青",3,true]`。参数可以为 `null`；未提供一个被查询引用的参数是 `INVALID_ARGUMENT`，与显式传入 null 不同。宿主参数的 tagged values 和禁止传回的图元素类型见 [Values](../reference/values.md)。

参数代表值，不能替换 clause keyword 或任意语法片段。动态属性使用 `n[$key]`，动态 Label/Type 使用其明确语法；将 `$text` 传入字符串参数不会被再次解析成 Cypher。推荐对值使用参数绑定，避免拼接字符串。

<!-- example: foundation-syntax-missing-param -->
```cypher
RETURN $required AS value
```

未提供 `required` 时，返回 `INVALID_ARGUMENT`。

<a id="literals"></a>

## 字面量与字符串转义

| 类别 | 支持形式 | 含义 |
| --- | --- | --- |
| null / Boolean | `null`、`true`、`false` | 关键字不区分大小写 |
| Integer | `42`、`-42`、`0x2A`、`0o52` | 有符号 64 位；十六进制和八进制均可带正负号 |
| Float | `1.0`、`.5`、`2e3`、`-1.2E-3` | 双精度；指数必须有数字 |
| String | `'text'`、`"text"` | 单引号或双引号；普通字符串不进行插值 |
| List | `[]`、`[1, null, 'x']` | 有序值序列；元素可以是表达式 |
| Map | `{}`、`{name:'阿青', 'unit-price':12}` | 字符串 key 到值的映射；key 可为名称或字符串 |

Integer 超过 `-9223372036854775808..9223372036854775807` 返回 `TYPE_ERROR`；需要 Float 时显式写小数或指数。Map 和 List 是运行时值，它们能否保存为 Property 见 [类型](types.md#property-types)。Temporal、Point、Vector 和 UUID 通过构造函数创建，详见 [类型](types.md) 和对应函数章节。

字符串支持 `\t`、`\b`、`\n`、`\r`、`\f`、`\'`、`\"`、`\\`、`\uXXXX` 转义；同种引号也可通过重复两次表示一个引号。这里的反斜杠是 Cypher 本身的字符，放进宿主字符串或 JSON 时还需要宿主层转义。

<!-- example: foundation-syntax-literals -->
```cypher
RETURN 0x2A AS hex, 0o52 AS octal, 2e3 AS exponential,
       'it''s' AS quoted, '\u4E2D\u6587' AS unicode
```

返回 `[42,42,2000.0,"it's","中文"]`。插值字符串的 `s"...{expression}..."` 规则见 [字符串插值](expressions.md#interpolation)。

<a id="comments"></a>

## 空白与注释

空格、制表符、换行可以分隔 token。`//` 从当前位置注释到行尾；`/* ... */` 是块注释。字符串和转义标识符中的这些字符仍是内容。块注释不使用嵌套结构。

<!-- example: foundation-syntax-comments -->
```cypher
// 说明一条查询
WITH 3 AS n /* 中间说明 */
RETURN n + 1 AS value
```

返回 `value = 4`。非法 token、不闭合字符串/注释、不完整 clause 或其它语法组合返回 `PARSE_ERROR`；错误包含可用的位置信息，具体 envelope 见 [Errors](../reference/errors.md)。作用域和类型合法性在解析后继续检查，解析成功并不意味着查询可执行。

相关主题：[行与作用域](query-model.md) · [读取与投影](reading.md) · [表达式](expressions.md)。

<a id="syntax-errors"></a>

## 语法错误与位置

未闭合表达式不能开始 execution，返回 `PARSE_ERROR` 并保留源位置；应用可以用 `lithograph_validate()` 获取结构化的 line/column，见 [SQL validation](../reference/sql-api.md#validation)。

<!-- example: foundation-syntax-error -->
```cypher
RETURN (
```

这是一个预期失败示例，不产生写入或 Commit。

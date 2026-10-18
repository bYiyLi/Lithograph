# 字符串函数

适用版本：Lithograph **v0.3.2** · Cypher Profile **`CY25-2026.08`**。

[Cypher 手册](../index.md) · [完整函数索引](../../reference/functions.md)

本页覆盖运行时 `String` 分类的全部函数与 String overload。签名中的 `::`、方括号和类型是参考记法；调用时使用下面的 Cypher 示例。所有参数按位置传入。

字符定位、截取和逆序按 Unicode scalar value 计数；组合音标可能占多个位置。字符串不自动按 locale 或 normalization form 比较。长度函数 `char_length()`、`character_length()`、`size()` 按注册分类列于 [标量函数](scalar.md)。错误 category 的 SQL 前缀为 `LITHOGRAPH_`。

[Neo4j String functions](https://neo4j.com/docs/cypher-manual/25/functions/string/) 是语义核对来源（核对日期 2026-10-15）；该在线页面继续演进。本手册以冻结 Profile 和当前实现为准，例如不包含 2026.09 后新增的结构值 `toString()` 转换。

## 函数导航

[btrim()](#fn-btrim) | [left()](#fn-left) | [lower()](#fn-lower) | [ltrim()](#fn-ltrim) | [normalize()](#fn-normalize) | [replace()](#fn-replace) | [reverse()](#fn-reverse) | [right()](#fn-right) | [rtrim()](#fn-rtrim) | [split()](#fn-split) | [string.indexOf()](#fn-string-indexof) | [string.join()](#fn-string-join) | [string.regexReplace()](#fn-string-regexreplace) | [substring()](#fn-substring) | [toLower()](#fn-tolower) | [toString()](#fn-tostring) | [toStringOrNull()](#fn-tostringornull) | [toUpper()](#fn-toupper) | [trim()](#fn-trim) | [upper()](#fn-upper)

<a id="fn-btrim"></a>

## btrim()

<!-- function-signatures: btrim -->
```text
btrim(input :: STRING[, trimCharacterString :: STRING]) :: STRING
```
<!-- /function-signatures -->

移除 `input` 两端连续出现的待裁剪字符，返回 String。省略 `trimCharacterString` 时移除 Unicode 空白；提供时把它解释为**字符集合**，不是完整前后缀，内部字符不变。字符集合为空时原样返回。

`input` 或显式第二参数为 `null` 时返回 `null`。非 null 参数必须是 String，否则 `TYPE_ERROR`。与 [trim()](#fn-trim) 的 `BOTH` 形式对应。

<!-- example: fn-btrim -->
```cypher
RETURN [btrim('._报告_.', '._'), btrim('  图  '), btrim('图', null)] AS x
```

结果：`["报告","图",null]`（列 `x` 的值）。

<a id="fn-left"></a>

## left()

<!-- function-signatures: left -->
```text
left(original :: STRING, length :: INTEGER) :: STRING
```
<!-- /function-signatures -->

从 `original` 左端取最多 `length` 个 Unicode 字符，返回 String；长度超过字符串时返回原字符串，`0` 返回空串。`length` 必须是 `0..2147483647` 的 Integer；负数、Float、`null` 或越界值产生 `TYPE_ERROR`。

`original = null` 时返回 `null`，包括 `left(null, null)`。其它非 String 的 `original` 产生 `TYPE_ERROR`。字符计数不是 UTF-8 字节数。

<!-- example: fn-left -->
```cypher
RETURN [left('图谱😀', 2), left('图', 9), left(null, null)] AS x
```

结果：`["图谱","图",null]`（列 `x` 的值）。

<a id="fn-lower"></a>

## lower()

<!-- function-signatures: lower -->
```text
lower(input :: STRING) :: STRING
```
<!-- /function-signatures -->

把 String `input` 转为 Unicode 小写，返回 String；无其它参数或默认值。它与 [toLower()](#fn-tolower) 行为相同，不根据宿主 locale 选择大小写规则。

`null` 返回 `null`，其它非 String 值产生 `TYPE_ERROR`。映射可能改变字符数量；大小写转换不自动执行 Unicode normalization。

<!-- example: fn-lower -->
```cypher
RETURN [lower('ABCÉ'), lower(null)] AS x
```

结果：`["abcé",null]`（列 `x` 的值）。

<a id="fn-ltrim"></a>

## ltrim()

<!-- function-signatures: ltrim -->
```text
ltrim(input :: STRING[, trimCharacterString :: STRING]) :: STRING
```
<!-- /function-signatures -->

仅移除 String `input` 左端连续的字符。第二参数省略时裁剪 Unicode 空白，提供时按 `trimCharacterString` 中的字符集合裁剪；右端、内部字符和原顺序保留，空集合不裁剪。

返回 String。任一参数为 `null` 返回 `null`；其余非 String 参数产生 `TYPE_ERROR`。也可用 `trim(LEADING … FROM …)` 表达。

<!-- example: fn-ltrim -->
```cypher
RETURN [ltrim('._图_.', '._'), ltrim('  图  '), ltrim(null)] AS x
```

结果：`["图_.","图  ",null]`（列 `x` 的值）。

<a id="fn-normalize"></a>

## normalize()

<!-- function-signatures: normalize -->
```text
normalize(input :: STRING [, normalForm = NFC :: [NFC, NFD, NFKC, NFKD]]) :: STRING
```
<!-- /function-signatures -->

按 Unicode normalization form 规范化 String `input`，返回 String。`normalForm` 省略时为 `NFC`；调用中可使用 keyword `NFC`、`NFD`、`NFKC`、`NFKD`。`NFC` 合成规范等价字符，`NFD` 分解；带 `K` 的形式另外应用兼容等价转换。

字符串比较不会自动规范化；需要统一文本表示时，在入库或比较前显式调用。任一参数为 `null` 返回 `null`。非 String 输入产生 `TYPE_ERROR`，不支持的 normalization form 产生 `SEMANTIC_ERROR`。

<!-- example: fn-normalize -->
```cypher
RETURN [normalize('é'), normalize('Ａ', NFKC), normalize(null)] AS x
```

结果：`["é","A",null]`（列 `x` 的值）。

<a id="fn-replace"></a>

## replace()

<!-- function-signatures: replace -->
```text
replace(original :: STRING, search :: STRING, replace :: STRING[, limit :: INTEGER]) :: STRING
```
<!-- /function-signatures -->

把 String `original` 中从左向右、互不重叠的字面 `search` 替换为 String `replace`，返回 String。`search` 不是 regex；不存在时返回原值。省略 `limit` 时替换全部，显式 `limit` 为非负 Integer，表示最多替换次数；`0` 不替换。空 `search` 匹配字符间边界。

任一已提供参数为 `null` 返回 `null`。前三个非 null 参数必须为 String；非法 `limit` 或其它类型产生 `TYPE_ERROR`。需要 regex 时使用 [string.regexReplace()](#fn-string-regexreplace)。

<!-- example: fn-replace -->
```cypher
RETURN [replace('a-a-a', 'a', 'b', 2), replace('abc', 'x', 'y'), replace('a', 'a', 'b', 0)] AS x
```

结果：`["b-b-a","abc","a"]`（列 `x` 的值）。

<a id="fn-reverse"></a>

## reverse()

<!-- function-signatures: reverse -->
```text
reverse(input :: STRING) :: STRING
```
<!-- /function-signatures -->

返回 String `input` 的 Unicode 字符逆序。无其它参数，空字符串仍为空字符串；`null` 返回 `null`。List 输入使用 [List overload](list.md#fn-reverse)；其它类型产生 `TYPE_ERROR`。

逆序单位是 Unicode scalar value，不是显示字形簇；组合音标或多个 codepoint 组成的 emoji 可能被拆开。

<!-- example: fn-string-reverse -->
```cypher
RETURN [reverse('😀图谱'), reverse(''), reverse(null)] AS x
```

结果：`["谱图😀","",null]`（列 `x` 的值）。

<a id="fn-right"></a>

## right()

<!-- function-signatures: right -->
```text
right(original :: STRING, length :: INTEGER) :: STRING
```
<!-- /function-signatures -->

从 String `original` 右端取最多 `length` 个 Unicode 字符，返回 String；`0` 返回空串，超过长度返回完整原值。`length` 必须是 `0..2147483647` 的 Integer，否则产生 `TYPE_ERROR`；它不能为 `null`。

`original = null` 时返回 `null`，包括 `right(null, null)`。其余非 String 原值产生 `TYPE_ERROR`。

<!-- example: fn-right -->
```cypher
RETURN [right('图谱😀', 2), right('图', 9), right(null, null)] AS x
```

结果：`["谱😀","图",null]`（列 `x` 的值）。

<a id="fn-rtrim"></a>

## rtrim()

<!-- function-signatures: rtrim -->
```text
rtrim(input :: STRING[, trimCharacterString :: STRING]) :: STRING
```
<!-- /function-signatures -->

仅移除 String `input` 右端连续的字符。省略 `trimCharacterString` 时裁剪 Unicode 空白，提供时按其字符集合裁剪；左端与内部字符不变。空集合保留原值。

返回 String。任一参数为 `null` 返回 `null`；其它类型产生 `TYPE_ERROR`。也可写作 `trim(TRAILING … FROM …)`。

<!-- example: fn-rtrim -->
```cypher
RETURN [rtrim('._图_.', '._'), rtrim('  图  '), rtrim(null)] AS x
```

结果：`["._图","  图",null]`（列 `x` 的值）。

<a id="fn-split"></a>

## split()

<!-- function-signatures: split -->
```text
split(original :: STRING, splitDelimiters :: STRING | LIST<STRING>) :: LIST<STRING>
```
<!-- /function-signatures -->

按一个 String 分隔符或 `LIST<STRING>` 中任一字面分隔符，拆分 String `original`，返回 `LIST<STRING>`。分隔符本身不进入结果，连续分隔符及首尾分隔符保留相应空字符串。多分隔符从当前扫描位置选择最早的匹配；它们不是 regex。

空分隔符列表返回只含原值的一项；分隔符集合中存在空串时按 Unicode 字符拆分，空输入对应 `['']`。任一外层参数为 `null` 返回 `null`。非 String 原值、非法分隔符类型或分隔符列表中的 null/非 String 项产生 `TYPE_ERROR`。

<!-- example: fn-split -->
```cypher
RETURN [split('a,b;c,', [',', ';']), split('图😀', ''), split('text', [])] AS x
```

结果：`[["a","b","c",""],["图","😀"],["text"]]`（列 `x` 的值）。

<a id="fn-string-indexof"></a>

## string.indexOf()

<!-- function-signatures: string.indexOf -->
```text
string.indexOf(input :: STRING, value :: STRING) :: INTEGER
```
<!-- /function-signatures -->

在 String `input` 中寻找字面 String `value` 的第一次出现，返回从 `0` 开始的 Unicode 字符位置 Integer；找不到返回 `-1`，空 `value` 返回 `0`。

两个参数都必需，没有起始偏移参数。任一参数为 `null` 返回 `null`，其余非 String 参数产生 `TYPE_ERROR`。大小写和 Unicode 表示必须精确匹配。

<!-- example: fn-string-indexof -->
```cypher
RETURN [string.indexOf('图😀x', 'x'), string.indexOf('abc', 'q'), string.indexOf('abc', '')] AS x
```

结果：`[2,-1,0]`（列 `x` 的值）。

<a id="fn-string-join"></a>

## string.join()

<!-- function-signatures: string.join -->
```text
string.join(input :: LIST<STRING>, delimiter :: STRING) :: STRING
```
<!-- /function-signatures -->

把 `input` 列表中的 String 项按原顺序用 String `delimiter` 连接，返回 String。两个参数都必需；空列表返回 `''`，空分隔符直接拼接。列表内 `null` 项被忽略，不为它额外加入分隔符。

外层 `input` 或 `delimiter` 为 `null` 返回 `null`。非 List 的 input、非 String delimiter 或列表内非 String/非 null 项产生 `TYPE_ERROR`；数字需要先显式转换。

<!-- example: fn-string-join -->
```cypher
RETURN [string.join(['甲', null, '乙'], '/'), string.join([], ','), string.join(null, ',')] AS x
```

结果：`["甲/乙","",null]`（列 `x` 的值）。

<a id="fn-string-regexreplace"></a>

## string.regexReplace()

<!-- function-signatures: string.regexReplace -->
```text
string.regexReplace(original :: STRING, regex :: STRING, replacement :: STRING) :: STRING
```
<!-- /function-signatures -->

以 regex `regex` 匹配 String `original`，把全部互不重叠匹配替换为 String `replacement`。返回 String；无匹配时返回原值，三个参数都必需。replacement 支持 `$1`、`${name}` 等捕获组引用；后面紧接普通字符时使用 `${1}` 明确组边界，`$$` 表示字面 `$`。

任一参数为 `null` 返回 `null`；其它非 String 值产生 `TYPE_ERROR`。不合法的 regex 产生 `SEMANTIC_ERROR`。当前引擎的 regex 支持 Unicode、字符类、分组与重复，不支持 look-around 或 regex 内反向引用；不可假定所有 Java regex 都可用。regex 中的反斜杠还需通过 Cypher 字符串转义。

<!-- example: fn-string-regexreplace -->
```cypher
RETURN string.regexReplace('id=12;id=7', '([0-9]+)', '[$1]') AS x
```

结果：`"id=[12];id=[7]"`（列 `x` 的值）。

<a id="fn-substring"></a>

## substring()

<!-- function-signatures: substring -->
```text
substring(original :: STRING, start :: INTEGER[, length :: INTEGER]) :: STRING
```
<!-- /function-signatures -->

从 String `original` 的 Unicode 字符位置 `start` 开始截取，返回 String。位置从 `0` 开始；省略 `length` 时取到末尾，提供时最多取该数量。`start` 超过长度或 `length = 0` 返回空串。

`start` 与显式 `length` 都必须是 `0..2147483647` 的 Integer；null、负数、Float 或超界产生 `TYPE_ERROR`。`original = null` 时返回 `null`；其余非 String 原值产生 `TYPE_ERROR`。

<!-- example: fn-substring -->
```cypher
RETURN [substring('甲乙丙丁', 1, 2), substring('abc', 9), substring(null, 0)] AS x
```

结果：`["乙丙","",null]`（列 `x` 的值）。

<a id="fn-tolower"></a>

## toLower()

<!-- function-signatures: toLower -->
```text
toLower(input :: STRING) :: STRING
```
<!-- /function-signatures -->

把 String `input` 转为 Unicode 小写，返回 String；与 [lower()](#fn-lower) 是同一行为，无额外参数。`null` 返回 `null`，其它非 String 值产生 `TYPE_ERROR`。

它不隐式执行 trim 或 normalization，适合在明确需要忽略大小写的应用规则中与这些函数组合。

<!-- example: fn-tolower -->
```cypher
RETURN [toLower('MiXeD'), toLower(''), toLower(null)] AS x
```

结果：`["mixed","",null]`（列 `x` 的值）。

<a id="fn-tostring"></a>

## toString()

<!-- function-signatures: toString -->
```text
toString(input :: ANY) :: STRING
```
<!-- /function-signatures -->

把一个可转换值转为 String。String 保持原值；Boolean 使用 `true`/`false`，Integer/Float 使用数值文本；Temporal/Duration 使用对应文本表示，UUID 使用标准 UUID 文本，Point 使用带 SRID 与坐标的文本。`null` 返回 `null`。无其它参数或默认值。

本 Profile 不接受 List、Map、Node、Relationship、Path 或 Vector 作为可转换值，它们产生 `TYPE_ERROR`；签名中的 `ANY` 仍要求具体类型校验。宽容转换使用 [toStringOrNull()](#fn-tostringornull)。这不是 JSON 序列化接口，宿主值编码见 [Values](../../reference/values.md)。

<!-- example: fn-tostring -->
```cypher
RETURN [toString(12), toString(true), toString(date('2026-10-01')), toString(uuid('550e8400-e29b-41d4-a716-446655440000')), toString(null)] AS x
```

结果：`["12","true","2026-10-01","550e8400-e29b-41d4-a716-446655440000",null]`（列 `x` 的值）。

<a id="fn-tostringornull"></a>

## toStringOrNull()

<!-- function-signatures: toStringOrNull -->
```text
toStringOrNull(input :: ANY) :: STRING
```
<!-- /function-signatures -->

与 [toString()](#fn-tostring) 使用同一转换规则，返回 String 或 `null`。对于本 Profile 不支持转换的 List、Map、图元素、Path 或 Vector，返回 `null`；输入 `null` 也返回 `null`。

只接受一个参数。它处理当前值的转换失败，不屏蔽参数表达式求值时的错误，例如除零或未知变量。需要保留失败原因时使用严格的 `toString()`。

<!-- example: fn-tostringornull -->
```cypher
RETURN [toStringOrNull(3), toStringOrNull({a: 1}), toStringOrNull([1]), toStringOrNull(null)] AS x
```

结果：`["3",null,null,null]`（列 `x` 的值）。

<a id="fn-toupper"></a>

## toUpper()

<!-- function-signatures: toUpper -->
```text
toUpper(input :: STRING) :: STRING
```
<!-- /function-signatures -->

把 String `input` 转为 Unicode 大写，返回 String；与 [upper()](#fn-upper) 行为一致，无额外参数。`null` 返回 `null`，其余非 String 值产生 `TYPE_ERROR`。

转换可能使字符数增加；例如 `ß` 的大写结果包含两个字符。它不按宿主 locale 分支，也不自动进行 normalization。

<!-- example: fn-toupper -->
```cypher
RETURN [toUpper('abcß'), toUpper(null)] AS x
```

结果：`["ABCSS",null]`（列 `x` 的值）。

<a id="fn-trim"></a>

## trim()

<!-- function-signatures: trim -->
```text
trim([[LEADING | TRAILING | BOTH] [trimCharacterString :: STRING] FROM] input :: STRING) :: STRING
```
<!-- /function-signatures -->

裁剪 String `input` 的两端或指定一端，返回 String。支持的调用形式为：

```text
trim(input)
trim([LEADING | TRAILING | BOTH] [trimCharacterString] FROM input)
```

省略方向时为 `BOTH`；省略字符集合时裁剪 Unicode 空白。`trimCharacterString` 是要移除的字符集合，空字符串表示不裁剪；裁剪停止于首个不在集合中的字符，内部字符不变。

输入或显式字符集合为 `null` 返回 `null`。这些值的非 null 类型必须是 String，否则 `TYPE_ERROR`。方向是语法 keyword，不是参数字符串。

<!-- example: fn-trim -->
```cypher
RETURN [trim('  图  '), trim(LEADING '._' FROM '._图_.'), trim(TRAILING '._' FROM '._图_.')] AS x
```

结果：`["图","图_.","._图"]`（列 `x` 的值）。

<a id="fn-upper"></a>

## upper()

<!-- function-signatures: upper -->
```text
upper(input :: STRING) :: STRING
```
<!-- /function-signatures -->

把 String `input` 转为 Unicode 大写，返回 String；与 [toUpper()](#fn-toupper) 对应，无其它参数。`null` 返回 `null`，其余类型产生 `TYPE_ERROR`。

原值为空串时结果仍为空串；大小写转换可能扩展字符数，不等价于按字节逐一变更。

<!-- example: fn-upper -->
```cypher
RETURN [upper('abcé'), upper(''), upper(null)] AS x
```

结果：`["ABCÉ","",null]`（列 `x` 的值）。

<a id="error-examples"></a>

## 失败示例

以下示例各自在已初始化的空数据库执行；预期为指定错误 category，宿主应检查 SQLite 返回码和 `LITHOGRAPH_` 前缀。

原字符串非 null 时，长度不能为 null。

<!-- example: fn-left-null-length-error -->
```cypher
RETURN left('图', null)
```

预期：`TYPE_ERROR`。

substring 不接受负起始位置。

<!-- example: fn-substring-negative-error -->
```cypher
RETURN substring('图谱', -1)
```

预期：`TYPE_ERROR`。

replace 的 limit 必须是非负 Integer。

<!-- example: fn-replace-limit-error -->
```cypher
RETURN replace('a', 'a', 'b', -1)
```

预期：`TYPE_ERROR`。

分隔符列表不能含 null 项。

<!-- example: fn-split-invalid-element-error -->
```cypher
RETURN split('a,b', [',', null])
```

预期：`TYPE_ERROR`。

不合法的 regex 在执行时返回语义错误。

<!-- example: fn-regex-pattern-error -->
```cypher
RETURN string.regexReplace('ab', '(', 'x')
```

预期：`SEMANTIC_ERROR`。

冻结 Profile 不支持把 Map 转成 String。

<!-- example: fn-tostring-container-error -->
```cypher
RETURN toString({a: 1})
```

预期：`TYPE_ERROR`。

## 相关主题

[表达式](../expressions.md) · [类型](../types.md) · [null 与排序](../null-and-ordering.md) · [错误参考](../../reference/errors.md)

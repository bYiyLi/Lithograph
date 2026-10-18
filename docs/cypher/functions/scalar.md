# 标量与图元素函数

适用版本：Lithograph **v0.3.2**；Cypher Profile：`CY25-2026.08`。

[语言手册](../index.md) · [全部函数索引](../../reference/functions.md)

本页按公开函数名称说明容器度量、图元素、值转换和 UUID。名字不区分 ASCII 大小写，下面保留 `SHOW FUNCTIONS` 的 canonical spelling。所有示例从独立、已初始化的空图开始；CREATE 示例会写入该临时图。返回表中的 `null` 是值，文字 `'NULL'` 是 String。

参数数量错误通常在解析/语义检查阶段失败；下文的 `TYPE_ERROR` 表示已经求值但类型不合法。完整错误与宿主识别方式见 [Errors](../../reference/errors.md)。`PROPERTY_EXISTS` 是语言 predicate，见 [表达式](../expressions.md)；`vector()` 构造器见 [Vector 函数](vector.md#fn-vector)。

## 目录

- [`cardinality()`](#fn-cardinality)
- [`char_length()`](#fn-char_length)
- [`character_length()`](#fn-character_length)
- [`coalesce()`](#fn-coalesce)
- [`db.nameFromElementId()`](#fn-db-namefromelementid)
- [`elementId()`](#fn-elementid)
- [`endNode()`](#fn-endnode)
- [`file()`](#fn-file)
- [`head()`](#fn-head)
- [`id()`](#fn-id)
- [`last()`](#fn-last)
- [`length()`](#fn-length)
- [`linenumber()`](#fn-linenumber)
- [`nullIf()`](#fn-nullif)
- [`path_length()`](#fn-path_length)
- [`properties()`](#fn-properties)
- [`randomUUID()`](#fn-randomuuid)
- [`size()`](#fn-size)
- [`startNode()`](#fn-startnode)
- [`toBoolean()`](#fn-toboolean)
- [`toBooleanOrNull()`](#fn-tobooleanornull)
- [`toFloat()`](#fn-tofloat)
- [`toFloatOrNull()`](#fn-tofloatornull)
- [`toInteger()`](#fn-tointeger)
- [`toIntegerOrNull()`](#fn-tointegerornull)
- [`type()`](#fn-type)
- [`uuid()`](#fn-uuid)
- [`uuid.leastSignificantBits()`](#fn-uuid-leastsignificantbits)
- [`uuid.mostSignificantBits()`](#fn-uuid-mostsignificantbits)
- [`valueType()`](#fn-valuetype)

<a id="fn-cardinality"></a>

## `cardinality()`

<!-- function-signatures: cardinality -->
```text
cardinality(input :: MAP | LIST<ANY> | PATH) :: INTEGER
```
<!-- /function-signatures -->

计算容器中的项数。`input` 接受 Map、List 或 Path，返回 Integer：Map 计算 key 数，List 计算元素数（包括 `null`），Path 计算节点和关系的总数，因此一跳路径的值为 `3`。空 Map/List 返回 `0`。

`cardinality(null)` 返回 `null`；String、Vector 和其它不支持的输入产生 `TYPE_ERROR`。路径的跳数用 [`length()`](#fn-length)，字符串长度用 [`char_length()`](#fn-char_length) 或 [`size()`](#fn-size)。

<!-- example: fn-cardinality -->
```cypher
RETURN cardinality({a:1,b:2}) AS fields, cardinality([1,null,3]) AS entries
```

预期列：`fields, entries`；预期 rows：`[[2,3]]`。


<a id="fn-char_length"></a>

## `char_length()`

<!-- function-signatures: char_length -->
```text
char_length(input :: STRING) :: INTEGER
```
<!-- /function-signatures -->

返回 `input` 的 Unicode 字符数，输入为 String，结果为 Integer。这里计算 Unicode scalar value，不计算 UTF-8 字节数，也不按用户感知的字形簇合并字符：`e` 加组合重音占两个字符，`😀` 占一个。

空串返回 `0`，`null` 返回 `null`；其它类型产生 `TYPE_ERROR`。与 [`character_length()`](#fn-character_length) 相同。

<!-- example: fn-char_length -->
```cypher
RETURN char_length('图😀é') AS length
```

预期列：`length`；预期 rows：`[[4]]`。


<a id="fn-character_length"></a>

## `character_length()`

<!-- function-signatures: character_length -->
```text
character_length(input :: STRING) :: INTEGER
```
<!-- /function-signatures -->

`char_length()` 的同义函数。接受一个 String，返回 Unicode 字符数 Integer；组合字符分别计数，空串为 `0`，`null` 为 `null`，非 String 输入产生 `TYPE_ERROR`。

用于希望代码接近 GQL 标准拼写的查询；与 `size()` 不同，本函数不接受 List 或 Vector。

<!-- example: fn-character_length -->
```cypher
RETURN character_length('北京') AS length
```

预期列：`length`；预期 rows：`[[2]]`。


<a id="fn-coalesce"></a>

## `coalesce()`

<!-- function-signatures: coalesce -->
```text
coalesce(input :: ANY) :: ANY
```
<!-- /function-signatures -->

按参数从左至右选择第一个非 `null` 的值并返回原值，保留其类型。它接受一个或多个表达式；全部为 `null` 时返回 `null`。空串、`false`、`0`、空 List 和空 Map 都是非 null 值，不会触发后备项。

实际调用形式为 `coalesce(expression [, expression ...])`；`SHOW FUNCTIONS` 的单个 `input` 字段概括其可变参数。零参数产生 `SEMANTIC_ERROR`。各参数仍须是合法表达式；不要用后备项掩盖其它表达式的类型错误。常用于缺失属性的显示默认值。

<!-- example: fn-coalesce -->
```cypher
RETURN coalesce(null, '未命名', '备用') AS name
```

预期列：`name`；预期 rows：`[["未命名"]]`。


<a id="fn-db-namefromelementid"></a>

## `db.nameFromElementId()`

<!-- function-signatures: db.nameFromElementId -->
```text
db.nameFromElementId(elementId :: STRING) :: STRING
```
<!-- /function-signatures -->

从 Node 或 Relationship 的 element ID 字符串取出数据库名。Lithograph 的一个 SQLite `main` database 对应一个图；当前 `n:` / `r:` element ID 返回 `main`，这不会选择或切换其它数据库、Branch 或 Commit。

`elementId` 必须为 String。`null` 或不属于上述 element ID 前缀的 String 返回 `null`；其它类型产生 `TYPE_ERROR`。此函数只解析标识符形式，不执行元素存在性检查。版本选择使用宿主 execution options。

<!-- example: fn-db-namefromelementid -->
```cypher
CREATE (n) RETURN db.nameFromElementId(elementId(n)) AS database
```

预期列：`database`；预期 rows：`[["main"]]`。


<a id="fn-elementid"></a>

## `elementId()`

<!-- function-signatures: elementId -->
```text
elementId(input :: NODE | RELATIONSHIP) :: STRING
```
<!-- /function-signatures -->

返回 Node 或 Relationship 的 String 标识。Node 使用 `n:<本地整数>`，Relationship 使用 `r:<本地整数>`；调用方应将结果视为元素标识符，使用参数进行相等比较，不从文本计算业务含义。

参数 `input` 为 Node/Relationship；`null` 返回 `null`，其它类型产生 `TYPE_ERROR`。结果只标识元素，不指定 Snapshot；同一元素在不同 Commit 的属性可能不同。跨数据库的业务键应使用显式属性和 Constraint。下面的 `n:1` 依赖示例从新初始化的空图开始。

<!-- example: fn-elementid -->
```cypher
CREATE (n:Person {name:'Ada'}) RETURN elementId(n) AS id
```

预期列：`id`；预期 rows：`[["n:1"]]`。


<a id="fn-endnode"></a>

## `endNode()`

<!-- function-signatures: endNode -->
```text
endNode(input :: RELATIONSHIP) :: NODE
```
<!-- /function-signatures -->

返回 Relationship 按存储方向指向的终点 Node。`input` 为 Relationship，返回的 Node 在当前查询 Snapshot 中读取；即使 MATCH 用反向箭头或无向模式匹配，终点仍由关系创建时的方向决定。

`null` 返回 `null`；其它类型产生 `TYPE_ERROR`。可继续访问其属性、Label 或 `elementId()`，与 [`startNode()`](#fn-startnode) 配合检查两端。

<!-- example: fn-endnode -->
```cypher
CREATE (a:Person {name:'Ada'})-[r:KNOWS]->(b:Person {name:'Lin'}) RETURN endNode(r).name AS name
```

预期列：`name`；预期 rows：`[["Lin"]]`。


<a id="fn-file"></a>

## `file()`

<!-- function-signatures: file -->
```text
file() :: STRING
```
<!-- /function-signatures -->

无参数，返回当前 `LOAD CSV` 行的本地来源文件路径 String。`file://` 来源返回规范化本地路径；HTTP(S) 来源返回该次加载所用的本地临时文件路径，不能据此假设输入 URL 或临时文件会长期存在。

没有活动 CSV 行 context 时返回 `null`，包括下面的独立 RETURN。CSV context 随行经过 `WITH` 和关联子查询传播；多个/嵌套加载取最近的活动 context。它只读取加载元信息，本身不打开文件或发起网络请求。完整导入流程见 [LOAD CSV](../load-csv.md)。

<!-- example: fn-file -->
```cypher
RETURN file() AS source
```

预期列：`source`；预期 rows：`[[null]]`。


<a id="fn-head"></a>

## `head()`

<!-- function-signatures: head -->
```text
head(list :: LIST<ANY>) :: ANY
```
<!-- /function-signatures -->

返回 List 的第一个元素，原样保留元素类型。`list` 可以是任意元素的 List；第一个元素恰为 `null` 时返回 `null`，不会寻找后续非 null 元素。

空 List 或 `null` 返回 `null`；非 List 参数产生 `TYPE_ERROR`。若输入来自 `collect()`，需要先保证输入行的顺序，才可赋予“第一个”业务含义。

<!-- example: fn-head -->
```cypher
RETURN head([null,1]) AS first, head([]) AS empty
```

预期列：`first, empty`；预期 rows：`[[null,null]]`。


<a id="fn-id"></a>

## `id()`

<!-- function-signatures: id -->
```text
id(input :: NODE | RELATIONSHIP) :: INTEGER
```
<!-- /function-signatures -->

返回 Node/Relationship 的本地整数 ID。此函数已 deprecated，新增接入使用 [`elementId()`](#fn-elementid)，不要把内部整数当作跨数据库业务键。Node 与 Relationship 有各自的标识空间。

`input` 为 Node/Relationship，`null` 返回 `null`；其它类型产生 `TYPE_ERROR`。本地 committed ID 不复用，但 ID 不编码 Branch/Snapshot。下面的 `1` 只适用于新初始化的空图。

<!-- example: fn-id -->
```cypher
CREATE (n) RETURN id(n) AS id
```

预期列：`id`；预期 rows：`[[1]]`。


<a id="fn-last"></a>

## `last()`

<!-- function-signatures: last -->
```text
last(list :: LIST<ANY>) :: ANY
```
<!-- /function-signatures -->

返回 List 的最后一个元素，保留原类型；末尾是 `null` 时就返回 `null`。参数 `list` 为 List，空 List 和 `null` 均返回 `null`，其它类型产生 `TYPE_ERROR`。

与 `head()` 一样，来自聚合或查询的 List 只有在输入顺序明确时，才有稳定的业务“最后一项”。

<!-- example: fn-last -->
```cypher
RETURN last([1,2,3]) AS last
```

预期列：`last`；预期 rows：`[[3]]`。


<a id="fn-length"></a>

## `length()`

<!-- function-signatures: length -->
```text
length(input :: PATH) :: INTEGER
```
<!-- /function-signatures -->

计算 Path 中 Relationship 的数量，即路径跳数，返回 Integer。零跳路径返回 `0`；它与 `nodes(path)` 的长度相差一，不是节点和关系的合计。

参数 `input` 必须是 Path；`null` 返回 `null`，String/List 等输入产生 `TYPE_ERROR`。String/List 的长度用 `size()`，容器及路径元素总数用 `cardinality()`。

<!-- example: fn-length -->
```cypher
CREATE p=(:A)-[:R]->(:B) RETURN length(p) AS hops
```

预期列：`hops`；预期 rows：`[[1]]`。


<a id="fn-linenumber"></a>

## `linenumber()`

<!-- function-signatures: linenumber -->
```text
linenumber() :: INTEGER
```
<!-- /function-signatures -->

无参数，返回当前 CSV 逻辑记录在源中的一基序号 Integer。WITH HEADERS 的标题记录占第 `1` 条，第一条数据记录为 `2`；引号字段中的换行不会使下一条记录跳号。

没有活动 CSV 行 context 时返回 `null`。它与 [`file()`](#fn-file) 使用相同的行级 context，适合在导入时保存来源和定位错误记录；多个/嵌套 LOAD CSV 读取最近加载的 context。

<!-- example: fn-linenumber -->
```cypher
RETURN linenumber() AS line
```

预期列：`line`；预期 rows：`[[null]]`。


<a id="fn-nullif"></a>

## `nullIf()`

<!-- function-signatures: nullIf -->
```text
nullIf(v1 :: ANY, v2 :: ANY) :: ANY
```
<!-- /function-signatures -->

比较 `v1` 和 `v2`：Cypher 相等判断为 `true` 时返回 `null`，否则返回 `v1` 的原值及原类型。常用于把特定哨兵值转换为 null。

比较使用普通 Cypher 值语义：`nullIf(1,1.0)` 为 `null`；`nullIf(10,null)` 的相等判断是未知，因此返回 `10`。首参数为 null 的结果仍为 null。参数类型不要求相同；嵌套值比较遵循 [null 与比较](../null-and-ordering.md)。

<!-- example: fn-nullif -->
```cypher
RETURN nullIf(10,10) AS equal, nullIf(10,null) AS unknown
```

预期列：`equal, unknown`；预期 rows：`[[null,10]]`。


<a id="fn-path_length"></a>

## `path_length()`

<!-- function-signatures: path_length -->
```text
path_length(input :: PATH) :: INTEGER
```
<!-- /function-signatures -->

`length()` 的同义函数，接受一个 Path，返回其关系数量 Integer。它对零跳路径返回 `0`，对 `null` 返回 `null`，其它类型产生 `TYPE_ERROR`。

路径模式的量化、重复和选择规则会改变 Path 的内容；本函数只统计已经产生的 Path，不改变路径匹配方式。

<!-- example: fn-path_length -->
```cypher
CREATE p=(:A)-[:R]->(:B) RETURN path_length(p) AS hops
```

预期列：`hops`；预期 rows：`[[1]]`。


<a id="fn-properties"></a>

## `properties()`

<!-- function-signatures: properties -->
```text
properties(input :: NODE | RELATIONSHIP | MAP) :: MAP
```
<!-- /function-signatures -->

把 Node 或 Relationship 的属性转换为 Map；Map 输入返回同一组 key/value 内容。它保留属性的原始值类型，不把图元素的 Label、relationship type 或 element ID 加入结果。

`input` 为 Node、Relationship 或 Map；空属性返回 `{}`，`null` 返回 `null`，其它类型产生 `TYPE_ERROR`。返回 Map 是当前查询 Snapshot 中的值；修改返回 Map 表达式不会隐式写回元素，持久写入须使用 SET。

<!-- example: fn-properties -->
```cypher
RETURN properties({name:'Ada',score:2}) AS properties
```

预期列：`properties`；预期 rows：`[[{"name":"Ada","score":2}]]`。


<a id="fn-randomuuid"></a>

## `randomUUID()`

<!-- function-signatures: randomUUID -->
```text
randomUUID() :: STRING
```
<!-- /function-signatures -->

无参数，返回随机 UUID v4 的规范小写 String，格式为 `8-4-4-4-12` 个十六进制字符，总长 `36`。每次调用使用随机源；结果不能作为可重复测试的固定文本。

需要 UUID 类型时使用 [`uuid()`](#fn-uuid)；`randomUUID()` 适合既有 String 属性/接口。不要依赖两次调用相等。无 `null` 参数形式；传参数属于参数数量错误。下面用长度检查演示，避免把随机结果写成固定期望。

<!-- example: fn-randomuuid -->
```cypher
RETURN size(randomUUID()) AS length
```

预期列：`length`；预期 rows：`[[36]]`。


<a id="fn-size"></a>

## `size()`

<!-- function-signatures: size -->
```text
size(input :: STRING | LIST<ANY> | VECTOR) :: INTEGER
```
<!-- /function-signatures -->

接受 String、List 或 Vector，分别返回 Unicode 字符数、元素数或维度数 Integer。String 按 Unicode scalar value 计数而非字节/字形簇；List 中的 null 也占一项。

空 String/List 返回 `0`，`null` 返回 `null`，Map、Path 等输入产生 `TYPE_ERROR`。Map 的键数用 `cardinality()`，Path 的跳数用 `length()`。

<!-- example: fn-size -->
```cypher
RETURN size('图😀') AS characters, size([1,null,3]) AS entries
```

预期列：`characters, entries`；预期 rows：`[[2,3]]`。


<a id="fn-startnode"></a>

## `startNode()`

<!-- function-signatures: startNode -->
```text
startNode(input :: RELATIONSHIP) :: NODE
```
<!-- /function-signatures -->

返回 Relationship 按存储方向起始的 Node。无向或反向 MATCH 不会调换这个起点；结果在当前查询 Snapshot 中读取，可继续访问 Node 属性或调用图元素函数。

`input` 必须是 Relationship；`null` 返回 `null`，其它类型产生 `TYPE_ERROR`。与 [`endNode()`](#fn-endnode) 配合得到有向关系的两端。

<!-- example: fn-startnode -->
```cypher
CREATE (a:Person {name:'Ada'})-[r:KNOWS]->(b:Person {name:'Lin'}) RETURN startNode(r).name AS name
```

预期列：`name`；预期 rows：`[["Ada"]]`。


<a id="fn-toboolean"></a>

## `toBoolean()`

<!-- function-signatures: toBoolean -->
```text
toBoolean(input :: BOOLEAN | STRING | INTEGER) :: BOOLEAN
```
<!-- /function-signatures -->

将 Boolean 原样返回；Integer 的 `0` 转为 `false`，非零转为 `true`；String 按不区分大小写的 `true` / `false` 转换。String 不自动去除前后空白，不能识别的 String 返回 `null`。

`null` 返回 `null`。Float、List、Map、图元素等不支持类型产生 `TYPE_ERROR`，不会按“是否为空”转换；若需要容错转换，使用 `toBooleanOrNull()`。

<!-- example: fn-toboolean -->
```cypher
RETURN toBoolean('TRUE') AS yes, toBoolean(0) AS no, toBoolean('unknown') AS invalid
```

预期列：`yes, no, invalid`；预期 rows：`[[true,false,null]]`。


<a id="fn-tobooleanornull"></a>

## `toBooleanOrNull()`

<!-- function-signatures: toBooleanOrNull -->
```text
toBooleanOrNull(input :: ANY) :: BOOLEAN
```
<!-- /function-signatures -->

按照 `toBoolean()` 的规则转换 Boolean、Integer、String，同时将不支持的类型转换为 `null`。因此 Float `1.5` 不会转为 true，而是返回 null。

`input` 接受任意运行时值；`null`、无法识别的 String 及不可转换类型均返回 `null`。只有调用本身的语法、参数数量或求参表达式错误仍会失败。

<!-- example: fn-tobooleanornull -->
```cypher
RETURN toBooleanOrNull(1.5) AS invalid
```

预期列：`invalid`；预期 rows：`[[null]]`。


<a id="fn-tofloat"></a>

## `toFloat()`

<!-- function-signatures: toFloat -->
```text
toFloat(input :: STRING | INTEGER | FLOAT) :: FLOAT
```
<!-- /function-signatures -->

将 Integer 转为 Float，将 Float 原样返回；String 去除两端空白后解析数值，无法解析时返回 `null`。Integer 转换可能因 IEEE 754 Float 的精度而舍入，大整数不保证精确。

`null` 返回 `null`；Boolean、List、Map、图元素等不支持类型产生 `TYPE_ERROR`。非有限 Float 的宿主 JSON 表示见 [值编码](../../reference/values.md)。需要容错接受任意类型时使用 `toFloatOrNull()`。

<!-- example: fn-tofloat -->
```cypher
RETURN toFloat('2.5') AS number, toFloat('not numeric') AS invalid
```

预期列：`number, invalid`；预期 rows：`[[2.5,null]]`。


<a id="fn-tofloatornull"></a>

## `toFloatOrNull()`

<!-- function-signatures: toFloatOrNull -->
```text
toFloatOrNull(input :: ANY) :: FLOAT
```
<!-- /function-signatures -->

使用 `toFloat()` 相同的 Integer/Float/String 转换规则，并将不支持类型的结果设为 `null`。空白可以包围数字；不可解析 String 与 null 也返回 null。

返回 Float 或 null。它适合在混合来源数据中做显式清洗，但不会屏蔽参数表达式本身的错误，也不消除 Integer 转 Float 的精度损失。

<!-- example: fn-tofloatornull -->
```cypher
RETURN toFloatOrNull({a:1}) AS invalid
```

预期列：`invalid`；预期 rows：`[[null]]`。


<a id="fn-tointeger"></a>

## `toInteger()`

<!-- function-signatures: toInteger -->
```text
toInteger(input :: BOOLEAN | STRING | INTEGER | FLOAT) :: INTEGER
```
<!-- /function-signatures -->

Integer 原样返回；Boolean 转为 `0`/`1`；有限 Float 或可解析的数值 String 朝零截断为有符号 64 位 Integer。String 会去除两端空白，小数文本如 `'-2.9'` 得到 `-2`。

`null`、无法解析的 String、非有限数或超出 64 位范围的数值返回 `null`。其它类型（例如 List/Map）产生 `TYPE_ERROR`；若输入来源可能含这些值，使用 `toIntegerOrNull()`。

<!-- example: fn-tointeger -->
```cypher
RETURN toInteger('-2.9') AS number, toInteger(true) AS flag
```

预期列：`number, flag`；预期 rows：`[[-2,1]]`。


<a id="fn-tointegerornull"></a>

## `toIntegerOrNull()`

<!-- function-signatures: toIntegerOrNull -->
```text
toIntegerOrNull(input :: ANY) :: INTEGER
```
<!-- /function-signatures -->

遵循 `toInteger()` 的朝零截断、Boolean 和 String 解析规则；对 List、Map、图元素等不可转换类型返回 `null`，不产生转换 TYPE_ERROR。

返回 64 位 Integer 或 null；null、无效文本、超范围与非有限数值也返回 null。容错转换只覆盖该函数的转换，不覆盖前置表达式失败。

<!-- example: fn-tointegerornull -->
```cypher
RETURN toIntegerOrNull([1,2]) AS invalid
```

预期列：`invalid`；预期 rows：`[[null]]`。


<a id="fn-type"></a>

## `type()`

<!-- function-signatures: type -->
```text
type(input :: RELATIONSHIP) :: STRING
```
<!-- /function-signatures -->

返回 Relationship 的 relationship type String，保留类型名称的大小写。它与 `valueType()` 不同：前者返回图模型中的类型名（例如 `KNOWS`），后者返回值类型 `RELATIONSHIP NOT NULL`。

`input` 为 Relationship，`null` 返回 `null`；Node、String 等其它类型产生 `TYPE_ERROR`。多种 Relationship Type 的模式匹配后可用它分类、投影或聚合。

<!-- example: fn-type -->
```cypher
CREATE (:A)-[r:KNOWS]->(:B) RETURN type(r) AS type
```

预期列：`type`；预期 rows：`[["KNOWS"]]`。


<a id="fn-uuid"></a>

## `uuid()`

<!-- function-signatures: uuid -->
```text
uuid() :: UUID
uuid(mostSigBits :: INTEGER, leastSigBits :: INTEGER) :: UUID
uuid(name :: STRING) :: UUID
```
<!-- /function-signatures -->

构造 UUID 类型，三种 overload 分别为：无参数生成随机 UUID v4；一个 String 解析既有 UUID；两个 Integer 按有符号 64 位高/低半部构造 128 位值。String 必须采用 `8-4-4-4-12` 十六进制形式，大小写均接受；`name` 参数名不表示任意名称哈希。

String overload 中 null 返回 null；双 Integer overload 任一 null 返回 null。错误类型或不合法 UUID 文本产生 `TYPE_ERROR`。UUID 与同文 String 属于不同类型；输出编码见 [值编码](../../reference/values.md)。高/低位 overload 可用于精确可重复构造，随机 overload 的示例仅检查结果类型。

下列三个例子依次覆盖全部 overload。

<!-- example: fn-uuid-random -->
```cypher
RETURN valueType(uuid()) AS type
```

预期列：`type`；预期 rows：`[["UUID NOT NULL"]]`。

<!-- example: fn-uuid-string -->
```cypher
RETURN uuid('550E8400-E29B-41D4-A716-446655440000') = uuid('550e8400-e29b-41d4-a716-446655440000') AS same
```

预期列：`same`；预期 rows：`[[true]]`。

<!-- example: fn-uuid-bits -->
```cypher
RETURN uuid.mostSignificantBits(uuid(1,2)) AS most, uuid.leastSignificantBits(uuid(1,2)) AS least
```

预期列：`most, least`；预期 rows：`[[1,2]]`。


<a id="fn-uuid-leastsignificantbits"></a>

## `uuid.leastSignificantBits()`

<!-- function-signatures: uuid.leastSignificantBits -->
```text
uuid.leastSignificantBits(uuid :: UUID) :: INTEGER
```
<!-- /function-signatures -->

提取 UUID 的低 64 位并按有符号 64 位 Integer 返回。参数名 `uuid` 要求 UUID 值，不接受 UUID 文本；文本先经 `uuid(string)` 解析。

`null` 返回 `null`，其它类型产生 `TYPE_ERROR`。最高位为 `1` 的半部会显示为负整数；与 `uuid(mostSigBits,leastSigBits)` 可无损拆解/重建，宿主大整数按 [值编码](../../reference/values.md) 处理。

<!-- example: fn-uuid-leastsignificantbits -->
```cypher
RETURN uuid.leastSignificantBits(uuid(0,2)) AS bits
```

预期列：`bits`；预期 rows：`[[2]]`。


<a id="fn-uuid-mostsignificantbits"></a>

## `uuid.mostSignificantBits()`

<!-- function-signatures: uuid.mostSignificantBits -->
```text
uuid.mostSignificantBits(uuid :: UUID) :: INTEGER
```
<!-- /function-signatures -->

提取 UUID 的高 64 位，按有符号 64 位 Integer 返回。要求 UUID 类型输入；String 需要先转换。高半部的最高位决定返回整数符号。

`null` 返回 `null`，非 UUID 值产生 `TYPE_ERROR`。与低半部一同传给 `uuid(mostSigBits,leastSigBits)` 可以重建原值；不要把 Float 当作 64 位整数中转。

<!-- example: fn-uuid-mostsignificantbits -->
```cypher
RETURN uuid.mostSignificantBits(uuid(1,0)) AS bits
```

预期列：`bits`；预期 rows：`[[1]]`。


<a id="fn-valuetype"></a>

## `valueType()`

<!-- function-signatures: valueType -->
```text
valueType(input :: ANY) :: STRING
```
<!-- /function-signatures -->

返回 `input` 的运行时类型描述 String，接受任何值。非 null 标量结果带 `NOT NULL`；null 返回字符串 `'NULL'`，不是 null 值。Vector 同时包含坐标类型和维度，List 根据实际元素类型构成内部 union；空 List 的元素类型为 `NOTHING`。

List 本身非 null 不表示其元素非 null：`[1,null]` 的结果是 `LIST<INTEGER> NOT NULL`，而 `[1]` 是 `LIST<INTEGER NOT NULL> NOT NULL`。这个函数用来检查当前值，不声明变量或 Schema 的允许类型；静态判断使用 `IS TYPED`。

<!-- example: fn-valuetype -->
```cypher
RETURN valueType([1,null]) AS type, valueType([]) AS empty, valueType(null) AS nil
```

预期列：`type, empty, nil`；预期 rows：`[["LIST<INTEGER> NOT NULL","LIST<NOTHING> NOT NULL","NULL"]]`。

## 类型错误与 null 示例

下列查询都应产生 `TYPE_ERROR`。它们分别演示 cardinality 的容器限制和严格 Boolean 转换；若希望 Float 等其它类型得到 null，改用 `toBooleanOrNull()`。

<!-- example: fn-cardinality-type-error -->
```cypher
RETURN cardinality('abc')
```

<!-- example: fn-toboolean-type-error -->
```cypher
RETURN toBoolean(1.5)
```

null 的传播与空容器有区别：`head([])` 为 null，而 List/String 的大小可以为零。

<!-- example: fn-scalar-null -->
```cypher
RETURN cardinality(null) AS missing, size([]) AS empty,
       elementId(null) AS element, toInteger('invalid') AS invalid
```

预期列：`missing, empty, element, invalid`；预期 rows：`[[null,0,null,null]]`。

## 相关主题

[函数索引](../../reference/functions.md) · [类型](../types.md) · [List 函数](list.md) · [String 函数](string.md) · [LOAD CSV](../load-csv.md)

# 表达式与运算符

[语言手册](index.md) · [版本与范围](../reference/cypher-compatibility.md)

本文适用于 Lithograph v0.3.3、`CY25-2026.08`。表达式对当前行中的变量、参数和图元素求值，产生一个运行时值。WHERE/FILTER 再把 Boolean 或 null 当作过滤条件；表达式本身不必是 Boolean。字面量见 [语法](syntax.md#literals)，值范围见 [类型](types.md)，函数签名及逐项说明见 [函数索引](../reference/functions.md)。

<a id="precedence"></a>

## 优先级与结合方式

从高到低：

| 优先级 | 形式 | 说明 |
| --- | --- | --- |
| 1 | `(expr)`、函数、容器、CASE | 括号显式确定计算边界 |
| 2 | `.key`、`[index]`、`[start..end]`、Label/type 后缀 | 可连续访问，例如 `m['child'].name` |
| 3 | 一元 `+`、`-` | 数值符号 |
| 4 | `^` | 幂；连续幂按右结合求值 |
| 5 | `*`、`/`、`%` | 乘、除、余数 |
| 6 | `+`、`-`、`\|\|` | 加/减、String/List 拼接 |
| 7 | 比较、IN、字符串谓词、IS 谓词 | 见后续说明 |
| 8 | `NOT` | 三值逻辑否定 |
| 9 | `AND` | 三值逻辑与 |
| 10 | `XOR` | 三值逻辑异或 |
| 11 | `OR` | 三值逻辑或 |

除幂外，通常同级算术运算按左结合计算。为了让意图明确，复杂比较、Label expression 与逻辑组合建议加括号。比较链 `1 < x <= 5` 表示相邻比较共同成立，不是把 Boolean 中间结果与数字比较。

<!-- example: foundation-expression-precedence -->
```cypher
RETURN 2 + 3 * 4 AS arithmetic, (2 + 3) * 4 AS grouped,
       2 ^ 3 ^ 2 AS power, NOT false AND true OR false AS logic,
       1 < 3 <= 5 AS chained
```

返回 `[14,20,512.0,true,true]`。

<a id="arithmetic"></a>

## 算术、拼接与逻辑

`+ - * / % ^` 用于数值，Integer/Float 可以混合，混合运算产生 Float；两个 Integer 的 `/` 产生截断的小数部分之外的 Integer 结果，幂返回 Float。Integer 溢出或 Integer 除零/余零返回 `TYPE_ERROR`。Float 运算按浮点规则产生有限数、Infinity 或 NaN，其宿主编码见 [Values](../reference/values.md)。Temporal/Duration 的合法加减乘除组合见 [Temporal 函数](functions/temporal.md)。

`+` 还支持 String+String、List+List、List+单值、单值+List；`||` 支持两个 String 或两个 List 拼接。String 与数字不隐式互转，需要 `toString()` 或显式插值。null 参与二元算术或拼接返回 null。Map 不通过 `+` 合并，使用 Map projection。

<!-- example: foundation-expression-concat -->
```cypher
RETURN 7 / 2 AS quotient, 7 / 2.0 AS fraction,
       '甲' || '乙' AS text, [1, 2] || [3] AS list,
       [1] + 2 AS appended, 0 + [1] AS prepended
```

返回 `[3,3.5,"甲乙",[1,2,3],[1,2],[0,1]]`。

<!-- example: foundation-expression-division-error -->
```cypher
RETURN 1 / 0 AS invalid
```

返回 `TYPE_ERROR`。

`AND`、`OR`、`XOR`、`NOT` 接受 Boolean/null；不会对数字或字符串进行真假转换。完整三值逻辑、`=`/`<>`/`!=`、不等比较、null 和 NaN 见 [null 与排序](null-and-ordering.md)。

<a id="property-access"></a>

## 属性、Map、下标和切片

```text
<node-or-relationship-or-map>.<key>
<node-or-relationship-or-map>[<string-key>]
<list>[<integer-index>]
<list>[<start>..<end>]
```

点访问的 key 是静态名称；动态访问的 key 是 String 表达式，适合参数 `n[$key]`。缺失 Property/Map key 或 null base 返回 null。List 下标从 0 开始，负数从末尾倒数；越界返回 null。切片包含 start、不包含 end；省略边界代表起点/终点，负边界从末尾计算，超界边界裁到有效范围。显式求值为 null 的切片边界返回 null，和省略边界不同。不正确的 key/index 类型返回 `TYPE_ERROR`。

<!-- example: foundation-expression-access -->
```cypher
WITH {child:{name:'阿青'}} AS m, [10,20,30] AS values
RETURN m['child'].name AS name, m.absent AS missing,
       values[-1] AS last, values[1..3] AS part,
       values[99] AS outOfRange, values[null..2] AS unknownSlice
```

返回 `["阿青",null,30,[20,30],null,null]`。Temporal、Duration 和 Point 的命名分量访问是其值类型的操作，见对应 [Temporal](functions/temporal.md) 与 [Spatial](functions/spatial.md) 章节。

<a id="maps"></a>

## List/Map 构造与 Map projection

```text
[<expression>, ...]
{<key>: <expression>, ...}
<variable> { .<key>, .*, <key>: <expression>, <variable>, ... }
```

List 保留元素位置，允许混合类型和 null；Map key 是静态名称或 String literal，value 可以是任意表达式。Map projection 以 Node、Relationship 或 Map 变量为基础：`.key` 复制指定 Property，`.*` 复制全部 Property，`key:expr` 计算字段，单独变量名以其名称创建字段。不存在的显式 Property selector 得到 null；base 为 null 时整个投影为 null。`.*` 提供基础字段，显式字段覆盖同名基础字段；多个同名显式项按书写顺序以后项为准。

<!-- example: foundation-expression-map -->
```cypher
WITH {name:'阿青',score:10} AS record, 'member' AS role
RETURN record{.*, score:record.score + 1, role, .missing} AS result
```

返回 `{"name":"阿青","score":11,"role":"member","missing":null}`。这些 Map/List 是运行时结果；作为 Property 保存时必须满足 [Property 类型限制](types.md#property-types)。

<a id="comprehensions"></a>

## List 与 Pattern comprehension

```text
[<variable> IN <list> [WHERE <predicate>] [| <expression>]]
[[<path-variable> =] <pattern> [WHERE <predicate>] | <expression>]
```

List comprehension 对每个元素建立局部变量，WHERE 只保留 true，再用 `|` 右侧投影；省略 WHERE 保留全部，省略投影返回元素本身。空列表得到空列表，null 列表得到 null。静态已知 IN 输入不是 List 时，prepare 以 `SEMANTIC_ERROR` 拒绝；参数等输入到运行时才暴露错误类型时返回 `TYPE_ERROR`。局部变量只在当前 comprehension 内可见。

<!-- example: foundation-expression-comprehension -->
```cypher
RETURN [x IN [1, null, 2, 3] WHERE x >= 2 | x * 10] AS transformed,
       [x IN [] | x] AS empty, [x IN null | x] AS unknown
```

返回 `[[20,30],[],null]`。

Pattern comprehension 将与当前行相关的每个合法匹配投影成 List 元素；无匹配时返回空 List，可以引用外层已绑定变量。它不产生新的外层行；Pattern 引入的变量限于表达式内。结果 List 不保证匹配顺序。下面先执行 [读取示例数据](reading.md#match) 前的建图块。

<!-- example: foundation-expression-pattern-comprehension -->
```cypher
MATCH (p:ReaderPerson {name:'阿青'})
RETURN [(p)-[:RECOMMENDS]->(friend) | friend.name] AS names
```

返回 `[["白川"]]`。

<a id="predicates"></a>

## 成员、字符串、类型与 Label 谓词

| 形式 | 结果与限制 |
| --- | --- |
| `value IN list` | 逐个按 Cypher equality 比较；找到相等值为 true，无相等但出现 unknown 比较为 null，否则 false。空列表为 false，null 右侧为 null |
| `text STARTS WITH prefix` | String 前缀测试 |
| `text ENDS WITH suffix` | String 后缀测试 |
| `text CONTAINS part` | String 子串测试 |
| `text =~ pattern` | 正则匹配整个 String；匹配任意片段需显式写相应模式 |
| `value IS [NOT] NULL` | 返回确定的 Boolean，不用 `= null` 替代 |
| `value IS [NOT] TYPED type` | 类型 predicate；也可用 `IS [NOT] :: type`，正向可写 `value :: type` |
| `text IS [NOT] [NFC\|NFD\|NFKC\|NFKD] NORMALIZED` | 省略形式为 NFC；只检查，不修改 String |
| `node:label-expression` | 检查 Label expression；也支持 `IS LABELED ...`、`IS NOT [LABELED] ...` |

String predicate 与 NORMALIZED 对 null 或非 String 返回 null；不将其它类型转成 String。正则格式不合法返回 `SEMANTIC_ERROR`。类型 predicate 的 nullable、List、union 和 Vector 规则见 [类型](types.md)；Label expression、动态名称和 wildcard 见 [Pattern](patterns.md)。

<!-- example: foundation-expression-predicates -->
```cypher
RETURN 2 IN [1,2,null] AS member, 4 IN [1,null] AS unknown,
       'guide.md' ENDS WITH '.md' AS suffix,
       'a12' =~ 'a[0-9]+' AS regex,
       'é' IS NFC NORMALIZED AS normalized,
       10 IS TYPED INTEGER NOT NULL AS typed
```

返回 `[true,null,true,true,true,true]`。比较不同且不具 equality comparability 的 value family 可能返回 TYPE_ERROR，而非 false；IN 也服从该规则。

<a id="case"></a>

## CASE

```text
CASE <value> WHEN <match-value> THEN <result> ...
     [ELSE <fallback>] END
CASE WHEN <predicate> THEN <result> ...
     [ELSE <fallback>] END
```

Simple CASE 按 equality 选择首个匹配值；searched CASE 按顺序选择首个 true predicate。false/null 不选择该分支；没有匹配且无 ELSE 时为 null。`CASE null WHEN null ...` 不匹配，因为 equality 得到 null；检测 null 使用 searched CASE 的 `IS NULL`。各分支表达式必须通过语义/类型检查，不能依赖某分支“不执行”来藏起无效语法或作用域错误。

<!-- example: foundation-expression-case -->
```cypher
RETURN CASE 2 WHEN 1 THEN '低' WHEN 2 THEN '中' ELSE '高' END AS level,
       CASE WHEN null IS NULL THEN '缺失' ELSE '存在' END AS state,
       CASE WHEN false THEN 1 END AS absent
```

返回 `["中","缺失",null]`。CASE 产生值，不能把 CREATE、RETURN 等 clause 放进 THEN；根据条件执行整段 query 使用 [WHEN](composition.md)。

<a id="interpolation"></a>

## 字符串插值

```text
s"<text>{<expression>}<text>..."
S"<text>{<expression>}<text>..."
```

插值在当前作用域求值，可使用变量、参数、函数和嵌套表达式。文本中的字面 `{`、`}` 使用 `\{`、`\}`；其它字符串转义仍有效。括号必须配对，空表达式非法。String、Boolean、数值、Temporal/Duration、Point、UUID 使用其字符串表示；null 插入文本 `null`。List/Map/Node/Relationship/Path/Vector 不自动 JSON 序列化，会产生 `TYPE_ERROR`。

<!-- example: foundation-expression-interpolation -->
```cypher
WITH '阿青' AS name, 2 AS count
RETURN s"读者={name}，数量={count + 1}，缺失={null}" AS message
```

返回 `"读者=阿青，数量=3，缺失=null"`。插值只构造 String，不会将插入内容作为新的 Cypher 执行。

<a id="expression-subqueries"></a>

## 表达式子查询与函数局部作用域

`EXISTS { ... }` 返回是否存在行，`COUNT { ... }` 返回行数，`COLLECT { ... }` 产生 List；关联、空输入及返回列要求见 [子查询](subqueries.md)。Pattern predicate 检查一个使用当前行绑定的 Pattern 是否存在，不返回新绑定到外层。

`all/any/none/single(variable IN list WHERE predicate)`、`reduce(acc = initial, variable IN list | expression)`、`allReduce(...)` 有自己的局部绑定与 null 规则，见 [Predicate 函数](functions/predicate.md) 和 [List 函数](functions/list.md)。普通函数、聚合函数和特殊语法函数使用各自签名，不能仅由函数名推断参数数量或 null 行为。

相关主题：[读取](reading.md) · [聚合](aggregation.md) · [类型](types.md) · [错误](../reference/errors.md)。

<a id="property-exists"></a>

## PROPERTY_EXISTS

```text
PROPERTY_EXISTS(<node-or-relationship-expression>, <property-key>)
```

第二项是属性名称标识符，不是要求值的变量，也不是 String 参数；需要动态 key 时使用 `entity[$key] IS NOT NULL`。对象拥有该属性返回 true，缺失返回 false；对象为 null 返回 null。只接受 Node/Relationship，Map 等其它输入返回 `TYPE_ERROR`。它是语言 predicate，不列入 SHOW FUNCTIONS；SET 属性为 null 后该属性不再存在。

<!-- example: foundation-property-exists -->
```cypher
CREATE (n {name:'Ada'})
RETURN PROPERTY_EXISTS(n,name) AS present,
       PROPERTY_EXISTS(n,missing) AS missing,
       PROPERTY_EXISTS(null,name) AS unknown
```

返回 `[true,false,null]`。

<!-- example: foundation-property-exists-type-error -->
```cypher
RETURN PROPERTY_EXISTS({name:'Ada'},name) AS invalid
```

预期 `TYPE_ERROR`；Map 的 key 查询使用 Map 表达式。

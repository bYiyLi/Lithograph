# 数值、三角与对数函数

适用版本：**Lithograph v0.3.2** · Cypher Profile：`CY25-2026.08`。本文覆盖 `SHOW FUNCTIONS` 的 Numeric、Trigonometric、Logarithmic 类别。

[Cypher 手册](../index.md) · [完整函数索引](../../reference/functions.md) · [值与宿主 JSON 编码](../../reference/values.md)

<a id="numeric-rules"></a>

## 公共参数与结果规则

除 `e()`、`pi()`、`rand()` 外，本文函数的参数不能省略；`round()` 的第二、第三参数除外。签名中的 `FLOAT` 参数同时接受 `INTEGER`，计算时转换为双精度浮点数，不接受数字字符串或 Boolean。超过 `2^53` 的整数转换成 `FLOAT` 后可能损失精度。没有参数的常量函数与 `rand()` 不接受多余参数。

普通数学函数对 `null` 返回 `null`，二元 `atan2()` 任一参数为 `null` 时返回 `null`；`round()` 的规则见其条目。错误参数类型产生 `TYPE_ERROR`，参数数量不符产生 `SEMANTIC_ERROR`；例如 `abs('2')` 不会隐式转换字符串。完整错误识别见 [错误参考](../../reference/errors.md)。

数学定义域之外的浮点计算可以产生 `NaN`，零点或溢出可以产生 Infinity，它们仍是 `FLOAT` 值。用 `isNaN()` 检测 `NaN`；不能用 `x = x` 代替。通过 SQL 获取结果时，非有限值使用 Values Reference 的 tagged Float JSON 编码，不是普通 JSON number。近似计算应使用误差容限比较，特别是三角函数在奇点附近的结果。

本文所有查询都可在已加载并初始化的空数据库独立运行；示例的 `x` 列可以直接用 `lithograph()` 获取。

| 类别 | 函数 |
| --- | --- |
| 数值 | [abs](#fn-abs)、[ceil](#fn-ceil)、[ceiling](#fn-ceiling)、[floor](#fn-floor)、[isNaN](#fn-isnan)、[rand](#fn-rand)、[round](#fn-round)、[sign](#fn-sign) |
| 常量与对数 | [e](#fn-e)、[exp](#fn-exp)、[ln](#fn-ln)、[log](#fn-log)、[log10](#fn-log10)、[sqrt](#fn-sqrt) |
| 三角 | [acos](#fn-acos)、[asin](#fn-asin)、[atan](#fn-atan)、[atan2](#fn-atan2)、[cos](#fn-cos)、[cot](#fn-cot)、[degrees](#fn-degrees)、[haversin](#fn-haversin)、[pi](#fn-pi)、[radians](#fn-radians)、[sin](#fn-sin)、[tan](#fn-tan) |
| 双曲 | [cosh](#fn-cosh)、[coth](#fn-coth)、[sinh](#fn-sinh)、[tanh](#fn-tanh) |

<a id="fn-abs"></a>

## `abs()`

<!-- function-signatures: abs -->
```text
abs(input :: INTEGER | FLOAT) :: INTEGER | FLOAT
```
<!-- /function-signatures -->

返回 `input` 的绝对值。`INTEGER` 输入保留 `INTEGER`，`FLOAT` 输入保留 `FLOAT`；`null` 返回 `null`。最小 64 位整数的正绝对值超出 `INTEGER64` 范围，产生 `TYPE_ERROR`，不会绕回负值。可用于数值差、距离或误差表达式。

<!-- example: fn-abs -->
```cypher
RETURN abs(-37) AS x
```

预期列：`x`；预期行：`[37]`。

<a id="fn-ceil"></a>

## `ceil()`

<!-- function-signatures: ceil -->
```text
ceil(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

返回不小于 `input` 的最小整数值，结果类型始终为 `FLOAT`。例如负的小数向正无穷取整；它与截断到零不同。输入为 `null` 返回 `null`，非数值产生 `TYPE_ERROR`。与 [`ceiling()`](#fn-ceiling) 同义。

<!-- example: fn-ceil -->
```cypher
RETURN ceil(-2.4) AS x
```

预期列：`x`；预期行：`[-2.0]`。

<a id="fn-ceiling"></a>

## `ceiling()`

<!-- function-signatures: ceiling -->
```text
ceiling(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

`ceil()` 的 Cypher 25 命名别名。参数 `input` 是要向正无穷方向取整的数；结果为 `FLOAT`，不因结果为整数值而改成 `INTEGER`。`null`、非数值错误与 [`ceil()`](#fn-ceil) 完全相同。

<!-- example: fn-ceiling -->
```cypher
RETURN ceiling(2.4) AS x
```

预期列：`x`；预期行：`[3.0]`。

<a id="fn-floor"></a>

## `floor()`

<!-- function-signatures: floor -->
```text
floor(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

返回不大于 `input` 的最大整数值，结果为 `FLOAT`。负数向负无穷方向取整，因此 `floor(-2.4)` 为 `-3.0`。`null` 返回 `null`；非数值产生 `TYPE_ERROR`。需要向零截断时应选用 `round(input, 0, 'DOWN')`。

<!-- example: fn-floor -->
```cypher
RETURN floor(-2.4) AS x
```

预期列：`x`；预期行：`[-3.0]`。

<a id="fn-isnan"></a>

## `isNaN()`

<!-- function-signatures: isNaN -->
```text
isNaN(input :: INTEGER | FLOAT) :: BOOLEAN
```
<!-- /function-signatures -->

检查 `input` 是否为浮点 `NaN`，返回 `BOOLEAN`。任意正常整数或有限浮点值为 `false`，Infinity 也不是 NaN；`null` 返回 `null`，非数值产生 `TYPE_ERROR`。常与 `sqrt()`、反三角或对数计算后的过滤组合使用。

<!-- example: fn-isnan -->
```cypher
RETURN isNaN(sqrt(-4.0)) AS x
```

预期列：`x`；预期行：`[true]`。

<a id="fn-rand"></a>

## `rand()`

<!-- function-signatures: rand -->
```text
rand() :: FLOAT
```
<!-- /function-signatures -->

不接收参数，每次调用产生区间 `[0.0, 1.0)` 中的随机 `FLOAT`。没有 seed 参数，也不承诺不同调用的结果相同。若要在多个表达式中使用同一个随机值，先用 `WITH rand() AS r` 绑定一次。底层随机源不可用时执行失败；随机值不适合充当唯一约束。下面只验证取值范围。

<!-- example: fn-rand -->
```cypher
WITH rand() AS r RETURN r >= 0.0 AND r < 1.0 AS x
```

预期列：`x`；预期行：`[true]`。

<a id="fn-round"></a>

## `round()`

<!-- function-signatures: round -->
```text
round(input :: FLOAT [, precision :: INTEGER | FLOAT, mode :: STRING]) :: FLOAT
```
<!-- /function-signatures -->

按十进制精度舍入 `input`，结果始终是 `FLOAT`。支持 `round(input)`、`round(input, precision)` 与 `round(input, precision, mode)`；传 `mode` 时必须同时给出 `precision`。

`precision` 默认 `0`，接受 32 位范围内的整数或整数值 `FLOAT`，例如 `2`、`2.0`；正值表示小数位，负值表示向十位、百位等舍入。非整数精度、超出范围的精度、非字符串 mode 或未知 mode 产生 `TYPE_ERROR`。

默认规则需要区分：单参数或 `precision=0` 且省略 mode 时，最近值平局取向正无穷方向，因此 `round(-1.5)=-1.0`；非零 precision 省略 mode 时，最近值平局远离零，因此 `round(-1.55,1)=-1.6`。需要明确策略的业务计算应直接传 mode。

| `mode`，不区分大小写 | 行为 |
| --- | --- |
| `UP` | 远离零，任何非零尾数都进位 |
| `DOWN` | 向零截断 |
| `CEILING` | 向正无穷 |
| `FLOOR` | 向负无穷 |
| `HALF_UP` | 最近值，平局远离零 |
| `HALF_DOWN` | 最近值，平局向零 |
| `HALF_EVEN` | 最近值，平局取偶数邻值 |

有效参数中任一为 `null` 时结果为 `null`。计算使用二进制浮点数，不能用于需要任意精度 decimal 的财务舍入；过大或过小的 scale 也会受浮点范围限制。

<!-- example: fn-round -->
```cypher
RETURN round(12.345, 2, 'HALF_EVEN') AS x
```

预期列：`x`；预期行：`[12.34]`。

<a id="fn-sign"></a>

## `sign()`

<!-- function-signatures: sign -->
```text
sign(input :: INTEGER | FLOAT) :: INTEGER
```
<!-- /function-signatures -->

返回数值的符号：负数为整数 `-1`，零为 `0`，正数为 `1`。输入 `INTEGER` 或 `FLOAT` 均返回 `INTEGER`；`+0.0` 与 `-0.0` 都返回 `0`。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。NaN 不具有正负意义，应先用 `isNaN()` 检测，避免把其结果用于分类。

<!-- example: fn-sign -->
```cypher
RETURN sign(-0.25) AS x
```

预期列：`x`；预期行：`[-1]`。

<a id="fn-e"></a>

## `e()`

<!-- function-signatures: e -->
```text
e() :: FLOAT
```
<!-- /function-signatures -->

不接收参数，返回自然对数底数 `e` 的双精度 `FLOAT` 近似值。它是常量，不涉及图状态或查询时钟；可与 `exp()` 或 `log()` 组合。多余参数产生 `SEMANTIC_ERROR`。

<!-- example: fn-e -->
```cypher
RETURN abs(log(e()) - 1.0) < 1e-12 AS x
```

预期列：`x`；预期行：`[true]`。

<a id="fn-exp"></a>

## `exp()`

<!-- function-signatures: exp -->
```text
exp(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

返回 `e` 的 `input` 次幂，结果为 `FLOAT`。实数输入都可计算；很大的正值可能溢出为正 Infinity，很小的负值可能下溢为 `0.0`。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。它是自然对数 `log()` 的反函数。

<!-- example: fn-exp -->
```cypher
RETURN exp(0.0) AS x
```

预期列：`x`；预期行：`[1.0]`。

<a id="fn-ln"></a>

## `ln()`

<!-- function-signatures: ln -->
```text
ln(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

返回 `input` 的自然对数，结果为 `FLOAT`；是 [`log()`](#fn-log) 的命名别名。正数属于实数定义域，`0.0` 得到负 Infinity，负数得到 NaN。`null` 返回 `null`，非数值产生 `TYPE_ERROR`；可用 `isNaN()` 处理定义域外值。

<!-- example: fn-ln -->
```cypher
RETURN ln(1.0) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="fn-log"></a>

## `log()`

<!-- function-signatures: log -->
```text
log(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

返回以 `e` 为底的自然对数，不是以 10 为底。`input` 为正数时返回有限 `FLOAT`；`0.0` 返回负 Infinity，负数返回 NaN。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。十进制对数用 `log10()`；`ln()` 与本函数同义。

<!-- example: fn-log -->
```cypher
RETURN log(1.0) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="fn-log10"></a>

## `log10()`

<!-- function-signatures: log10 -->
```text
log10(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

返回 `input` 以 10 为底的对数，结果为 `FLOAT`。正数属于实数定义域，零返回负 Infinity，负数返回 NaN。`null` 返回 `null`；非数值产生 `TYPE_ERROR`。与 `log()` 的底数不同。

<!-- example: fn-log10 -->
```cypher
RETURN log10(1000.0) AS x
```

预期列：`x`；预期行：`[3.0]`。

<a id="fn-sqrt"></a>

## `sqrt()`

<!-- function-signatures: sqrt -->
```text
sqrt(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

返回 `input` 的非负平方根，结果为 `FLOAT`。非负实数为正常定义域；负数得到 NaN，`null` 得到 `null`，非数值产生 `TYPE_ERROR`。可通过 `isNaN()` 检查无实数根的情况。

<!-- example: fn-sqrt -->
```cypher
RETURN sqrt(81) AS x
```

预期列：`x`；预期行：`[9.0]`。

<a id="fn-acos"></a>

## `acos()`

<!-- function-signatures: acos -->
```text
acos(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

参数 `input` 是余弦值，返回反余弦的弧度 `FLOAT`，实数结果范围 `[0, pi()]`。正常定义域为 `[-1,1]`，超出该区间得到 NaN；`null` 返回 `null`，非数值产生 `TYPE_ERROR`。需要角度时再调用 `degrees()`。

<!-- example: fn-acos -->
```cypher
RETURN acos(1.0) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="fn-asin"></a>

## `asin()`

<!-- function-signatures: asin -->
```text
asin(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

参数 `input` 是正弦值，返回反正弦的弧度 `FLOAT`，实数结果范围 `[-pi()/2, pi()/2]`。正常定义域为 `[-1,1]`，区间外得到 NaN；`null` 返回 `null`，非数值产生 `TYPE_ERROR`。

<!-- example: fn-asin -->
```cypher
RETURN asin(0.0) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="fn-atan"></a>

## `atan()`

<!-- function-signatures: atan -->
```text
atan(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

参数 `input` 是正切值，返回反正切的弧度 `FLOAT`。有限输入的结果位于 `(-pi()/2, pi()/2)`；无限输入达到相应端点。只给斜率无法确定平面的完整象限，已知 `y,x` 时使用 `atan2()`。`null` 返回 `null`；非数值产生 `TYPE_ERROR`。

<!-- example: fn-atan -->
```cypher
RETURN atan(0.0) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="fn-atan2"></a>

## `atan2()`

<!-- function-signatures: atan2 -->
```text
atan2(y :: FLOAT, x :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

按 `y,x` 两个数值确定点 `(x,y)` 的方向角，返回 `[-pi(),pi()]` 范围的弧度 `FLOAT`，保留象限信息。参数次序是先 `y` 后 `x`。任一参数为 `null` 返回 `null`；其它非数值产生 `TYPE_ERROR`。原点和有符号零遵循浮点角度规则；不要用 `atan(y/x)` 替代。

<!-- example: fn-atan2 -->
```cypher
RETURN abs(atan2(1.0, 0.0) - pi()/2) < 1e-12 AS x
```

预期列：`x`；预期行：`[true]`。

<a id="fn-cos"></a>

## `cos()`

<!-- function-signatures: cos -->
```text
cos(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

`input` 是弧度角，返回余弦 `FLOAT`。有限实数输入的结果位于 `[-1,1]`；不是角度制，角度必须先经 `radians()`。`null` 返回 `null`，非数值产生 `TYPE_ERROR`；非有限角度可得到 NaN。

<!-- example: fn-cos -->
```cypher
RETURN cos(0.0) AS x
```

预期列：`x`；预期行：`[1.0]`。

<a id="fn-cosh"></a>

## `cosh()`

<!-- function-signatures: cosh -->
```text
cosh(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

返回实数 `input` 的双曲余弦 `FLOAT`，定义为 `(exp(input)+exp(-input))/2`。有限实数输入的数学结果不小于 1；大幅值可能溢出为 Infinity。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。它不是余弦函数，没有角度制转换。

<!-- example: fn-cosh -->
```cypher
RETURN cosh(0.0) AS x
```

预期列：`x`；预期行：`[1.0]`。

<a id="fn-cot"></a>

## `cot()`

<!-- function-signatures: cot -->
```text
cot(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

`input` 是弧度角，返回余切 `FLOAT`，即 `1/tan(input)`。在正切为零的位置有奇点，`cot(0.0)` 得到 Infinity；浮点近似使其它理论奇点附近产生很大的有限值。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。

<!-- example: fn-cot -->
```cypher
RETURN abs(cot(pi()/4) - 1.0) < 1e-12 AS x
```

预期列：`x`；预期行：`[true]`。

<a id="fn-coth"></a>

## `coth()`

<!-- function-signatures: coth -->
```text
coth(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

返回 `input` 的双曲余切 `FLOAT`，即 `1/tanh(input)`。`input=0.0` 为奇点，可得到 Infinity；它使用双曲函数的实数参数。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。

<!-- example: fn-coth -->
```cypher
RETURN abs(coth(1.0) * tanh(1.0) - 1.0) < 1e-12 AS x
```

预期列：`x`；预期行：`[true]`。

<a id="fn-degrees"></a>

## `degrees()`

<!-- function-signatures: degrees -->
```text
degrees(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

将弧度 `input` 转换为角度，结果为 `FLOAT`，关系是 `input*180/pi()`。可接反三角函数输出；本函数不把角度规范到某个周期。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。

<!-- example: fn-degrees -->
```cypher
RETURN degrees(pi()) AS x
```

预期列：`x`；预期行：`[180.0]`。

<a id="fn-haversin"></a>

## `haversin()`

<!-- function-signatures: haversin -->
```text
haversin(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

`input` 是弧度角，返回半正矢 `FLOAT`，即 `(1-cos(input))/2`。有限角度的结果位于 `[0,1]`；它是角度函数，不直接返回长度或地理距离，Point 距离应使用 Spatial 函数。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。

<!-- example: fn-haversin -->
```cypher
RETURN haversin(0.0) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="fn-pi"></a>

## `pi()`

<!-- function-signatures: pi -->
```text
pi() :: FLOAT
```
<!-- /function-signatures -->

不接收参数，返回圆周率的双精度 `FLOAT` 近似值。可用于构建弧度角、面积等表达式；不代表任意精度实数。多余参数产生 `SEMANTIC_ERROR`。

<!-- example: fn-pi -->
```cypher
RETURN abs(pi() - 3.141592653589793) < 1e-15 AS x
```

预期列：`x`；预期行：`[true]`。

<a id="fn-radians"></a>

## `radians()`

<!-- function-signatures: radians -->
```text
radians(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

将角度 `input` 转换为弧度，结果为 `FLOAT`，关系是 `input*pi()/180`。可直接接入 `sin()`、`cos()`、`tan()`；不把输入规范到 0–360 度。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。

<!-- example: fn-radians -->
```cypher
RETURN radians(0.0) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="fn-sin"></a>

## `sin()`

<!-- function-signatures: sin -->
```text
sin(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

`input` 是弧度角，返回正弦 `FLOAT`。有限实数输入的结果位于 `[-1,1]`；角度输入必须先用 `radians()` 转换。`null` 返回 `null`，非数值产生 `TYPE_ERROR`；非有限角度可得到 NaN。

<!-- example: fn-sin -->
```cypher
RETURN sin(radians(90.0)) AS x
```

预期列：`x`；预期行：`[1.0]`。

<a id="fn-sinh"></a>

## `sinh()`

<!-- function-signatures: sinh -->
```text
sinh(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

返回 `input` 的双曲正弦 `FLOAT`，定义为 `(exp(input)-exp(-input))/2`。接受实数参数，大幅值可能溢出为相应符号的 Infinity。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。与 `sin()` 的周期性不同。

<!-- example: fn-sinh -->
```cypher
RETURN sinh(0.0) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="fn-tan"></a>

## `tan()`

<!-- function-signatures: tan -->
```text
tan(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

`input` 是弧度角，返回正切 `FLOAT`。在奇数倍 `pi()/2` 有数学奇点；浮点表示的近似角可以返回极大的有限值，因此不能通过恰好等于 Infinity 来判断奇点。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。

<!-- example: fn-tan -->
```cypher
RETURN tan(0.0) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="fn-tanh"></a>

## `tanh()`

<!-- function-signatures: tanh -->
```text
tanh(input :: FLOAT) :: FLOAT
```
<!-- /function-signatures -->

返回 `input` 的双曲正切 `FLOAT`。有限实数输入的数学值在 `(-1,1)`，大幅值的浮点结果可能舍入为 `-1.0` 或 `1.0`。`null` 返回 `null`，非数值产生 `TYPE_ERROR`。

<!-- example: fn-tanh -->
```cypher
RETURN tanh(0.0) AS x
```

预期列：`x`；预期行：`[0.0]`。

<a id="numeric-edge-examples"></a>

## null、定义域与舍入组合

下面分别验证 null 传播、定义域外的 NaN 和三种不同的平局策略。计算 NaN 不等于查询失败；把字符串传给数值函数才会触发类型错误。

<!-- example: fn-mathematical-null -->
```cypher
RETURN abs(null) AS a, atan2(null, 2.0) AS b, round(1.2, null) AS c, isNaN(null) AS d
```

预期列：`a`, `b`, `c`, `d`；预期行：`[null, null, null, null]`。

<!-- example: fn-mathematical-domain -->
```cypher
RETURN isNaN(acos(2.0)) AS a, isNaN(log(-2.0)) AS b, isNaN(sqrt(-2.0)) AS c
```

预期列：`a`, `b`, `c`；预期行：`[true, true, true]`。

<!-- example: fn-round-modes -->
```cypher
RETURN round(-1.5) AS a, round(-1.55,1) AS b, round(2.5,0,'HALF_EVEN') AS c, round(1.201,1,'UP') AS d
```

预期列：`a`, `b`, `c`, `d`；预期行：`[-1.0, -1.6, 2.0, 1.3]`。

语义核对来源：[Neo4j Cypher 25 数值函数](https://neo4j.com/docs/cypher-manual/25/functions/mathematical-numeric/)、[三角函数](https://neo4j.com/docs/cypher-manual/25/functions/mathematical-trigonometric/)、[对数函数](https://neo4j.com/docs/cypher-manual/25/functions/mathematical-logarithmic/)（2026-10-15 核对）。在线手册会继续更新，Lithograph 的函数集合与示例以本文标明的冻结 Profile 和版本为准。

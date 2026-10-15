# 列表函数

适用版本：Lithograph **v0.3.1** · Cypher Profile **`CY25-2026.08`**。

[Cypher 手册](../index.md) · [完整函数索引](../../reference/functions.md)

本页覆盖运行时 `List` 分类的完整函数以及 List 的 `reverse()` overload。所有转换和集合操作都返回新值；不修改传入的 List，也不直接修改图。签名中的 `::` 和类型表示参考信息，参数按位置传递。

List 是有序运行时值，元素可为 null 或不同类型；是否能作为图 Property 持久化另由 [类型](../types.md) 决定。`head()`、`last()`、`size()`、`cardinality()` 按注册分类见 [标量函数](scalar.md)。[Neo4j List functions](https://neo4j.com/docs/cypher-manual/25/functions/list/) 用于标准语义核对（2026-10-15）；只采用冻结 Profile 内能力。

## 函数导航

[coll.distinct()](#fn-coll-distinct) | [coll.flatten()](#fn-coll-flatten) | [coll.indexOf()](#fn-coll-indexof) | [coll.insert()](#fn-coll-insert) | [coll.max()](#fn-coll-max) | [coll.min()](#fn-coll-min) | [coll.remove()](#fn-coll-remove) | [coll.sort()](#fn-coll-sort) | [keys()](#fn-keys) | [labels()](#fn-labels) | [nodes()](#fn-nodes) | [range()](#fn-range) | [reduce()](#fn-reduce) | [relationships()](#fn-relationships) | [reverse()](#fn-reverse) | [tail()](#fn-tail) | [toBooleanList()](#fn-tobooleanlist) | [toFloatList()](#fn-tofloatlist) | [toIntegerList()](#fn-tointegerlist) | [toStringList()](#fn-tostringlist)

<a id="fn-coll-distinct"></a>

## coll.distinct()

<!-- function-signatures: coll.distinct -->
```text
coll.distinct(list :: LIST<ANY>) :: LIST<ANY>
```
<!-- /function-signatures -->

对 List `list` 去重，返回保留**首次出现顺序**的新 List；输入本身不改变。按 Cypher `DISTINCT` 的值规则识别重复：数值相等的 Integer/Float 合并，多个 null 保留一个，嵌套容器按内容比较，图元素按 identity 区分。

只接受一个参数。空列表返回 `[]`，外层 `null` 返回 `null`；非 List 输入产生 `TYPE_ERROR`。这不消除查询行，行去重使用 `RETURN DISTINCT`。

<!-- example: fn-coll-distinct -->
```cypher
RETURN [coll.distinct([2, 1, 2, null, null, 1.0]), coll.distinct([]), coll.distinct(null)] AS x
```

结果：`[[2,1,null],[],null]`（列 `x` 的值）。

<a id="fn-coll-flatten"></a>

## coll.flatten()

<!-- function-signatures: coll.flatten -->
```text
coll.flatten(list :: LIST<ANY>, depth = 1 :: INTEGER) :: LIST<ANY>
```
<!-- /function-signatures -->

展开 List `list` 中嵌套的 List，按原遍历顺序返回新 List。`depth` 缺省为 `1`；每进入一个嵌套 List 消耗一层，`0` 保留原结构。非 List 元素（包括 null）原样保留，空嵌套列表在展开时不产生元素。

外层列表或显式 depth 为 `null` 返回 `null`。非 List 输入、非 Integer 或负 depth 产生 `TYPE_ERROR`。它不会改写 Map 内部的 List，也不会把 Vector 当作嵌套 List 展开。

<!-- example: fn-coll-flatten -->
```cypher
RETURN [coll.flatten([1, [2, [3]], null]), coll.flatten([1, [2, [3]]], 2), coll.flatten([[1]], 0)] AS x
```

结果：`[[1,2,[3],null],[1,2,3],[[1]]]`（列 `x` 的值）。

<a id="fn-coll-indexof"></a>

## coll.indexOf()

<!-- function-signatures: coll.indexOf -->
```text
coll.indexOf(list :: LIST<ANY>, value :: ANY) :: INTEGER
```
<!-- /function-signatures -->

返回 List `list` 中首个与 `value` 按 Cypher `=` 比较为 true 的元素位置 Integer，位置从 `0` 开始；未找到返回 `-1`。两个参数都必需，元素顺序不改变。

任一外层参数为 `null` 返回 `null`，因此它不能用来查找列表内 null 的位置。非 List 输入产生 `TYPE_ERROR`；元素与查找值若属于不可作 equality 比较的不同 family，比较可能产生 `TYPE_ERROR`。空列表且 value 非 null 时返回 `-1`。

<!-- example: fn-coll-indexof -->
```cypher
RETURN [coll.indexOf([1, 2, 2], 2), coll.indexOf([], 2), coll.indexOf([1, null], null)] AS x
```

结果：`[1,-1,null]`（列 `x` 的值）。

<a id="fn-coll-insert"></a>

## coll.insert()

<!-- function-signatures: coll.insert -->
```text
coll.insert(list :: LIST<ANY>, index :: INTEGER, value :: ANY) :: LIST<ANY>
```
<!-- /function-signatures -->

在 List `list` 的 `index` 位置之前插入 `value`，返回新 List。三个参数都必需；index 从 `0` 开始，可取 `0..size(list)`，等于列表长度时追加。`value` 可以为任意运行时值，包括 null 或嵌套 List，不会自动展平。

list 或 index 为 `null` 返回 `null`。非法列表、非 Integer/负数/越界 index 产生 `TYPE_ERROR`；空列表只有 index `0` 合法。

<!-- example: fn-coll-insert -->
```cypher
RETURN [coll.insert([1, 3], 1, 2), coll.insert([], 0, null), coll.insert([1], 1, [2])] AS x
```

结果：`[[1,2,3],[null],[1,[2]]]`（列 `x` 的值）。

<a id="fn-coll-max"></a>

## coll.max()

<!-- function-signatures: coll.max -->
```text
coll.max(list :: LIST<ANY>) :: ANY
```
<!-- /function-signatures -->

从 List `list` 的非 null 项中选择最大值，返回该值并保持其类型。只接受一个参数；按 `ORDER BY` 的 total ordering 比较，具体跨类型顺序见 [null 与排序](../null-and-ordering.md)。它对列表内部求极值，不在输入行之间聚合。

null 项被忽略；空列表、全 null 列表或外层 null 返回 `null`。非 List 输入产生 `TYPE_ERROR`。需要跨行聚合时使用 `max()`。

<!-- example: fn-coll-max -->
```cypher
RETURN [coll.max([3, null, 8, 1]), coll.max([]), coll.max([null]), coll.max(null)] AS x
```

结果：`[8,null,null,null]`（列 `x` 的值）。

<a id="fn-coll-min"></a>

## coll.min()

<!-- function-signatures: coll.min -->
```text
coll.min(list :: LIST<ANY>) :: ANY
```
<!-- /function-signatures -->

从 List `list` 的非 null 项中选择最小值，返回原类型的元素。只接受一个参数，采用 `ORDER BY` 的 total ordering；它不把多个查询行分组，也不改变列表。

空列表、全 null 列表或外层 null 返回 `null`；其它 null 项被忽略。非 List 输入产生 `TYPE_ERROR`。跨行聚合使用 `min()`。

<!-- example: fn-coll-min -->
```cypher
RETURN [coll.min([3, null, 8, 1]), coll.min([]), coll.min([null]), coll.min(null)] AS x
```

结果：`[1,null,null,null]`（列 `x` 的值）。

<a id="fn-coll-remove"></a>

## coll.remove()

<!-- function-signatures: coll.remove -->
```text
coll.remove(list :: LIST<ANY>, index :: INTEGER) :: LIST<ANY>
```
<!-- /function-signatures -->

从 List `list` 删除 `index` 处的一个元素，返回保留其它项顺序的新 List；不会按元素值删除全部匹配项。index 从 `0` 开始，必须满足 `0 <= index < size(list)`，两个参数都必需。

外层 list 或 index 为 `null` 返回 `null`。非 List 输入、非 Integer/负数/越界 index 产生 `TYPE_ERROR`；空列表没有可删除的有效位置。

<!-- example: fn-coll-remove -->
```cypher
RETURN [coll.remove([1, 2, 2, 3], 1), coll.remove([null], 0), coll.remove(null, 0)] AS x
```

结果：`[[1,2,3],[],null]`（列 `x` 的值）。

<a id="fn-coll-sort"></a>

## coll.sort()

<!-- function-signatures: coll.sort -->
```text
coll.sort(list :: LIST<ANY>) :: LIST<ANY>
```
<!-- /function-signatures -->

把 List `list` 按升序 total ordering 排列，返回新 List；只接受一个参数，不能传 comparator 或 descending 参数。null 保留并排在非 null 值之后，嵌套值和不同 family 的顺序见 [null 与排序](../null-and-ordering.md)。

空列表返回 `[]`，外层 null 返回 `null`，非 List 输入产生 `TYPE_ERROR`。降序可组合 `reverse(coll.sort(list))`；这会连同 null 位置一起反转。

<!-- example: fn-coll-sort -->
```cypher
RETURN [coll.sort([3, null, 1, 2]), reverse(coll.sort([3, 1, 2])), coll.sort(null)] AS x
```

结果：`[[1,2,3,null],[3,2,1],null]`（列 `x` 的值）。

<a id="fn-keys"></a>

## keys()

<!-- function-signatures: keys -->
```text
keys(input :: NODE | RELATIONSHIP | MAP) :: LIST<STRING>
```
<!-- /function-signatures -->

从 Map、Node 或 Relationship `input` 返回属性/字段 key 的 `LIST<STRING>`。只接受一个参数；不包含值、Node labels、Relationship type 或 elementId。Map 中值为 null 的字段仍有 key；图中 `SET … = null` 删除的属性不再有 key。

空对象返回 `[]`，null 返回 `null`；其它类型产生 `TYPE_ERROR`。需要稳定业务顺序时显式使用 `coll.sort(keys(input))`，不要依赖 Map 或图属性的书写顺序。

<!-- example: fn-keys -->
```cypher
RETURN [coll.sort(keys({z: 1, a: null})), keys({}), keys(null)] AS x
```

结果：`[["a","z"],[],null]`（列 `x` 的值）。

<a id="fn-labels"></a>

## labels()

<!-- function-signatures: labels -->
```text
labels(input :: NODE) :: LIST<STRING>
```
<!-- /function-signatures -->

返回 Node `input` 的 Label 名称 `LIST<STRING>`。只接受一个 Node，不接受 Relationship type、Map key 或字符串 label 名；无 Label 的 Node 返回 `[]`，null 返回 `null`，其它值产生 `TYPE_ERROR`。

Label 的顺序不承担业务语义；输出需要固定顺序时使用 `coll.sort()`。结果反映当前 execution 所选 Snapshot 和可见 Node。

<!-- example: fn-labels -->
```cypher
CREATE (n:Person:Reader) RETURN coll.sort(labels(n)) AS x
```

结果：`["Person","Reader"]`（列 `x` 的值）。

<a id="fn-nodes"></a>

## nodes()

<!-- function-signatures: nodes -->
```text
nodes(input :: PATH) :: LIST<NODE>
```
<!-- /function-signatures -->

把 Path `input` 转为按路径遍历顺序排列的 `LIST<NODE>`。节点重复经过时会重复出现在结果中；零 Relationship 的路径包含它的唯一节点。只接受一个参数，null 返回 `null`；其它非 Path 值产生 `TYPE_ERROR`。

返回的是 Node 值，可继续读取属性、调用 `labels()`，或 `UNWIND nodes(p)` 进入行处理。图值的宿主 JSON 编码见 [Values](../../reference/values.md)。

<!-- example: fn-nodes -->
```cypher
CREATE (a {name: '甲'})-[:R]->(b {name: '乙'}) WITH a MATCH p=(a)-[:R]->(b) RETURN [n IN nodes(p) | n.name] AS x
```

结果：`["甲","乙"]`（列 `x` 的值）。

<a id="fn-range"></a>

## range()

<!-- function-signatures: range -->
```text
range(start :: INTEGER, end :: INTEGER[, step :: INTEGER]) :: LIST<INTEGER>
```
<!-- /function-signatures -->

生成含首尾边界的 Integer 序列，返回 `LIST<INTEGER>`。`start`、`end` 必需，`step` 默认 `1`；负 step 可生成降序，step 方向无法从 start 到达 end 时返回 `[]`。只有步长能到达的 end 才出现在结果里。

任一参数为 null 返回 `null`。所有非 null 参数必须为 Integer，step 不能为 `0`，否则 `TYPE_ERROR`。单次最多产生 2,000,000 项，超过产生 `RESOURCE_ERROR`；Integer 加法超界时停止，不绕回。

<!-- example: fn-range -->
```cypher
RETURN [range(1, 5, 2), range(3, 1, -1), range(3, 1), range(2, 2)] AS x
```

结果：`[[1,3,5],[3,2,1],[],[2]]`（列 `x` 的值）。

<a id="fn-reduce"></a>

## reduce()

<!-- function-signatures: reduce -->
```text
reduce(accumulator :: VARIABLE = initial :: ANY, variable :: VARIABLE IN list :: LIST<ANY> expression :: ANY) :: ANY
```
<!-- /function-signatures -->

沿 List 按顺序更新一个局部 accumulator，返回最后 accumulator，可以是任意类型。实际调用语法为：

```text
reduce(accumulator = initial, variable IN list | expression)
```

`initial` 是初始值；每步把旧 accumulator 和当前 element 绑定到局部变量后计算 expression，再把结果作为下一步的 accumulator。局部变量只在该表达式内可见，不增加查询结果列；expression 也可使用外层可见变量。

空列表返回 initial，列表为 null 返回 `null`；动态输入不是 List 时产生 `TYPE_ERROR`，明显非法的字面 IN 输入在静态检查时先产生 `SEMANTIC_ERROR`。initial 或中间 accumulator 可以是 null，其后的行为由 expression 的 null 规则决定。输入中 null 项不会自动跳过。上面的 introspection 签名用来描述参数，不代替包含 `|` 的实际语法。

<!-- example: fn-reduce -->
```cypher
RETURN [reduce(total = 0, n IN [1, 2, 3] | total + n), reduce(total = 7, n IN [] | total + n), reduce(total = 0, n IN null | total + n)] AS x
```

结果：`[6,7,null]`（列 `x` 的值）。

<a id="fn-relationships"></a>

## relationships()

<!-- function-signatures: relationships -->
```text
relationships(input :: PATH) :: LIST<RELATIONSHIP>
```
<!-- /function-signatures -->

按 Path `input` 的遍历顺序返回 `LIST<RELATIONSHIP>`。只接受一个参数；返回元素保留 Relationship identity/type/属性以及存储方向，不因路径从反方向遍历而交换真实端点。路径重复经过同一 Relationship 时，结果也会重复该值。

零 Relationship 的路径返回 `[]`，null 返回 `null`，非 Path 输入产生 `TYPE_ERROR`。可继续使用 `type(r)` 或 `r.property` 投影结果。

<!-- example: fn-relationships -->
```cypher
CREATE (a)-[:READ {order: 1}]->(b) WITH a MATCH p=(a)-[:READ]->(b) RETURN [r IN relationships(p) | r.order] AS x
```

结果：`[1]`（列 `x` 的值）。

<a id="fn-reverse"></a>

## reverse()

<!-- function-signatures: reverse -->
```text
reverse(input :: LIST<ANY>) :: LIST<ANY>
```
<!-- /function-signatures -->

返回 List `input` 的元素逆序副本；只接受一个参数，元素的类型和 null 保留，不递归反转嵌套 List。空列表返回 `[]`，外层 null 返回 `null`。

String 输入使用 [String overload](string.md#fn-reverse)，其它类型产生 `TYPE_ERROR`。它改变列表内次序，不保证查询输出行的次序；后者使用 `ORDER BY`。

<!-- example: fn-list-reverse -->
```cypher
RETURN [reverse([1, null, [2, 3]]), reverse([]), reverse(null)] AS x
```

结果：`[[[2,3],null,1],[],null]`（列 `x` 的值）。

<a id="fn-tail"></a>

## tail()

<!-- function-signatures: tail -->
```text
tail(input :: LIST<ANY>) :: LIST<ANY>
```
<!-- /function-signatures -->

删除 List `input` 的第一项并返回其余项组成的新 List；只接受一个参数，不递归处理嵌套项。空列表和单元素列表返回 `[]`，null 返回 `null`，非 List 输入产生 `TYPE_ERROR`。

结果中的 null 元素保留。需要第一项使用 `head()`；需要末项使用 `last()`。

<!-- example: fn-tail -->
```cypher
RETURN [tail([1, null, 3]), tail([1]), tail([]), tail(null)] AS x
```

结果：`[[null,3],[],[],null]`（列 `x` 的值）。

<a id="fn-tobooleanlist"></a>

## toBooleanList()

<!-- function-signatures: toBooleanList -->
```text
toBooleanList(input :: LIST<ANY>) :: LIST<BOOLEAN>
```
<!-- /function-signatures -->

逐项把 List `input` 转为 Boolean，返回同长度 `LIST<BOOLEAN>`。Boolean 原样保留，Integer 的 0 为 false、非 0 为 true，String 仅接受忽略大小写的 `true`/`false`；不自动 trim。null 元素、不可解析文本和不支持类型的元素转换为 null。

只接受一个参数，空列表返回 `[]`，外层 null 返回 `null`；非 List 的外层输入产生 `TYPE_ERROR`。与严格单值 `toBoolean()` 不同，单个坏元素不会使整个转换失败。

<!-- example: fn-tobooleanlist -->
```cypher
RETURN toBooleanList([true, 0, 2, 'FALSE', ' true ', 1.5, null]) AS x
```

结果：`[true,false,true,false,null,null,null]`（列 `x` 的值）。

<a id="fn-tofloatlist"></a>

## toFloatList()

<!-- function-signatures: toFloatList -->
```text
toFloatList(input :: VECTOR | LIST<ANY>) :: LIST<FLOAT>
```
<!-- /function-signatures -->

逐项将 List 或 Vector `input` 转为 `LIST<FLOAT>`。Float 保留，Integer 转为 Float，String 先去两端空白再解析；null、不可解析文本及不支持类型的项变为 null。Vector 按原坐标顺序转换，窄坐标扩展到 Float 值。

只接受一个参数；空列表返回 `[]`，外层 null 返回 `null`。非 List/Vector 外层输入产生 `TYPE_ERROR`。大 Integer 转 Float 可能丢失精度；需要保留精确整数时不要转 Float。

<!-- example: fn-tofloatlist -->
```cypher
RETURN [toFloatList([1, ' 2.5 ', 'bad', true, null]), toFloatList(vector([1, 2], 2, INTEGER8))] AS x
```

结果：`[[1.0,2.5,null,null,null],[1.0,2.0]]`（列 `x` 的值）。

<a id="fn-tointegerlist"></a>

## toIntegerList()

<!-- function-signatures: toIntegerList -->
```text
toIntegerList(input :: VECTOR | LIST<ANY>) :: LIST<INTEGER>
```
<!-- /function-signatures -->

逐项把 List 或 Vector `input` 转为 `LIST<INTEGER>`。Integer 保留，Boolean 为 1/0；有限且在 Integer64 范围的 Float 向零截断。String 去除两端空白后尝试整数或浮点解析，再按同样规则截断。非法文本、非有限/越界数、不支持类型及 null 项变为 null。

只接受一个参数；空列表返回 `[]`，外层 null 返回 `null`。非 List/Vector 外层输入产生 `TYPE_ERROR`，坏元素不会中断整列转换。

<!-- example: fn-tointegerlist -->
```cypher
RETURN [toIntegerList([2.9, -2.9, ' 4.8 ', true, 'bad', null]), toIntegerList(vector([1.2, -1.2], 2, FLOAT64))] AS x
```

结果：`[[2,-2,4,1,null,null],[1,-1]]`（列 `x` 的值）。

<a id="fn-tostringlist"></a>

## toStringList()

<!-- function-signatures: toStringList -->
```text
toStringList(input :: LIST<ANY>) :: LIST<STRING>
```
<!-- /function-signatures -->

逐项对 List `input` 应用本 Profile 的 [toString()](string.md#fn-tostring) 转换，返回同长度 `LIST<STRING>`。可转换的数值、Boolean、String、Temporal/Duration、UUID 和 Point 生成文本；null 或不支持转换的容器/图元素/Vector 项变为 null。

只接受一个参数。空列表返回 `[]`，外层 null 返回 `null`；非 List 外层输入产生 `TYPE_ERROR`。它不递归序列化嵌套 Map/List；需要宿主 JSON 时使用 [Values 编码](../../reference/values.md)。

<!-- example: fn-tostringlist -->
```cypher
RETURN toStringList([1, true, '图', {a: 1}, [2], null]) AS x
```

结果：`["1","true","图",null,null,null]`（列 `x` 的值）。

<a id="error-examples"></a>

## 失败示例

以下示例各自在已初始化的空数据库执行；预期为指定错误 category，宿主应检查 SQLite 返回码和 `LITHOGRAPH_` 前缀。

range 的 step 不能为 0。

<!-- example: fn-range-zero-step-error -->
```cypher
RETURN range(1, 3, 0)
```

预期：`TYPE_ERROR`。

coll.insert 不支持负 index。

<!-- example: fn-coll-insert-index-error -->
```cypher
RETURN coll.insert([1], -1, 2)
```

预期：`TYPE_ERROR`。

空列表没有可删除的位置。

<!-- example: fn-coll-remove-index-error -->
```cypher
RETURN coll.remove([], 0)
```

预期：`TYPE_ERROR`。

flatten 的 depth 不能为负。

<!-- example: fn-coll-flatten-depth-error -->
```cypher
RETURN coll.flatten([[1]], -1)
```

预期：`TYPE_ERROR`。

逐项宽容转换不放宽外层 List 类型。

<!-- example: fn-tobooleanlist-input-error -->
```cypher
RETURN toBooleanList(1)
```

预期：`TYPE_ERROR`。

## 相关主题

[表达式](../expressions.md) · [类型](../types.md) · [null 与排序](../null-and-ordering.md) · [错误参考](../../reference/errors.md)

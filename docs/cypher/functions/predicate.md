# 谓词函数

适用版本：Lithograph **v0.3.3** · Cypher Profile **`CY25-2026.08`**。

[Cypher 手册](../index.md) · [完整函数索引](../../reference/functions.md)

本页覆盖全部已注册 Predicate 函数。返回 true/false/null 的结果可投影到 `RETURN`，也可作为 `WHERE` 条件；`WHERE` 只保留 true，对 false 和 null 都不保留行。

`all()`、`any()`、`none()`、`single()`、`allReduce()` 有局部变量语法，不能把 introspection 中的参数表直接改写为普通逗号参数调用。普通列表谓词不承诺逐项短路，不要依赖某个 true/false 掩盖其它元素的类型错误。当 IN 输入是明显非 List 的字面量时，静态检查会先返回 `SEMANTIC_ERROR`；动态参数产生非法输入时返回 `TYPE_ERROR`。共有 null 规则见 [null 与排序](../null-and-ordering.md)。[Neo4j Predicate functions](https://neo4j.com/docs/cypher-manual/25/functions/predicate/) 用于标准语义核对（2026-10-15），以本页冻结版本为准。

## 函数导航

[all()](#fn-all) | [allReduce()](#fn-allreduce) | [any()](#fn-any) | [exists()](#fn-exists) | [isEmpty()](#fn-isempty) | [none()](#fn-none) | [single()](#fn-single)

<a id="fn-all"></a>

## all()

<!-- function-signatures: all -->
```text
all(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN
```
<!-- /function-signatures -->

当 List `list` 的每一项都满足 predicate 时返回 true。实际调用为 `all(variable IN list [WHERE predicate])`；variable 是每步局部绑定的元素名，不是普通参数值。省略 WHERE 时列表项本身必须是 Boolean 或 null。

只要有 false 就为 false；没有 false 但出现 null 结果时为 null；其它情况为 true，所以空列表为 true。列表为 null 返回 null，非 List 或非 Boolean/null 的 predicate 产生 `TYPE_ERROR`。局部 variable 不泄漏到外部作用域。

<!-- example: fn-all -->
```cypher
RETURN [all(n IN [2, 4] WHERE n % 2 = 0), all(n IN [] WHERE false), all(n IN [true, null]), all(n IN [false, null])] AS x
```

结果：`[true,true,null,false]`（列 `x` 的值）。

<a id="fn-allreduce"></a>

## allReduce()

<!-- function-signatures: allReduce -->
```text
allReduce(accumulator = initial, stepVariable IN list | reductionFunction, predicate) :: BOOLEAN
```
<!-- /function-signatures -->

在每次归约之后检查 accumulator 是否满足 predicate，返回 Boolean 或 null。实际调用为：

```text
allReduce(accumulator = initial, stepVariable IN list | reductionFunction, predicate)
```

initial 和 list 在外层作用域求值；每步先把旧 accumulator 与当前元素绑定后计算 reductionFunction，再用**更新后的 accumulator**和当前元素计算 predicate。局部绑定只在函数内可见，可以结合外层变量。它不返回最终 accumulator，那个用途使用 [reduce()](list.md#fn-reduce)。

某步 predicate 为 false 时立即返回 false；若无 false 但至少一步为 null，返回 null；否则为 true。空列表直接为 true，不对 initial 单独检查 predicate。null 列表返回 null，非 List 或非 Boolean/null predicate 产生 `TYPE_ERROR`；归约表达式自身的错误正常传播。

<!-- example: fn-allreduce -->
```cypher
RETURN [allReduce(total = 0, n IN [1, 2] | total + n, total < 4), allReduce(total = 0, n IN [1, 3] | total + n, total < 4), allReduce(total = 0, n IN [] | total + n, false), allReduce(total = 0, n IN [1] | total + n, null), allReduce(total = 0, n IN null | total + n, total > 0)] AS x
```

结果：`[true,false,true,null,null]`（列 `x` 的值）。

<a id="fn-any"></a>

## any()

<!-- function-signatures: any -->
```text
any(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN
```
<!-- /function-signatures -->

存在至少一个满足 predicate 的 List 元素时返回 true。实际语法为 `any(variable IN list [WHERE predicate])`；variable 是局部元素绑定，省略 WHERE 时直接检查 Boolean/null 列表项。

至少一个 true 的结果为 true；没有 true 且出现 null 时为 null；其它情况为 false，所以空列表为 false。null 列表返回 null。非 List 输入或非 Boolean/null 的 predicate 产生 `TYPE_ERROR`；不以一般数值/字符串的“真值”代替 Boolean。

<!-- example: fn-any -->
```cypher
RETURN [any(n IN [1, 3] WHERE n > 2), any(n IN [] WHERE true), any(n IN [false, null]), any(n IN [true, null])] AS x
```

结果：`[true,false,null,true]`（列 `x` 的值）。

<a id="fn-exists"></a>

## exists()

<!-- function-signatures: exists -->
```text
exists(input :: ANY) :: BOOLEAN
```
<!-- /function-signatures -->

判断当前可见图中是否存在满足 pattern expression 的匹配，返回 Boolean；不会返回匹配行，也不会让外层行按匹配数量倍增。实际调用是 `exists((boundNode)-[:TYPE]->())` 一类 pattern expression。已绑定变量用于相关匹配，遵循当前 Snapshot、方向、Label/Type、属性及 Graph View。

introspection 的 `input :: ANY` 不表示任意值合法。`exists(1)`、`exists(n.property)` 属于 `SEMANTIC_ERROR`；属性存在性可使用 `n.property IS NOT NULL` 或语言 predicate `PROPERTY_EXISTS(n, propertyName)`，更复杂存在查询用 `EXISTS { … }` 子查询。关联模式无匹配时返回 false，包括已绑定端点为 null 的情形。

<!-- example: fn-exists -->
```cypher
CREATE (a:Reader)-[:READS]->(b:Book) RETURN [exists((a)-[:READS]->()), exists((b)-[:READS]->())] AS x
```

结果：`[true,false]`（列 `x` 的值）。

<!-- example: fn-exists-null-endpoint -->
```cypher
OPTIONAL MATCH (n:Missing) RETURN exists((n)--()) AS x
```

结果：`false`；没有节点绑定时也不存在对应 Relationship 匹配。

<a id="fn-isempty"></a>

## isEmpty()

<!-- function-signatures: isEmpty -->
```text
isEmpty(input :: LIST<ANY> | MAP | STRING) :: BOOLEAN
```
<!-- /function-signatures -->

判断 List、Map 或 String `input` 的外层项数/字符数是否为 0，返回 Boolean。`[]`、`{}` 和 `''` 为 true；`[null]`、带 null 字段的 Map 和空白字符串均不是空。

只接受一个参数，没有默认值。null 返回 null；Node、Relationship、Path、Vector 或其它不支持类型产生 `TYPE_ERROR`。属性不存在应使用 `IS NULL`，不能用 `isEmpty()` 把 null 等同于空值。

<!-- example: fn-isempty -->
```cypher
RETURN [isEmpty([]), isEmpty({}), isEmpty(''), isEmpty([null]), isEmpty({a: null}), isEmpty(' '), isEmpty(null)] AS x
```

结果：`[true,true,true,false,false,false,null]`（列 `x` 的值）。

<a id="fn-none"></a>

## none()

<!-- function-signatures: none -->
```text
none(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN
```
<!-- /function-signatures -->

没有 List 元素满足 predicate 时返回 true；实际语法为 `none(variable IN list [WHERE predicate])`。variable 在函数内部绑定每个元素，省略 WHERE 时检查元素本身，它必须是 Boolean/null。

出现 true 时返回 false；没有 true 但有 null 时返回 null；其余为 true，所以空列表为 true。null 列表返回 null，非 List 输入或非法 predicate 类型产生 `TYPE_ERROR`。它与 `NOT any(…)` 对应，仍保留三值逻辑。

<!-- example: fn-none -->
```cypher
RETURN [none(n IN [1, 3] WHERE n % 2 = 0), none(n IN [] WHERE true), none(n IN [false, null]), none(n IN [true, null])] AS x
```

结果：`[true,true,null,false]`（列 `x` 的值）。

<a id="fn-single"></a>

## single()

<!-- function-signatures: single -->
```text
single(variable :: ANY, list :: LIST<ANY>, predicate :: ANY) :: BOOLEAN
```
<!-- /function-signatures -->

当且仅当 List 中**恰好一项**满足 predicate 时返回 true，实际语法为 `single(variable IN list [WHERE predicate])`。它统计匹配元素，不消除重复值；两个相等的元素各自都满足时仍算两项。

超过一项为 true 时为 false；否则存在未知 predicate/null 时为 null；没有未知项时根据 true 项是否恰为一项决定。空列表为 false，null 列表为 null，非法列表或 predicate 类型产生 `TYPE_ERROR`。省略 WHERE 时元素必须是 Boolean/null。

<!-- example: fn-single -->
```cypher
RETURN [single(n IN [1, 2, 3] WHERE n = 2), single(n IN [] WHERE true), single(n IN [true, null]), single(n IN [true, true, null])] AS x
```

结果：`[true,false,null,false]`（列 `x` 的值）。

<a id="error-examples"></a>

## 失败示例

以下示例各自在已初始化的空数据库执行；预期为指定错误 category，宿主应检查 SQLite 返回码和 `LITHOGRAPH_` 前缀。

predicate 不使用 Integer 真值转换。

<!-- example: fn-all-predicate-error -->
```cypher
RETURN all(n IN [1] WHERE n)
```

预期：`TYPE_ERROR`。

IN 后必须是 List 或 null；这个字面 Integer 在静态检查时拒绝。

<!-- example: fn-single-list-error -->
```cypher
RETURN single(n IN 1 WHERE true)
```

预期：`SEMANTIC_ERROR`。

exists 只接受 pattern expression。

<!-- example: fn-exists-argument-error -->
```cypher
RETURN exists(1)
```

预期：`SEMANTIC_ERROR`。

isEmpty 不接受 Integer。

<!-- example: fn-isempty-input-error -->
```cypher
RETURN isEmpty(1)
```

预期：`TYPE_ERROR`。

## 相关主题

[表达式](../expressions.md) · [类型](../types.md) · [null 与排序](../null-and-ordering.md) · [错误参考](../../reference/errors.md)

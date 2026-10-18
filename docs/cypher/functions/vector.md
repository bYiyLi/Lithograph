# Vector 函数

适用版本：**v0.3.2** · Cypher Profile：`CY25-2026.08`。

[Cypher 手册](../index.md) · [函数索引](../../reference/functions.md) · [值类型](../types.md) · [宿主 JSON 编码](../../reference/values.md)

本页包含 `vector()` 构造器及 Vector 数值函数；构造器在 `SHOW FUNCTIONS` 中分类为 Scalar。向量是带坐标类型与维度的值，不等于普通 List。存储、类型约束和 Raw Vector SEARCH 分别见[值类型](../types.md)、[约束](../constraints.md)和[检索](../search.md)。

这些函数不调用 Embedding Provider，不生成文本 embedding。Vector 相似度函数还接受 numeric List；distance、norm 和 dimension_count 要求真正的 Vector。任何整个必需参数为 null 都返回 null；List 内的 null 不构成合法坐标。

距离、norm 与 similarity 运算使用 Float32 计算坐标与中间结果，然后返回 Cypher Float；即使输入 Vector 保存的是 FLOAT64 或 INTEGER64，也应考虑这一步的精度。不要依赖大整数的逐位差异或超出 Float32 精度的微小差值来设计距离判据。

<a id="fn-vector"></a>

## `vector()`

<!-- function-signatures: vector -->
```text
vector(vectorValue :: STRING | LIST<INTEGER | FLOAT>, dimension :: INTEGER, coordinateType :: [INTEGER64, INTEGER32, INTEGER16, INTEGER8, FLOAT64, FLOAT32]) :: VECTOR
```
<!-- /function-signatures -->

从数值 List 或逗号分隔的数值 String 构造 `VECTOR`。三个参数都必需：`vectorValue` 提供坐标，`dimension` 为 1–4096 的 Integer且与坐标数相等；`coordinateType` 是类型名，推荐使用不带引号的 keyword。支持 `INTEGER8/16/32/64`、`FLOAT32/64`；整型坐标要求数值为整数并在声明宽度内，Float32 会发生精度舍入。

String 例如 `'[1,2,3]'`；不能含非数值项。声明 dimension 不会自动截断或补零。整型 List 和整数字面量 String 在构造为 `INTEGER64` 时保留完整 64 位精度；Float 输入按其已有数值转换，不能恢复此前已舍入的整数。整个参数 null 返回 null；非数值/非有限坐标、长度或类型不匹配、超范围整数产生 `TYPE_ERROR`；静态非法维度或类型名也可能在 semantic 阶段拒绝。Vector 不能用 List subscript 直接取坐标；可先通过 [`toIntegerList()`](list.md#fn-tointegerlist) 或 [`toFloatList()`](list.md#fn-tofloatlist) 转换为 List，或在宿主解码 tagged value。

<!-- example: fn-vector -->
```cypher
RETURN vector_dimension_count(vector('[1,2,3]',3,INTEGER16)) AS fromString,
       vector_dimension_count(vector([1.0,2.0],2,FLOAT32)) AS fromList
```

预期 `['fromString', 'fromList']` 对应一行 `[3, 2]`。

构造和存储保留坐标的声明类型；前述 Float32 限制针对距离计算。下面直接比较构造后的整数值，不经过 distance/similarity：

<!-- example: fn-vector-integer64-exact -->
```cypher
RETURN head(toIntegerList(vector([9007199254740993],1,INTEGER64))) = 9007199254740993 AS exact,
       head(toIntegerList(vector('[9223372036854775807]',1,INTEGER64))) = 9223372036854775807 AS maxExact
```

预期 `exact` 与 `maxExact` 都为 `true`。

<a id="fn-vector_dimension_count"></a>

## `vector_dimension_count()`

<!-- function-signatures: vector_dimension_count -->
```text
vector_dimension_count(vector :: VECTOR) :: INTEGER
```
<!-- /function-signatures -->

唯一参数必须为 Vector，返回 Integer 维度。它不计算范数、非零坐标数或 Index dimension。null 返回 null；普通 List 等非 Vector 输入产生 `TYPE_ERROR`。

<!-- example: fn-vector_dimension_count -->
```cypher
RETURN vector_dimension_count(vector([1,0,0],3,INTEGER8)) AS dimension
```

预期 `['dimension']` 对应一行 `[3]`。

<a id="fn-vector-similarity-cosine"></a>

## `vector.similarity.cosine()`

<!-- function-signatures: vector.similarity.cosine -->
```text
vector.similarity.cosine(a :: VECTOR | LIST<INTEGER | FLOAT>, b :: VECTOR | LIST<INTEGER | FLOAT>) :: FLOAT
```
<!-- /function-signatures -->

两个必需参数各自接受 Vector 或非空 numeric List，可混合使用，维度必须相等。返回归一到 `[0,1]` 的 Float：`(1 + cosine(a,b)) / 2`，相同方向为 1，正交为 0.5，反方向为 0。数值边缘结果夹在范围内。

两边都必须是有限坐标且有非零 norm；零向量、维度不等、空 List、List 中 null/非数值或其它输入类型产生 `TYPE_ERROR`。任一整个参数 null 返回 null。它返回相似度，不等于 `vector_distance(...,COSINE)`。

<!-- example: fn-vector-similarity-cosine -->
```cypher
RETURN vector.similarity.cosine([1,0],[0,1]) AS similarity
```

预期 `['similarity']` 对应一行 `[0.5]`。

<a id="fn-vector-similarity-euclidean"></a>

## `vector.similarity.euclidean()`

<!-- function-signatures: vector.similarity.euclidean -->
```text
vector.similarity.euclidean(a :: VECTOR | LIST<INTEGER | FLOAT>, b :: VECTOR | LIST<INTEGER | FLOAT>) :: FLOAT
```
<!-- /function-signatures -->

两个必需参数接受 Vector 或非空 numeric List，维度相等且坐标有限。返回 `(1 / (1 + sum((aᵢ-bᵢ)²)))`，相同向量为 1，越远越接近 0；这是基于平方距离的相似度。零向量合法。

null 参数返回 null；空 List、维度不等、非数值/null坐标及不支持的输入类型产生 `TYPE_ERROR`。不要把结果当作实际 Euclidean 距离。

<!-- example: fn-vector-similarity-euclidean -->
```cypher
RETURN vector.similarity.euclidean([0,0],[1,0]) AS similarity
```

预期 `['similarity']` 对应一行 `[0.5]`。

<a id="fn-vector_distance"></a>

## `vector_distance()`

<!-- function-signatures: vector_distance -->
```text
vector_distance(vector1 :: VECTOR, vector2 :: VECTOR, vectorDistanceMetric :: [EUCLIDEAN, EUCLIDEAN_SQUARED, MANHATTAN, COSINE, DOT, HAMMING]) :: FLOAT
```
<!-- /function-signatures -->

两个 Vector 与 metric 都必需；metric 推荐写成 keyword，也接受对应 String，大小写不敏感。维度必须相等；坐标类型可以不同。返回 Float，metric 的定义如下。

| metric | 计算与解释 |
| --- | --- |
| `EUCLIDEAN` | `sqrt(sum((aᵢ-bᵢ)²))` |
| `EUCLIDEAN_SQUARED` | `sum((aᵢ-bᵢ)²)` |
| `MANHATTAN` | `sum(abs(aᵢ-bᵢ))` |
| `COSINE` | `1 - cosine(a,b)`；非零 norm 要求 |
| `DOT` | `-sum(aᵢ*bᵢ)`，可为负数 |
| `HAMMING` | 坐标数值不同的位置数，以 Float 返回；不计算二进制 bit distance |

null 参数返回 null；非 Vector、维度不等、COSINE 零 norm 产生 `TYPE_ERROR`。未知 metric 产生 `SEMANTIC_ERROR`。用于 SEARCH 排序时应选择与 Index 相同的度量，不将 distance 数值直接解释成 similarity score。

<!-- example: fn-vector_distance -->
```cypher
RETURN vector_distance(vector([0,0],2,FLOAT32),
                       vector([3,4],2,FLOAT32),EUCLIDEAN) AS distance,
       vector_distance(vector([1,2],2,INTEGER8),
                       vector([3,4],2,INTEGER8),DOT) AS negativeDot
```

预期 `['distance', 'negativeDot']` 对应一行 `[5.0, -11.0]`。

<a id="fn-vector_norm"></a>

## `vector_norm()`

<!-- function-signatures: vector_norm -->
```text
vector_norm(vector :: VECTOR, vectorDistanceMetric :: [EUCLIDEAN, MANHATTAN]) :: FLOAT
```
<!-- /function-signatures -->

Vector 与 metric 都必需，返回 Float。`EUCLIDEAN` 计算 `sqrt(sum(aᵢ²))`，`MANHATTAN` 计算 `sum(abs(aᵢ))`；metric 使用 keyword 或对应 String，大小写不敏感。零向量返回 0。null 参数返回 null；非 Vector 产生 `TYPE_ERROR`，不支持的 metric 产生 `SEMANTIC_ERROR`。

<!-- example: fn-vector_norm -->
```cypher
RETURN vector_norm(vector([3,4],2,INTEGER16),EUCLIDEAN) AS euclidean,
       vector_norm(vector([3,4],2,INTEGER16),MANHATTAN) AS manhattan
```

预期 `['euclidean', 'manhattan']` 对应一行 `[5.0, 7.0]`。

## null 与失败示例

<!-- example: fn-vector-null -->
```cypher
RETURN vector.similarity.cosine(null,[1,0]) AS missing,
       vector_dimension_count(null) AS noDimension
```

预期 `['missing', 'noDimension']` 对应一行 `[null, null]`。

<!-- example: fn-vector-zero-cosine -->
```cypher
RETURN vector.similarity.cosine([0,0],[1,0]) AS value
```

预期错误类别：`TYPE_ERROR`。

<!-- example: fn-vector-unknown-metric -->
```cypher
RETURN vector_norm(vector([1],1,FLOAT32),'unknown') AS value
```

预期错误类别：`SEMANTIC_ERROR`。

标准语义参考：[Neo4j Cypher 25 vector functions](https://neo4j.com/docs/cypher-manual/25/functions/vector/)。Raw Vector 与 Managed Semantic 的接口和执行边界以本产品手册为准。

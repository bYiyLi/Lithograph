# 开发 Embedding Provider

适用版本：**v0.3.3 / EmbeddingProviderV1**。上级：[开发者手册](README.md) · 精确接口：[Provider SPI](../reference/embedding-provider-spi.md) · 随附生产实现：[OpenAI-compatible](../reference/openai-compatible-provider.md)。

本教程使用仓库已有的 C synthetic fixture 跑通 **编译 → SQLite 注册 → 本地 validate → Semantic embedding → 错误清理**。fixture 用文本 hash 生成确定性向量，不理解自然语言，只用于集成验证；产品 Provider 在同样 SPI 后实现自己的模型或网络调用。

<a id="build-fixture"></a>

## 1. 编译本地 fixture

在仓库根目录准备 C11 compiler 和 SQLite 3.45.0+ 开发头文件。macOS 使用 Homebrew SQLite 的示例：

```sh
cargo build --locked -p lithograph-extension
scripts/build-phase13-synthetic-provider.sh /opt/homebrew/opt/sqlite/include target/phase13/synthetic_embedding_provider.dylib
```

Linux 使用实际 SQLite include directory 与 `.so` output。构建脚本支持 macOS/Linux；Windows 开发者按相同 C 源与公开 header 构建 DLL，并提供 SQLite entrypoint。文件名中的 phase13 仅是现有 fixture 路径，不表示另一套插件协议。

输入源码为 [synthetic_embedding_provider.c](../../tests/synthetic_embedding_provider.c)，头文件为 [lithograph_embedding_provider.h](../../include/lithograph_embedding_provider.h)。fixture 不链接 Core，也不链接私有 SQLite；它使用 `sqlite3ext.h` 的 host API。

<a id="load-fixture"></a>

## 2. 在独立演示库加载

```text
.load ./target/debug/liblithograph.dylib
.load ./target/phase13/synthetic_embedding_provider.dylib
SELECT lithograph_init();
SELECT synthetic_embedding_embed_calls('synthetic-a');
```

最后一条应为 0。fixture 同时注册 `synthetic-a` 与 `synthetic-b`，具有各自 context/identity，用于证明多个 Provider 可以共存。同名重复注册报错。fixture 暴露的 `synthetic_embedding_*` SQL functions 仅供测试，不是 Lithograph 产品 API。

下列每段为一条完整 Cypher statement；通过 `SELECT lithograph(?, ?, ?)` 绑定执行，或用 [Python helper](examples/python_quickstart.py) 的 `query(db, text)`。每个示例的输入与期望由 [示例索引](examples/cypher/providers.json) 登记，互相依赖的步骤在同一个隔离 fixture database 顺序执行。

<a id="validate-without-embedding"></a>

## 3. 证明 Schema create 不调用模型

创建一条已知文本：

<!-- example: provider-development-data -->
```cypher
CREATE (:ProviderDocument {name:'Hello', content:'hello'})
```

创建四维 Semantic Index；synthetic 的正常配置为 `{}`：

<!-- example: provider-development-create -->
```cypher
CALL db.index.semantic.createNodeIndex(
  'provider_demo', ['ProviderDocument'], 'content',
  {provider:'synthetic-a', providerConfig:{}, dimensions:4, similarity:'euclidean'}
)
```

两个 statement 都成功且没有结果列。此时检查：

```sql
SELECT synthetic_embedding_validate_calls('synthetic-a') > 0 AS validated,
       synthetic_embedding_embed_calls('synthetic-a') = 0 AS did_not_embed;
```

两列均应为 1。这里的 validate 只检查本地 config、dimensions/FLOAT32；自己的实现不能在 validate 中发 HTTP、加载大型模型或打开外部 cache。

<a id="query-fixture"></a>

## 4. 执行 embedding 并检查输入

<!-- example: provider-development-query -->
```cypher
CALL db.index.semantic.queryNodes('provider_demo', 'hello', {limit:1})
YIELD node, score
RETURN node.name AS name
```

结果为 `name = 'Hello'`。检查实际调用：

```sql
SELECT synthetic_embedding_embed_calls('synthetic-a') > 0 AS embedded,
       synthetic_embedding_embed_inputs('synthetic-a') AS input_count,
       synthetic_embedding_last_inputs('synthetic-a') AS last_inputs;
```

`embedded = 1`，此 fixture 的唯一 source 和 query 都是精确文本 `hello`，一次 execution 去重后 input_count 应为 1，last_inputs 为 `5:hello`。这说明文本长度按 bytes 传入，Provider 不需要 graph owner/Commit/Index identity。

不要把 fixture 的 hash 坐标算法用作真正 Semantic 模型；它只证明 SPI 和实际 SQL execution surface 相连。需要相似度结果时，替换 embed callback，但保留输入顺序、维度、finite FLOAT32 和 release 合同。

<a id="failure-cleanup"></a>

## 5. 验证失败传播

fixture 的 `{validate:'invalid'}` 注入本地配置失败：

<!-- example: provider-development-invalid-config -->
```cypher
CALL db.index.semantic.createNodeIndex(
  'provider_invalid', ['ProviderDocument'], 'content',
  {provider:'synthetic-a', providerConfig:{validate:'invalid'}, dimensions:4, similarity:'euclidean'}
)
```

期望 `INVALID_ARGUMENT`，该 definition 不发布。下面的 definition 可以通过 validate，但实际 embedding 注入 I/O failure：

<!-- example: provider-development-failure-index -->
```cypher
CALL db.index.semantic.createNodeIndex(
  'provider_failure', ['ProviderDocument'], 'content',
  {provider:'synthetic-a', providerConfig:{fail:'io'}, dimensions:4, similarity:'euclidean'}
)
```

<!-- example: provider-development-failure-query -->
```cypher
CALL db.index.semantic.queryNodes('provider_failure', 'hello', {limit:1})
YIELD node, score
RETURN node.name AS name
```

创建成功，查询期望 `IO_ERROR`。失败不能返回部分 top-k。fixture 另有 `fail:'resource'/'cancel'`、非法维度/NaN/零向量等测试注入，属于测试 fixture 配置，不是通用 Provider config。完整 fixture 与宿主边界回归可运行：

```sh
LITHOGRAPH_SQLITE3=/opt/homebrew/opt/sqlite/bin/sqlite3 scripts/synthetic-embedding-provider-smoke.sh target/phase13/synthetic_embedding_provider.dylib
```

Linux 把 SQLite 可执行文件与动态库后缀替换成实际值。该 C smoke 直接验证注册、ABI、callback、取消和析构；本页 Cypher 示例另通过真实 Lithograph SQL surface 验证。

<a id="implement-your-provider"></a>

## 6. 换成自己的实现

保留 fixture 的 registration 生命周期与四个 callbacks，按下列顺序改动：

1. 分配新的区分大小写 provider name 与稳定 semantic_identity；完整设置 ABI version/struct_size；注册前检查 key 冲突。
2. 在 validate 解析自己的 JSON schema，检查未知字段、维度、坐标类型及静态范围；不做外部等待。
3. 在 embed_batch 按 `data/len` 读取 exact texts，用有界 batch/timeout 调模型；按输入顺序返回 `text_count * dimensions` 个有限 float。
4. 通过匹配的 free_batch/free_error 释放 output/error buffer；空结构和失败后的部分 buffer 也必须可清理。不要让 C++ exception/Rust panic 穿过 ABI。
5. 在工作前、批次间、重试等待检查 is_cancelled，映射为 CANCELLED。不要把 borrowed text/config/cancel pointer 留给 callback 返回后的线程。
6. 关闭 SQLite connection 时由 client-data destructor 释放 registration/context，检查重复加载失败不泄漏、SQLITE_NOMEM 不 double-free。

具体 struct、callback 签名、memory ownership、status 映射与 identity 规则只在 [SPI Reference](../reference/embedding-provider-spi.md) 维护。Provider 若自建 cache，需要自己的明确配置、有效请求 identity、失败合同与文件生命周期；不要写 `_lithograph_*`、改变 Commit，或增加独立 application query API。

上线前至少覆盖：正常双扩展加载、两种加载顺序、重复注册、多个 connection、无输出/错误输出释放、维度/NaN 错误、缺失 Provider、取消、create 零模型调用和 shutdown 析构。模型变更时更新 identity 或 deployment salt，固定 binary/model revision；数据库 history 不会快照外部模型。

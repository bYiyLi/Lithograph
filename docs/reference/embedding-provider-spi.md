# Embedding Provider SPI

适用版本：**v0.3.2 / EmbeddingProviderV1**。上级：[接口参考](README.md) · 教程：[开发 Provider](../guide/provider-development.md) · 随附实现：[OpenAI-compatible](openai-compatible-provider.md)。

<a id="public-boundary"></a>

## 公开边界

Provider 是普通 SQLite loadable extension，通过同一个 `sqlite3*` connection 的 client-data 注册，供 Managed Semantic 调用。它没有 application-facing Cypher execution API。C/C++ 声明以仓库公开头文件 [lithograph_embedding_provider.h](../../include/lithograph_embedding_provider.h) 为准；Rust ABI 类型位于 `lithograph-embedding-provider` crate，使用 `#[repr(C)]`，布局与该头文件一致。

Provider 不依赖 Core 内部 API，不安装独立 loader、server 或 daemon。所有 SQLite 调用必须使用加载它的 host API table，包括 Provider 自己打开 cache database 的调用；不能链接另一份 bundled/private SQLite runtime。v0.3.1 最低宿主版本为 SQLite 3.45.0。

<a id="registration"></a>

## 注册与连接生命周期

Provider 名是非空、无 NUL 的 UTF-8 字符串，区分大小写。注册 key 是 `lithograph.embedding.v1/<provider-name>`。注册步骤：

1. SQLite extension init 中安装 host API table，验证最低 SQLite 版本。
2. 调用 `sqlite3_get_clientdata(db, key)`；已有同名注册则返回加载错误，不能替换。
3. 分配 registration/context，完整初始化 V1 结构与 callback。
4. `sqlite3_set_clientdata(db, key, provider_pointer, destructor)` 交付给 SQLite。
5. registration、identity、context 和 callback 在 connection lifetime 内保持有效。SQLite 关闭 connection 时调用 destructor，释放 Provider state。

注册与 Lithograph 的加载顺序不限。每个 connection 单独注册，不能把另一 connection 的 pointer 借来执行。`sqlite3_set_clientdata` 返回 `SQLITE_NOMEM` 时 SQLite 会调用传入 destructor；实现不能再释放同一对象。其它错误按所用 SQLite API 的 ownership 规则处理。不要在仍可能执行 callback 时主动卸载或替换注册。

Host 读取 `abi_version/struct_size` 前缀，要求 ABI version 为 1、size 至少为当前 V1 size，再检查 identity 和四个必需 callback。结构布局要使用正常 C ABI alignment，不能启用 packed；不兼容时 fail closed。原生 extension 本身拥有进程级权限，size 检查不能把恶意 dangling pointer 变成安全内存。

<a id="provider-layout"></a>

## Provider 结构与 identity

```c
typedef struct lithograph_embedding_provider_v1 {
    uint32_t abi_version;
    size_t struct_size;
    void *context;
    const unsigned char *semantic_identity;
    size_t semantic_identity_len;
    lithograph_embedding_validate_v1 validate;
    lithograph_embedding_embed_batch_v1 embed_batch;
    lithograph_embedding_free_batch_v1 free_batch;
    lithograph_embedding_free_error_v1 free_error;
} lithograph_embedding_provider_v1;
```

| 字段 | 合同 |
| --- | --- |
| `abi_version` | `LITHOGRAPH_EMBEDDING_PROVIDER_ABI_VERSION_V1`，值为 1 |
| `struct_size` | `sizeof(lithograph_embedding_provider_v1)`；Host 至少要求覆盖完整 V1 |
| `context` | Provider 自己的 opaque state，原样传给 callbacks；Host 不释放、不解释 |
| `semantic_identity/len` | 非空 UTF-8，1..256 bytes；Provider 持有，不需要 NUL terminator；registration lifetime 内稳定 |
| 四个 callback | 必须均为非 NULL；指向与宿主 architecture/calling convention 匹配的函数 |

设计中的 `semanticIdentity()` 在 V1 C ABI 中表现为以上 pointer/length 字段，**不存在额外的 identity callback**。它表示 implementation/runtime 的向量语义版本，不是 versioned Schema 字段。实现、模型映射或归一化语义改变时，应在新的 registration/connection lifecycle 中使用新的 identity；执行中不能热切换。

Lithograph 用它隔离 TEMP Semantic materialization；execution 内的 text 去重还包含 provider name、完整 canonical providerConfig、dimensions 和 FLOAT32。它不替 Provider 建立 persistent result cache，也不能证明远程模型不会漂移。OpenAI-compatible 的固定 implementation identity 为 `lithograph-openai-compatible/v1`；该 Provider 的可选 `providerConfig.semantic_identity` 是另一项由调用方管理的配置 salt。

<a id="validate"></a>

## validate：纯本地检查

```c
int validate(
    void *context,
    const unsigned char *config_json, size_t config_len,
    size_t dimensions, uint32_t coordinate_type,
    lithograph_embedding_error_v1 *error
);
```

`config_json/config_len` 是借用的 UTF-8 JSON bytes，表达完整 providerConfig Map；不是以 NUL 结尾的 C string。只在 callback 期间有效，不能保存其 pointer。`dimensions` 为 Index 维度，v0.3.1 Managed Semantic 范围为 1..4096；coordinate type 固定 `LITHOGRAPH_EMBEDDING_COORDINATE_FLOAT32_V1`（1）。

validate 只能执行 bounded 本地配置、维度和能力检查，不能联网、枚举 corpus、调用大型模型或打开/维护外部 cache。创建或发布改变的 Semantic definition 会调用它；每次实际 query/rebuild 也解析当前 Provider 并验证。需要运行时环境变量、凭据或网络可用性时，应在 embed 阶段解决，不能让 Schema publication 等待外部服务。

<a id="embed-batch"></a>

## embed_batch：输入、输出与取消

```c
int embed_batch(
    void *context,
    const unsigned char *config_json, size_t config_len,
    const lithograph_embedding_text_v1 *texts, size_t text_count,
    size_t dimensions, uint32_t coordinate_type,
    lithograph_embedding_cancel_callback_v1 is_cancelled,
    void *cancel_user_data,
    lithograph_embedding_batch_v1 *result,
    lithograph_embedding_error_v1 *error
);
```

`texts` 为 input 顺序的数组，每个 text 是 `{const unsigned char *data; size_t len;}`。len 计 UTF-8 bytes，数据不保证 NUL 结尾，合法 String 也可能含内嵌 NUL，不能使用 `strlen()` 代替长度。配置、texts、每个 text buffer、cancel userdata 都由 Host 借出，仅 callback 期间有效。需要异步内部工作时，在 callback 返回前 join/完成，不能让后台任务继续使用借用地址。

source/query text 按精确值处理，不暗中 trim、lowercase、拼接、truncate。Provider 可以内部 batching、去重或缓存，但必须把结果映射回输入顺序并完整返回。Lithograph 不向 Provider 传 owner ID、Commit 或 Index name，不应为 cache identity 依赖这些信息。

成功结果结构：

```c
typedef struct lithograph_embedding_batch_v1 {
    float *values;
    size_t value_count;
    size_t embedding_count;
    size_t dimensions;
} lithograph_embedding_batch_v1;
```

`embedding_count == text_count`，`dimensions == 请求维度`，`value_count == text_count * dimensions`。`values` 是连续、正确对齐的 row-major FLOAT32 数组：第 i 个 input 的坐标位于 `values[i * dimensions ... (i+1)*dimensions)`。坐标必须 finite，非空 output 的 values 不可为 NULL；安全检查乘法/分配大小 overflow。cosine 的非零向量等 similarity 要求仍由查询语义验证，不能通过返回 NaN/Infinity 或部分 batch 跳过输入。

`is_cancelled(cancel_user_data)` 非零表示取消；callback 可为 NULL。Provider 应在网络/模型工作前、批次之间、重试与有界等待期间检查，取消后返回 `LITHOGRAPH_EMBEDDING_CANCELLED_V1`。Host 在调用前后也检查取消。V1 没有跨线程抢占 callback 的机制；Provider 必须给阻塞 I/O 设置有限 timeout，不能承诺立即打断所有第三方调用。

<a id="ownership-and-errors"></a>

## 内存所有权与状态码

result 与 error 结构本身由 Host 提供并初始化为空；其内部 buffer 由 Provider 分配。Provider 必须在失败前保留可清理的状态，不能返回栈地址。Host 在检查/复制结果后调用 Provider 的 `free_batch(context, result)`；失败和取消路径也可能传入空或部分 result。`free_error(context, error)` 同样会在成功/失败路径调用。

因此两个 free callback 都应接受空结构，仅释放自身分配的 buffer 并清零字段，不释放结构本身、不抛异常、不触发另一个网络操作。即使 Provider 使用自己的 allocator，也只通过其匹配的 free callback 回收，不能要求 Host 直接 `free()`。registration/context 最后由 SQLite destructor 释放，与单次 batch/error 的释放分开。C++ exception/Rust panic 不能越过 C ABI，需在 Provider callback 边界转换为状态。

error 结构为 `{int status; unsigned char *message; size_t message_len;}`，message 是 Provider 持有的长度化文本，不要求 NUL 结尾。返回 int 是 Host 的状态判定依据；保持它与 `error.status` 一致。错误信息不得序列化配置、API key、认证/header value 或私有 input 全文。

| C 常量 | 值 | Lithograph code / 行为 |
| --- | --- | --- |
| `LITHOGRAPH_EMBEDDING_OK_V1` | 0 | 成功；Host 继续验证完整 shape 和 finite 坐标 |
| `LITHOGRAPH_EMBEDDING_INVALID_CONFIG_V1` | 1 | `INVALID_ARGUMENT` |
| `LITHOGRAPH_EMBEDDING_IO_ERROR_V1` | 2 | `IO_ERROR`，含网络、独立 Provider cache I/O |
| `LITHOGRAPH_EMBEDDING_RESOURCE_ERROR_V1` | 3 | `RESOURCE_ERROR` |
| `LITHOGRAPH_EMBEDDING_CANCELLED_V1` | 4 | `INTERRUPTED`，附 SQLite `SQLITE_INTERRUPT` |
| `LITHOGRAPH_EMBEDDING_INTERNAL_ERROR_V1` | 5 | `INTERNAL_ERROR` |

未知返回值、无效 ABI registration、结果 shape/finite 合同违规也按 `INTERNAL_ERROR` 失败；Provider 缺失按 `INVALID_ARGUMENT`。不要用 `STORAGE_ERROR` 表示独立 Provider cache 的问题，该 code 属于 Lithograph main storage。

<a id="execution-boundaries"></a>

## 事务、缓存与部署边界

实际 embed 可发生于普通 committed read、read-write execution 或 active Lithograph explicit transaction。它读取该 execution 当前的 graph/schema state；不能在 Provider 中偷换到另一 connection 的旧 Snapshot。read-write/explicit transaction 已经持有 writer 时，外部等待会延长 writer hold。

Provider 网络调用和独立 cache 不参加 graph rollback。后续 constraint/serialization 失败可以回滚 graph 但不能撤回已完成的外部请求；合法 cache entry 可继续使用。cache 自己负责正确 identity、损坏处理与文件生命周期，不进入 Lithograph main/Commit/integrity/GC。`readOnlyNoExternalIO=true` 会在 prepare 阶段拒绝 Managed Semantic，Provider 不能因“本地模型”或“已有 cache”绕过它。

connection/thread 使用遵循 SQLite connection 自身规则；SPI 不赋予后台线程重新进入同一个 SQLite connection 的权限。多个 connection/process 的 Provider state 若共享文件或模型资源，需要 Provider 自己同步。固定 ABI identity、Provider binary/model revision 和服务资源后才能管理历史可复现性；严格要求 vector 本身属于历史真源时使用 Raw Vector。

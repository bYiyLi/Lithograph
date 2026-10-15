# OpenAI-compatible Embedding Provider

适用版本：**v0.3.1**。上级：[接口参考](README.md) · 操作流程：[使用 Embedding Provider](../guide/embedding-providers.md) · 扩展协议：[Embedding Provider SPI](embedding-provider-spi.md)。

<a id="identity-and-loading"></a>

## 能力与加载

`lithograph-openai-compatible` 是与 Lithograph 平级加载的 SQLite extension。注册名固定为 `openai-compatible`，同一 connection 的注册 key 为 `lithograph.embedding.v1/openai-compatible`。它只实现 Embeddings HTTP 请求，不提供 Chat、Responses、Audio、Images 或 Azure deployment/api-version 接口。

每个实际创建或改变 Semantic Index definition、执行 Semantic query/rebuild 的 connection 都必须加载 Provider。先后加载顺序不限；同一 connection 重复加载同名 Provider 会失败，不会替换已有注册。关闭 connection 时 SQLite 释放注册和 Provider runtime。加载不建立网络连接，不把某个 endpoint 固定到整个进程。

发布包包含对应平台 Provider 动态库；Cargo 构建名为 `liblithograph_openai_compatible.dylib`、`liblithograph_openai_compatible.so` 或 Windows 的 `lithograph_openai_compatible.dll`。SQLite entrypoint 支持 `sqlite3_extension_init` 和 `sqlite3_lithographopenaicompatible_init`。宿主运行时要求与 Lithograph 相同：SQLite 3.45.0+、允许加载受信任的 extension。

<a id="configuration"></a>

## 完整配置

配置通过 Semantic Index create options 中的 `providerConfig` 传入。`dimensions` 与 `similarity` 属于 Index 顶层 options；把它们再次写入 `providerConfig` 会被当作未知字段拒绝。

| key | 类型 | 默认值 / 必填 | 行为与合法范围 |
| --- | --- | --- | --- |
| `base_url` | STRING | `https://api.openai.com/v1` | absolute `http://` / `https://` URL；不能带 query/fragment；去掉尾部 `/` 后追加 `/embeddings` |
| `model` | STRING | 必填 | 非空；原样放入 request body；不使用 model whitelist、不在 create 时联网探测 |
| `api_key` | STRING 或 null | 无 | 直接 Bearer credential；非 null 时必须非空 |
| `api_key_env` | STRING 或 null | 无 | 提供 credential 的环境变量名；非 null 时必须非空；实际使用时在运行时读取 |
| `send_dimensions` | BOOLEAN | `true` | 是否把 Index dimensions 放入 request body；`false` 仍要求结果维度完全匹配 |
| `encoding_format` | STRING | `float` | 只允许 `float` / `base64`，大小写敏感 |
| `user` | STRING 或 null | 无 | 非 null 时放入 body 的 `user` 字段；允许空字符串 |
| `organization` | STRING 或 null | 无 | 生成 `OpenAI-Organization` header；允许空字符串 |
| `project` | STRING 或 null | 无 | 生成 `OpenAI-Project` header；允许空字符串 |
| `headers` | MAP<STRING, STRING> | `{}` | 最后覆盖同名生成 header；HTTP name 大小写不敏感；不允许仅大小写不同的重复 key |
| `timeout_ms` | INTEGER | `30000` | `1..600000`；每次 HTTP attempt 的 global timeout，不是整个 Cypher execution 的总期限 |
| `max_retries` | INTEGER | `2` | `0..8`；初次请求以外的最大 retry 次数，每个 HTTP batch 单独计算 |
| `batch_size` | INTEGER | `32` | `1..2048`；按文本条数划分 HTTP request，不按 token 数切分 |
| `semantic_identity` | STRING 或 null | 无 | 非 null 时为非空、最多 256 UTF-8 bytes；调用方的 embedding-space salt |
| `cache` | MAP | `{enabled: false}` | 完整字段见下表 |

| cache key | 类型 | 默认值 | 行为与合法范围 |
| --- | --- | --- | --- |
| `enabled` | BOOLEAN | `false` | 省略或 false 时不打开、创建、读取、写入 cache 文件 |
| `path` | STRING 或 null | 无 | enabled 时必填且非空；普通 filesystem path，拒绝 `:memory:` 和 `file:` URI；相对路径基于宿主工作目录 |
| `max_bytes` | INTEGER | `1073741824` | `1..9223372036854775807`；vector payload budget，以 bytes 计 |

`cache` 只允许 `enabled/path/max_bytes`。未知顶层或 cache key、错误类型、范围错误返回 `INVALID_ARGUMENT`。除表中可为 null 的可选 STRING 外，显式 null 不等同于省略。所有字符串须合法 UTF-8 且不含 NUL；header name/value 还须通过 HTTP header 格式验证。自定义 header 的 value 总量最多 **60 KiB**，最终 request headers 总量最多 **64 KiB**。

`cache.enabled=false` 时仍检查配置的静态类型、NUL 和 `max_bytes` 范围，但不检查文件是否存在，也不触碰文件；启用后的文件打开、marker/schema 检查和 main identity 检查发生在实际 `embed_batch`，不是 Schema create 的本地 `validate` 中。

配置完整示意如下。值需要按实际服务设置，此 JSON 本身不会发请求：

```json
{
  "base_url": "https://api.openai.com/v1",
  "api_key_env": "LITHOGRAPH_EMBEDDING_API_KEY",
  "model": "text-embedding-3-small",
  "send_dimensions": true,
  "encoding_format": "float",
  "user": "documentation-demo",
  "organization": null,
  "project": null,
  "headers": {},
  "timeout_ms": 30000,
  "max_retries": 2,
  "batch_size": 32,
  "semantic_identity": "deployment-revision-1",
  "cache": {
    "enabled": true,
    "path": "./embedding-cache.db",
    "max_bytes": 1073741824
  }
}
```

<a id="authentication"></a>

## Authentication 与 header 优先级

`Authorization` 的选择顺序是：

1. `headers` 已包含任意大小写形式的 `Authorization`：直接使用该值，不读取未使用的 `api_key_env`。
2. 否则，非 null 的 `api_key`：生成 `Authorization: Bearer <api_key>`。
3. 否则，非 null 的 `api_key_env`：读取指定环境变量并生成 Bearer header；变量不存在、为空或不是有效 credential 时，本次调用返回 `INVALID_ARGUMENT`。
4. 三者都没有：不发送 Authorization。

Provider **不隐式读取 `OPENAI_API_KEY`**。该变量名只有显式配置为 `api_key_env` 后才生效。Schema create 只验证变量名，不要求变量此刻存在；实际查询即使可能命中旧 cache，也需要先解析有效 request identity，使用的环境变量缺失仍会失败。

默认先生成 `Content-Type: application/json`、organization/project header，再应用 `headers`。自定义值最后覆盖同名生成值，包括 Content-Type 和 authentication。自定义 header 中不能同时出现 `Authorization` 与 `authorization`。

整个 `providerConfig` 进入 versioned Schema、SHOW、Diff/Patch/Merge 与备份。直接配置 `api_key` 或 secret header 会把原值写入历史；`api_key_env` 只保存变量名。Provider 的错误信息不回显 credential、完整配置或 HTTP response body；也不修改宿主的全局 logger。随附 artifact 在编译时关闭 HTTP/TLS 依赖的日志，加载时检查该条件。

<a id="request-and-response"></a>

## 请求、响应与重试

请求固定 POST 到 `<base_url>/embeddings`。Body 始终包含 `model`、`input`（String 数组）和 `encoding_format`；按配置选择性加入 `dimensions`、`user`。input 是 source/query String 的精确 UTF-8 内容，Provider 不 trim、lowercase、拼接、分词、truncate 或 chunk。模型 token 限制由 endpoint 决定，`batch_size` 只限制 item 数量。

不会自动跟随 3xx redirect。Provider 关闭 HTTP client 的环境 proxy autodiscovery；`HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 不会改变实际路由。v1 没有 proxy key，需要网关时把可直接访问的最终 URL 配到 `base_url`。是否向 HTTP 或第三方 endpoint 发送所配 credential，由宿主选择。

`data` 的条数必须等于当前 input 数。每个 `data[].index` 必须唯一且在范围内，Provider 按 index 恢复 input 原顺序。float 格式解析 JSON number；base64 格式解析 FLOAT32 little-endian coordinate bytes。每条 vector 必须恰有 Index dimensions 个坐标，转换成 FLOAT32 后均须 finite。错误数量、重复 index、维度错误、NaN/Infinity、无效 JSON/base64 整批失败，不返回部分结果。响应中的无关新增字段被忽略。

HTTP **408、429、5xx** 和 transport I/O failure 可以重试；其它失败状态（含 3xx 和其它 4xx）直接返回 `IO_ERROR`。成功 HTTP 的无效 embedding payload 返回 `INTERNAL_ERROR`，不按 transport retry 掩盖。每批最多 `1 + max_retries` 次 attempt；正常 backoff 为 100、200、400、800、1600 ms，之后保持 1600 ms；合法 `Retry-After`（秒数或 HTTP-date）优先使用，单次最多 5 秒。

Provider 在发 batch、HTTP 返回、重试前、backoff 和下一批前检查取消，backoff 每 25 ms 检查。阻塞中的 HTTP attempt 受 `timeout_ms` 约束，不保证异步抢占底层系统调用。整次 query 可能包含多个批次和重试，总时长会超过 `timeout_ms`。HTTP response body 上限为 384 MiB，batch output 的内部预算为 256 MiB；达到资源上限返回 `RESOURCE_ERROR`。

<a id="persistent-cache"></a>

## 独立 SQLite cache

启用 cache 后，Provider 使用宿主 SQLite runtime 打开独立 database，不能把它 `ATTACH` 为 Lithograph 的 canonical state，也不能把 cache.path 指向 main 文件（包括同一底层文件的别名）。Provider 不自动创建父目录。已有非空 database 必须有匹配的 Provider marker/schema；不会接管任意用户 database。

cache key 包含 ABI implementation identity、cache-key encoding version、dimensions、调用方 `semantic_identity`、有效 endpoint/model/body/header/auth 语义和 exact text。credential/header 以 digest 参与，不将原值或 source/query 原文写入 cache。只改变 timeout、retry、batch、cache 开关、路径或容量，不改变有效 embedding identity。显式默认值与省略默认值可以共享 cache。变更实际 credential、tenant、model、endpoint 或 salt 可能产生 miss。

cache entry 保存 FLOAT32 vector、hash、dimension、原文 byte length 与 FIFO ordinal。命中时验证长度、维度及 finite 数值；可安全识别的单行损坏可删除并按 miss 重算。整个 cache 结构、accounting 或 SQLite I/O 损坏则明确失败。

`max_bytes` 仅计算 vector payload：每条为 `dimensions * 4` bytes，不包括 SQLite page、hash/metadata、WAL/freelist。每次 enabled 调用 lookup 前和 publish 后执行预算检查，按最早插入顺序 FIFO 逐出。cache hit 不刷新 age。单条 vector 大于 budget 时正常返回但不缓存；budget 不是 SQLite 文件大小上限，逐出不会自动 VACUUM。共享同一路径但配置不同 budget 时，较小 budget 的调用可逐出其它调用留下的 entry。

SQLite BUSY/LOCKED 使用内部有界重试：每个重试操作预算 500 ms、10 ms step 并检查取消。这与 HTTP `max_retries` 无关。没有 TTL、LRU、后台清理或 Lithograph cache stats/clear Procedure。删除 cache 文件只影响后续重算；应先结束使用它的连接，并按 SQLite database 文件及 WAL/SHM 的正常生命周期管理。

一次调用先验证完整有效配置、打开/验证 cache、执行预算维护、读取命中并去重 miss，再请求 HTTP、验证整批、原子发布并恢复原输入顺序。失败、取消或非法 vector 不发布部分/negative cache。enabled cache 的真实 I/O/publish failure 会使本次 Provider 调用失败，不会静默绕开 cache。合法 entry 不受外层 graph query rollback 影响；Provider cache 不属于 Lithograph backup、integrity、Commit、Branch、GC 或事务回滚边界。

<a id="errors"></a>

## 失败与排查

| 情况 | Lithograph code | 操作 |
| --- | --- | --- |
| Provider 未加载、配置 shape/范围/有效认证错误 | `INVALID_ARGUMENT` | 检查当前 connection 的加载和参数；create 成功不代表远程认证可用 |
| cache 指向 main、非 Provider DB、版本不兼容 | `INVALID_ARGUMENT` | 改用独立的 cache path；不要覆盖已有业务 database |
| HTTP/transport、cache open/read/write/锁等待耗尽 | `IO_ERROR` | 检查 endpoint、状态、宿主文件权限与并发；保留失败现场 |
| 内存、文件满、大小上限 | `RESOURCE_ERROR` | 减小批量/维度或释放资源；cache budget 与文件系统余量分别检查 |
| Provider 取消 | `INTERRUPTED`、SQLite `SQLITE_INTERRUPT` | 按宿主取消流程清理 execution/transaction |
| ABI、响应 shape/坐标合同或 Provider 内部错误 | `INTERNAL_ERROR` | 固定 Provider/endpoint 版本并报告不含 secret 的最小复现 |

只读 main 允许 Semantic read，Provider cache 仍可能写独立文件；`options.readOnlyNoExternalIO=true` 在 prepare 阶段拒绝 Managed Semantic，cache hit 不能豁免。远程模型不由 SQLite history 快照：需要历史精确复现时，固定 Provider/model revision 与外部资源，或把 vector 作为 Raw Vector Property 纳入历史。

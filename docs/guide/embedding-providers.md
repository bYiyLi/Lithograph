# 使用 Embedding Provider

适用版本：**v0.3.3**。上级：[开发者手册](README.md) · 参数查询：[OpenAI-compatible Provider](../reference/openai-compatible-provider.md) · 检索流程：[Search](search.md)。

本指南把已有文本 Property 接到兼容 Embeddings endpoint。需要自己提供 vector、让向量值进入 Commit 历史，或使用 Cypher `SEARCH WHERE` 时，选择 [Raw Vector](../cypher/search.md)。Managed Semantic 将文本保存在 graph 中，生成的向量只是 derived search data。

<a id="prerequisites"></a>

## 1. 准备宿主与 endpoint

准备 SQLite 3.45.0+、FTS5 与 extension loading，以及同平台的 Lithograph 和 `lithograph-openai-compatible` 两个动态库。检查的是应用实际使用的 SQLite runtime；例如 Python 应先检查 `sqlite3.sqlite_version`。完整加载步骤见 [安装](installation.md)。

准备 endpoint 的 base URL、模型名及匹配维度；endpoint 必须接受 `<base_url>/embeddings` POST、String 数组 input 和所选 wire format。需要鉴权时，在启动宿主进程前设置一个自行命名的环境变量，并在配置中显式引用，例如：

```sh
export LITHOGRAPH_EMBEDDING_API_KEY='由部署环境注入的凭据'
```

不要把示意值作为真实凭据。Provider 不自动读取 OPENAI_API_KEY，不自动跟随 redirect，也不读取 HTTP_PROXY/HTTPS_PROXY/ALL_PROXY。需要兼容网关时，将直接可访问的最终 base URL 写入配置。

以下远程服务流程依赖部署者提供的 endpoint、credential 与模型，不属于无网络示例的自动通过证据。要先验证本地双扩展接入，使用 [synthetic Provider 教程](provider-development.md)，该教程无需远程服务。

<a id="load-both-extensions"></a>

## 2. 在实际查询 connection 上加载两个扩展

以 macOS 开发构建为例；Linux/Windows 换用相应平台后缀，发布制品的文件名以解压包为准：

```sh
cargo build --locked -p lithograph-extension -p lithograph-openai-compatible
```

在 SQLite CLI 执行：

```text
.load ./target/debug/liblithograph.dylib
.load ./target/debug/liblithograph_openai_compatible.dylib
SELECT lithograph_version();
SELECT lithograph_init();
```

两者加载顺序不限，但必须是同一个 connection。连接池需要对每条实际使用 Semantic 的 connection 加载 Provider；新 connection 不继承旧 connection 的注册。重复加载同名 Provider 会报错。

<a id="create-index"></a>

## 3. 保存文本并创建 Semantic Index

下面是用于远程部署的 Cypher 操作模板。先按实际模型确定 `dimensions`，必要时设置 `send_dimensions:false` 以省略 endpoint 不接受的可选 HTTP 字段。示例配置使用环境变量 indirection，避免把真实 credential 写入 history：

```text
CREATE (:Document {id:'intro', content:'Lithograph stores graph history in SQLite'})

CALL db.index.semantic.createNodeIndex(
  'document_semantic', ['Document'], 'content',
  {
    provider:'openai-compatible',
    providerConfig:{
      base_url:'https://api.openai.com/v1',
      model:'text-embedding-3-small',
      api_key_env:'LITHOGRAPH_EMBEDDING_API_KEY',
      timeout_ms:30000,
      max_retries:2,
      batch_size:32
    },
    dimensions:1536,
    similarity:'cosine'
  }
)
```

分别执行上面两条 statement。create 只执行本地配置/Provider 检查，不请求 endpoint、不生成 corpus embedding。因此 create 成功不能证明 API key、网络或模型可用；第一次实际 query 才验证完整外部路径。definition 的 provider/config/dimensions/similarity 都进入 Schema history。更换这些字段时，用 `DROP INDEX document_semantic` 后重新 create；需要原子发布时，把两条操作放在同一个 [Lithograph explicit transaction](transactions.md) 中。旧 Commit 保留旧 definition，连接上的新 Provider 配置不会改写它。

下面的 Python host 使用已有 SQL helper，可绑定并执行上述查询文本。用实际共享库的绝对路径创建 `lithograph_path/provider_path`；`create_query` 是上面完整 CALL statement 字符串，`query_text` 是下一步的完整查询字符串：

```python
from pathlib import Path
import sys

sys.path.insert(0, "docs/guide/examples")
from python_quickstart import open_graph, query

def run_semantic(lithograph_path: Path, provider_path: Path,
                 create_query: str, query_text: str) -> dict:
    db = open_graph(lithograph_path)
    try:
        db.enable_load_extension(True)
        try:
            db.load_extension(str(provider_path.resolve(strict=True)))
        finally:
            db.enable_load_extension(False)
        query(db, "CREATE (:Document {id:'intro', content:'graph history in SQLite'})")
        query(db, create_query)
        return query(db, query_text, {"text": "versioned graph database"})
    finally:
        db.close()
```

此函数用独立内存库演示，一次完整调用后关闭 connection。产品持久化时传入受管理的 database path，并复用同一条已加载扩展的 connection；具体 driver 生命周期见 [应用集成](integration.md)。

<a id="query-and-observe"></a>

## 4. 查询并核对

```text
CALL db.index.semantic.queryNodes('document_semantic', $text, {limit:5})
YIELD node, score
RETURN node.id AS id, score
```

参数 `text` 是查询 String，`limit` 必填，`skip` 省略为 0。正确结果应含 `id/score` 两列，并且在此单 Document fixture 中返回 `intro`。不要固定断言第三方模型的分数；检查所有结果来自实际文本 owner，score 为有限数值。只有 String source 参与；missing、null 和其它类型不会自动转成文本。

query 会为需要参与的 source/query text 调用 Provider；Graph View 在 Provider input 和 top-k 之前生效。历史 query 使用历史 definition，而 active explicit transaction 读取当前 staged source 与 definition。相同 execution 内对 exact text 做去重；跨 execution 复用是否发生，由具体 Provider cache 决定。

`YIELD ... WHERE` 只过滤 procedure 已返回的 top-k。需要真正 filter-before-top-k 的复杂 Cypher 组合时使用 Raw Vector SEARCH。全部 Procedure 字段、Relationship 版本与 rebuild 行为见 [Semantic Procedure](../reference/procedures/semantic.md)。

<a id="enable-cache"></a>

## 5. 可选：启用独立 cache

在新的 Semantic definition 的 `providerConfig` 加入：

```json
{"cache":{"enabled":true,"path":"./embedding-cache.db","max_bytes":1073741824}}
```

先创建父目录并确保宿主可以写入。path 不能指向 Lithograph main、内存数据库或 SQLite URI；相对路径基于宿主工作目录，部署中优先使用自己管理的明确路径。配置完整规则与 key identity 见 [Provider cache 参考](../reference/openai-compatible-provider.md#persistent-cache)。

v0.3.2 已修复首次并发初始化同一空 cache 的竞争，并通过真实多进程、完整源码门禁与新包验收，范围见 [发布说明](../releases/v0.3.2.md)。已发布 v0.3.1 仍存在该竞争：在该版本启动多个 worker 前，先由单个进程用同一配置完成一次有效请求并等待成功，再开放并发；不能只创建一个空文件。v0.3.1 的适用范围与失败处理见 [已知问题](../reference/known-issues.md#provider-cache-first-use)。

重复执行相同有效请求可以减少远程重算。cache 不保存 source/query 原文或 credential 原文，但保存 derived vectors 与 hashes；独立管理其文件权限和生命周期。容量是 vector payload budget，FIFO eviction 不保证文件立即缩小。删除 cache 不改变 graph/history；缺失后将重新请求 endpoint。

只读 Lithograph main 仍可执行 Semantic read，并按配置写独立 cache。graph rollback 不能撤回网络请求或已成功发布的合法 cache entry。需要禁止这些操作的宿主应强制设置 `readOnlyNoExternalIO:true`，见 [安全执行](safe-execution.md)。

<a id="diagnose-provider"></a>

## 6. 从失败恢复

| 观察到的结果 | 检查与恢复 |
| --- | --- |
| create 报 Provider 未注册 | 确认执行 create 的同一 connection 已加载 Provider；其它 connection 的加载无效 |
| create 成功、query 报变量或认证错误 | 检查显式配置的 api_key_env 在宿主进程可见；不假定终端/另一个进程的环境相同 |
| HTTP 3xx | 将 base_url 改为最终 endpoint；Provider 不跟随 redirect |
| HTTP 400 或维度/encoding 错误 | 对照模型和 endpoint 能力修正配置；`send_dimensions:false` 仅省略请求字段，不放宽返回维度验证 |
| HTTP 429/5xx 或 timeout | 使用服务状态和请求量定位；max_retries 有界，避免无条件重复长 query |
| cache 错误 | 检查独立路径、父目录、权限、marker/schema、文件余量和并发；不会静默降级为无 cache |
| 切换模型后结果不可复现 | 固定模型 revision/Provider binary；space 改变时更新 semantic_identity，并形成新的 versioned definition |

错误 code 与精确上限统一见 [Provider Reference](../reference/openai-compatible-provider.md#errors)。报告问题时包含 Lithograph/Provider 版本、SQLite 版本、错误 code、HTTP status 和去除凭据的最小配置；不要粘贴 credential、secret header 或私有文本。

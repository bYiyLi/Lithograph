# Native API v1

**版本：v0.1.0，ABI 1。** 完整可编译声明以 [lithograph.h](../../include/lithograph.h) 为准。所有 handle 来自宿主 SQLite；先在同一 `sqlite3*` 上加载扩展完成注册，不自行实例化第二套 private SQLite。

## 符号与签名

```c
int lithograph_v1_execute(sqlite3 *db,
    const char *query, size_t query_len,
    const char *params_json, size_t params_len,
    const char *options_json, size_t options_len,
    lithograph_event_callback_v1 callback, void *user_data,
    char **error_json);

int lithograph_v1_validate(sqlite3 *db,
    const char *query, size_t query_len, char **error_json);

int lithograph_v1_tx_begin(sqlite3 *db,
    const char *options_json, size_t options_len,
    char **result_json, char **error_json);

int lithograph_v1_tx_execute(sqlite3 *db,
    const char *query, size_t query_len,
    const char *params_json, size_t params_len,
    const char *options_json, size_t options_len,
    lithograph_event_callback_v1 callback, void *user_data,
    char **error_json);

int lithograph_v1_tx_commit(sqlite3 *db,
    char **result_json, char **error_json);
int lithograph_v1_tx_abort(sqlite3 *db, char **error_json);
void lithograph_v1_free(void *ptr);
```

普通 execute 支持与 SQL Bridge 相同的 query / params / options，也提供 SQL Bridge 不能拥有的 transaction-batching boundary。tx_execute 仅在本 connection 的 active Native transaction 中工作，不能代替普通 execute 任意调用。

## Callback 协议

```c
typedef int (*lithograph_event_callback_v1)(
    void *user_data,
    lithograph_event_kind_v1 kind,
    const unsigned char *json,
    size_t json_len);
```

| Event | 数值 | JSON payload |
| --- | --- | --- |
| `LITHOGRAPH_EVENT_COLUMNS_V1` | 1 | 列名数组 |
| `LITHOGRAPH_EVENT_ROW_V1` | 2 | 按相同位置排列的值数组 |
| `LITHOGRAPH_EVENT_SUMMARY_V1` | 3 | 与 SQL envelope.summary 相同的 object |

成功流为 COLUMNS 一次、ROW 零到多次、SUMMARY 一次。失败/取消的流可能只有前缀，不保证收到 SUMMARY。ROW 可包含重复列名对应的值，不应直接丢失位置关系。

Callback 的 JSON bytes 仅在回调期间有效，不保证 NUL-termination。要保存就复制 `json_len` 字节，不能调用 free，不能用 strlen 代替长度。避免在 callback 中对同一 connection 重入执行 graph/transaction 操作；先消费/复制事件，再在外层发下一次调用。

Callback 返回非零表示取消，返回码为 `SQLITE_INTERRUPT`；host `sqlite3_interrupt()` 也是取消入口。收到 SUMMARY 仍应检查整个 API 返回码，caller-owned SQL transaction 还需检查最终 COMMIT。独立 transaction batch 的前面成功部分不因后续取消消失。

## 字符串与内存所有权

长度单位为 UTF-8 **bytes**，不是 Unicode 字符数。query 必须提供合法输入。**v0.1.0–v0.2.0 实际要求 params/options 显式使用 `"{}",2` 表示空 object；`NULL,0` 不会自动补成 `{}`，会被当作空 JSON 输入拒绝。**这与设计的默认值约定不同，详见 [Known Issues](known-issues.md)。不能传非零长度的空指针。callback 和 db 必须有效。

把 `char *result = NULL; char *error = NULL;` 的地址传给输出参数。非空输出由 Lithograph 分配，读取后恰好用 `lithograph_v1_free` 释放一次；不要用 `free`、Rust allocator、Python allocator 或 `sqlite3_free`。宿主 `sqlite3_load_extension` 返回的错误则属于 SQLite，使用 `sqlite3_free`。

输入缓冲在同步调用期间保持有效。API 返回后由调用方按自己的 allocator 管理输入；Lithograph 不接管它们。关闭 sqlite3 connection 前结束事务、关闭游标并确保没有执行仍在使用它。

## Return code 与 error_json

返回 SQLite primary code：成功 `SQLITE_OK`，失败时可获取 error JSON：

```json
{"category":"PARSE_ERROR","message":"...","sqliteCode":1,"line":1,"column":1}
```

line/column 不适用时可为 null。不要仅根据英语 message 匹配程序分支；使用稳定 category 与 code。OOM 等资源故障下不要假定 error_json 一定可分配。类别清单见 [Errors](errors.md)。

## Explicit transaction

begin options：`branch`、`expectedHead`、`author`、`message`。成功返回 `{"baseCommit":"commit/..."}`。tx_execute options 只接受本语句 `graphView`；不能覆盖 Branch 或 metadata。

tx_commit 成功返回 `{"commit":"commit/...","counters":{...}}`。有 mutation 时产生恰好一个 Commit；纯读取时为 base Commit，计数为零。tx_abort 丢弃全部 staged changes；任何 tx_execute 错误已经自动 abort，后续 commit/abort 不再有 active transaction。

begin 要求 idle/autocommit connection；不能与 SQL BEGIN 叠加。Version Procedure、LOAD CSV、IN TRANSACTIONS 或普通 graph API 不得穿插到 active Native transaction。完整生命周期和重试规则见 [事务指南](../guide/transactions.md)。

可运行 C 示例见 [应用集成](../guide/integration.md)。checkout 的 adapter 缺陷单独记录在 [Known Issues](known-issues.md)。依据：[公开 header](../../include/lithograph.h)、[Native implementation](../../crates/lithograph-extension/src/native.rs)、[Native C ABI](../design/interfaces.md#native-c-abi)。

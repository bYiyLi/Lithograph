# 安装与加载 v0.1.0

目标：在自己的 SQLite connection 中加载 v0.1.0，并确认版本、运行时和数据库格式。新手先用全新的演示文件；已有数据库的初始化可能触发迁移，先阅读 [备份与升级](operations.md)。

## 1. 选择制品

下载固定版本，而不是 `releases/latest`，以保持本文的版本边界：

| 运行进程的平台 | v0.1.0 下载 | 解压后的扩展 |
| --- | --- | --- |
| Linux x64 | [lithograph-linux-x64.tar.gz](https://github.com/bYiyLi/Lithograph/releases/download/v0.1.0/lithograph-linux-x64.tar.gz) | `lithograph.so` |
| Linux arm64 | [lithograph-linux-arm64.tar.gz](https://github.com/bYiyLi/Lithograph/releases/download/v0.1.0/lithograph-linux-arm64.tar.gz) | `lithograph.so` |
| macOS x64 | [lithograph-macos-x64.tar.gz](https://github.com/bYiyLi/Lithograph/releases/download/v0.1.0/lithograph-macos-x64.tar.gz) | `lithograph.dylib` |
| macOS arm64 | [lithograph-macos-arm64.tar.gz](https://github.com/bYiyLi/Lithograph/releases/download/v0.1.0/lithograph-macos-arm64.tar.gz) | `lithograph.dylib` |
| Windows x64 | [lithograph-windows-x64.zip](https://github.com/bYiyLi/Lithograph/releases/download/v0.1.0/lithograph-windows-x64.zip) | `lithograph.dll` |
| Windows arm64 | [lithograph-windows-arm64.zip](https://github.com/bYiyLi/Lithograph/releases/download/v0.1.0/lithograph-windows-arm64.zip) | `lithograph.dll` |

每个包还包含 `README.md`、`VERSION`、`LICENSE`、`COMMERCIAL-LICENSE.md`。Native header 不在二进制包中；需要编译 Native 客户端时，另取 [v0.1.0 header](https://raw.githubusercontent.com/bYiyLi/Lithograph/v0.1.0/include/lithograph.h)。

以 macOS arm64 为例，在新建的下载目录执行：

```sh
curl -fLO https://github.com/bYiyLi/Lithograph/releases/download/v0.1.0/lithograph-macos-arm64.tar.gz
curl -fLO https://github.com/bYiyLi/Lithograph/releases/download/v0.1.0/SHA256SUMS
grep '  lithograph-macos-arm64.tar.gz$' SHA256SUMS | shasum -a 256 -c -
tar -xzf lithograph-macos-arm64.tar.gz
cat VERSION
```

应看到校验 `OK` 和版本 `0.1.0`。Linux 使用对应包名，校验命令可改为 `sha256sum -c -`。Windows 下载 ZIP 与同一 [SHA256SUMS](https://github.com/bYiyLi/Lithograph/releases/download/v0.1.0/SHA256SUMS)，用 `Get-FileHash -Algorithm SHA256` 比对该文件名对应的 hash，再解压。校验缺失或不匹配时停止，不加载该文件。

校验用于核对下载内容，不能取代对下载来源的信任。不要为了加载未知扩展而关闭整机安全策略。

## 2. 检查真正执行查询的 SQLite

要求 SQLite **3.45.0 或更高**、loadable extension 和 FTS5。检查的是应用实际链接的运行时，而不只是终端里另一个 `sqlite3`：

```sql
SELECT sqlite_version();
SELECT sqlite_compileoption_used('ENABLE_FTS5') AS fts5;
```

版本应满足最低要求，FTS5 应可用。某些 Python / 移动端 / 沙箱构建没有扩展加载入口，即使版本足够新也不能加载。`sqlite_compileoption_used()` 仅提供编译选项线索，最终以目标 connection 的加载和查询结果为准。

## 3. SQLite CLI 中加载

在解压目录打开一个**新的**演示文件：

```sh
sqlite3 lithograph-demo.sqlite
```

在 CLI 中执行；`.load` 是 CLI 命令，不是 SQL：

```text
.bail on
.load ./lithograph.dylib
```

先查询版本，再显式初始化：

```sql
SELECT lithograph_version();
SELECT lithograph_init();
SELECT lithograph_version();
```

未初始化时 `databaseId` 和 `storageFormat.current` 为 `null`。初始化后，`extension` 为 `"0.1.0"`，`abi` 为 `1`，`cypherProfile` 为 `"CY25-2026.08"`，`storageFormat.current` 为 `3`。`databaseId` 每个新库不同。

`lithograph_init()` 的 `root` 是裸 64 位十六进制 hash；需要 Version Descriptor 时使用 `commit/` 前缀，或读取 `lithograph.commit.get('branch/main')` 的 `commit` 列。

`.load` 本身不创建图。每个 connection 都需要加载；已经初始化的数据库不需要每次连接都重新 init。重复 init 虽然幂等，但可能执行完整检查，不适合作为每个请求的健康检查。

## 4. 在应用中加载

通过 binding 的 extension-loading API，仅在加载受信任文件期间启用加载能力，随后关闭。不要把扩展路径交给不可信请求选择。具体 Python 和 Native 示例见 [应用集成](integration.md)。

不能通过同一 connection 的 `ATTACH` 选择另一张 Lithograph repository；另一个文件应作为另一个 connection 的 `main` 打开。

## 从源码构建

使用预编译包不需要 Rust。只有自行构建才需要仓库固定的 Rust `1.98.1`：

```sh
git clone --branch v0.1.0 --depth 1 https://github.com/bYiyLi/Lithograph.git
cd Lithograph
cargo build --locked --release -p lithograph-extension
```

Cargo 输出 Linux `target/release/liblithograph.so`、macOS `target/release/liblithograph.dylib`、Windows `target/release/lithograph.dll`，与发布包无 `lib` 前缀的文件名不同。加载实际文件路径，不要假定存在 `pip install lithograph`、npm SDK 或独立 `lithograph` CLI。

当前 Unreleased `main` 若要试用 Phase 13 Managed Semantic，请使用当前 `main` checkout，而不是上面固定的 v0.1.0 tag；还需要单独构建并加载 OpenAI-compatible Provider：

```sh
cargo build --locked --release -p lithograph-extension
cargo build --locked --release -p lithograph-openai-compatible
```

Cargo 会额外生成 `liblithograph_openai_compatible.so` / `.dylib` 或 Windows 对应 DLL。Provider 与 Lithograph 是两个平级 SQLite extension；每个真正执行 Semantic create/query/rebuild 的 connection 都要加载所需 Provider。例如 SQLite CLI：

```text
.load ./target/release/liblithograph.dylib sqlite3_lithograph_init
.load ./target/release/liblithograph_openai_compatible.dylib sqlite3_lithographopenaicompatible_init
```

这两个文件目前只属于 Unreleased 源码构建/开发验收范围；现有 v0.1.1 GitHub Release package 不包含 Provider artifact。

加载失败时转到 [排障](troubleshooting.md)。SQLite 加载机制依据 [SQLite 官方说明](https://www.sqlite.org/loadext.html)；版本依据 [v0.1.0 Release Notes](../releases/v0.1.0.md)。

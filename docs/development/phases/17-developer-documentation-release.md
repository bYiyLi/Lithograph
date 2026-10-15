# Phase 17：完整开发者手册与 v0.3.1 发布

**状态：`done`**

## 目标与输入

面向将 Lithograph 嵌入产品的开发者，交付独立完整的使用手册、Cypher 语言参考、精确接口与可执行示例，并发布 v0.3.1。产品行为以 [设计职责表](../../design.md#design-ownership) 中对应专题为准；语言范围由 [Compatibility](../../design/compatibility.md) 定义。保留 `CY25-2026.08`、storage format 3 与 `EmbeddingProviderV1`。

依赖 [Phase 16](16-safe-execution-option.md) 完成安全执行选项验收。文档可并行编写，但正式发布必须包含该选项的完整证据。本 Phase 不建设文档站点、独立文档分发协议或其它产品接入。

## 文档职责

| 内容 | 正文拥有者 |
| --- | --- |
| 产品手册导航、版本与按任务入口 | `docs/guide/README.md` |
| 安装、集成、事务、子图、安全、搜索、导入、版本和运维流程 | `docs/guide/` |
| 语言语法、类型、表达式、路径、组合、更新、DDL、SEARCH、batch 与函数语义 | `docs/cypher/` |
| SQL、options、JSON、事务生命周期、Procedure、版本对象、Provider 与错误限制 | `docs/reference/` |
| 当前覆盖和验收 | 本计划与 `documentation-coverage.json` |
| 发布变化和升级要求 | `docs/releases/v0.3.1.md`、`CHANGELOG.md` |

保留现有公开路径与旧章节入口；拆出完整说明时原页面留下准确导航。Guide 保留任务步骤，Reference 拥有完整接口表；类型语义与 JSON 编码、CALL 语法与 Procedure 参数、Cypher batch 与宿主事务分别维护。普通使用所需信息必须在用户手册内完整，不以源码/设计链接代替正文。

## 工作与验收矩阵

| ID | 交付与验收 | 状态 |
| --- | --- | --- |
| DOC17-01 | 完整语言分章、稳定锚点；覆盖 50 个 capability family、required coverage、delta 与适用补充能力 | 已通过 |
| DOC17-02 | 全部函数/overload 有参数、返回、null/type/error 和执行示例；运行时 inventory 与正文一致 | 已通过 |
| DOC17-03 | 全部 Procedure、SQL/options、版本对象、Provider 配置/SPI 有精确参考和使用入口 | 已通过 |
| DOC17-04 | Guide 补齐安全、导入、Provider、stream/transaction 生命周期，示例可独立运行 | 已通过 |
| DOC17-05 | Markdown 查询单一来源；全部可执行 Cypher 登记并断言结果或错误；Provider/CSV 使用合成 fixture | 已通过 |
| DOC17-06 | 版本驱动生成器与只检查模式、覆盖/链接/锚点/空白检查接入 CI；无静默跳过 | 已通过 |
| DOC17-07 | Phase 16、完整 `cargo make quality`、`scripts/ci.sh` 及最终 diff review 通过 | 已通过 |
| DOC17-08 | 最终候选精确 SHA 的 CI 与六平台 Release Matrix 通过；annotated v0.3.1 tag 发布 | 已通过 |
| DOC17-09 | tag CI/Matrix、六包+SHA256SUMS、VERSION/双扩展/许可和发布制品文档回归通过 | 已通过 |

## 验证与完成标准

复用现有 SQL-only 示例与 fixture；从 Markdown 提取有 ID 的查询，以独立临时数据库执行。结构化清单根据目标扩展 `lithograph_version()`、`SHOW FUNCTIONS` 和 `SHOW PROCEDURES` 核对；生成内容不得覆盖手写语义。introspection 与实际类型不一致时单独记录，不把 metadata 当作正确性替代。

先做定向文档/Phase 16 检查，再运行仓库强制门禁。Core/test-support、extension 与 Provider 按现有脚本分组。review 覆盖遗漏、错误路径、事务/版本副作用、示例断言、稳定链接和历史事实。修复影响验收的问题并重验受影响输入。

全部 DOC17 项目及前置 Phase acceptance 同时满足才标记 `done`。提交、推送、CI、tag 和发布状态分别记录；旧版本 CI/Release 不能替代本候选证据。发布前若产生新提交，等待最终 SHA 的全部必要检查。

## 执行证据

2026-10-15 开始：基于 `aa882b6652f909fc234c23d1dae45fcfe3ae93be` 工作树，保留已有 Phase 16 的 8 个 tracked 和 3 个 untracked 文件；当前公开 Release 为 v0.3.0，远端尚无 v0.3.1。后续结果在实际完成后记录。

### 2026-10-16 文档与 review

- 新增完整 Cypher 分章和 9 类函数正文，保留 Guide / Reference 的公开路径与旧章节入口。`check_docs.py` 核对 78 个用户 Markdown 页面、50 个 capability family、151 个 required coverage 细项、38 个 delta/supplemental inventory 项和 48 个公开接口/配置项；历史 Native / format 4 / Core-owned cache 三项明确记录被当前合同替代。
- 运行时生成器核对 v0.3.1 的 169 个函数名称、172 条 overload 签名与 38 个 Procedure，管理签名块/索引而不覆盖手写语义。实际 Procedure metadata 的参数/返回类型不完整以及 `SHOW INDEXES.createStatement` 导出局限保留在 Known Issues，正文依据真实行为编写。
- Markdown 登记 526 个可执行 Cypher 示例。索引保存 setup 依赖、参数和结果/错误断言，查询文本只来自 Markdown；scalar/rows 分别执行，Provider 使用 synthetic extension，CSV 使用临时教学数据。错误断言匹配实际 category code，整数不经浮点近似比较；附带验证器 self-test。
- 保留原有 Python/C 示例，新增 streaming lifecycle 示例；Guide SQL 验证使用 125 条语句。CI 增加覆盖/链接/锚点、生成一致性、全部 Cypher 示例、Python 示例和 C host 编译运行，缺失 Provider fixture 直接失败。
- 独立 review 检查文档责任、完整接口、旧链接、scope、空输入、路径、事务/版本副作用、Provider/SPI、示例断言与生成器。补齐 C host 编译入口，明确 Diff/Patch 仅在同一 database 内使用；上游手册仅用于标准语义核对，语言解释与教学数据独立编写。

可执行文档暴露并修复的真实实现问题：UUID `toString()` 静态范围；path function 组合的 operand 分类；regex 与双写字符串引号解析；Map projection 显式 key；表达式子查询的 alias/投影修饰符越界；`count(DISTINCT *)` 误接受；INTEGER64 Vector 精度/边界；单项 Full-text prohibited query；rows 活跃 statement 下 Schema 验证的 TEMP DROP 锁；native tokenizer NOMEM 的 SQLite canonical error 传播。每项均补有定向回归。legacy variable-length 空区间保留 inherited TCK 的空结果语义；没有修改 TCK 预期或排除项来获得通过。

Phase 16 review 另闭合了字符串插值子表达式的 prepare 检查，包含未执行的 EXPLAIN/PROFILE 路径；SQL surface 与 synthetic Provider 验证拒绝前无写入/外部调用。标准化 NOMEM 修复通过 SQLite 3.45.0 与 3.51.0 的完整 Phase 12 probe，scalar/rows 均保留原有 canonical 错误断言与失败原子性检查。

### 2026-10-16 本地完整门禁

`cargo make quality` 与 `scripts/ci.sh` 均完整退出 0。Core/test-support、extension、Provider 保持原有分组；没有跳过 HTTP fixture。覆盖率 regions 83.56%、functions 84.38%、lines 85.43%，全部阈值通过；inherited TCK 为 3,777 applicable passed、0 failed，120 项仍使用原有显式分类，18 个冻结 fixtures 均通过。SQLite 3.45.0/3.51.0/3.53.4 的真实加载、SQL surface、双扩展、NOMEM/rollback 与 Provider cache 门禁通过。

最终文档执行结果为 526 示例 × scalar/rows = 1,052 次，另有 125 条 Guide/Reference SQL、4 个 Python 程序及 C host 编译运行全部通过。172 条函数签名/38 个 Procedure 的生成一致性、验证器 self-test、78 页链接/锚点/覆盖和 whitespace 检查均通过。远端候选 SHA 与正式发布尚待下节实际记录，不使用本地成功替代六平台验收。

### 首轮远端候选与 macOS Python 修复

候选 `7aacd96dd55aa17f08e4f49af10adfcc086deefe` 已提交并推送。首轮 CI `38770018246` 中，Linux canonical gates 和 Windows load/ABI gates 通过；macOS 的 526 × 2 Cypher 与 125 条 SQL 均通过，但 `actions/setup-python` 的 macOS Python SQLite 旧于最低版本且禁用 extension loading，使原生 Python 示例失败。这是可复现的验证环境问题，不能将该候选记作完整 CI 成功，也不能跳过 Python 示例发布。

修复采用 Homebrew Python 3.14 + 同源 SQLite，CI 显式选择其 python3；Python boundary probes 的不合格环境改为失败。Runtime 与示例文本不因该环境问题改写，最终候选仍须重新取得自己的 CI 与六平台结果。

首轮 Release Matrix `38770018229` 的六个平台全部成功，CI `38770018246` 的 quality、Linux 和 Windows 也全部成功；仅 macOS Python 环境导致该轮 CI 整体失败。后续修复候选仍需新的完整检查。发布说明中的用户手册链接统一为固定 v0.3.1 在线地址，使同一正文在仓库和 GitHub Release 页面均有效；发布验收记录链接到主分支的实际更新记录。

### 最终候选、tag 与正式发布验收（2026-10-16）

发布源码冻结为 `8a66f19eff835e603aa5b3fa60ae5aca3214df67`。其 [CI 38772676914](https://github.com/bYiyLi/Lithograph/actions/runs/38772676914) 的 4 个 jobs 与 [Release Matrix 38772676779](https://github.com/bYiyLi/Lithograph/actions/runs/38772676779) 的 storage fixture + 六平台 7 个 jobs 全部成功，才创建并推送 annotated `v0.3.1`。tag object 为 `522bdac750039d2364016e07cc54bf0d43ffdd32`，远端 peeled commit 与冻结源码一致。中间候选 `5c9e0b8` 在固定发布链接最终整理后被替代，其未完成检查主动取消，不作为验收成功证据。

Tag 的 [CI 38774756406](https://github.com/bYiyLi/Lithograph/actions/runs/38774756406) 与 [Release Matrix 38774756423](https://github.com/bYiyLi/Lithograph/actions/runs/38774756423) 均成功；后者 8 个 jobs 包含正式 publish job。GitHub [v0.3.1 Release](https://github.com/bYiyLi/Lithograph/releases/tag/v0.3.1) 的 `publishedAt=2026-10-15T17:19:19Z`（北京时间 2026-10-16），`isDraft=false`、`isPrerelease=false`。

从正式 Release 下载六包和 `SHA256SUMS`，逐包核对原始资产 SHA-256、唯一完整文件集合、VERSION、主扩展/Provider、ELF/Mach-O/PE 的 x64/arm64 machine、两份许可证，以及 README 三个固定 v0.3.1 入口。文本内容与固定 tag blobs 一致，正常化 Windows CRLF 后比较文本，checksum 始终检查原始字节。

| 正式资产 | SHA-256 |
| --- | --- |
| `lithograph-linux-x64.tar.gz` | `b4170e31a307e71f243365d4629b359f8e2e1c1fcf9703722c7f314b858a8d1d` |
| `lithograph-linux-arm64.tar.gz` | `1f0081ce3478f69575153e1c0b9cf7a22922a21170bd22c0404a4a2692e95e80` |
| `lithograph-macos-x64.tar.gz` | `8ee1b0cb824d7f2c45101d1d84ed49c23378fdc95be621a2c0fa7994d918dd92` |
| `lithograph-macos-arm64.tar.gz` | `27de06ef3528c477d3730274c525ca081bbad4f711f9dd32615811e97237c6fe` |
| `lithograph-windows-x64.zip` | `3efed000b48dabfa2ea8748900955b8db2af99bb33b6353acf36fa84ac92d1a9` |
| `lithograph-windows-arm64.zip` | `d7ceb9eceeccb318099ed668a5d15b1a9aa165e9eaad72564782fe91dc0bbd43` |

实际下载的 macOS arm64 主扩展在 Darwin arm64 / Homebrew SQLite 3.51.0 重新通过：526 × 2 Cypher、125 条 Guide/Reference SQL及 Python boundary probes、4 个 Python 应用程序、C host 编译运行、172 条函数签名和 38 个 Procedure 的生成一致性。没有用本地 debug build 替代发布库。

同一发布主库 + 发布 OpenAI-compatible Provider 通过普通 `.load` basename 检查、OpenAI-compatible smoke、7 项 Provider cache gate；发布主库 + synthetic Provider + 发布真实 Provider 的 Phase 13 probe 20/20 通过，包含安全选项零 embedding 调用、加载顺序、历史/Graph View、失败原子性、scalar/rows、只读 main 与 explicit staged Semantic。对真实远程服务的配置模板不计入已通过示例；Provider 回归使用 synthetic Provider 和本地测试服务。

DOC17-01–09 与前置 Phase 16 全部闭合。提交、推送、tag 和正式 Release 已实际完成；本次发布后的记录提交只同步 Development 验收与状态，不改变冻结源码、用户手册、二进制、tag 或发布资产。发布源码与该记录提交之间只能有 Development 文档差异；固定 v0.3.1 用户手册始终由 immutable tag 提供。

# Merge Session：逐步解决冲突后再提交

适用 v0.3.0。目标：从 source Branch 合并到 target Branch，允许跨连接、重启和多轮人工/程序审查，不生成中间 Commit。

## 从 start 到 finalize

```text
start → get / conflicts → resolve（可多次）→ candidate query → finalize
                 └──────────────────────────────────────────→ abort
```

`ours` 是目标 Branch 的 pinned head，`theirs` 是 source 解析后的 Commit，不是“时间上较新/较旧”。Session 一开始就固定输入；source 后续写入不会自动加入本次合并。**start 即使返回 fast_forward 也不移动 Branch。**

本页 Python 片段使用 [version_workflow.py](examples/version_workflow.py) 中的 `query`、`one`、`open_graph` helper 和已打开的 `db`；完整文件包含 Alice→Alicia / Ally 的分支冲突、断开重连和所有断言，可直接执行。

## 1. 固定目标，建立 Session

```python
expected = one(db, "CALL lithograph.commit.get('branch/main')")["commit"]
state = one(db, "CALL lithograph.merge.start($source,$expected)",
            {"source": "branch/feature", "expected": expected},
            {"branch": "main"})
session = state["session"]
revision = state["revision"]
```

`expectedHead` 可省略；提供时要求 session.ours 恰好等于你已检查的 head。返回 `session,targetBranch,ours,theirs,revision,status,unresolved`。

| status | 含义 | 现在能否检查 candidate |
| --- | --- | --- |
| `up_to_date` | source 已在目标历史中 | 可以，candidate 为 ours |
| `fast_forward` | 目标是 source 的祖先 | 可以，candidate 为 theirs |
| `conflicted` | 有未解决冲突 | 不可以，先 resolve |
| `ready` | 分叉合并结果无未解决冲突 | 可以 |

## 2. 分页查看冲突

```python
page = query(db, "CALL lithograph.merge.conflicts($session,100)",
             {"session": session})
```

每行包含 `conflictId,slot,base,ours,theirs,resolution`，以及 `session,revision,cursor`。使用最后一行非空 cursor 获取下一页：`merge.conflicts($session,100,$cursor)`。

cursor 绑定 Session 的 revision。任何有效 resolution 更新后，应按新 revision 重新分页；旧 cursor 返回 `MERGE_SESSION_CHANGED`。冲突不仅包括同一 Property 的不同值，也包括删除与修改、Relationship 端点依赖、合并后的 Constraint 违反。

## 3. 提交一批选择

```python
choices = [{"conflictId": conflict_id, "choice": "theirs"}]
state = one(db, "CALL lithograph.merge.resolve($session,$revision,$choices)",
            {"session": session, "revision": revision, "choices": choices})
revision = state["revision"]
```

`conflict_id` 来自刚读到的 conflict 行；不能自行生成。choice 为 `ours`、`theirs` 或 `value`，后者还需 `value` 字段且类型必须适合该 slot。

一次 resolve 原子提交一批选择；有效变化只使 revision 增加一次。在**当前** revision 上重复相同选择是 no-op；拿旧 revision 重试仍然报错。unknown / duplicate conflictId、非法 choice/value 使整批失败。

每次 resolve 后重新查看 status/unresolved。处理一个冲突可能暴露新的约束冲突，因此不能仅以“已经遍历过最初所有 conflictId”为完成标准。

## 4. 在 exact revision 上验证候选图

```python
candidate = query(db, "MATCH (p:Person) RETURN p.id,p.name ORDER BY p.id",
                  options={"mergeSession": {"id": session, "revision": revision}})
```

只有 unresolved 为 `0` 才可读取 candidate。这里可执行只读 Cypher / Search / Schema introspection，不能写入、调用版本 mutation 或混入 `branch` / `at` / `author` / `message`。

`candidate.summary.commit` 为 `null`，因为候选图不是已提交版本。`summary.mergeSession` 指明 id/revision。应用自己的验证规则在这里运行，Lithograph 不替应用决定业务冲突。

## 5. 提交或放弃

```python
result = one(db, "CALL lithograph.merge.finalize($session,$revision)",
             {"session": session, "revision": revision},
             {"message": "Merge reviewed candidate"})
```

finalize 不再传 `options.branch`，目标已绑定 Session。它要求 revision 未变、unresolved 为零、目标 head 仍等于 pinned ours。成功结果总有 `commit`：`up_to_date` 返回 ours；`fast_forward` 移动目标到 theirs；`merged` 创建以 ours/theirs 为两个 parent 的新 Commit。成功后 Session 原子删除。

放弃时调用 `merge.abort($session,$revision)`；不创建 Commit、不移动 Branch。不要为了“退出冲突模式” reset 目标分支。

## 恢复与错误处理

Session 持久化，可在重启后用 `merge.list(100)` 发现，再 `merge.get($session)` 恢复。list 是运行时清单，不保证跨多页集合不变；需要最新完整列表时从第一页重新开始。Session 不自动 TTL 删除，应用应显式 finalize 或 abort。

`MERGE_SESSION_CHANGED`：重新 get / conflicts，审查新 revision 后重试。`BRANCH_HEAD_MOVED`：Session 保留，但旧 candidate 不能提交到已经变化的 target；建立新 Session，重新审查，不盲目套用旧 resolution。`MERGE_SESSION_NOT_FOUND`：可能已经完成或被其他调用方 abort，先核实目标状态，不凭网络重试再次执行。

大量冲突不需要一次放进客户端内存。分批读取、分批 resolve、每批使用返回的 revision；Session 在等待期间不长期占用 writer。最终 Commit Data 需要 finalize 成功后另行 set，合并不会推测或合并业务 JSON 注释。

接口列与参数见 [Procedure Reference](../reference/procedures.md)，并发边界见 [事务](transactions.md)。

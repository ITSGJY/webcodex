# WebCodex 整体复盘：盲盒感、大型开发差距与测试瘦身

日期：2026-07-26 ｜ 基线：main @ 9018814 ｜ 上一次整体评估：`docs/PROJECT_ASSESSMENT.zh-CN.md`（2026-07-11）

## 0. 三句话结论

1. **"盲盒感"不是错觉，是架构事实**：全链路不存在任何 server→聊天窗口 的推送通道（无 SSE、无 MCP progress 通知），所有可见性都是拉取式的，而两条拉取路径（模型调 `task_review`、人开 console/CLI）各自缺了关键信息——执行中不给 diff、审批看不到命令原文、console 丢弃 timeline、审计无人消费。
2. **"不能像本地 agent 做大型开发"一半是形态天生、一半是自己定的 Non-Goal**：`docs/ROADMAP.zh-CN.md:124` 和 `docs/PRODUCT_DEVELOPMENT_PLAN.zh-CN.md:103` 白纸黑字不做 agent loop / prompt loop / compaction。决策循环 100% 在聊天窗口的模型手里，WebCodex 是"无状态、同步、有界的工具执行网关"。差距里可以补的部分有明确清单（见 §3.3），第一名是给模型一个 `task_list`/`task_resume` 入口。
3. **测试确实占了近一半代码量（约 9.0 万行 / 47%），但"重复"不是主要病灶**：机械扫描 2141 个测试，逐字重复仅 2 组、模板克隆 76 个（3.7%）。真正的问题按序是：**没有 CI（9 万行测试零自动执行）→ 2 个并发 flaky → 测试基建复制 9 份 → 千行黄金表与不变量测试双轨 → 守卫断言散落**。可立即清理约 2,500 行（P0+P1+P2），但更高杠杆的动作是先上 CI。

---

## 1. 项目现状快照

| 指标 | 数值 |
|---|---|
| Rust 源码 | 349 个文件，191,156 行 |
| 生产代码 | ≈ 101,000 行 |
| 测试代码 | ≈ 90,000 行（**47%**；含部分测试辅助代码则约一半） |
| 测试函数 | 2,141 个（专用测试文件 ≈1,300 + 内嵌 `mod tests` ≈840），平均 40 行/测试 |
| 测试耗时 | 主二进制 1,713 个测试 44.5s；全量 `cargo test`（含增量编译）≈ 2 分钟 |
| 开发速度 | 近 30 天 642 个 commit（solo） |
| e2e shell 脚本 | 5,685 行，**受环境变量门控，实际执行次数为 0**（无 CI、无 Makefile） |

### 1.1 相比 7 月 11 日评估，已兑现的部分

两周内 +59.5k/−14.0k 行，旧评估的多项 P0 已落地：

- SQLite 打开路径加了 WAL/busy_timeout + auth GC（a5c0546）；死表 `messages`/`command_requests` 等已删（1c05434）；旧 OAuth 迁移已删（1d15d10）。
- 工作流会话有了真实生命周期与关闭语义（d9607f5、30ccc22）。
- 权限闸门从"纯脚手架"升级为**真实闸门**：`dispatch.rs:278-308` 在执行前评估并可拦截，fail-closed 处理未知状态与非法配置。
- 产品面从"工具大杂烩"收敛为 **9 个 project-bound capability**（`task_start → files_read/search → edits_apply → checks_run → task_finish/review/cancel`，`src/connector_runtime/surface.rs:10-20`），加上本机人审（CLI + 浏览器 console 的 accept/reject）。
- durable execution 落了 SQLite（`wc_tasks`/`wc_runs`/`wc_executions`/`wc_task_events`/`wc_approvals`），带 `operation_id` 幂等、断电对账、进程组终止。

### 1.2 仍未兑现的旧账

- **`require_approval` 仍是未实现**：`src/tool_runtime/permissions/policy.rs:122-132` 直接返回 `Denied("require_approval_not_implemented")`；默认模式仍是 `DevAutoApprove` 全放行。真正会 pending 的审批只存在于 connector 层的 `commands_run` 一个能力上（`src/connector_runtime/mod.rs:1383`），且只能 CLI 审批。
- **仍然没有 CI**（旧评估的第一条建议）。
- workspace 拆分 / `#[path]` 共享问题未动。

---

## 2. "盲盒感"根因（按层）

> 详细证据链见本节各条 file:line。核心事实：`grep` 全仓无 `text/event-stream`、无 `notifications/progress`、无 `progressToken`。

### 2.1 协议层：没有进度通道

- MCP `initialize` 只声明 `tools.listChanged=false`（`src/mcp.rs:328-342`），无 logging/sampling/progress capability；方法白名单仅 5 个（`src/mcp.rs:492-498`）。
- `tools/call` 一次性 `rpc_result` 返回全部内容（`src/mcp.rs:467-489`）。执行 8 秒（quick-yield，`execution.rs:20`）到 150 秒（`MCP_DISPATCH_HARD_TIMEOUT`，`src/mcp.rs:26`）之间，聊天窗口收不到任何字节；超时文案自己承认盲盒："the tool may still be running"（`src/mcp.rs:211`）。
- 同步工具在服务端就是一个 `oneshot::channel`（`src/shell_client/requests.rs:179`），只有终态一个包。250ms 增量流只存在于 job 类请求，且只进服务端内存表，要模型再调 `job_log` 才拿得到。

### 2.2 执行层：执行中的信息被有意置空

- **执行中 `task_review` 不给 diff**：命令活跃时返回 `{"source":"live_workspace_deferred","changed_paths":[],"diff_preview":null}`（`src/connector_runtime/mod.rs:1542-1552`）。这是盲盒感最直接的一条。
- **任务事件 payload 极简**：`record_event` 只记 `{ok, dry_run, operation_id, change_count}` 级别的摘要（`src/connector_runtime/mod.rs:1042-1051`），timeline 里没有文件名、命令、输出。
- **审计只记"有没有"不记"是什么"**：`tool_audit.rs:18-24` 对 `run_shell` 只存 `command_present: true`。这是 `docs/ARCHITECTURE.md:99-104` 写明的隐私取舍——但取舍的代价就是人看不清。

### 2.3 人侧：两个人类界面各缺一半

- **审批是盲签**：`commands_run` 的审批摘要是 `"raw project command (N bytes, workspace X)"`（`src/connector_runtime/mod.rs:1373-1381`）——人在 `webcodex task approve` 时只知道命令的字节数。
- **console 丢弃 timeline**：后端 `task_review` 返回 `recent_events`（`mod.rs:1639`），前端 `frontend/src/` 里 `recent_events` 零命中，直接不渲染；且 `host_console_http.rs:129-132` 禁止 console 传 `max_events`。
- **console 没有审批面板**：六条 console 路由（`host_console_http.rs:13-20`）无 approve/deny，`frontend/dist/app.js` 里 `approval` 零命中——pending 审批出现时浏览器用户毫无感知，只能靠 CLI 发现。
- **audit 写了没人看**：32 处 `ActionAudit::start` 全在 REST 路径；`src/mcp.rs` 和 connector HTTP 层一处都没有 → 聊天窗口经 MCP 做的事 audit 表里没有记录；audit 的读取面是 POST + `SCOPE_ACCOUNT_MANAGE`，前端无入口。
- **本机 agent 终端也是黑的**：`webcodex-agent` 的 println 只有启动/关闭/错误 5 处——坐在自己电脑前也看不到模型正在跑什么。

### 2.4 修复清单（性价比排序）

| # | 动作 | 改动量 | 效果 |
|---|---|---|---|
| 1 | 前端渲染 `recent_events` timeline（后端已返回） | 前端 1 个组件 + 放开 `max_events` 禁令 | console 变成真任务视图 |
| 2 | 审批摘要带命令预览（复用 `command_preview`，120 字符，`shell_client/jobs.rs:8`） | 改 1 处 format | 消除盲签 |
| 3 | console 补 approvals 面板（后端 `decide_connector_approval` 已存在） | 2 条路由 + 前端 | 人审闭环不再依赖 CLI |
| 4 | 执行中至少返回 `changed_paths`（保持不给 diff） | 改 `mod.rs:1542` 一处 | 执行中不再全黑 |
| 5 | 丰富 `record_event` payload（文件名/命令预览/退出码） | 中 | timeline 有内容可看 |
| 6 | MCP/connector 路径补写 action_audit | 中 | 审计覆盖主路径 |
| 7 | MCP 流式进度（SSE/streamable-http） | 大，且与 Quick Tunnel fail-closed 立场冲突 | 独立设计议题，不建议顺手做 |

---

## 3. "大型开发"差距

### 3.1 形态天生、改不了的

1. **决策循环在对面**：每一步都要聊天窗口的模型发起下一次 `tools/call`；MCP 反向通道 `sampling` 未实现（`src/mcp.rs:328-342`），且主流网页客户端也不支持。100 步的任务=100 次聊天往返的 token 成本与延迟。
2. **上下文不归你管**：服务端看不到聊天历史，compaction 只能靠"少返回一点"（现有 compact 系列全是响应体裁剪，不是上下文压缩）。
3. **单步必须在客户端超时内返回**：同步工具硬上限 120s（`helpers.rs:330`），MCP 分发 150s，Salvo 全局 300s。超过的只能走 `run_job` 异步轮询。
4. **句柄易失**：`task_id`/`session_id`/`job_id` 都活在聊天上下文里，窗口一关就丢。

而且这是**主动选择**：`PRODUCT_DEVELOPMENT_PLAN.zh-CN.md:103`——"WebCodex 不提供 prompt loop、模型选择、context compaction、token budget 或本地推理……不用于把 WebCodex 变成另一个 coding agent"。**产品定位应当照此对齐：它是"远程、可审计、有界的执行手"，不是"网页版 Claude Code"。** 用它做大型开发的正确姿势是"人+聊天模型规划、小步提交、本机验收"，而不是指望它自主跑长循环。

### 3.2 关键约束数字（写文档/设预期用）

| 约束 | 值 | 位置 |
|---|---|---|
| 同步工具超时 | ≤120s（超范围拒绝，非 clamp） | `tool_runtime/helpers.rs:330` |
| MCP 分发硬超时 | 150s | `mcp.rs:26` |
| agent 离线拒绝窗口 | 60s（不入队，直接失败） | `shell_client/mod.rs:56`、`shell_client/jobs.rs:211` |
| `read_file` 单次 | ≤2000 行 | `tool_runtime/files.rs:114` |
| Connector `files_read` | ≤8 文件 × 500 行 | `connector_runtime/surface.rs:46,52` |
| shell 输出 | 12,000 字符 tail（同步）/ 256KiB（agent 上限） | `helpers.rs:325`、`shell_client/mod.rs:55` |
| `apply_text_edits` | ≤16 文件 × 20 edits | `apply_edits_shared.rs:86,89` |
| session 事件 | 每 session 200 条（FIFO 淘汰，先丢最早的决策） | `sessions/model.rs:15` |
| 可写任务槽 | 每 project 同时 1 个 | `connector_runtime/mod.rs:614-633` |
| 逃生口 | `run_job` 最长 7 天，但需模型轮询 | `tool_runtime/jobs.rs:608` |

### 3.3 现架构上值得补的（ROI 排序）

1. **`task_list` / `task_resume` capability（最高价值）**：durable 状态齐全，唯独缺 model-visible 的"我上次做到哪"。目前中断恢复的指引是让人敲 `webcodex task resume`（`connector_runtime/mod.rs:1941-1958`），9 个 capability 里模型自己够不着。加上它，"关窗口第二天接着做"才成立——这是对"大型开发"最实质的一步。
2. **粗粒度批处理**：`edit_and_check`（apply→自动跑 checks→失败带结构化诊断返回）把 3 次往返压成 1 次；MCP 面的 `read_many`。参照 `start_coding_task` 聚合器的既有思路（`coding_task.rs:1-6`）。这是不引入 agent loop 的前提下把步数降一个数量级的唯一办法。
3. **有界的确定性自动重试**：checks 遇 flaky 重跑 1 次、cargo lock 争用退避。纯确定性，不违反 Non-Goal。
4. **`read_file` 分页游标**：带 `next_cursor`，让模型不用猜行号。
5. **离线容忍**：60s 硬拒对"合盖两分钟"太严苛，可对异步 job 保留短排队窗口（同步仍 fail-fast）。
6. **session 分层保留**：decisions/risks 不淘汰，tool_call 事件先淘汰；接近上限时在工具返回里挂 `context_pressure: high` 提示模型先 handoff。

### 3.4 Claude Code provider 实验的定位澄清

712b81c + da2afd3 **不是**把 Claude Code 接进来当执行引擎：它只把 `claude mcp serve` 当作 agent 内部 `search_project_text`/`edit_file` 两个 capability 的可选后端，白名单固定 `Read/Edit/Write/Bash` 四个工具，显式拒绝全部编排类工具（`Agent`/`Task*`/`Workflow`…，`docs/experiments/claude-tool-harness.md:314-329`）；且服务端请求 kind 白名单里没有任何 `claude_*`（`shell_client/validation.rs:226-251`）——**生产链路当前无法触发它，纯属研究性探针**。这个边界是对的：接进编排工具会让权限/审计/有界性全部失效。

---

## 4. 测试审计与瘦身方案

### 4.1 先说事实：重复 ≠ 主要问题

对 2141 个测试做归一化哈希扫描（去空白注释、字面量抽象）：

- **逐字重复：2 组 4 个**（`runtime_http/tests/jobs_tests.rs` 与 `audit_http/tests.rs` 各一对 bearer-auth 测试）。
- **模板克隆（仅字面量不同）：30 组 76 个（3.7%）**，且大半是合理的参数变体（`validate_id_rejects_empty/nul/slash` 之类）。
- 体量真正的去处：**场景级长测试**（`schema/migration.rs` 平均 176 行/测试、`coding_task.rs`/`handoff.rs` 各 ~92 行、`execution_tests.rs` 77 行）和**同一份工具定义表在 MCP/OpenAPI/policy 多个投影下的反复断言**（`tests/schema/` 子树 7,130 行）。

换句话说：**删"重复测试"能省的行数有限（约 2,500 行 / 2.7%），感觉"多"的根源是多层投影 + 场景测试 + 基建复制。**

### 4.2 比清理更优先的两件事

1. **上 CI**。全仓 9 万行测试目前只在你手动 `cargo test` 时执行；5,685 行 e2e shell 脚本被环境变量门控、执行次数为 0。一个最小的 GitHub Actions（`cargo fmt --check` + `cargo test` + 缓存）比删几千行测试更能兑现这些测试的价值。主二进制 44.5s 的运行时间完全可以承受。
2. **修 2 个并发 flaky**（本次实测发现）：全量 `cargo test` 满载并行时 `connector_runtime::execution_tests::transient_check_status_recovers_within_grace`（`execution_tests.rs:2116`，宽限期时序）和 `starting_cancel_late_attach_binds_job_and_dispatches_compensating_stop`（`execution_tests.rs:1984`，`cancel_requested` vs `cancelled` 竞态）会挂，单独跑两次均通过。上 CI 前必须先修，否则 CI 天天红。

### 4.3 清理方案

#### P0 — 纯删，零覆盖损失（≈ −500 行 / −19 测试）

| 目标 | 位置 | 说明 |
|---|---|---|
| 孪生 operation-count 测试 | `openapi.rs:3520` | 与 `:3274` 逐字节同体，且少了 `<=30` 上界断言（已逐行核实） |
| 被蕴含的 operation-ids 弱版 | `openapi.rs:2082` | `:2070` 的 `assert_eq!(ids, expected)` 严格更强 |
| **自证假测试** | `tests/schema/specs.rs:224` | 自己序列化 specs 再断言等于 specs（x==x）；真实覆盖已在 `mcp.rs:627`（已核实） |
| 源码/文档 grep 型测试 ×2 | `schema/migration.rs:1306`、`:1479` | 断言源码含某注释短语、文档含某措辞，改注释即红 |
| run_codex 缺席测试（被完全覆盖） | `schema/definitions.rs:148` | `migration.rs:1385` 逐条覆盖同样断言 |
| LEGACY_FORBIDDEN_PATHS 两个子集切片 | `openapi.rs:3485`、`:3507` | 注释自认是子集重述 |
| 被映射表蕴含的路径集合测试 | `openapi.rs:2206` | 由 `:2554` 的 (path, operationId) 表可推出 |
| 常量自证 + 纯 serde derive 往返 | `files.rs:569`、`openapi.rs:3456`、`shell_protocol.rs` 约 5 处 | 几乎不可能失败 |
| token.rs 三对"弱版/强版" | `oauth_http/tests/token.rs:503/535/674` | 把 `invalid_grant` 断言并入强版后删弱版 |

#### P1 — 表驱动合并（≈ −970 行 / −61 测试，覆盖不变）

最大的几笔：`auth/mod.rs` 的 `enforce_token_surface_*` 10 连（−140 行）与 OAuth2 scope 门禁 11 连（−65 行）；`runtime_http.rs` 的 flatten 7 连（−100 行）；`db.rs` 的 `consume_authorization_code_*` 5 连提 fixture（−100 行）；`token.rs` 两组镜像错误路径（−120 行）；`jobs.rs` 的 `run_shell_*` 5 连提 helper（−91 行）；`files.rs` 的 search 退出码 5 连——其中 grep 版与 rg 版逐字节只差 backend 字符串（−46 行）；`shell_client` 两组（−100 行）；`schema/spot_checks.rs` 同一循环体复制两遍（−35 行）。

#### P2 — 结构性（≈ −1,050 行 + 大幅降低未来维护成本）

1. **新建 `src/test_support/`**：`test_config` 逐字节复制 6 份、`test_db` 9 份、`seed_user` 7 份、`seed_oauth_client` 4 份（均已核实）。收敛后每个新 HTTP 模块的测试样板从 ~40 行降到 1 行 import（−220 行）。
2. **精简 `migration.rs` 千行黄金表**：`tool_definition_runtime_tool_policy_inventory_is_stable` 约 1,000 行硬编码，其中 6 列已被 `schema/policy.rs:4` 的不变量测试覆盖，删列保留 name→category 精简清单（−600 行；加新工具的成本从 12 行降到 2 行）。此项改变回归防护形态，建议单独一个 commit 做。
3. **守卫断言收敛**：`run_codex` 缺席断言散落 10 个文件、`delete_files` 55 处——统一改调已存在的 `assert_model_facing_surfaces_do_not_list_name`（`migration.rs:1554`）（−80 行）。
4. **测试块外移**：`db.rs`（52 行门面 + 1,911 行测试）、`auth/mod.rs`（276+2,222）移到独立测试文件，纯可读性。
5. **e2e 脚本裁剪**：`test-agent-config-reload-e2e.sh` 的四个字段断言与 `webcodex-agent.rs:1853/1901/1945` 的单测完全重叠，裁到只剩它独有的 PID 存活 + marker 检查（−150 行 shell）。其余 5,685 行脚本的去留是产品决策：要么给它们一个 CI job 定期跑，要么归档删除——**最差的状态就是现在这样养着不跑**。
6. **环境变量锁耦合**：MCP compact 测试借 `admin_cli::TEST_ENV_LOCK` 做 env 隔离（`mcp.rs:632` 等），改显式参数注入后可并行。

#### 不要动的

`connector_runtime/execution_tests.rs`（3,090 行/40 测试）fixture 分层是全仓最佳实践，测试间差异真实——**它应该作为其他文件重构的模板**，而不是清理对象（但要先修 §4.2 的两个 flaky）。

### 4.4 建议执行顺序

```
① 修 2 个 flaky（execution_tests.rs:1984/2116）
② 加最小 CI（fmt + cargo test，Linux 单平台即可）
③ P0 纯删（一个 commit，~500 行）
④ P1 按文件逐个表驱动化（每文件一个 commit，方便回退）
⑤ P2#1 test_support 收敛 → P2#3 守卫收敛 → P2#2 黄金表精简（单独决策）
⑥ e2e 脚本去留决策
```

---

## 5. 总路线图（把三条线合在一起）

| 周 | 主题 | 内容 |
|---|---|---|
| 第 1 周 | 测试地基 | flaky ×2 → CI → P0 → P1 |
| 第 2 周 | 反盲盒速赢 | timeline 渲染、审批命令预览、console approvals 面板、执行中 changed_paths（§2.4 的 1-4） |
| 之后 | 大型开发能力 | `task_list`/`task_resume` capability → `edit_and_check` 批处理 → read 游标（§3.3 的 1-2-4） |
| 长期决策 | 定位 | 是否实现真 `require_approval` 人审闭环；MCP 流式作为独立设计议题；产品文案明确"执行手而非 agent"定位 |

---

## 6. 执行记录（2026-07-26，同日）

按 §4.4 顺序执行了 ①②③ 及部分 ④（未提交，工作树中）：

1. **flaky ×2 已修**（`execution_tests.rs`）：`starting_cancel_late_attach…` 的 quick-yield 预算 20ms→2s（终态即提前返回，通过路径不变慢）；`transient_check_status_recovers_within_grace` 的固定 30ms sleep 改为有界轮询（grace 从 monitor 首次观测失败才起算，等待安全），grace 200ms→2s，恢复轮询 100→400 次。生产代码未动。
2. **CI 已加**：`.github/workflows/ci.yml`（fmt --check + cargo test --locked + rust-cache，Linux）。
3. **P0 完成**：删 `openapi.rs` 孪生 count 测试、弱版 ids 测试、两个 LEGACY 子集切片；`:2554` 重写为 25 条全量 `(path, operationId)` 表（新增钉扎此前两处都漏掉的 `/api/artifacts/import`，并加 `expected.len()==GPT_ACTION_OPS.len()` 防漏）；删 `specs.rs` 自证假测试、`migration.rs` 源码措辞 grep 测试（`:1479` 保留 dead_code 防回潮断言，删文档措辞段）、`definitions.rs` run_codex 测试（migration.rs 逐条覆盖已核实）、`files.rs` 常量自证；`token.rs` 三对弱/强合并（`invalid_grant` 断言并入强版）。
4. **P1 起步**：`spot_checks.rs` 两份同体循环合并为 14 case 单表；`runtime_http.rs` params/arguments 三连与 git 工具 dispatch 三连各并为表驱动。
5. **净变化**：9 文件 +177/−661 行，−17 个测试函数（12 删除 + 5 并入表驱动）；全量套件三个二进制 2242 通过 / 0 失败，fmt 干净。

**对 §4 审计报告的两处修正**（复核后推翻原判断）：
- `shell_protocol.rs` 的往返测试**不删**：这是跨版本线协议，它们钉住的是 wire 格式契约（type tag、`auth_token` 不得出现在 WebSocket 帧、`None` 字段必须省略），不是 derive 自证。`openapi.rs:3456` 同理保留（钉 OpenAPI 3.1.0 顶层结构）。
- "完全重复 2 组"系扫描脚本误报：注释剥离正则把 URL 中 `//` 之后的差异吞掉了。`run_shell/run_job`、`sessions/stats` 两对各测不同端点，均保留。真实的逐字重复为 0。

**P1 第二批（同日晚些）**：`auth/mod.rs` 的 `enforce_token_surface_*` 10 个合并为 `enforce_token_surface_matrix`（7 行 ctx×路径×预期矩阵，统一断言 FORBIDDEN + 消息子串）；OAuth2 scope 门禁 11 个合并为 `oauth2_scope_gate_matrix`（允许表 / insufficient_scope 表 / shared-key 账户封锁循环）。auth 模块 123→104 个测试，行为覆盖不变。累计：10 文件 +347/−896 行，测试函数 2149→2113（−36）。

**手写 `json!` 排查**（用户提示）：产品面三大手写 JSON——`openapi.rs` 整份 spec（有 schema 漂移测试群守护）、`surface.rs` 九能力 schema（有 connector 测试守护）、`mcp.rs:118` `mcp_info` 的方法清单（**手写且无测试钉扎**，当前与 dispatch 分支一致；若要守护可加 10 行 pin 测试）。测试侧原有的手写 `json!` 自证（`specs.rs:224` 自己序列化再断言自己）已在 P0 删除。

**P1 第三批（同日，P1 完结）**：jobs.rs（48→44：read_lines 4→1 表驱动 + run_shell exit 码 2→1，提取 `log_fixture`/`run_shell_via_agent`）；files.rs（search 退出码 5→1、validate_patch 4→1）；shell_client/mod.rs（validate_file_request 8→4、enforce_register_owner 6→1）；db.rs（`seed_consumable_code()` fixture，4 个 consume 测试各瘦 ~30 行）；token.rs（5 个无状态错误路径并为 `malformed_token_requests_return_structured_errors`）；mcp.rs 新增 `MCP_INFO_METHODS` 单一事实源常量 + `mcp_info_advertised_methods_match_dispatch` 钉扎测试（handwritten json! 契约从此有守护）。

**事故记录（重要，含事后取证修正）**：本轮并行执行期间发生一次工作树清空事故。取证后的准确时间线（本地时间，UTC+8）：15:38 三个测试重构子代理启动；**15:42:47 files 子代理**为"还原被整仓 cargo fmt 顺带重排的无关文件"执行 `git checkout -- <13 个文件>`，把当时全部未提交改动打回 HEAD；**15:43:52 jobs 子代理**又执行了一次同类 checkout。最初曾怀疑本机存活的 `webcodex-agent` 守护进程（sg4.yyjeqhc.cn，polling 传输）参与还原，**取证否定了这一点**：其日志（agent.log）自 11:57 后零写入，事发窗口无任何活动，QUIC 未配置、WebSocket 超时、无活动连接——15:45 出现的"陌生" `cargo test files` 进程也是 files 子代理自己。**不存在远端线上会话。** 全部改动已按会话记录逐字节重放并验证。**教训**：(1) 并行子代理的任务书必须明令禁止 `git checkout/restore/stash` 与整仓 fmt（"改动只限本文件"的措辞会诱导代理用 checkout 清理别人的文件）；(2) 已用 `git stash store` 留下不改工作树的快照兜底；(3) 该守护进程 file_write/git/raw_shell 全开且注册了 17 个项目——虽然本次清白，但任何能触达 sg4 服务端的窗口都可写本机，开发期间不用时建议停掉。

**反盲盒第一批（同日，§2.4 的 #1 与 #4 + 部分 #5）**：
- **console 渲染 timeline**：前端新增 Timeline 区块（`console.html` + `app.ts` 的 `renderTimeline`，倒序、textContent-only、payload 摘要化），消费后端一直在返回却被丢弃的 `recent_events`；`styles.css` 补配套样式；dist 已重建（tsc + build.mjs + 前端测试 15/15）。
- **执行中不再全黑**：`edits_apply` 事件 payload 现在带有界 `changed_paths`（取自请求本身，schema 上限 16 文件）；执行活跃时 `task_review` 的 changes 从"全空"改为**从任务自己的事件日志聚合已应用路径**（`aggregate_applied_paths`，`changed_paths_source: "applied_edits"`），diff 仍延迟。设计要点：**没有**采用"执行中调 show_changes 扫工作区"的方案——那会让 review 长轮询卡在同步 agent 调用后面（现有测试钉了 500ms 内返回），事件日志聚合是零延迟、零 agent 往返的替代。
- 新增链路测试 `active_review_surfaces_applied_paths_without_diff`（edits_apply → 长命令 running → review 断言路径/来源/空 diff/事件 payload）+ 现有活跃 review 测试补形状断言。

---

*本报告由三路并行深查（盲盒溯源 / 大型开发差距 / 测试重复度审计）+ 人工核实关键代码点合成；所有 file:line 均对照 main @ 9018814 验证。*

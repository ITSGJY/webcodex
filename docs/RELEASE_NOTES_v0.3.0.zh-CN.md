# WebCodex 0.3.0

[English](RELEASE_NOTES_v0.3.0.md) | [简体中文](RELEASE_NOTES_v0.3.0.zh-CN.md)

WebCodex 0.3.0 把自托管 coding runtime 收敛成一个可持久、Project-first 的工作流：任务能够跨客户端窗口继续，执行证据可以审阅，人类仍然掌握接受、拒绝和取消决定。

这个版本面向通过 ChatGPT、MCP 客户端或 GPT Actions 操作私有仓库，同时要求执行留在自有机器上的用户和运维者。

## 主要变化

- **持久化项目任务。** 托管客户端可以启动绑定项目的任务，在隔离 task workspace 中应用编辑，执行项目感知的 validation recipes，审阅结果并明确接受或拒绝。
- **跨会话续接。** `task_list` 与 `task_resume` 不依赖某个客户端专属的隐藏 session 信号，即可找回近期任务；拒绝理由可以作为一次性 human guidance 回传给模型。
- **可实际使用的 review console。** 浏览器 console 现在包含任务时间线、applied paths、diff review、accept/reject/cancel、guidance、待审批命令、设备与活动视图，以及集中展示 MCP、GPT Actions 和 OAuth 地址的 Connect 面板。
- **更好的检查与验证。** runtime 增加基于 Git index 的文件发现、确定性 project overview、更强的项目搜索、紧凑 numbered read、只读 Rust LSP 导航和结构化 validation summary。
- **更严格的执行边界。** Session 生命周期、权限决策、持久证据、transport timeout、断线处理、进程组回收、provenance 检查、敏感路径策略与 symlink 防护均得到加强。
- **更一致的运维命令。** Project setup、doctor/status、login/logout/status、enrollment、service 安装和 runner 命名契约更加统一。

## Project-first Execution

0.3.0 在 hosted connector surface 后面引入了持久化 execution path：

- Task 由认证主体拥有，并绑定到 agent 注册的项目。
- 编辑与验证通过可复用 Execution Engine 执行，而不是依赖临时客户端循环。
- Task result 持久保存 applied paths、validation 状态、execution evidence 和本地 accept/reject 决定。
- 当实时 workspace scan 退化时，review 会回退到已持久化的 applied-path evidence，不会让实际改动消失。
- 确定性的 workspace provenance 失败会快速进入诚实终态，不再被压平成可重试的 storage error。
- `doctor` 会提示 untracked build artifacts、缺少 `.gitignore` 等常见 hygiene 问题，但不会把仍可使用的项目错误判定为不可用。

## Review、Guidance 与跨窗口续接

Host-local console 不再只是最小结果页：

- 运行中和已完成任务都能显示有界 event timeline 与当前 applied paths。
- Review action 绑定稳定的 task/result identity，支持 accept、reject 和 cancel。
- Reject 可以附带有界理由；即使后续 guidance 投递失败，拒绝决定本身仍然持久有效。
- Human guidance 通过正常 capability response 路径一次性投递，不会被 console 自动刷新抢先消费。
- `task_list` 与 `task_resume` 允许新聊天窗口使用同一凭据查找并重新绑定已有任务。
- Connect 面板根据已配置公网 origin 或当前浏览器 origin 生成非秘密的 MCP、OpenAPI 与 OAuth 地址；当页面位于 hosted client 无法访问的 loopback 地址时，会明确提示先建立 tunnel。

## 工具与开发体验

- `start_coding_task` 返回确定性的 startup package：项目解析、Git 状态、项目规则、runtime health、semantic navigation 能力和有界 tool manifest。
- `finish_coding_task`、session handoff、validation summary 与 hygiene check 提供有界 closeout evidence，不暴露原始命令输出或凭据。
- `list_project_tracked_files` 从 Git index 发现文件，支持 roll-up、分页和 glob 过滤。
- `search_project_text` 支持有界上下文、include/exclude globs、结果模式，以及明确的 backend/truncation metadata。
- `read_file` 去掉重复的行级 payload，只保留稳定的 `numbered_text` 表示。
- Rust workspace 可以通过 agent-side language server bridge 使用只读 document symbols、diagnostics、definitions、references、hover 和 workspace symbols。
- 结构化 Cargo validation 会记录 parser-backed events，供 closeout 和恢复分析使用。
- 普通编辑优先 `apply_text_edits`，复杂 unified diff 优先 `apply_patch_checked`；有界 shell/job 仍是 escape hatch，而不是默认编辑或验证路径。

## 可靠性与安全变化

- Workflow session 具有显式 lifecycle 与 close 语义；已关闭的有效 session 仍可查询，但 consequential tools 会被拒绝。
- Permission decision 在 mutation 或 agent enqueue 之前集中执行，并带审计关联与 fail-closed read-only guard。
- Agent transport 断开或超出 online window 时，等待中的同步请求会快速失败。
- MCP、HTTP service、本地命令、Git 与 validation 路径均加入有界 timeout backstop。
- 本地命令会回收整个 process group，后台后代进程不能再无限持有输出 pipe。
- Session persistence 移出请求关键路径，SQLite storage 使用更严格的 open/WAL/cleanup 行为。
- 敏感路径检查在文件工具之间共享；`read_file` 与 connection storage 会拒绝 symlink escape 和未验证目录。
- 实验性的 Landlock command-sandbox foundation 仍不对 `read_only` task 启用，因为它不能限制读取、继承环境变量和网络访问；因此 `read_only` 继续拒绝命令执行。

## Breaking Changes

### `webcodex-agent` 更名为 `webcodex-runner`

Executable、npm command、systemd unit、配置示例与 QUIC ALPN 统一使用：

```text
webcodex-runner
webcodex-runner.service
webcodex-runner/1
```

不提供旧名称 binary、npm、service 或 protocol alias。混用 0.2.x 与 0.3.0 的 runner/server 可能无法启动或连接。

### GPT Actions 编辑 surface 收缩

25-operation GPT Actions schema 不再包含独立的 `writeProjectFile` 与 `replaceProjectFileText`。兼容工具仍可通过 `callRuntimeTool` 使用；新工作流应优先使用 `apply_text_edits` 与 `apply_patch_checked`。升级后必须重新导入 OpenAPI schema。

### `read_file` 行号输出统一

当 `with_line_numbers: true` 时，客户端应读取 `numbered_text`；不再返回重复的 `lines` array。

### `read_only` task 不执行命令

`commands_run` 仍属于 consequential operation；在 `read_only` task 中，会在审批、reservation 和 agent enqueue 之前被拒绝。

### Response shape 使用 canonical 字段

若干过时 wire alias 与重复 closeout 字段已经删除。固定读取旧字段的客户端应刷新 MCP/OpenAPI schema，并使用 0.3.0 返回的 canonical names。

## 升级说明

1. 用同一版本的 0.3.0 build 替换三个 binaries：`webcodex`、`webcodex-cli`、`webcodex-runner`。
2. 停止并禁用旧 `webcodex-agent.service`，再安装并启用 `webcodex-runner.service`；确认两个 unit 没有同时运行。
3. 更新仍引用 `webcodex-agent` 的脚本、service override、binary path 与 runner 配置。
4. 使用 QUIC 时，server 与 runner 必须同时升级，以统一使用 `webcodex-runner/1` ALPN。
5. Custom GPT Actions 需要重新导入 `/openapi.json`；会缓存 tool schema 的 MCP 客户端也需要重新连接。
6. 重启 server 与 runners，执行 compact `runtime_status`，确认所有部署 binary 都报告 0.3.0、相同 clean build revision 和 `dirty=false`。
7. 在新客户端窗口继续持久任务时，先调用 `task_list`，再调用 `task_resume`。

npm package 仍是 thin installer。0.3.0 只按 `linux-x64` 准备；在把 GitHub Release 实际 tarball 的 checksum 写入 `npm/webcodex/manifest.json` 之前，不得发布 npm package。

## Security Model

- Repository access 仅限 agent 注册项目与配置的 allowed roots。
- Server 不扫描任意 agent filesystem。
- Token、pairing code、Authorization header、env file、完整 client config 和可复用 token hash 不得出现在 prompt、日志、示例或 commit 中。
- Shell/job 仍是能力很强的 consequential operation，需要收窄配置并由 operator 审阅。
- Browser console 只投影有界、非秘密的运维事实；凭据不会进入 DOM 或 response payload。
- Session、task、validation、audit 与 finish evidence 能提高可审阅性，但不能替代常规 code review、host logging、backup 或基础设施加固。

参见 [../SECURITY.md](../SECURITY.md)、[CONCEPTS.zh-CN.md](CONCEPTS.zh-CN.md) 与 [READ_ONLY_COMMAND_SANDBOX.zh-CN.md](READ_ONLY_COMMAND_SANDBOX.zh-CN.md)。

## 已知限制

- WebCodex 是自托管基础设施，不是 hosted SaaS。
- 0.3.0 npm wrapper 只按 Linux x64 准备。
- Browser console 是 review/operations surface，不是完整 IDE。
- LSP navigation 只读、以 Rust 为主、仅限 workspace，且不导航 dependency source。
- `read_only` task 不能运行命令。
- WebSocket 与 polling 是标准 zero-config release-smoke transports；QUIC 仍属于需要单独 focused coverage 的高级部署选项。
- Desktop GUI、PTY terminal workflow 与完整多窗口 optimistic coordination 不属于本版本。
- Production security 仍依赖 HTTPS、reverse-proxy policy、scoped tokens、OS-user isolation、agent configuration 与 operator discipline。

## 验证

当前 release candidate 已通过完整 Rust binary suite（main 1,750 passed / 4 ignored，CLI 220 passed，runner 402 passed / 2 ignored）、focused process-group cleanup tests、源码与 release checks、前端 typecheck/test/dist 检查以及 npm self-test。WebSocket 与 polling zero-config E2E 均为 108/108，coding-loop compare 为 6/6；83 个 Markdown 文件中的 438 个本地链接全部有效；release-mode npm package smoke 也成功临时安装并运行三个 0.3.0 binaries。

Release-preparation commit 会故意保留 checksum placeholder。只有在 immutable release tag 之后，把实际上传的 0.3.0 Linux x64 artifact checksum 提交到 manifest，且不移动 tag，npm package 才具备发布条件。最终 binaries 安装完成后，仍需由 release operator 执行 post-deployment acceptance。

## 下一步

0.3.0 之后应优先减少 round trips、增加确定性 retry、改善 deployment health diagnostics，并提供 decision-ready review summary，而不是继续扩大 public capability surface。参见 [ROADMAP.zh-CN.md](ROADMAP.zh-CN.md)。

# WebCodex 0.3.1

[English](RELEASE_NOTES_v0.3.1.md) | [简体中文](RELEASE_NOTES_v0.3.1.zh-CN.md)

WebCodex 0.3.1 重点改善官方托管路径的新用户接入体验，同时收紧 Runner transport 与 Job 恢复行为。

## 主要更新

- **一条命令完成 Hosted 接入。** 安装 npm package 后，用户进入仓库并执行 `webcodex connect https://sg4.yyjeqhc.cn`。CLI 会自动生成强随机 shared key、写入项目边界 profile、启动唯一 detached Runner，并确认托管 Server 确实能看到 Runner 与项目。
- **更安全的 shared-key 运行方式。** Hosted Runner 注册有数量边界；自动生成的 key 只完整显示一次；日志采用有界轮转；shared-key 过期不会再把已经 lost 的 Job 改写成误导性的终态。
- **统一 transport supervision。** WebSocket 与 QUIC 共用重连、关闭、fallback 和错误分类生命周期。Auto 模式可以回退到 WebSocket；strict QUIC 会把证书错误视为 fatal，把临时网络错误视为可重试。
- **macOS persistent-shell 兼容。** Darwin 使用 `pipe` 后立即设置 `FD_CLOEXEC`；Linux/Android 继续使用原子的 `pipe2(O_CLOEXEC)`。
- **更可靠的 Job 恢复。** Reconciliation 在 Server 重启后保留 job identity 与日志 cursor，区分可恢复和 legacy Runner 断线，并确保 hidden handoff/cleanup Job 不会被 public history retention 清理。
- **更低轮次的模型工作流。** Startup brief、continuation feedback、有界批量文件读取/搜索、异步 validation Job、SSH Session context 和 managed temporary project 降低调用轮次，同时不扩大项目边界。
- **中英文发布文档。** 仓库与 npm package 都补齐英文/简体中文接入、平台、恢复、免责声明和 LINUX DO 鸣谢内容。

## Hosted 最快接入

计划发布 Linux x64 与 macOS arm64 artifacts：

```bash
npm install -g @yyjeqhc/webcodex
cd /path/to/your/repository
webcodex connect https://sg4.yyjeqhc.cn
```

自动生成的 MCP key 只在首次创建时完整显示，请立即复制。它会保存在 owner-only profile 中，但 status 与日志命令不会再次显示。Detached hosted Runner 在关闭终端后继续运行，但机器重启会终止它；重启后重新执行 `connect`，或运行 `webcodex agent start --profile <profile>`。

## 升级说明

1. 使用同一个 v0.3.1 artifact/build revision 同时升级 `webcodex`、`webcodex-server` 和 `webcodex-runner`。
2. 重启 Server 与所有 Runner，确认三个二进制都报告 `0.3.1`、相同 commit 且 `dirty=false`。
3. MCP 或 GPT Actions client 缓存旧 tool list 时，刷新 schema。
4. Existing managed credential 与 hosted profile 继续严格分离；`wc_*` managed credential 永远不会 fallback 成 shared-key auth。

本补丁版本没有有意删除公共 CLI command 或 canonical MCP operation。

## 打包

npm package 是 thin installer/wrapper。v0.3.1 manifest 声明：

- `webcodex-v0.3.1-linux-x64.tar.gz`
- `webcodex-v0.3.1-darwin-arm64.tar.gz`

每个 artifact 都包含由同一个干净 tag revision 构建的 `webcodex`、`webcodex-server` 与 `webcodex-runner`。Release-preparation tag 会暂时保留 checksum placeholder。只有两个不可变 artifact 都上传完成，并在不移动 `v0.3.1` tag 的前提下，通过明确报告的 post-tag manifest commit 写入实际 SHA-256 后，才能发布 npm。

## 已知限制

- Hosted shared key 是 capability credential；持有者可以使用关联项目边界 Runner profile 的权限。
- Detached Runner 不是 OS startup service，机器重启后需要重新启动。
- Linux arm64、macOS x64 与 Windows 不在计划的 v0.3.1 npm 覆盖范围内。
- Browser console 是 review/operations 界面，不是完整 IDE。
- Production 安全仍依赖 HTTPS、收窄 credential、OS user 隔离、备份和 operator review。

## 免责声明

WebCodex 仅用于研究与学习。它能够在配置的项目边界内读取、修改文件并执行命令；请只在有版本控制和可恢复备份的环境中使用。若因使用本软件造成文件系统损坏、数据丢失或其他后果，作者概不负责。

## 验证要求

创建 tag 前，release candidate 必须通过格式化、workspace 编译/测试、hosted-connect 与 Job recovery 真实进程 E2E、npm installer/package smoke、release binary identity、Markdown 本地链接检查，以及 clean worktree/hygiene review。

## 鸣谢

感谢 [LINUX DO](https://linux.do/) 社区提供的交流氛围与开源推广支持。

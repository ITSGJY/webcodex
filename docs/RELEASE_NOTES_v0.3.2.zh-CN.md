# WebCodex 0.3.2

[English](RELEASE_NOTES_v0.3.2.md) | [简体中文](RELEASE_NOTES_v0.3.2.zh-CN.md)

WebCodex 0.3.2 让安装和运维更简单，同时收紧混合版本 Runner 行为，并精简 coding
tool surface。

## 主要更新

- **Server-only Docker Compose 部署。** 仓库新增经过收紧的 Dockerfile、Compose、
  env 模板和 bootstrap 脚本，可以只运行协调 Server，而不把项目仓库或 Runner
  工具链放进容器。
- **更安全的非 root Runner service。** Runner 生命周期命令支持明确的 user 与
  system scope。普通用户可以不使用 `sudo` 安装 `systemctl --user` 常驻服务；
  system service 必须显式指定 Runner 用户，root 运行需要明确 opt-in。
- **更清楚的 credential 指引。** CLI 诊断与文档明确区分 user/runtime credential
  和 Agent transport token，同时保持现有 Server 鉴权边界不变。
- **按 capability 控制项目注册。** 只有连接的 Runner 声明所需 capability 时，
  Server 才能根据绝对路径解析或注册项目；新 Server 会在发送不支持的内部请求前
  拒绝旧 Runner。
- **更小的编辑工具面。** 已删除退役的单用途 edit tools 和兼容分支；写入路径集中
  到 whole-file write、事务 text edits 和 checked patch。未知或已退役的 `file_*`
  请求会在 provider 或 shell fallback 之前失败。
- **更短的公共文档。** README 现在重点说明产品用途、安装、hosted 接入、Docker
  自托管，以及在线 AI 助手能够在已连接机器上完成的日常任务。

## 最快接入

```bash
npm install -g @yyjeqhc/webcodex
cd /path/to/your/repository
webcodex connect https://sg4.yyjeqhc.cn
```

把 `connect` 输出的 MCP URL 和生成的 key 添加到 ChatGPT 或 Claude，就可以让它
查看文件、修改代码、运行测试或操作 Git。

## Docker 自托管

镜像有意保持 server-only：包含 `webcodex-server` 和管理用 `webcodex` CLI，
不包含 `webcodex-runner`、项目仓库或语言工具链。

```bash
git clone --branch v0.3.2 --depth 1 \
  https://github.com/yyjeqhc/webcodex.git
cd webcodex
./deploy/docker/bootstrap.sh https://webcodex.example.com
```

当前 Compose 从 tag 对应的源码 checkout 构建。把同一个 server-only image 发布到
GHCR 或 Docker Hub 是独立的 release operation，不是 source 或 binary release
成立的必要条件。

## 破坏性变更

- 七个已退役的单用途 edit tools 不再暴露；请改用事务 text edits、checked patch
  或 whole-file write。
- 缓存 MCP 或 GPT Actions schema 的 client 必须刷新后再使用 0.3.2 tool surface。
- 旧 Server 向 0.3.2 Runner 发送已退役或未知的 `file_*` request kind 时，会收到
  确定性的 unsupported-request 失败，不再进入 provider 或 shell fallback。
- 本次升级不建议混用不同版本的 Server 与 Runner。

## 升级说明

1. 使用同一个 v0.3.2 tag revision 同时升级 `webcodex`、
   `webcodex-server` 和 `webcodex-runner`。
2. 重启 Server 与所有 Runner，确认三个 binary 都报告 `0.3.2`、相同的干净 build
   revision，并且 `dirty=false`。
3. 旧 edit tool surface 已删除，缓存旧 tool list 的 MCP 或 GPT Actions client 需要
   刷新 schema。
4. Hosted profile 与 managed credential 继续严格分离；Agent token 仍不能用于
   project/runtime API。
5. 非 root Runner 优先使用 `--scope user`；重新安装前先检查已有 system service。

## Binary 打包

计划发布的 binary artifacts：

- `webcodex-v0.3.2-linux-x64.tar.gz`
- `webcodex-v0.3.2-linux-arm64.tar.gz`
- `webcodex-v0.3.2-darwin-arm64.tar.gz`

每个 artifact 都必须包含从不可变 `v0.3.2` tag 构建的 `webcodex`、
`webcodex-server` 和 `webcodex-runner`。只有实际上传 bytes 的真实 SHA-256 已写入
release manifest 后，才能发布 npm package。

## 已知限制

- npm package 当前不覆盖 Windows、macOS x64 和其他 targets。
- Docker 容器只运行协调 Server；每台持有仓库的机器仍需要 Runner。
- Detached hosted Runner 在关闭终端后继续运行，但机器重启后需要重新启动，除非已
  安装为 OS service。
- 已连接的 client 可以在配置边界内修改文件和执行命令；请使用版本控制、可恢复
  备份和权限合适的 OS 用户。

## 发布前验证

最终 tag candidate 应通过格式化、workspace 编译与测试、hosted-connect 和 Runner
service 生命周期 E2E、npm package smoke、release binary identity、Docker
build/health smoke、Markdown 本地链接检查与 clean-worktree review。

## 后续检查

升级后刷新 client schema，核对 Server 与 Runner 的 build identity，并先运行一次
只读项目任务，再恢复写入操作。

## 鸣谢

感谢 [LINUX DO](https://linux.do/) 社区提供的交流氛围与开源推广支持。

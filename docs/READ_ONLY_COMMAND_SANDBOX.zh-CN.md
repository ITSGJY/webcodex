# Inspect 命令沙箱

[English](READ_ONLY_COMMAND_SANDBOX.md) | [简体中文](READ_ONLY_COMMAND_SANDBOX.zh-CN.md)

## 会话模式

WebCodex 的 Workflow Session 有三种模式：

| 模式 | 结构化写工具 | shell/job/验证命令 |
|---|---|---|
| `normal` | 按用户 guard 和策略正常允许 | 普通执行 |
| `inspect` | 拒绝 | 只允许进入 fail-closed Landlock inspect 沙箱 |
| `read_only` | 拒绝 | 在入队或执行前拒绝 |

`read_only` 仍然没有 shell。只有在可信项目检查确实需要 `rg`、
`git status`、`node --check` 或 `cargo check` 等命令时才使用 `inspect`。

## 安全承诺

`inspect` 是可信检查模式，只承诺一条狭窄边界：

> inspect 命令及其所有子进程不能在唯一的私有 scratch 目录之外执行普通本地文件系统写入。

它不是“无副作用”或保密沙箱。读取不受限，runner 环境大体继承，网络没有隔离，
命令也能访问外部服务。因此命令仍可能读取敏感数据、传输数据或产生远程副作用。
不得把 `inspect` 描述为完全隔离、完全无害或普遍无副作用。

## Linux Landlock 边界

inspect 命令仅支持 Linux，并要求 Landlock ABI v3。最低要求 v3 是因为其写权限集合
包含 `TRUNCATE`；同一规则集还覆盖该 ABI 表达的创建、删除、rename/refer 及其他写权限。

runner 会：

- 以 hard compatibility 处理 ABI v3 的全部写权限；
- 只接受 `FullyEnforced`，拒绝 partial 和 best effort；
- 在 `pre_exec` 中施加规则，确保目标程序启动前生效并由所有子进程继承；
- 让项目、依赖缓存和系统路径保持可读但不可写；
- 只允许一个命令/job 私有 scratch 目录保存写入数据；为支持常规 Git/Cargo
  行为，精确的非持久化 `/dev/null` 字符设备接受 `WriteFile`，但 `/dev`
  目录层次仍不可修改；
- 以 `0700` 原子创建 scratch，确认它是真实目录而非符号链接，并在命令/job 进入终态后清理；
- 在非 Linux、内核不支持、探测失败、能力缺失、未知模式或规则应用失败时拒绝 inspect。

绝不会静默降级到普通 shell。

## 临时写环境

每条 inspect 命令都会获得：

- `TMPDIR=<scratch>`
- `CARGO_TARGET_DIR=<scratch>/target`

现有 Cargo registry、Git 数据、依赖缓存、工具链和项目文件仍可读取。Cargo 构建产物
写入 scratch，因此常规 `cargo check` 和 `cargo test` 不需要在 checkout 里创建
`target/`。需要改写 checkout 的命令（例如不带 `--check` 的 `cargo fmt`、包安装）
应正常失败。

shell profile 的准备阶段可能在目标命令前执行 init script。inspect 因此跳过 prepared
profile 初始化并使用基础 shell；全局 shell init script 如存在，会在 Landlock 边界内执行。

## 推荐检查流程

代码搜索和定向检查优先使用 `run_shell` 配合 `rg` 或 `git grep`。
`search_project_text` 暂时保留兼容。常见命令：

```text
rg 'pattern' src
git grep 'pattern'
git status --short
git diff
git show
node --check path/to/file.js
cargo check --all-targets
cargo test
```

通过重定向、`truncate`、`rm`、`mv` 或子 shell 修改项目文件都会被拒绝；
`$TMPDIR` 下的写入允许。

## 已知限制

- 文件读取不受限制，包括项目外路径；
- 不承诺环境变量隔离；
- 文件系统规则不限制网络和 IPC；
- 外部 API 和其他服务仍可能被修改；
- Landlock 只治理所要求 ABI 暴露的文件系统权限，不是容器、虚拟机、syscall filter
  或完整主机沙箱；
- 进程或主机异常终止时，残留 scratch 可能只能依赖操作系统的临时目录维护清理。

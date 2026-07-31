# Shell Profiles（Prepared Environment Snapshots）

[English](SHELL_PROFILES.md) | [简体中文](SHELL_PROFILES.zh-CN.md)

默认情况下，`run_shell` 和 `run_job` **不会**保持持久 shell session。它们会为每个
project/profile 准备一次 environment snapshot，然后将每次命令作为独立进程运行。
第 9 节的显式 Workflow Session persistent-shell 工具是唯一的长生命周期例外。

本文档说明 shell profiles 的工作方式、配置方法和安全边界。

> 适用于 `webcodex-runner`，即真正执行 shell commands 的 host agent。server 不会读取或保存 shell env values、`init_script` bodies 或 tokens。

## 1. 什么是 prepared shell env snapshot

执行项目命令时，agent 会：

1. 解析有效 profile：`project.shell_profile`，否则 `shell.default_profile`，否则 plain shell config。
2. 为 `project/cwd + profile` 准备一次 environment snapshot：启动 profile program，应用 `env_clear` 和 profile `env`，运行可选 `init_script`，并通过 `env -0` 捕获环境变量。
3. 按 `project/cwd + profile name` 缓存 snapshot。
4. 后续每次命令都以 fresh process 运行，并应用 cached snapshot。

因此没有 long-lived shell，不会每条命令都 `source`，默认也不会加载 `.bashrc` 或 `.profile`。

## 2. 为什么默认不 source `.bashrc` / `.profile`

WebCodex 有意不在准备 snapshot 时 source 交互式 shell 启动文件：

- `.bashrc` 可能很慢。
- 可能包含 prompt、`stty`、echo 等交互式命令，导致非交互式 capture 卡住或污染输出。
- 可能泄露或污染环境。
- 在不同 host/user 上不可复现。

应使用显式 shell profile。显式 profile 更容易审计、更快，也不依赖 agent 用户的交互式 shell 配置。

## 3. Rust / Cargo 示例

```toml
[shell]
default_profile = "rust"

[shell.profiles.rust]
program = "sh"
args = ["-c"]

[shell.profiles.rust.env]
PATH = "/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
CARGO_HOME = "/root/.cargo"
RUSTUP_HOME = "/root/.rustup"
```

该 profile 不需要 `init_script`，因为 env block 已经设置了 `PATH`、`CARGO_HOME` 和 `RUSTUP_HOME`。

## 4. Python venv 示例

```toml
[shell.profiles.py-venv]
program = "bash"
args = ["-lc"]
init_script = '''
source .venv/bin/activate
'''
```

`init_script` 是 project-relative：`.venv/bin/activate` 从项目根目录解析，因此每个项目可以激活自己的 venv。

## 5. Conda 示例

```toml
[shell.profiles.conda-ml]
program = "bash"
args = ["-lc"]
init_script = '''
source /opt/miniconda3/etc/profile.d/conda.sh
conda activate ml
'''
```

## 6. 将项目绑定到 profile

项目 TOML（`projects.d/<id>.toml`）可以指定 profile：

```toml
id = "paper-exp"
path = "/root/git/paper-exp"
shell_profile = "conda-ml"
```

## 7. 解析规则

有效 profile 按以下顺序选择：

1. `project.shell_profile`；否则
2. `shell.default_profile`；否则
3. fallback 到 plain shell config。

`listProjects` 会暴露 `shell_profile`、`resolved_shell_profile` 和 `shell_profile_status`（`configured` / `missing` / `not_configured` / `unknown`）。

## 8. Dialect 上报

runner 注册时会上报 shell dialect 事实，server 与 agent 永远不需要猜测远端 shell：

- shell profiles summary 携带 `default_dialect`（`sh` | `bash` | `custom`，即
  runner 默认 shell program 的 dialect）和 `available_dialects`（始终包含 `sh`
  和 `bash`；存在 custom profile 时额外包含 `custom`）。
- 每个 profile 条目携带自己的 `dialect`。
- program 无法映射到 sh/bash 的 custom profile 上报 `custom`；需要确定性命令
  语法的 agent 必须显式选择 shell，而不是依赖 profile dialect。
- `run_shell`/`run_job` 上的显式 `shell=sh|bash` 始终覆盖默认值；server 不会
  猜测远端 shell。
- Dialect 上报只是 metadata：不会向 server 发送任何 PATH、env values 或
  `init_script` 内容。

## 9. Workflow Session 执行默认值

在 full operator runtime 上，绑定已注册项目的 Workflow Session 可以保存可选的
project-relative `default_cwd` 和可选的 `default_shell`（`sh` 或 `bash`）。
它们只是执行默认值，不是 prepared environment snapshot，也不会隐式复用进程。

`run_shell`、`run_job` 和 `open_session_shell` 按以下优先级解析：

1. 单次调用显式传入的 `cwd` / `shell`；
2. 与项目精确匹配且仍为 active 的 Workflow Session 默认值；
3. 现有的项目根目录 / configured shell-profile 行为。

不传 Session 时行为不变；其他项目的 Session 永远不会提供默认值。无效、不存在或
越出项目根目录的 cwd 会沿用现有安全检查并失败，不会静默退回项目根目录。
`run_shell` 和 `run_job` 仍是独立进程，某次命令中的 `cd sub` 不会影响后续
一次性调用。

`start_coding_task(execution_context=...)` 可以设置或替换该上下文；续接时省略会保留，
显式 `{}` 会清空。`update_session_context` 仅在必填 project 已授权且精确匹配
Session project 时，针对显式 active Workflow Session 执行完整替换；不允许跨项目
逃逸。context 与 event 在内存中共同提交，JSON ledger 由后台 writer 异步写入，
所以成功不表示同步落盘。上下文不能保存 env values、tokens、任意 options 或 shell state。

`open_session_shell` 是显式例外：它为该 Workflow Session 创建一个真正的长生命周期
`sh`/`bash` 进程。所选 profile 的环境和初始化脚本只在 open 时执行一次，因此后续
`session_shell_exec` 会保留 cwd、export、函数、umask 和 shell variable。Profile
的 `args` 仍是一种一次性命令调用约定，不会传给长生命周期进程；persistent `bash`
使用 `--noprofile --norc`，persistent `sh` 不使用 command-mode 参数。更新 Session
context 不会改变已经打开的进程；需要 close/reopen 才应用新的 cwd、dialect 或
profile 默认值。`read_only`/`inspect` Session 以及设置了 SSH resource 的 Session
会拒绝 persistent shell。

## 10. 修改配置

修改 `agent.toml` 后 reload Runner service。有效的 hot reload 会让新的一次性命令进入
新的 profile-cache generation，并在下一次使用时 lazy re-prepare；project TOML
独立刷新。

已经打开的 persistent shell 不会因 reload 被静默重启或改写。后续 exec/status 前
仍会重检当前 policy 与 project/profile 选择，但进程内已有的环境和初始化结果不变。
需要显式 close/reopen 才能应用 profile 内容、cwd 或 dialect 默认值的变化。Hot
reload 边界之外的配置仍需重启 Runner，详见
[DEPLOYMENT.zh-CN.md](DEPLOYMENT.zh-CN.md#agent-配置)。

## 11. 安全提示

- **不要**在 `init_script` 中放 tokens。
- **不要**在 `init_script` 中 `echo`/`printf` secrets。
- `runtime_status`、`listAgents` 和 `listProjects` 只暴露 sanitized metadata：profile name、`has_init_script`、`env_keys_count`、`program`、`args_count`、每个 profile 的 `dialect`，以及 summary 的 `default_dialect` / `available_dialects`。
- 它们不会暴露 `init_script` bodies、env values、tokens、Authorization header、完整 `agent.toml` 或完整 env snapshot。
- Agent token 相关环境变量会从 child process environment 中剥离。
- `prepare` 使用 `env_clear` 和显式 inherited keys allowlist；profiles 必须声明所需 env。

## 12. Troubleshooting

canonical project 使用共享只读 readiness：

```bash
webcodex doctor
```

高级 enrolled profile 使用 `webcodex agent status --profile workstation`
和 `webcodex ops status --strict`。profile preparation failure 继续经过
sanitization，不暴露 `init_script` body 或 env value。显式 project roundtrip 属于
operator smoke，不是普通 onboarding。

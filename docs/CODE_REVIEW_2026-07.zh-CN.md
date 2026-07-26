# WebCodex 代码评审（2026-07-26）

评审基线：`bd88a30`（`feat/claude-raw-tool-harness`）
修复分支：`review/path-boundary-fixes`

---

## 1. 代码量分布

**总计 190,332 行 Rust / 349 个文件**。生产代码约 8.0 万行，测试约 11.0 万行——**58% 是测试**。

| 模块 | 生产 | 测试 | 测试占比 |
|---|---:|---:|---:|
| `tool_runtime/` | 29,785 | 47,588 | 61% |
| `bin/`（两个二进制） | 21,115 | 18,364 | 46% |
| `src/*.rs`（根） | ~19,600 | ~8,700 | 31% |
| `connector_runtime/` | 2,465 | 8,682 | **77%** |
| `oauth_http/` | 3,315 | 6,220 | 65% |
| `db/` | 5,103 | 462 | **8%** |
| `shell_client/` | 3,027 | 3,615 | 54% |
| `auth/` | 1,762 | 3,106 | 63% |
| `runtime_http/` | 1,442 | 1,599 | 52% |
| 其余 HTTP 模块 | ~2,500 | ~3,800 | — |

`tool_runtime` 单模块占全仓 41%。

**测试投入与风险不匹配**：`connector_runtime` 是一层薄门面，却有 77% 覆盖；`db/`（含 `task_kernel.rs` 2,207 行，任务状态机核心）只有 8%。

### 最大的单文件

| 文件 | 行数 |
|---|---:|
| `src/bin/webcodex-agent.rs` | 7,114 |
| `src/tool_runtime/files.rs` | 4,297 |
| `src/connector_runtime/mod.rs` | 3,740 |
| `src/openapi.rs` | 3,567 |
| `src/shell_client/mod.rs` | 3,525 |

---

## 2. 为什么"功能不多但代码很多"

功能其实不少：**75 个 model-visible 工具**（`tool_definition/` 中 75 处 `def(`）+ 9 个 connector 能力。

README 说的"恰好九个能力"只是 `connector_runtime` 那层产品门面，它通过 `invoke_kernel` 委托给底下的 75 个工具。**这层是薄封装，不是重复实现**——两套工具面并不是膨胀的原因。

真正的膨胀来自四处：

### 2.1 Schema 全手写，且与响应体无编译期关联（最大一块）

`registry/input_schemas/` + `registry/output_schemas/` 共 **6,748 行**手写 `json!`；而响应体同样是手工 `json!` 拼装：

| 文件 | `json!` 出现次数 |
|---|---:|
| `tool_runtime/git.rs` | 81 |
| `connector_runtime/mod.rs` | 80 |
| `tool_runtime/files.rs` | 68 |

两边靠人力对齐，**改一处忘另一处编译器不会报错**。这既是代码量来源，也是持续的正确性风险。

建议：用 `schemars` 从 Rust 类型派生 schema。能砍掉绝大部分手写代码，并把漂移变成编译错误。

> 注：`openapi.rs` 已经从 `registered_tool_specs()` 派生，没有第三份重复——这一点做得是对的。

### 2.2 单文件过大

`bin/webcodex-agent.rs` 7,114 行。它已经拆出了 `webcodex_agent/` 子模块，但主文件里仍混着 JobManager、HTTP 错误分类、注册逻辑、轮询循环、CLI 解析和约 5,400 行测试。

### 2.3 参数累积式 API

`tool_runtime/dispatch.rs`：

```
dispatch
 → dispatch_with_auth
   → dispatch_with_auth_transport
     → dispatch_with_auth_transport_options
       → dispatch_with_auth_transport_options_and_metadata
```

五层转发只为逐个追加参数。改成一个 `DispatchOptions` 结构体可塌成一层。

### 2.4 测试样板重复

`tool_runtime/tests/` 50 个文件 37,290 行，其中 `handoff.rs` 3,505 行、`files.rs` 3,144 行。这个体量通常意味着大量 setup 样板可抽成 fixture。

---

## 3. 问题清单

### 🔴 P1 —— `read_file` 不做项目边界校验，可读取项目外文件（**已修复**）

调用链上每一层都没有拒绝 `..`：

| 层 | 位置 | 检查内容 | 拦 `..`？ |
|---|---|---|:--:|
| connector | `connector_runtime/mod.rs:2991` | 拒绝前导 `/`、NUL | ❌ |
| 服务端工具 | `tool_runtime/files.rs:3374` | **无任何校验，原样转发** | ❌ |
| 队列 | `shell_client` `enqueue_file_op` | 无 | ❌ |
| Agent 解析 | `bin/webcodex_agent/files.rs:19` | 直接 join，无词法检查 | ❌ |
| Agent 策略 | `bin/webcodex_agent/shell.rs:539` | 只对 `allowed_roots` 设界，**从不对项目根设界** | ❌ |

**实测**（最严格配置：`allow_cwd_anywhere=false`、`allowed_roots=[$HOME]`，即文档默认值）：

```
RESOLVED = Ok(".../git/myproject/../../.ssh/id_rsa")
LEAKED CONTENT = "PRIVATE KEY"
```

项目位于 `$HOME/git/myproject` 时，`files_read` 传 `../../.ssh/id_rsa` 即可读到 `$HOME` 下任意文件，**跨越了被授权的项目边界**，与 README 的 project-scoped 承诺相悖。

**这是不一致，不是设计取舍。** 仓库里已有两个现成守卫，只是没用在 `read_file` 上：

- `list_project_files`（`files.rs:3582`）、`project_overview`（`files.rs:3731`）**调用了** `validate_project_relative_path`
- 所有写路径（`patches.rs:1422`）**调用了** `validate_line_edit_agent_path`，严格拒绝 `..`/绝对路径/敏感文件
- 该守卫的测试甚至明确断言 `../outside` 必须失败（`tests/files.rs:621-622`）

唯独最常用的读工具一个都没调。

### 🔴 P1 —— `allow_cwd_anywhere` 默认为 `true`，agent 可无边界（**已修复**）

`config.rs:139` 原为 `#[serde(default = "default_true")]`。

注意有**两条**默认值路径，二者都必须 fail-closed：

- `agent.toml` 完全没有 `[policy]` 段 → 走 `impl Default for AgentPolicy`
- 有 `[policy]` 但缺该字段 → 走 per-field serde 默认

任一为 `true`，`cwd_allowed` 首行即 `return Ok(())`，**完全没有文件系统边界**。实测默认策略下可直接读 `/etc/passwd`：

```
RESOLVED = "/etc/passwd"
LEAKED FIRST LINE = Some("root:x:0:0:root:/root:/bin/bash")
```

对一个卖点是"源码不离开本机、按项目隔离"的产品，fail-open 的默认值方向反了。

### 🟡 P2 —— `read_file` 没有敏感文件过滤（未修复）

其他表面都有敏感路径守卫，唯独 `read_file` 没有：

| 表面 | 守卫 |
|---|---|
| `search_project_text` | 排除 `.env`（`files.rs:977`） |
| artifact | `is_sensitive_artifact_path`（`files.rs:1500`） |
| 行编辑 / `apply_text_edits` | `is_sensitive_line_edit_path`（`patches.rs:43`） |
| **`read_file`** | **无** |

即 `read_file(".env")` 直接返回明文密钥。

四套敏感路径判定散落在四处、覆盖面各不相同，本身也是设计问题。建议收敛为一个共享判定，在 dispatch 层统一施加。

### 🟡 P2 —— `db/` 测试覆盖 8%（未修复）

5,103 行生产代码只有 462 行测试，其中 `task_kernel.rs` 2,207 行是任务状态机核心。

### 🟡 P3 —— `advanced_search_without_rg_returns_structured_capability_error` 不稳定（未修复）

`tool_runtime/tests/files.rs:2166`。该测试改写 `PATH` 后跑真实子进程，在全量并行负载下偶发失败；隔离运行与重跑均通过。**与本次修复无关**（已在未修改的 HEAD 与修复后各跑一遍确认）。

---

## 4. 本次修复内容

### 4.1 生产代码（3 处）

| 文件 | 改动 |
|---|---|
| `tool_runtime/files.rs` | `read_file` 入口加 `validate_project_relative_path`，与同文件 `list_project_files` 一致 |
| `connector_runtime/mod.rs` | `validate_path` 增加 `ParentDir` 拒绝（纵深防御） |
| `bin/webcodex_agent/config.rs` | `allow_cwd_anywhere` 的 serde 默认与 `impl Default` 双双改为 `false` |

### 4.2 新增回归测试（5 个）

| 测试 | 覆盖 |
|---|---|
| `read_file_rejects_parent_traversal_before_reaching_agent` | 4 种越界路径被拒，且**未进入 agent 队列** |
| `read_file_still_routes_project_relative_paths_to_agent` | 正常路径未被误伤，仍正确路由 |
| `validate_path_rejects_parent_traversal` | connector 层拒绝 `..`，放行合法路径 |
| `load_config_defaults_allow_cwd_anywhere_to_false` | 两条默认值路径均 fail-closed |
| `default_policy_denies_paths_outside_allowed_roots` | 默认策略拒 `/etc/passwd`，但界内路径仍可解析（证明是收紧而非弄坏） |

### 4.3 测试夹具调整（因默认值收紧）

`AgentPolicy::default()` 变为 fail-closed 后，31 处测试夹具需显式声明宽松策略——它们测的是 shell/profile 行为而非边界。改动是让**测试意图显式化**，而不是依赖不安全的生产默认值：

- `webcodex-agent.rs`：新增 `unrestricted_test_policy()`，替换 `test_config` + 16 处调用点
- `job_manager_tests.rs`：2 处
- `external_tools_tests.rs`：新增 `permissive_test_policy()`，替换 12 处
- `transport.rs`：1 处

**刻意保留 fail-closed 默认的边界测试**（未改动，且在新默认下仍通过）：

- `router_rejects_absolute_parent_and_symlink_escape_paths`（`external_tools_tests.rs:587,603`）
- `shell_job_rejects_cwd_symlink_escape`（`webcodex-agent.rs:5958`）
- `project_policy` / `register_project_rejects_dangerous_subpaths_without_explicit_root`

### 4.4 验证结果

```
cargo test --bins
  webcodex        1712 passed; 0 failed
  webcodex-agent   370 passed; 0 failed
  webcodex-cli     165 passed; 0 failed
cargo check --bins   0 warnings
```

### 4.5 部署影响（需注意）

`allow_cwd_anywhere` 默认值翻转对**现有部署是行为变更**：

- 依赖隐式 `true`（即 `agent.toml` 未显式写该字段）的 agent，升级后文件操作将被限制在 `allowed_roots` 内（缺省为 `$HOME`）
- 需要旧行为的用户，在 `agent.toml` 显式写 `[policy] allow_cwd_anywhere = true` 即可
- 建议在 release notes 中说明

---

## 5. 后续建议（按优先级）

1. **P2** —— 给 `read_file` 加敏感文件过滤，并把四套敏感路径判定收敛为一个共享实现
2. **P2** —— 补 `db/task_kernel.rs` 的测试
3. **P2** —— 为所有 agent 文件操作入口补一张边界测试表。目前 `apply_text_edits`、artifact 系列、`run_agent_json_file_op` 在服务端仍无 `validate_project_relative_path`（写路径靠 agent 侧 `validate_line_edit_agent_path` 兜底，但服务端缺一致校验）
4. **P3** —— 修 `advanced_search_without_rg` 的不稳定性
5. **中期** —— schema 改为从 Rust 类型派生。这一项能同时解决"代码量大"和"schema 漂移"两个问题
6. **中期** —— 拆分 `webcodex-agent.rs`（7,114 行）；`dispatch` 五层链改为 `DispatchOptions`

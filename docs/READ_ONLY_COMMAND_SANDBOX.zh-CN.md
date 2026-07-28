# 只读任务的命令沙箱

[English](READ_ONLY_COMMAND_SANDBOX.md) | [简体中文](READ_ONLY_COMMAND_SANDBOX.zh-CN.md)

## 现状

**未启用。** `read_only` 任务拒绝 `commands_run`。Agent 从不广告
`sandbox_read_only_commands`，服务器也不读取该字段。

`crates/webcodex-sandbox/src/lib.rs` 里有一份可用的 Linux Landlock 基础实现。保留它是因为
形状是对的，丢掉等于将来重做；但**基础不等于边界**，本文就是它保持关闭的理由。

## `read_only` 应当意味着什么

`read_only` 任务对启动它的人做出的承诺是：**不会发生有后果的事**——项目不被修改，
这台机器能触达的其他东西也不被改变。这才是 `commands_run` 要守住的承诺，而不只是
"checkout 没变"。

## Landlock 基础实际做到了什么

它对命令进程及其全部后代不可逆地施加一条"禁止写入"的规则集，只允许写入一个显式
列出的 scratch 目录。**读取被刻意放开**，因此策略永远不需要枚举可读路径——那正是
沙箱随项目增长而腐坏的地方。

这是一个真实的性质，但它只是若干访问类别中的一个。

## 它覆盖不了什么

以下都是当前基础的实际缺口，不是推测。第一条由 `crates/webcodex-sandbox/src/lib.rs` 里一个
断言"读取会成功"的测试钉住。

1. **读取不受限。** 命令可以读取 agent 用户能读的任何文件，包括 checkout 之外的
   一切：`~/.ssh`、`~/.aws`、其他项目、其他租户的状态目录。写入过滤器挡不住数据
   外泄，它挡的是修改。

2. **环境变量被继承。** 除非显式清理，子进程会拿到 agent 的全部环境变量。部署时
   放进去的任何东西——token、端点、云凭据——命令都读得到。

3. **网络完全不受影响。** Landlock 的文件系统规则对 socket 没有任何约束。一个
   `read_only` 命令可以联网、把刚读到的文件 POST 出去、或调用一个会改变别处状态的
   内部 API。事后在工作区里看不到任何痕迹。

4. **部分元数据操作可能不受管辖。** 规则集覆盖的是协商到的 ABI 所定义的写访问
   类别。`chmod`、`utimes`、属主变更这类操作在不同 ABI 版本中的表示并不一致，只
   支持旧 ABI 的内核会施加比预期更少的限制。实现要求 `FullyEnforced` 并拒绝
   `PartiallyEnforced`，正是为了让"施加得比预期少"变成失败而不是放行——但这也
   意味着覆盖范围是内核的函数。

5. **因此该基础不能支撑"无审批的任意 shell"。** 第 1~3 条中的任何一条，都足以让
   `read_only` 命令产生使用者被告知不会发生的后果。以此为由跳过审批，等于拿一个
   真实的关卡换一个不完整的关卡。

## 重新启用前必须满足的条件

以下全部，不是其中一部分。每一条都对应当前基础的一处具体不足。

**执行边界**

- checkout 之外的读取被拒绝，而不只是未枚举；
- 敏感环境变量不被继承，子进程从显式白名单起步；
- 网络默认关闭，或所有网络副作用都需要与 mutation 同等的审批；
- 项目文件的内容**和元数据**都不可修改；
- private scratch 目录原子创建、权限 `0700`、经校验不是 symlink，并在任务结束后
  清理。

**请求完整性**

- sandbox request 绑定到具体的 `agent_instance_id`；
- capability 检查与入队在同一个 registry 临界区内完成，避免"读到 capability 之后
  它已改变"；
- agent 被替换时，未决的 sandbox request 不得转移给新实例；
- 旧 agent 不能通过忽略新增字段降级成普通 shell——遇到无法识别的 sandbox 模式必须
  拒绝（agent 目前已如此）；
- agent 自身独立校验 sandbox 模式，而不是采信服务器的说法；
- 只有在**真实验证过** `FullyEnforced` 的前提下才广告 capability，且探测必须实际
  施加规则集并确认写入被拒。

**流程**

- 针对上述边界的独立 threat model；
- 针对该 threat model 的端到端验收，而不是对规则集做单元测试。

## 当前基础是如何 fail closed 的

- 探测会在一次性子进程中真正施加规则集，要求 `FullyEnforced`，并确认内核拒绝了
  一次本应被拒的写入。只创建规则集文件描述符，证明的仅仅是系统调用存在。
- `PartiallyEnforced` 与 `NotEnforced` 一律拒绝。规则集使用 hard compatibility
  而非 best effort，因此无法履行策略的内核会明说，而不是少施加一些。
- 在非 Linux 主机上，sandbox request 在 spawn 之前就是错误，绝不会变成一条无约束
  执行的命令。
- agent 遇到未知 sandbox 模式会拒绝，而不是回退。
- `doctor` 会报告基础是否存在，并在同一句话里说明它今天不改变任何事。

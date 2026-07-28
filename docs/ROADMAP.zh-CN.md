# Roadmap

WebCodex 是面向 coding assistant 的远程、可审计、有界执行层。它不是内置模型、自主 agent loop，也不是完整浏览器 IDE。

## 当前已交付基线

- project-bound MCP 与 OpenAPI 暴露精简的 canonical capability surface。
- Task、Execution、Event、Result、Approval、续接 review 和有界输出均可持久化。
- server、CLI 和 runner 通过 workspace library crates 共享代码，并由 package boundary 检查约束。
- 认证、project grant、allowed roots、路径策略、authority mode 和审计证据保持显式边界。
- structured validation 支持 Rust、Node、Python 和 Go recipe，不安装依赖，也不运行联网 setup hook。
- review console、重连续接、只读 LSP 导航、shell profile 和 transport fallback 已可用。

## 下一阶段优先级

1. 改善任务续接和 operator 可见性，同时避免无必要扩大公开 capability surface。
2. 完善自托管安装、升级、回滚和混合版本诊断。
3. 在保持协议兼容的前提下继续减少重复 projection 和过大返回体。
4. 扩展认证、transport 恢复、validation provenance 和进程清理的端到端覆盖。
5. 只在能够保持 project、permission、timeout 和 audit 边界时评估更多 provider 集成。

## 完成标准

Roadmap 项目只有在公开合同已文档化、聚焦与回归验证通过、失败行为明确，并且涉及运维时具备部署或回滚说明后才算完成。

## 明确非目标

- 内置模型选择、prompt loop、context compaction 或 token budget。
- 完整 IDE replacement 或任意 computer use。
- 默认自主部署或生产环境变更。
- 为假想消费者保留 compatibility alias。
- 把工具数、测试数或 LOC 当作产品进度。

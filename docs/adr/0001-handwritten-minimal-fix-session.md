# 手写最小 FIX 会话层，而非引入 FIX 库

FIX 协议层由本项目自己实现（约 1000 行：tag=value 帧、Logon/Heartbeat/序列号、6 种业务消息编解码），而不使用 quickfix-rs（C++ 绑定）或 fefix（纯 Rust 0.x）。依据：会话语义刻意保持最小（PersistMessages=N、ResetOnLogout/Disconnect=Y、无数据字典、动态会话），所需子集远小于任何现成库的能力面；自研底座不引入 C++ 构建链依赖，会话层成为完全可控的自有资产。

## Considered Options

- **quickfix-rs**：与 quickfixgo 同源、功能最全——拒绝：需要 cmake/C++ 工具链，Windows 构建麻烦，给底座加重的长期依赖。
- **fefix**：纯 Rust——拒绝：0.x 阶段，文档与社区小，出 bug 需自己修，长期维护不确定；为用其 5% 的能力面引入不确定依赖不划算。
- **手写（选定）**：风险是会话健壮性靠自己的测试覆盖。

## Consequences

- 升级路径已预留：若将来会话需求超出子集（真重传、持久化恢复、多字典版本），再评估 fefix/quickfix-rs，届时仅替换 `src/fix/` 模块，`core/` 与对外契约不动。
- FIX 协议正确性成为本项目的测试责任：任何会话行为变更必须有相应测试覆盖。

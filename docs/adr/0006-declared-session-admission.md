# acceptor 准入采用预定义会话表（关闭任意接入）

QuickFIX 引擎的默认准入模型是会话级声明：acceptor 只接受 settings 里声明过的会话，未声明身份收到即拒（"Session not found"）。本会话层最初采用 `DynamicSessions=Y`（任意 CompID 均可接入、Logon 无鉴权）。2026-09-13 决定对齐 QuickFIX 默认：`qf_exchange_settings` 用两个 `[SESSION]` 行的 `TargetCompID=CLIENT` 与 `TargetCompID=PLAYBACK` 声明允许的客户端会话，未声明者在握手阶段被引擎级拒绝；`DynamicSessions=Y` 作为显式项目扩展保留，缺省按 N 处理。

## Consequences

- Logon 的 56 字段改为**必填**（会话三元组 BeginString+Sender+Target 匹配的一部分），修复缺失即绕过的漏洞。
- 未声明客户端被拒时不创建任何会话日志文件（不为陌生人留痕），仅在交易所 stderr 记 warn；已声明客户端的握手失败仍进其会话 event 日志（"Failed handshake: ..."）。
- 新增合法客户端需要在 `qf_exchange_settings` 添加 `[SESSION]` 并重启 exchange；每个 SESSION 可继承 DEFAULT 并覆盖其连接参数。
- 同一监听端口上的 SESSION 必须使用同一 BeginString、服务端 SenderCompID 和监听端口；当前单 acceptor 监听器还要求共享 `Logging`/`FileLogPath`，不一致会在启动时报错，避免静默采用第一条配置。

# acceptor 准入采用预定义会话表（关闭任意接入）

quickfix 引擎的默认准入模型是会话级声明：acceptor 只接受 settings 里声明过的会话，未声明身份收到即拒（"Session not found"）。go-trader 参照实现选择了 `DynamicSessions=Y`（任意 CompID 均可接入、Logon 无鉴权），Rust 版如实继承——后果是 0.0.0.0:5001 上任何进程都能以任意 CompID 登录交易，且 Logon 缺失 56（TargetCompID）时连对端校验也会被跳过。2026-09-13 决定对齐 quickfix 引擎默认：`qf_got_settings` 用 `TargetCompIDs=CLIENT,PLAYBACK` 声明允许的客户端 CompID，未声明者在握手阶段被引擎级拒绝；文件中原有的 `DynamicSessions=Y` 键保留为兼容开关（显式写 Y 才恢复任意接入，缺省按 N 处理）。

## Consequences

- Logon 的 56 字段改为**必填**（会话三元组 BeginString+Sender+Target 匹配的一部分），修复缺失即绕过的漏洞。
- 未声明客户端被拒时不创建任何会话日志文件（不为陌生人留痕），仅在交易所 stderr 记 warn；已声明客户端的握手失败仍进其会话 event 日志（"Failed handshake: ..."）。
- 新增合法客户端需要改 `qf_got_settings` 并重启 exchange——即插即用换准入控制，是本决策的核心取舍。
- 与 go-trader 的行为分叉再添一处（其仍为 DynamicSessions=Y 无鉴权）；与 4.2 线协议分叉同见 ADR-0004，交叉验证 oracle 已退役。
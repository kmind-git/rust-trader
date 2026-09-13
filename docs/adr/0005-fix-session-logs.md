# 会话日志采用 quickfixgo 风格的 message/event 双文件

手写会话层（ADR-0001）原本只有 env_logger 生命周期打点：出站消息零日志、入站仅类型+序列号、无消息原文、无文件日志，排错时只能看到"收到了什么"看不到"回了什么"。2026-09-13 决定为 acceptor 与 initiator 补齐 quickfixgo `Log` 接口等价的会话日志：每会话两个文件——`{BeginString}-{Sender}-{Target}.messages.current.log`（每条收发报文的原始 tag=value 行，含心跳）与 `.event.current.log`（会话状态机事件，措辞对齐 quickfixgo 官方文案，如 "Received logon request" / "MsgSeqNum too high, expecting N but received M" / "Sent SequenceReset TO: N"）。配置沿用 quickfixgo 键语义：`Logging=Y/N`（默认 Y）总开关、`FileLogPath` 目录（各二进制默认：exchange=`logs/exchange`、client=`logs/client`、playback=`logs/playback`）。

## Considered Options

- **维持 env_logger 打点**——拒绝：无法回答"交易所回了什么/到底发了哪条报文"，4.2 排错（尤其与外部严格客户端联调）缺基本证据。
- **仅屏幕日志（ScreenLog 等价）**——拒绝：与 exchange 控制台 REPL 输出交错，不可留档离线分析。
- **文件双日志（选定）**：quickfix 经典形态；方向标记是我们对 quickfixgo 文件格式的唯一偏离。

## Consequences

- 日志行前缀用**北京时间（UTC+8 固定偏移）**便于本地排错阅读；FIX 报文字段时间（52=SendingTime、60=TransactTime）保持 UTC——对照日志与报文时间时相差 8 小时是预期行为。
- quickfixgo 的文件版 messages 日志不区分进/出方向（OnIncoming/OnOutgoing 写同一文件）；我们每行加 `in ` / `out ` 前缀——诊断价值优先于逐字节对齐。
- Logon 握手被拒（非 Logon 首条/版本不符/未知 target）也会留下 event 记录（"Failed handshake: ..."），为此 acceptor 先取 SenderCompID(49) 再校验；无 49 的连接无文件日志（与此前一致走 env_logger）。
- 心跳进 messages 日志（对齐 quickfixgo，每 HeartBtInt 一条）；日志文件 append 模式、无轮转，长期运行需自行清理 `logs/`。
- README 排错段原先指向的 quickfixgo `Logging=Y` 机制随之退役（该机制属于 Go 版）。
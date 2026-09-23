# 会话日志采用 QuickFIX/Go 风格的 message/event 双文件

手写会话层（ADR-0001）原本只有 env_logger 生命周期打点：出站消息零日志、入站仅类型+序列号、无消息原文、无文件日志，排错时只能看到"收到了什么"看不到"回了什么"。2026-09-13 决定为 acceptor 与 initiator 补齐 QuickFIX/Go `Log` 接口等价的会话日志：每会话两个文件——`{BeginString}-{Sender}-{Target}.messages.current.log`（每条收发报文的原始 tag=value 行，含心跳）与 `.event.current.log`（会话状态机事件，措辞对齐 QuickFIX/Go 官方文案，如 "Received logon request" / "MsgSeqNum too high, expecting N but received M" / "Sent SequenceReset TO: N"）。`FileLogPath` 沿用 QuickFIX 键语义；项目扩展开关 `Logging=Y/N`（默认 Y）总开关、`FileLogPath` 目录（各二进制默认：exchange=`logs/exchange`、client=`logs/client`、playback=`logs/playback`）。

## Considered Options

- **维持 env_logger 打点**——拒绝：无法回答"交易所回了什么/到底发了哪条报文"，4.2 排错（尤其与外部严格客户端联调）缺基本证据。
- **仅屏幕日志（ScreenLog 等价）**——拒绝：与 exchange 控制台 REPL 输出交错，不可留档离线分析。
- **文件双日志（选定）**：QuickFIX 经典形态；方向标记、UTC+8 展示时间和损坏帧 hex 是本项目明确标注的扩展。

## Consequences

- 日志行前缀用**北京时间（UTC+8 固定偏移）**便于本地排错阅读；FIX 报文字段时间（52=SendingTime、60=TransactTime）保持 UTC——对照日志与报文时间时相差 8 小时是预期行为。
- QuickFIX/Go 的文件版 messages 日志不区分进/出方向（OnIncoming/OnOutgoing 写同一文件）；我们每行加 `in ` / `out ` 前缀。合法报文的 tag=value 字节直接写入；解码失败但已读到的原始帧用 `in-hex` 无损十六进制保留，避免有损 UTF-8 重构。
- 日志文件名由 BeginString、SenderCompID、TargetCompID 组成；每个组件会把路径分隔符、控制字符和其它不安全字符替换为 `_`，防止配置值穿越日志目录。
- 文件创建或写入失败会写入 `log::warn!`；日志失败不会伪装成成功写入。`Logging=N` 使用禁用句柄，不创建日志目录或文件。
- Logon 握手被拒（非 Logon 首条/版本不符/未知 target）也会留下 event 记录（"Failed handshake: ..."），为此 acceptor 先取 SenderCompID(49) 再校验；无 49 的连接无文件日志（与此前一致走 env_logger）。
- 心跳进 messages 日志（对齐 QuickFIX/Go，每 HeartBtInt 一条）；日志文件 append 模式、无轮转，长期运行需自行清理 `logs/`。
- README 排错段现在指向本项目的 `Logging=Y/N` 与 `FileLogPath`；这些键由本项目配置解析器实际消费。

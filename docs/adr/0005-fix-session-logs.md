# 会话日志采用 QuickFIX/J 风格前缀，按本地日期保存 message/event 双文件

手写会话层（ADR-0001）原本只有运行诊断日志，缺少完整报文记录。2026-09-13 为 acceptor 与 initiator 补齐 message/event 双文件；2026-10-27 根据部署评审，将文件名和行前缀调整为 QuickFIX/J FileLog 风格，并使用本地日期轮转。每个会话写入 `FileLogPath/YYYY-MM-DD/{BeginString}-{Sender}-{Target}.messages.log` 和 `.event.log`；前者记录每条收发报文（含心跳），后者记录登录、注销、序列号和超时等会话事件。`Logging=Y/N`（默认 Y）独立控制 FIX 文件日志；`FileLogPath` 沿用现有配置，路径由启动程序相对安装根目录解析。

日志行采用 `YYYYMMDD-HH:MM:SS.mmm: 内容`，对齐 [QuickFIX/J FileLog 源码](https://github.com/quickfix-j/quickfixj/blob/master/quickfixj-core/src/main/java/quickfix/FileLog.java) 的时间戳及 `: ` 分隔符；本项目按用户要求使用本地时区。正常报文不添加 `in`/`out` 标记，报文头的 SenderCompID/TargetCompID 可用于辨认方向。

## Considered Options

- **维持 env_logger 打点**——拒绝：无法回答"交易所回了什么/到底发了哪条报文"，4.2 排错（尤其与外部严格客户端联调）缺基本证据。
- **仅屏幕日志（ScreenLog 等价）**——拒绝：与 exchange 控制台 REPL 输出交错，不可留档离线分析。
- **文件双日志（选定）**：保留完整线路报文，按会话、日期分文件；本地时区和损坏帧 hex 是本项目扩展。

## Consequences

- 日志行时间使用操作系统的本地时区，不再固定 UTC+8；FIX 报文字段时间（52=SendingTime、60=TransactTime）继续采用 UTC。
- 合法报文的 tag=value 字节直接写入，包含业务层忽略的扩展字段和 SOH 分隔符；解码失败但已读到的原始帧用 `in-hex` 无损十六进制保留。运行诊断的 `RUST_LOG` 级别不过滤这些报文。
- 日志文件名由 BeginString、SenderCompID、TargetCompID 组成；每个组件会把路径分隔符、控制字符和其它不安全字符替换为 `_`，防止配置值穿越日志目录。
- 文件创建或写入失败会写入 `log::warn!`；日志失败不会伪装成成功写入。`Logging=N` 使用禁用句柄，不创建日志目录或文件。
- Logon 握手被拒（非 Logon 首条/版本不符/未知 target）也会留下 event 记录（"Failed handshake: ..."），为此 acceptor 先取 SenderCompID(49) 再校验；无 49 的连接无文件日志（与此前一致走 env_logger）。
- 每次写入依据同一本地日期切换两个文件；即使跨日后首先发生的是 event，也会建立当天的 messages/event 文件。重启按 append 模式继续写入当日文件。轮转失败会报告警告，不会将当天报文误写入前一天文件。
- `configs/runtime.env` 的 `FIX_LOG_RETENTION_DAYS`（环境变量优先）默认 `14`，按日历日保留当天和之前 13 天；`0` 禁止自动清理。会话初始化及日期切换时清理该会话的过期 `.messages.log` / `.event.log`。不会递归删除目录，不跟随日期目录的符号链接，保留其他会话和用户文件；未再建立的历史会话需由部署用户另行归档或清理。
- `Logging=N` 禁用会话文件写入，不创建 FIX 日志目录；运行日志和 FIX 日志分别管理。现有配置中的 `FileLogPath` 优先于各二进制内置默认目录，例如随包 connector 配置将 playback 日志写到 `logs/client`。

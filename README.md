# rust-trader

模拟电子交易所（自研底座）：价格-时间优先撮合引擎、手写最小 FIX 4.2 会话层、只读 REST 查询、命令行客户端与行情回放。

> 决策记录见 [docs/adr/](docs/adr/)。
> 系统架构图：[docs/rust-trader-architecture.html](docs/rust-trader-architecture.html)（交互式，支持明暗主题 / 视图导览 / 导出；规格见 [docs/architecture.json](docs/architecture.json)）。

## 构建

```bash
cargo build --release   # 产物在 target/release/{exchange,client,playback}.exe
cargo test              # 单元测试（含 FIX 配置、日志和独立的撮合核心测试）
```

## 运行

```bash
# 终端 1：交易所（REST :8080 + FIX acceptor :5001）
./target/release/exchange
# 终端 2：回放模拟市场
./target/release/playback -fix configs/qf_connector_settings -id PLAYBACK -file configs/playback.txt
# 终端 3：下单 REPL
./target/release/client -fix configs/qf_connector_settings -id CLIENT
buy AAPL 3          # 市价单
buy AAPL 5 100.5    # 限价单
quit
```

REST（无鉴权，只读）：

```bash
curl http://localhost:8080/api/instruments/
curl http://localhost:8080/api/book/AAPL
curl http://localhost:8080/api/stats/AAPL
curl http://localhost:8080/api/sessions
```

后台服务使用 `exchange --server`：进程持续监听 FIX/REST，不读取标准输入，由 systemd、容器或调用方管理进程生命周期。默认仍为交互模式，`quit` 或 stdin EOF 退出。

交易所交互控制台：`help` / `sessions` / `book SYMBOL` / `list` / `quit`。

FIX 接入采用 QuickFIX 的预定义会话表：`configs/qf_got_settings` 的每个 `[SESSION]` 通过 `TargetCompID` 声明一个允许的客户端 CompID（未声明即拒，见 [docs/adr/0006-declared-session-admission.md](docs/adr/0006-declared-session-admission.md)）；`DynamicSessions=Y` 是本项目扩展，可恢复任意接入。`qf_connector_settings` 同时声明 `CLIENT` 和 `PLAYBACK` 两个 initiator，两个工具用 `-id` 选择对应会话。

## FIX settings

配置文件使用 QuickFIX 的 `[DEFAULT]`/`[SESSION]` 结构。`[DEFAULT]` 中的值由每个 `[SESSION]` 继承，SESSION 中的同名值覆盖默认值；多个 SESSION 按文件顺序保留，不会被压成一个全局 map。相同 BeginString/SenderCompID/TargetCompID 的重复会话会被拒绝。命令行程序会按 `ConnectionType` 和 `-id` 选择会话，缺少必要字段或出现多个匹配会话都会在启动时失败。

当前实现的 FIX 4.2 profile 支持 `ConnectionType`、`BeginString`、`SenderCompID`、`TargetCompID`、`SocketAcceptPort`、`SocketConnectHost`、`SocketConnectPort`、`HeartBtInt`、`ResetOnLogout`、`ResetOnDisconnect`、`PersistMessages`、`UseDataDictionary`、`DataDictionary` 和 `FileLogPath`。布尔值必须是大写 `Y` 或 `N`，端口和心跳必须是有效的正整数；`BeginString` 必须为 `FIX.4.2`。未知键会被拒绝。

省略配置时采用 QuickFIX 默认值（PersistMessages=Y、ResetOnDisconnect=N、ResetOnLogout=N），这些默认值超出当前实现范围，会明确报错。当前会话层是内存态实现，因此必须显式使用 `PersistMessages=N`、`ResetOnDisconnect=Y`、`ResetOnLogout=Y` 和 `UseDataDictionary=Y`；`PersistMessages=Y`、任一非重置设置或 `UseDataDictionary=N` 会明确报错。数据字典使用内置 FIX 4.2 profile；其它路径不会被假装当作已加载的字典。

`Logging`、`DynamicSessions` 和 `TargetCompIDs` 是项目扩展，已经在配置解析器中显式列出。`Logging=Y/N` 控制文件日志；`DynamicSessions=Y` 允许动态客户端；兼容旧配置的 `TargetCompIDs` 只用于准入扩展，QuickFIX 风格配置应优先为每个客户端写独立 `[SESSION]` 的 `TargetCompID`。

最小 acceptor 配置如下（同一监听端口可继续添加 `[SESSION]` 声明其它客户端）：

```ini
[DEFAULT]
ConnectionType=acceptor
BeginString=FIX.4.2
HeartBtInt=30
ResetOnLogout=Y
ResetOnDisconnect=Y
PersistMessages=N
UseDataDictionary=Y
Logging=Y

[SESSION]
SenderCompID=GOX
TargetCompID=CLIENT
SocketAcceptPort=5001
```

initiator SESSION 还必须提供 `SenderCompID`、`TargetCompID`、`SocketConnectHost` 和 `SocketConnectPort`；`client` 与 `playback` 通过 `-id` 在同一配置文件中选择不同 SESSION。

## 模块

```
src/
├── core/            # 领域核心（纯逻辑，无 IO）
│   ├── instrument.rs  # 品种表
│   ├── order.rs       # 订单/状态机
│   ├── orderbook.rs   # 价格档 + 撮合循环
│   ├── exchange/mod.rs# 引擎（会话、订单入口、缓存更新）
│   └── stats.rs       # 统计聚合
├── fix/             # 手写最小 FIX 4.2 会话层（见 ADR-0001）
│   ├── frame.rs       # tag=value 帧、BodyLength/CheckSum
│   ├── codec.rs       # 业务消息编解码（D/F/G/i ↔ 8/b/j/d）
│   ├── session.rs     # acceptor + initiator
│   ├── log.rs         # quickfixgo 风格 message/event 会话日志（见 ADR-0005）
│   └── config.rs      # quickfix 风格配置解析
├── rest.rs          # 只读 REST（tiny_http，见 ADR-0002）
└── bin/             # exchange / client / playback
```

## 契约与线协议

当前协议范围、扩展与审计结果见 [FIX 4.2 审计与实现范围](docs/fix42-audit.md)。独立线协议验证：先 `cargo build --bins`，再 `python tests/fix42_wire.py`；字典来源和测试边界见 [tests/README.md](tests/README.md)。

## 排错

- **FIX 会话日志**：每个会话两个文件（QuickFIX/Go 风格）。acceptor 写 `logs/exchange/{BeginString-Sender-Target}.messages|event.current.log`；client 写 `logs/client/`、playback 写 `logs/playback/`。messages 记每条收发报文原文（`in`/`out` 方向前缀，含心跳）；帧损坏时保留 `in-hex` 的无损十六进制；event 记会话事件（登录/注销/序列号/超时，措辞对齐 QuickFIX/Go）。`Logging=Y/N`（默认 Y）与 `FileLogPath` 可覆盖，见 ADR-0005。日志行时间使用 UTC+8、方向标记和损坏帧 hex 是本项目扩展，FIX 报文字段时间仍按 UTC。
- Windows 下构建前确保没有残留的 exchange/client/playback 进程锁住 target 下的 exe；运行前确认 8080/5001 端口未被占用（报 os error 10048 即端口冲突）。

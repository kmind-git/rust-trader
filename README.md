# rust-trader

go-trader 的 Rust 重写（自研底座）。价格-时间优先撮合引擎、手写最小 FIX 4.2 会话层、只读 REST 查询、命令行客户端与行情回放。

> 重写方案与决策记录见 [docs/rust-rewrite-plan.md](docs/rust-rewrite-plan.md)、[docs/adr/](docs/adr/)。
> 系统架构图：[docs/rust-trader-architecture.html](docs/rust-trader-architecture.html)（交互式，支持明暗主题 / 视图导览 / 导出；规格见 [docs/architecture.json](docs/architecture.json)）。
> 参照实现：`../go-trader`（Go 版，开发期作为测试对照 oracle 保留）。

## 构建

```bash
cargo build --release   # 产物在 target/release/{exchange,client,playback}.exe
cargo test              # 26 个单元测试（含翻译自 Go 版的撮合核心测试）
```

## 运行

```bash
# 终端 1：交易所（REST :8080 + FIX acceptor :5001）
./target/release/exchange
# 终端 2：回放模拟市场（复用 go-trader 的 configs/）
./target/release/playback -file configs/playback.txt
# 终端 3：下单 REPL
./target/release/client
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

交易所交互控制台：`help` / `sessions` / `book SYMBOL` / `list` / `quit`。

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
│   ├── codec.rs       # 业务消息编解码（D/F/G/i/c/x ↔ 8/b/d）
│   ├── session.rs     # acceptor + initiator
│   └── config.rs      # quickfix 风格配置解析
├── rest.rs          # 只读 REST（tiny_http，见 ADR-0002）
└── bin/             # exchange / client / playback
```

## 与 Go 版的契约

FIX 端口/消息流/回报语义、REST 端点与 JSON 字段、撮合规则与订单状态机与 Go 版一致。契约基线自 2026-09-13 起收缩（见 [docs/adr/0003-contract-shrink.md](docs/adr/0003-contract-shrink.md)）：不再支持 SecurityDefinitionRequest(c) 运行期动态建品种（品种仅来自 instruments.txt）、交易所控制台无 watch/unwatch、客户端不再读取 got_settings。已实现交换流程（playback 行情、client 下单/回报、REST 查询）两版二进制仍可互换；双向交叉验证（Go playback 驱动 Rust exchange；Rust playback 驱动 Go exchange）已通过。

## 排错

- FIX 交互失败时，用 `configs/qf_debug_settings`（Logging=Y）启动客户端，quickfixgo 会打印逐条消息与拒绝原因。
- Windows 下构建前确保没有残留的 exchange/client/playback 进程锁住 target 下的 exe。

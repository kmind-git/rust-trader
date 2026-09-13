# rust-trader 重写方案

> 状态：已实施完成（2026-09-13）。各里程碑 ✅ 见第 5 节。
> **2026-09-13 变更**：线协议由 FIX 4.4 改为 **FIX 4.2**（决策见 [ADR-0003](adr/0004-fix42-wire-protocol.md)）。本文中提及 FIX 4.4 的历史表述以该 ADR 为准。
> 参照实现：`D:\projects\zcodeworkspace\go-trader`（Go 精简版，~3100 行）——**原地保留，作为测试对照 oracle**，验收通过后再决定去留。

## 0. 背景与术语

go-trader（Go）已完成精简：撮合核心 + FIX 4.4 接入 + 只读 REST + client/playback 两个示例。本次将其**用 Rust 重写**（术语说明：换语言属于重写/移植，不是重构——对外契约保持，内部实现自由）。

定位：**自研底座升级**。Rust 版是未来长期开发的基础，选型保守、依赖干净、结构地道。

## 1. 已确认的核心决策（5 项）

| # | 决策 | 内容 | 关键理由 |
|---|---|---|---|
| D1 | 动机 | 自研底座升级，非学习载体 | 选型偏成熟保守，能复用的设计尽量复用 |
| D2 | 行为契约 | **契约等价 + 内部自由** | FIX 消息流/端口、REST 端点与 JSON 字段、撮合规则与订单状态机与 Go 版完全一致，两版二进制可互换；内部用 idiomatic Rust 自由实现。契约小（4 个 REST 接口 + 6 种 FIX 消息），守住成本极低，换来免费的双向测试 oracle |
| D3 | FIX 协议层 | **手写最小 FIX 会话层** | Go 版会话语义已最小化（PersistMessages=N、ResetOnLogout/Disconnect=Y、无字典、动态会话），所需子集仅 ~1000 行；零 C++ 构建链、零第三方 FIX 依赖 → 见 [ADR-0001](adr/0001-handwritten-minimal-fix-session.md) |
| D4 | 并发模型 | **纯同步线程**，零 async | std::net + 每连接一线程 + Mutex，与 Go 版心智模型 1:1；REST 用 tiny_http。当前负载（个位数连接、轻撮合）不需要 async → 见 [ADR-0002](adr/0002-synchronous-threading-model.md) |
| D5 | 仓库布局 | 独立目录 `rust-trader`，Go 版原地保留 | 开发期 Go↔Rust 交叉对照；验收后另行决定 Go 版去留 |

## 2. 技术选型

| 领域 | 选择 | 说明 |
|---|---|---|
| 语言/工具链 | Rust 1.98（本机已装于 `D:\Program Files\rust`） | stable，不 pin 夜间特性 |
| 价格类型 | `rust_decimal::Decimal` | 对应 Go 版 `robaho/fixed` 的定点语义；REST 输出时转 `f64` 保持 JSON 一致 |
| HTTP | `tiny_http` + `serde_json` | 阻塞式小库，匹配纯同步模型；REST 只读无状态 |
| FIX | 自研模块（本项目资产） | 无 quickfix 绑定、无 fefix |
| 日志 | `log` + `env_logger` | 保守标准选择 |
| 异步 | 无 | 见 D4 |

依赖总数预计 5 组以内（rust_decimal、tiny_http、serde/serde_json、log/env_logger）。

## 3. 契约基线（移植对照清单）

> **2026-09-13 收缩记录**：验收完成后经功能评审，契约基线改为可收缩——已裁剪 SecurityDefinitionRequest(c) 动态建品种、控制台 watch/unwatch、-props/got_settings；其余基线不变。见 [adr/0003-contract-shrink.md](adr/0003-contract-shrink.md)。

以下必须与 Go 版**逐项一致**（详细描述见 go-trader `docs/04-interfaces.md`、`docs/05-order-lifecycle.md`）：

**FIX 4.4 acceptor**：端口 5001，`SenderCompID=GOX`，FIX.4.4，动态会话（任意 TargetCompID），Logon 无鉴权，断线重置序列号。
- 入站消息：NewOrderSingle(D)、OrderCancelRequest(F)、OrderCancelReplaceRequest(G)、MassQuote(W，仅 1 QuoteSet×1 QuoteEntry)、SecurityDefinitionRequest(c，未知品种自动创建)、SecurityListRequest(x，末条 `endofdownload`)
- 出站消息：ExecutionReport(8)、MassQuoteAcknowledgement(b，仅 QuoteResponseLevel 要求时)
- 回报语义：无成交 → Status（Booked/Cancelled/Rejected）；成交 → Fill（LastPx/LastQty/LeavesQty/CumQty，双方各一条）；撤单/改单找不到订单不回 reject；会话断开撤销该会话全部订单与报价并逐笔回报 Cancelled

**撮合规则**：价格-时间优先；成交价取先到（resting）方委托价；一次撮合批次共用一个 tradeid；市价单=极端有效价（买 9999999999999 / 卖 0）+ 剩余立即撤；改单=撤旧挂新（时间优先级重置）；报价 replace 语义（价格 0 表示撤该侧）。

**REST（:8080，无鉴权，只读）**：
- `GET /api/instruments/` → symbol 数组
- `GET /api/book/{SYMBOL}` → `{"symbol","sequence","bids":[{"price","quantity"}],"asks":[...]}`；未知品种 404
- `GET /api/stats/{SYMBOL}` → `{"symbol","bidPrice","bidQty","askPrice","askQty","volume","high","low","hasHighLow"}`（累计口径：自交易所启动）
- `GET /api/sessions` → 会话 ID 数组
- sequence 为全所全局自增快照版本号

**配置文件兼容**：直接复用 go-trader 的 `configs/`——`qf_got_settings`（acceptor，解析 SenderCompID/SocketAcceptPort/BeginString 等 ini 键）、`qf_connector_settings`（initiator）、`instruments.txt`（`ID SYMBOL` 行）、`playback.txt`（`时间戳 品种 买量 买价 卖量 卖价`，相对 `+5s/us/ms/min` 与绝对毫秒两种时间戳）、`got_settings`（客户端属性，允许空）。

**交易所控制台**：help/sessions/book/watch/unwatch/list/quit（行为同 Go 版）。

## 4. 模块划分（单 crate，多 bin）

```
rust-trader/
├── Cargo.toml            # package rust-trader, lib gotrader
├── docs/                 # 本方案 + ADR
└── src/
    ├── lib.rs
    ├── core/             # 领域核心（纯逻辑，无 IO，单测密集区）
    │   ├── instrument.rs # 品种表（ID↔Symbol，动态创建，对应 instrumentmap）
    │   ├── order.rs      # 订单/状态机/Fill
    │   ├── orderlist.rs  # 同价 FIFO 链
    │   ├── orderbook.rs  # 价格档 + 撮合循环
    │   ├── exchange.rs   # 会话、订单入口(Create/Modify/Cancel/Quote)、断线清理
    │   └── stats.rs      # book 缓存 + 统计聚合 + 全局 sequence
    ├── fix/
    │   ├── frame.rs      # tag=value 解析/组装、BodyLength/CheckSum、粘包处理
    │   ├── session.rs    # acceptor/initiator 通用：Logon/Logout/Heartbeat/TestRequest/
    │   │                 #   ResendRequest→SequenceReset(GapFill)、序列号管理、心跳超时
    │   ├── codec.rs      # 业务消息编解码：D/F/G/W/c/x → 8/b
    │   └── config.rs     # quickfix 风格 ini 配置解析（只取所需键）
    ├── rest.rs           # tiny_http 路由 + DTO + JSON（字段名与 Go 版一致）
    └── bin/
        ├── exchange.rs   # 交易所进程：acceptor + REST + 控制台
        ├── client.rs     # 下单 REPL（buy/sell SYMBOL QTY [PRICE]、quit）
        └── playback.rs   # 报价回放（时间轴 + MassQuote）
```

原则：`core/` 不依赖 IO（Go 版教训——撮合与缓存同步更新保证一致性），`fix/` 与 `rest.rs` 是两个独立边界。

## 5. 里程碑与交叉验证

| 里程碑 | 内容 | 验证方式 | 预估 |
|---|---|---|---|
| ✅ M0 脚手架 | cargo init、依赖接入、configs/ 复制或链接 | cargo build/test 跑通 | 0.5 天 |
| ✅ M1 撮合核心 | core/ 全部 + 单元测试 | **翻译 Go 版三个测试文件**（orderbook/orderlist/exchange_test，含 issue #23 回归）逐条对齐；Go 版同输入对照 | 2 天 |
| ✅ M2 REST | rest.rs 四端点 | 启动 Go exchange 与 Rust exchange，同操作序列后 **JSON 语义 diff**（数值比较，非字符串） | 0.5 天 |
| ✅ M3 FIX acceptor | fix/ 全部 + 接线 | **用 Go 版 playback 驱动 Rust exchange**：回放后 REST 盘口/统计与 Go 版对照；client REPL 下单验证回报 | 3 天 |
| ✅ M4 客户端 | client REPL + playback | **用 Rust client/playback 驱动 Go exchange**：反向验证 initiator 与会话层 | 1.5 天 |
| ✅ M5 交叉验收 | Rust 全家桶互跑 + 双向交叉 | Go↔Rust 交叉冒烟全通过 = 契约等价的实证；粗测吞吐；编写 rust-trader README | 1 天 |

总计约 8.5 人日。**交叉验证是核心策略**：每个里程碑都用另一侧实现当 oracle，契约等价不是"声称"而是"证明"。

## 6. 风险与对策

| 风险 | 对策 |
|---|---|
| FIX 会话健壮性（粘包/半包、心跳超时、序列号错位、ResendRequest） | BodyLength 长度前缀循环读满；ResendRequest 一律回 SequenceReset(GapFill)（ResetOnDisconnect 语义下无需真重传）；M3/M4 双向交叉驱动覆盖 |
| rust_decimal → f64 的 JSON 精度与 Go 版不完全一致 | M2 对照测试用数值语义比较；若出现差异，评估在 Decimal 层定标（scale）而非改契约 |
| tiny_http 多线程 accept 行为与 net/http 差异 | REST 只读无状态，风险低；必要时每请求一线程模型即可 |
| 手写 FIX 的长期维护成本 | 已在 ADR-0001 记录边界与升级路径（语义超子集时再评估 fefix/quickfix-rs） |
| Go 版会话语义有未文档化细节（如 logout 时序） | 以 Go↔Rust 交叉测试的实际行为为准，发现差异回填 go-trader docs |

## 7. 明确不做

- 不移植 gRPC、行情多播分发、Web 前端（Go 版已删，Rust 版从不存在）
- 不做异步/tokio（ADR-0002）
- 不改对外契约（D2）；不持久化（与 Go 版一致：内存态，重启清零）
- 不在 M5 验收前删除或修改 go-trader 仓库

## 8. 验收标准

1. `cargo test` 全绿（含翻译自 Go 版的核心单测）
2. M5 双向交叉冒烟：Go playback → Rust exchange、Rust playback → Go exchange、Rust client ↔ Rust exchange 全链路成交，REST JSON 语义一致
3. `bin/exchange + bin/playback + bin/client` 在 Windows 本机一键起跑，行为与 Go 版无感切换

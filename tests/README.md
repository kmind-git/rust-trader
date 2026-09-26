# FIX 4.2 verification

Run from the project root:

```text
cargo test --locked --offline
cargo build --locked --offline
python tests/fix42_wire.py
```

`fix42_wire.py` uses Python's standard library and the unmodified official
QuickFIX dictionary in `fixtures/FIX42.xml`. It does not use this project's
encoder or decoder to construct/validate its peer messages.

The harness starts the built exchange on loopback ports selected by the OS,
keeps stdin open, and terminates that process after testing. It then acts as a
mock acceptor for the real client and playback binaries. Generated settings,
playback input, and captured process output remain in `target/fix42-wire/`.
No existing settings or logs are overwritten.

Checks cover required fields, enum values, repeating-group ordering, wire
BodyLength/CheckSum, standard market/limit mapping, string ClOrdID, partial-fill
replacement/cancellation quantities, average price, execution ID uniqueness,
reject responses, sequence recovery, heartbeats, and Logout.

This is a regression harness for the supported profile, not FIX certification
or an implementation of every conditional rule in the complete dictionary.

## 部署缺陷回归

运行 `cargo build --bin exchange` 后执行 `python tests/deployment_smoke.py`。
检查 `--server` 在 stdin EOF 下保持 REST/FIX 可用，默认交互模式 EOF 正常退出，以及 systemd 启动参数。
还会使用工作流中的实际 tar 命令打包本地 fixture，验证生成路径与 artifact/release 匹配；这不替代 Linux musl 构建、systemd 或 GitHub Actions 实跑。测试文件保存在 `target/deployment-smoke/`。

## 性能与队列回归

`cargo test` 包含有界队列溢出、关闭 TCP 连接、盘口增量总量/索引一致性及 REST 快照独立性检查。
`cargo run --release --example book_bench` 测量 20000 订单的插入、500 次盘口快照及逆序撤单。方法、前后结果与限制见 [性能优化验证](../docs/performance.md)。

行情 ArcSwap 改造增加 `tests/market_data.rs` 并发读取测试，以及核心的改单、断连、到期快照合并检查。`cargo test --locked --offline` 会一并执行。REST 单元测试覆盖持有 Engine 锁期间仍可完成行情路由。

`cargo run --locked --offline --release --example market_snapshot_bench` 比较同一最终引擎上的旧式锁内深复制与新快照读取，输出 1/4 读者、5 次测量的原始数据。它不包含 JSON/TCP，不是完整版本的端到端速度对比。机制和兼容范围见 [行情快照说明](D:/projects/zcodeworkspace/rust-trader/docs/market-data-snapshots.md)。

## FIX 回报延迟与输入缓冲回归

```text
cargo build --locked --offline
python tests/fix_delivery.py
cargo build --locked --offline --release --bin exchange
python tests/fix_delivery.py --release
```

使用独立 loopback 进程、端口和 FIX42 字典校验器，五轮验证输入空闲的 MAKER 与主动下单的 TAKER 都能收到成交回报。双方第一次将 Logon 与首单合并发送，验证握手预读数据保留、Logon 回包顺序、成交数量/价格、连续序号、唯一 ExecID，以及 Logout 后不再发送消息并关闭连接。

2 秒接收期限仅用于捕获旧路径接近 10 秒的等待，不能用作生产 SLO。打印的每轮耗时含 Python、socket 与校验成本，配置、日志及 `diagnostic.json` 保存在 `target/fix-delivery/<run-id>/`；进程在退出时清理。输入跨超时续读、超限拒绝、模拟 Read 合并、单方出站满载及最终 Logout 顺序另由 Rust 测试覆盖。机制与限制见 [FIX 回报路径说明](../docs/fix-delivery.md)。

## 分档成交模式（FillPolicy=Tiered）回归

```text
cargo build --bin exchange
python tests/tiered_fill.py
```

以 `FillPolicy=Tiered` 启动独立 exchange 进程，走真实 FIX 4.2 socket 验证四个数量档的完整回报序列（≤100 仅已报；≤1000 全成；≤2000 成交 50%；≤25600 每笔 100 加尾笔）、市价单 LastPx 固定 66.88、超限（>25600）业务拒单、部分成交后撤单保留 CumQty、ExecID 全局唯一，以及 REST `/api/book` 的挂单可见性与 `/api/stats` 零成交量。配置、进程日志保存在 `target/test-runs/tiered-fill/<run-id>/`。模式语义与边界见 [分档成交方案](../docs/tiered-fill-plan.md)。

非法配置（未知值、SESSION 作用域携带 `FillPolicy`）会在监听启动前直接退出，由 `src/fix/config.rs` 单测与启动检查共同覆盖。

## Real 撮合边界矩阵

```text
cargo build --bin exchange
python tests/real_boundary.py
```

默认策略（无 `FillPolicy`）下的真实撮合边界：穿越单直接回成交（无独立 New）且成交价取挂单方价格、双档深度扫单顺序与余量挂簿、市价单余量取消与空簿即撤、改单换价重排队、撤不存在订单与重复 ClOrdID 的拒绝路径、MassQuote level=2 回执与报单穿越报价腿、REST `/api/stats` 成交量逐笔对账。产物在 `target/test-runs/real-boundary/<run-id>/`。

## 分档成交边界矩阵

```text
cargo build --bin exchange
python tests/tiered_boundary.py
```

`FillPolicy=Tiered` 下的边界与改单路径：101/1000/1001/2000/2001/2500/25600 各档回报数量与终态、102400 拒单、市价单 ≤100 挂簿于 66.88 且可撤（价格档 key 设计路径）、改单到累计量的 R=0 单条 Replaced、改单上调按剩余量重新分档、改单超限 OrderCancelReject 且原单不动、限价改市价按 66.88 全成、断线后挂单清理（ResetOnDisconnect）、双会话交叉挂单互不撮合。产物在 `target/test-runs/tiered-boundary/<run-id>/`。

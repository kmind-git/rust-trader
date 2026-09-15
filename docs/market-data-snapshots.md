# 行情快照发布与查询

2026-09-15：实施按品种的最新完整快照方案。用户允许订阅者/查询者跳过行情中间版本；交易报告的有界队列与失败语义不在此次改造范围内。

## 已实现的路径

```text
串行修改盘口与累计统计
    → 记录品种的最终待发布 sequence
    → 操作内每品种构建一次完整盘口
    → ArcSwap 原子替换该品种的 InstrumentSnapshot
    → 多个 REST worker 独立读取 Arc 并生成 JSON
```

[market_data.rs](D:/projects/zcodeworkspace/rust-trader/src/market_data.rs) 提供只读 `MarketDataReader` 和仅由 Engine 持有的发布器。每品种一个快照槽，包含 `Book`、`Statistics`、`version` 与 `published_at`。`version` 等于 `Book.sequence`；品种目录独立原子发布，早期取得的 reader 也可发现后创建或加载的品种。更新行情不会复制整个品种目录。

`ArcSwap` 管理快照引用的并发替换及旧引用生命周期。快照读取不获取 Engine Mutex；引用发布与读取采用库的无锁机制，但构建、分配、旧对象释放、JSON 和网络发送仍有成本，不宣称整个服务或完整发布过程都是无锁的。库特性见 [arc-swap 官方文档](https://docs.rs/arc-swap/latest/arc_swap/docs/performance/index.html)。

## 发布时点与一致性

- 新单、撤单、报价：完成该操作的盘口和统计计算后发布。
- 改单：移除旧单和重新入单之间不构建中间快照，保留两次逻辑 sequence 递增。改单后剩余量为零时也立即发布空侧。
- 会话断连、批量 DAY 到期：每品种仅构建一次最终快照，同时保留原有每笔逻辑版本增量。
- 成交量、高低价每笔完整累计，不从可能被跳过的已发布快照反推统计。
- 每个操作返回前已发布全部受影响品种。最后一笔更新不需要等下一笔交易或定时器。暂未加入跨操作的固定发布间隔。

查询取得的是读取时的某个完整已发布版本。操作进行期间可能仍读到先前版本；读到新版本时，Book 与 Stats 必须相互一致。多品种各自发布，不承诺跨品种原子切换。两次独立 REST 请求也可能读取不同版本，与原接口约定一致。

消费者可以从版本 100 直接读到 103。查询不出队，也不会删掉快照；一个查询读取后，其他查询仍可读取。当前槽只保留最新引用，已有读者可以继续持有旧版本，不存在无限增长的内部行情历史队列。外部调用方若自行长期保存大量旧 Arc，仍会增加内存占用。

只合并完整快照的中间发布，不丢增量计算和交易回报。此次没有新增 FIX/WSS 行情订阅协议或推送线程，也不改变 FIX 发送序号和 QuickFIX 配置。

## REST 兼容范围

| 路径 | 读取方式 |
|---|---|
| `/api/book/{symbol}` | `MarketDataReader`，不取 Engine Mutex |
| `/api/stats/{symbol}` | 与 Book 同一类完整快照，不取 Engine Mutex |
| `/api/instruments`、原兼容前缀 | 原子品种目录，返回排序后的品种名 |
| `/api/sessions` | 仍在 Engine Mutex 下读取会话列表 |

保留原 JSON 字段、Decimal 到 f64 的 REST 表示、404 状态和文本。已知但尚未交易的品种返回 sequence=0 的空盘口及零统计；未知品种仍为 404。

兼容的 `Engine::book()` 仍返回拥有所有权的 Book，用于已有调用者；REST worker 不使用这条深复制路径。新调用者先取得 `engine.market_data()`，之后直接用 reader 查询。

## 验证与基准

```text
cargo test --locked --offline
cargo build --locked --offline
python tests/fix42_wire.py
cargo run --locked --offline --release --example market_snapshot_bench
```

新增测试覆盖真实 REST 路由在 Engine 锁被持有时仍可完成，旧快照在更新/断连/Engine 退出后仍可用，旧 reader 跟随品种注册，4 个读者和单个写者并发时快照一致、版本不倒退，以及最后状态立即可见。核心测试同时验证批量操作的构建次数、sequence 增量与累计统计/到期回报不丢。

`market_snapshot_bench` 在同一最终 Engine 上比较旧式 `Mutex + Engine::book()` 深复制与新 reader 读取，512 档、1/4 个读者、每读者 25000 次读取，预热后交替顺序测量各 5 次。该结果仅比较读取路径，不含 REST JSON/TCP，不是两个完整发行版本的端到端比较。另单独记录当前实现撤销 8000 单的断连耗时，不据此推算整体吞吐或 p99。实际运行记录见 [性能说明](D:/projects/zcodeworkspace/rust-trader/docs/performance.md)。

当前交易修改仍由调用者的 Engine Mutex 串行保护；本次降低行情查询对撮合锁的干扰，没有实施专属 EngineHost 或整体 FIX 架构重写。

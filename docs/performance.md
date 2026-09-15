# 性能优化验证

## 第一轮已完成的四项改动

以下保留第一轮优化及其测量记录；后续 ArcSwap 行情与 FIX 回报路径改造的当前行为和结果见本页末尾。

- 慢客户端：引擎报告队列、FIX 出站队列均使用容量 1024 的 sync_channel，通过 try_send 非阻塞发送。两个队列共享失败标记；任一队列满载或消费者退出后拒绝继续入队，连接关闭并走断连清理。容量是项目内部常量，不是 QuickFIX 配置项。发生溢出不保证交付最后一条回报，需要重新连接并协调业务状态。
- REST：锁内只读取拥有所有权的快照，锁外转换 DTO 和序列化 JSON。响应发送也在锁外，保留原端点与字段。
- 盘口聚合：增量维护每个价格档的剩余总量，入单、部分成交、完全成交、撤单均更新总量；生成快照只遍历价格档。
- 价格及订单索引：价格档使用 BTreeMap；档内使用按入档顺序排序的 BTreeMap 保持 FIFO，并用 HashMap 将 session/order ID 定位到顺序键。撤单通过已知 side/price 定位价格档，不再扫描档内全部订单。报价的买卖两侧独立，允许沿用相同报价 ID。

## 本地微基准

命令：`cargo run --locked --offline --release --example book_bench`。
同一 Windows 工作区、release 构建；每个场景插入 20000 个不交叉限价买单，生成 500 次完整快照，然后逆序撤销全部订单。以下为改动前后各一次测量，单位毫秒；不能等同于生产吞吐、延迟分位数或统计显著性结论。

| 价格档数 | 插入：优化前 → 后 | 500 次快照：前 → 后 | 撤单：前 → 后 |
|---|---:|---:|---:|
| 1 | 1566.882 → 37.467 | 67.428 → 0.061 | 1238.784 → 11.790 |
| 100 | 714.397 → 18.400 | 68.427 → 0.196 | 16.494 → 10.325 |
| 2000 | 738.007 → 19.839 | 100.221 → 5.178 | 10.635 → 11.654 |

索引和有序节点有额外内存及维护成本；2000 档、每档约 10 单场景的撤单单次测量略慢。保留该结构是为了降低深档扫描成本，同时显著减少插入后的全盘口定位和快照聚合开销。尚未量化内存增量，也没有 REST 并发压测或慢客户端负载下的持续吞吐测试。

## 验证与限制

53 项 Rust 测试通过，包括随机样式的确定性混合订单流中增量总量/索引一致性、原有价格时间优先测试、共享队列溢出终止、真实 TCP 连接关闭、REST 快照独立性。FIX 字典/TCP 流程以及部署回归通过。

第一轮结束时的限制：队列按条数限额，不是总字节配额；大字段消息仍占较多内存。连接线程轮询失败标记，受操作系统调度和现有 socket 超时约束，不承诺实时关闭上界。全局撮合锁、快照复制、同步文件日志、重复的断连深度重建当时仍是后续热点；其中行情查询复制和操作内重复构建已在下述改造中处理。

## 2026-09-15：ArcSwap 行情快照

按品种原子发布完整的 Book、Stats、版本和发布时间。REST 行情查询通过独立 reader 读取 Arc，不获取 Engine Mutex，也不深复制 Book；仅 `/api/sessions` 仍读取引擎锁。改单、批量断连和到期按品种合并中间快照，累计统计与原 sequence 增量保留，操作返回前发布最终状态。没有增加行情队列、后台发布线程或固定发布延迟。详情见 [行情快照说明](D:/projects/zcodeworkspace/rust-trader/docs/market-data-snapshots.md)。

命令：`cargo run --locked --offline --release --example market_snapshot_bench`。本机 Windows、rustc 1.98.1、arc-swap 1.9.2，512 个买价档、10000 张订单构成同一最终盘口。对比当前引擎上的旧式 `Mutex + Engine::book()` 深复制与新 `MarketDataReader::snapshot()` 读取。每读者读取 25000 次，各模式预热，交替运行顺序，各测 5 轮。

| 读者数 | 每轮总读取次数 | 旧式读取中位数 | ArcSwap 读取中位数 | 读取耗时比 |
|---:|---:|---:|---:|---:|
| 1 | 25000 | 73.818 ms | 7.865 ms | 约 9.4 倍 |
| 4 | 100000 | 438.410 ms | 29.524 ms | 约 14.8 倍 |

这是读取路径微基准，不含 JSON、TCP 或持续撮合，不是完整旧版与新版发行程序的端到端对比。当前兼容 `Engine::book()` 也从发布槽取得数据再深复制，因此不能将此表解释为精确重现旧版所有开销。样本有明显调度波动：4 读者的新路径单轮为 26.341–141.744 ms，应使用重复测量而非挑最快值。另测当前实现单品种断连清理 8000 单为 40.961 ms，仅一个样本，没有旧版对照。

原始 5 轮数据：[CSV](D:/projects/zcodeworkspace/rust-trader/target/performance-review/market-snapshot-bench-20260915.csv)。基准源代码：[market_snapshot_bench.rs](D:/projects/zcodeworkspace/rust-trader/examples/market_snapshot_bench.rs)。

验证：`cargo test --locked --offline` 通过 59 项库测试及 3 项行情集成测试，共 62 项；`cargo build --locked --offline` 通过。独立 FIX42 字典/TCP 测试通过 exchange、client、playback 三端场景。已检查改动文件格式与 diff。尚未测量生产端到端 p99、持续交易吞吐或长期 RSS；Arc 的分配/回收、全量深度构建和 JSON 生成仍有成本。

## 2026-09-15：FIX 回报直达 writer 与持久输入缓冲

移除中间 Report 队列及 reader 转发循环，Engine 通过非阻塞适配器直接进入目标 writer 的出站队列。管理消息和报告现在共用一个容量 1024 的队列。保留全局撮合锁与每连接 reader/writer 两线程，在锁内保持回报入队顺序，在 writer 中统一发送序号。实际 reader 句柄显式设置运行期超时；输入采用跨握手、跨报文持有的 BufReader。最终 Logout 在引擎清理后入队，写完即关闭。详情见 [FIX 回报路径说明](fix-delivery.md)。

`python tests/fix_delivery.py --release` 在本机 Windows 运行独立进程，FIX message 日志关闭。五轮中，MAKER 挂出 4 手、收到确认后不再发送任何数据，TAKER 买入 4 手；同一 selector 接收双方回报。首轮还将 TAKER 的 Logon 与订单一次发送，计时包含握手。时间从发送买单前起算，包含 Python 校验、socket 和调度开销。

| 本次 release 五轮诊断 | MAKER 成交回报 | TAKER 成交回报 |
|---|---:|---:|
| 范围 | 1.137–3.136 ms | 1.060–6.134 ms |
| 中位数 | 1.376 ms | 1.868 ms |

先前基线的两次空闲 MAKER 诊断为 9987.933 ms 与 9806.095 ms，见 [原始基线](../target/performance-review/current-report-latency.json)。本次五轮均未出现原先接近 10 秒的等待。两次诊断的数量、首轮握手方式及样本数不同，不能据此计算整体系统加速倍数，也不能充当生产 p99 或同输入交替 A/B 性能验收。输入缓冲有模拟 Read 次数的回归证据，尚未分别测量其真实 syscall 数量或独立吞吐收益。

本次原始结果：[release JSON](../target/fix-delivery/1789485125468132700/diagnostic.json)、[debug JSON](../target/fix-delivery/1789485021456686800/diagnostic.json)。测试配置和进程日志保存在对应目录，进程在脚本 finally 中清理。

验证：63 项库测试及 3 项行情集成测试通过，共 66 项。debug 构建、release exchange 构建、独立 FIX42 字典/TCP 三端回归，以及 debug/release 的五轮真实 TCP 投递回归通过。投递回归同时验证合并发送的 Logon/订单、Logon 回包顺序、双方成交数量/价格、连续序号、唯一 ExecID、Logout 后关闭；Rust 测试覆盖单方满载不阻塞另一方和最终 Logout 后不发送已排队消息。

出站满载仍会使连接失败，未增加持久化与重放保证。全局锁、同步日志、慢 socket 写入、DAY 日期表扫描与持续高负载下的队列积压仍需后续独立处理或测量。

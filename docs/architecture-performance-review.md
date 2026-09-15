# 架构重构方案性能复核

日期：2026-09-15。基线：`8186df2dbd72997d3cc898be18a0d1fe881f9c51`。

复核对象：[架构重构方案](D:/projects/zcodeworkspace/rust-trader/docs/architecture-refactor-plan.md)。下文保留基线版本的只读复核记录与诊断样本，代码位置和“当前”均指复核时的基线，不代表后续实施状态。

后续实施进展：已完成 [ArcSwap 行情快照与操作内构建合并](market-data-snapshots.md)，以及 [回报直达发送端、实际 reader 超时修正和持久输入缓冲](fix-delivery.md)。最新验证见 [性能记录](performance.md)；其余建议仍待实施或性能验证。下文的约 10 秒诊断是改动前记录。

结论：方向可保留，但性能工作的顺序应调整。先消除回报等待、细碎 socket 读取、重复扫描和复制，再通过对照测试决定是否切换 EngineHost、替换队列。模块拆分与增加线程本身不保证更快。

## 1. 建议优先级

这里的 P1/P2 是本轮优化实施优先级。除回报等待外，收益为根据代码机制作出的判断，尚未测量优化前后的差值。

| 优先级 | 优化项 | 新方案覆盖情况 | 主要收益与实施建议 |
|---|---|---|---|
| P1 | 回报直接唤醒发送端，脱离 reader 的网络等待 | 描述了 delivery，但未明确消除这段等待；协调器还可能新增转发层 | 优先降低空闲挂单方的回报延迟；实测当前 Windows 存在约 10 秒等待，见下文 |
| P1 | 给 FIX 输入增加持久缓冲，避免头部逐字节读 socket | 未列为性能工作 | 降低每条报文的系统调用数；先评估持久 `BufReader<TcpStream>`，不必重写整个解析器 |
| P1 | DAY 到期不再每条消息扫描订单日期表 | 已有，建议前移 | 从日常 O(D) 扫描改为日期检查；统一时间输入与到期处理不必等 EngineHost |
| P1 | 同一命令中同品种的盘口只构建最终快照 | 已提断连合并，需扩大到改单、批量到期等并具体化 | 减少 O(价格档数) 的重复遍历、分配和锁占用；保留原 sequence 递增语义 |
| P2 | Arc 查询快照与 EngineHost 解耦推进 | 已有，但同列 R4 | 先缩短大盘口查询的锁占用；写侧仍要承担构建快照的成本 |
| P2 | 减少 Order 深复制和字符串索引分配 | 仅笼统提缓冲复用 | 先去掉明确冗余 clone，再评估数字 SessionId、稳定订单键；不改变外部字符串 ClOrdID |
| P2 | 合并已有待发送消息的编码/写入，限制批量预算 | 引擎批次已有，发送端未细化 | 降低每消息调度、分配和写系统调用成本；不能为了凑批次额外等待 |
| 条件项 | EngineHost、无锁队列、异步日志、按品种分片 | 部分已有性能关卡 | 只针对测得的瓶颈启用；它们各自可能增加排队、CPU、内存或可靠性成本 |

## 2. 最高优先级：成交回报被读线程挡住

当前路径：

```text
TAKER 读线程 → Engine 撮合 → MAKER report_queue
                                   ↓
                 等 MAKER 读线程结束 socket.read
                                   ↓
                         MAKER outgoing_queue → writer → 网络
```

证据：[session.rs:924](D:/projects/zcodeworkspace/rust-trader/src/fix/session.rs:924) 先进行 DAY 扫描，再将 `report_rx` 转入 `tx`，随后调用 `reader.read_message()`。因此，挂单方不主动发送数据时，已生成的回报也要等其读循环重新运行。单纯更换这两个队列的数据结构不会消除该等待。

### 本机实际观察

使用当前代码重新构建 release exchange，启动独立进程、独立端口和两个 FIX 会话；关闭 FIX message 日志，使用现有独立 FIX42 字典校验器验证收到的报文。MAKER 挂出 1 手卖单并收到确认后，TAKER 发出能完全成交的买单。两个客户端并行接收回报。

| 挂单确认后等待多久再发买单 | TAKER 成交回报 | MAKER 成交回报 |
|---:|---:|---:|
| 0 ms | 2.187 ms | 9987.933 ms |
| 200 ms | 1.645 ms | 9806.095 ms |

时间从测试端发出买单前计起，包含 Python 接收、校验和调度开销。只有两个诊断样本，证明当前路径存在长等待，不代表生产 p99，也不是新旧实现性能对照。最初使用 5 秒接收期限的两次诊断均未等到 MAKER 回报；延长观测期限后得到上表。测试进程已退出。

进一步做了独立 Rust socket 复现：先给原句柄设置 2 秒读超时，再 clone reader，然后仅给原句柄改成 100 ms。在本机，reader 仍报告 2 秒超时，实际等待 2007.875 ms；直接给 reader 设置 100 ms 时，实际等待 112.098 ms。环境为 Windows build 26200、rustc 1.98.1。

项目正好采用对应顺序：[session.rs:795](D:/projects/zcodeworkspace/rust-trader/src/fix/session.rs:795) 设置 10 秒握手超时并 clone reader，之后在 [session.rs:862](D:/projects/zcodeworkspace/rust-trader/src/fix/session.rs:862) 仅在原句柄上改为 500 ms。因此，之前仅根据常量推断“最多约 500 ms”的说法不能作为本机实际延迟结论。

[Rust 官方文档](https://doc.rust-lang.org/std/net/struct.TcpStream.html#method.try_clone) 描述 clone 句柄共享选项；上述本机实际观察与这一描述不同，不能推广成所有 Rust/Windows 或 Linux 的行为。部署平台需要检查实际读取句柄和超时行为。

建议分两步：

1. 作为局部修正候选，在实际 reader 句柄上落实运行期超时，并检查握手到正常运行的切换。它可以缩短当前等待，但仍保留轮询延迟。
2. 架构上让提交后的领域事件直接进入该会话发送端可被唤醒的有界入口；不再由阻塞于 socket 的 reader 搬运成交回报。`delivery` 可以是调用层，不必单独配置转发线程。

输出序号仍由一个执行上下文管理。保持 Logon、业务回报、GapFill、Logout 的合法顺序，以及连接代次隔离。若暂时保留多个 reader + Engine Mutex，不能随意改成“各 reader 解锁后各自投递”，否则后提交的事件可能先入队；过渡期可在同一提交锁内进行短暂、非阻塞、有界的投递，或采用严格保序的交接机制。

## 3. 解析输入还有明显的系统调用成本

[frame.rs:110](D:/projects/zcodeworkspace/rust-trader/src/fix/frame.rs:110) 在未读完 BeginString/BodyLength 之前，使用 `want = 1`。实际 acceptor 和 initiator 将 `TcpStream` 直接传给 FrameReader，分别见 [session.rs:797](D:/projects/zcodeworkspace/rust-trader/src/fix/session.rs:797) 与 [session.rs:1364](D:/projects/zcodeworkspace/rust-trader/src/fix/session.rs:1364)。

这意味着正常短报文的长度头会触发十几次甚至更多 `TcpStream::read` 调用，而不是仅在内存中逐字节解析；循环还重复扫描前缀并创建临时位置 Vec。仅加快撮合或换队列，会把这些开销留下。

优先评估让每个连接从握手开始持续拥有一个 `BufReader<TcpStream>`，再交给 FrameReader。读取头部时可从内存缓冲消费字节。缓冲必须跨消息保留，不能每次读完一帧就丢弃已预读的后续数据。保留原始字节校验、1 MiB 上限、长度头上限、部分消息超时续读语义。下一步再根据分配剖析评估字段切片、编码缓冲复用，不直接引入 unsafe 解析。

验证需包括：多帧合并发送、逐字节分片、跨超时分片、超长头、错误 BodyLength/CheckSum，以及真实 socket 的调用数量和端到端表现。本项尚未实施或测量改善量。

## 4. DAY 扫描和快照应前移优化

### DAY 到期

[exchange/mod.rs:235](D:/projects/zcodeworkspace/rust-trader/src/core/exchange/mod.rs:235) 每次遍历 `order_dates`，读循环又在每条消息/每次轮询时调用它。若有 D 个日期记录、每秒 R 次循环，检查工作量随 R × D 增长，并占用引擎全局锁。

新方案已有“不跨日不全扫描”，建议在 R2 就落地：每次业务提交使用统一 UTC 上下文检查日期，必要时先到期再撮合；空闲时由统一定时入口处理。只清理到期索引中不再需要的终态条目，不据此删除拒绝、关联或防重复需要的订单历史。虚拟时钟覆盖午夜边界、空闲跨日和时钟回拨；不能在某线程提前记录“今天已扫描”后漏掉另一时间上下文插入的旧日期记录。

### 盘口构建

已完成的每档数量增量维护值得保留，但 [orderbook.rs:289](D:/projects/zcodeworkspace/rust-trader/src/core/orderbook.rs:289) 每次 `build_book()` 仍遍历全部价格档并分配 Vec。

- [session_disconnect](D:/projects/zcodeworkspace/rust-trader/src/core/exchange/mod.rs:166) 对会话内每个订单重建盘口；同一品种大量撤单时重复明显。
- [modify_order_with_type](D:/projects/zcodeworkspace/rust-trader/src/core/exchange/mod.rs:356) 移除旧单后构建一次，重新入单后再构建最终状态。
- DAY 批量到期也逐单 `record_book`。

建议区分“逻辑版本/统计更新”与“物化完整盘口”。同一命令或既定原子操作结束时，对每个脏品种只构建最终快照，保留原全局 sequence 增量和各品种最终版本，不把中间版本直接压缩掉。保留逐笔成交量、高低价、最佳档清零及业务回报顺序。

一个包含 K 次同品种变动、每次约 L 档的操作，重复快照部分原来约 O(K × L)，合并后可接近 O(L)，不包括实际撤单/撮合成本。这是复杂度判断，不是实测加速倍数。独立客户端命令之间的快照发布不应擅自合并；否则可能改变 REST 可见性。

Arc 快照解决读取复制，不自动解决写侧 O(L) 物化。可先在现有引擎模型中推进一致的 `Arc<InstrumentSnapshot>` 发布，保留 Book/Stats/版本同提交，随后独立测试 EngineHost。REST 原来已经在锁外做 JSON 序列化，不能把这一点再次算作新架构收益。慢查询长期持有旧 Arc 时，还要计入旧版本存活的内存。

## 5. 减少复制与队列成本的具体边界

[orderbook.rs:202](D:/projects/zcodeworkspace/rust-trader/src/core/orderbook.rs:202) 先 clone 买卖档首订单，再分别 clone 成 buyer/seller；第二次 clone 可以优先评估改成所有权移动。部分成交更新还复制整份订单。

[orderbook.rs:94](D:/projects/zcodeworkspace/rust-trader/src/core/orderbook.rs:94) 查找/删除索引时构造 `session.to_string()`，会在热路径分配临时字符串。先评估不分配的查询键/索引布局；进一步使用数字 SessionId 和稳定订单键，应随 R2/R3 的身份设计实施。外部 ClOrdID 必须保留原字符串，代次与映射生命周期覆盖仍在排队的回报，不能靠提前删除映射节省内存。

对于新方案，应明确“模块数量不等于线程数量”。单 EngineHost 接收多个 reader 命令，入口仍是多生产者；同一个 writer 若同时接收 reader 的管理报文和 EngineHost 的成交事件，也是多生产者。不能因为引擎只有一个线程，就把整个出站队列直接换成 SPSC。

先保留有界、可唤醒的队列，测量排队时间与同步开销占比。只有明确一生产者一消费者的独立边才适合评估 SPSC；如果拆成两个 SPSC，又需计入多入口等待、唤醒和公平性的成本。忙等会占用 CPU，只有专核预算和低延迟目标明确时再测。

批量处理采用“拿到第一条就处理，再有限地取已经到达的消息”，同时限制消息数、字节数和处理时间。空载不为凑批次增加等待，高负载不能让一个会话垄断引擎。发送端可复用编码缓冲，并合并已经就绪的多帧写入；保留每帧的 FIX 序号、校验和和日志边界，处理部分写入与失败。

同步日志单独测量开/关差异。优先减少临时分配和细碎写入；异步日志若需要另立队列、满载和退出策略，不能作为“免费加速”。

## 6. “消息不能丢”需要单独约束

当前实现和原重构方案保留“队列满则断开”的策略。它限制资源，但不能保证已经成交的所有回报都能交给客户端。换成无锁队列也不提供这个保证。

如果“不丢”指运行期不能静默漏掉已承诺处理的消息，方案需明确接纳点、背压、失败恢复责任和回报保留方式；不能忽略 `Full` 中退回的消息。若还要求进程崩溃或重连后恢复，则需要将持久化记录、成交提交、回报确认/重放与幂等性作为一个整体设计。现有日志不是这样的可靠恢复日志。

可靠性方案会影响确认时点、排队和磁盘成本，必须与性能指标一起定义，不能同时沿用“无需持久化”的原范围又承诺崩溃后不丢。此项本轮未扩展实现。

## 7. 建议修订后的实施顺序与验收

1. R0 加入“空闲挂单方被另一会话成交”的双端延迟基线，并在 Windows/Linux 分别确认实际 reader 超时。只测主动下单方会漏掉这次发现的问题。
2. 在独立小提交中修正实际 reader 超时、评估持久输入缓冲；R1 模块移动保持单独提交。
3. R2 前移 DAY 检查、命令内快照合并和明显冗余 clone；明确保序的直接回报投递。
4. R3 统一状态机/代次时，明确协调器与 writer 的执行归属，避免无依据地为每层增加线程和队列。
5. 将 Arc 查询发布和 EngineHost 分别做对照实验。比较对象应是“已完成上述低风险优化的旧调用模型”，避免把去除旧缺陷的收益误归因于专属引擎线程。
6. 最后按剖析结果决定队列算法、有限批处理、日志缓冲和进一步分片。

至少覆盖：无成交入单、深档部分/完全成交、改单、批量断连、跨日到期、多会话竞争、REST 并发、慢接收方和大回报扇出。记录核心计算、锁等待/入口排队、事件入队到发送、双端成交回报、快照发布、CPU、RSS 与分配量；日志开/关分别记录。保持原计划 release、同输入、交替重复至少 5 次的验收方法，不用本轮两个诊断样本作 p99 或吞吐关卡。

## 8. 本轮验证材料

- `cargo build --release --bin exchange` 成功。
- [成交回报探针](D:/projects/zcodeworkspace/rust-trader/target/performance-review/delivery_probe.py) 与 [结果 JSON](D:/projects/zcodeworkspace/rust-trader/target/performance-review/current-report-latency.json)，包括基线提交、二进制 SHA-256 与进程退出确认。
- [独立 socket 超时复现](D:/projects/zcodeworkspace/rust-trader/target/performance-review/socket_timeout_probe.rs) 与 [结果](D:/projects/zcodeworkspace/rust-trader/target/performance-review/socket-timeout-result.txt)。
- 材料位于当前项目 `target/performance-review`，不修改配置源文件和业务代码。没有在本轮重跑全量回归，也没有修改、提交或推送新架构实现。

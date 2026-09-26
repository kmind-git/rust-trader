# 模拟分档成交模式（FillPolicy=Tiered）实施方案 v3

> v3：根据源码复核补充分笔上限与满载处理、空价格档清理、改单单次确认、
> 配置作用域校验及内部错误路径；同时统一簿内与 session 镜像状态。
> 本文是待实施方案，不表示 Tiered 已实现或已通过运行验证。

## 1. 目标与范围

在现有 `exchange` 二进制上新增可配置成交模式：报单不进入真实价格撮合，
按剩余量分档生成成交回报。真实撮合模式保留，默认仍为 Real。

- 分批成交在单次请求内执行，不增加时间间隔或后台任务。
- 不新增 REST 端点；分档成交不产生 `RawTrade`，volume/high/low 不变，
  但簿快照照常发布，REST `/api/book/{symbol}` 与 console `book` 可见挂单。
- 模式在 Engine 构造时确定，整个实例共用；不支持带存量订单切换模式。
- Tiered 下拒绝 `quote()`，防止报价腿与模拟挂单发生真实撮合。
- **playback 不适用于 Tiered；需要保留报价回放时，使用独立 Real 实例。**
- 不增加可靠投递、持久化或重放。队列满载和断线的明确边界见 3.4。

## 2. 行为定义

设分档基数 R = 报单**剩余量**：新单 R=Q；改单 R=新总数量−已成交量。
下表中的“确认”对新单是唯一一条 New，对改单是唯一一条 Replaced。

| 档位 | 条件 | 正常回报序列 |
|---|---|---|
| 不成交 | 0 < R ≤ 100 | 确认，订单留簿等待撤改或日终过期 |
| 全成 | 100 < R ≤ 1000 | 确认 → 一笔全量成交 |
| 成交 50% | 1000 < R ≤ 2000 | 确认 → 一笔 R/2 成交，余量留簿 |
| 分批成交 | 2000 < R ≤ 25600 | 确认 → 连续每笔 100，尾笔为不足 100 的余量 |
| 超限拒绝 | R > 25600 | 拒绝本次请求，不确认、不成交；改单保留原订单 |

边界：R=100 不成交；R=1000 全成；R=1001 成交 500.5；R=2000 成交 1000；
R=2001 → 20×100+1；R=2500 → 25×100；R=2550 → 25×100+50；
R=25600 → 256×100；R=25600.01 拒绝。全程使用 `Decimal`，不转浮点数。

新单数量必须大于 0；改单总数量不得小于 cum。改单 R=0 合法：仅发送一条
Replaced，保留累计成交和均价，状态为 Filled，不重新入簿、不生成零量成交。
注：Real 现状中 R>0 且 cum>0 的改单重入簿后状态会被重置为 Booked
（orderbook.rs `add()` 无条件置 Booked），Tiered 的 `insert_only` 契约 2
保持 PartialFill 是有意差异；Real 回归断言应按 Booked 写。

回报细节：

- 新单先 New 再成交；改单先 Replaced 再成交，**不得另发 New 或重复 Replaced**。
  Real 分支保留现有回报行为。
- 每条状态/成交回报独立 `ExecID`，复用 `Report::Status`/`Report::Fill`。
  Replaced 携带本次 `ClOrdID` 与原 `OrigClOrdID`，沿用现有改单关联规则。
- `LastPx = order.price`。市价单在同步记录、入簿和发送确认之前，将内部
  `order.price` 统一为 `Decimal::new(6688, 2)`，`order_type` 仍为 Market。
  FIX 编码继续沿用市价单省略 Price(44) 的现有规则，成交价由 LastPx(31) 表达。
- 部分成交订单可撤、可改、受日终过期和断线清理约束。撤单保留 CumQty/AvgPx；
  FIX 终态回报的 LeavesQty=0，不能把撤销前剩余量作为仍可成交量回报。
- “正常回报序列”以连接存活且投递成功为前提，不代表客户端必然收到全部消息。

## 3. 设计

### 3.1 有界分档计划与拒绝时点

`src/core/exchange/mod.rs` 增加私有纯函数：

```rust
const MAX_TIER_FILLS: usize = 256;
fn tier_fill_plan(remaining: Decimal) -> Result<Vec<Decimal>, EngineError>
```

`MAX_TIER_FILLS` 是本方案选定的固定资源上限，不新增配置键，也不宣称这是
测量得到的吞吐上限。单次请求最多生成 256 条成交回报，加确认共 257 条。

先校验 R，再分配/填充 Vec：负数报错；R=0 返回空计划；R>2000 时先比较
R 是否超过 `Decimal::from(25600)`，超限返回 `EngineError::TooManyTierFills`。
不得先展开任意大数量再判断长度，也不得先把任意大 Decimal 转为整数笔数。
上限从 `MAX_TIER_FILLS × 100` 派生，避免实现中维护两份独立常量。

- 新单：完成普通参数、会话和重复 ID 校验后，**在分配订单序号、写入 session、
  日期索引、订单簿及发送 New 之前**生成并校验计划。
- 改单：在确认原单有效、`quantity >= cum`、新 ID 可用后，使用候选 R 生成计划，
  **在移除原单、更新 ID/日期/镜像及发送 Replaced 之前**拒绝超限请求。
- 超限属于普通业务拒绝，沿用新单拒单/改单 CancelReject 编码。原单、行情版本、
  累计成交均不变；FIX 层已经登记的请求 ClOrdID 仍沿用现有请求 ID 占用语义。

### 3.2 OrderBook：只入簿与定点执行

新增仅供引擎使用的方法；真实撮合的 `add()`、`match_trades()` 算法保持不变：

```rust
pub(crate) fn insert_only(&mut self, order: Order) -> Result<Order, TieredBookError>
pub(crate) fn execute(&mut self, session: &str, id: OrderId, side: Side,
                     key_price: Decimal, qty: Decimal, fill_price: Decimal)
                     -> Result<Order, TieredBookError>
```

`TieredBookError` 区分订单不存在、重复入簿及非法执行参数；这些错误在校验完成后的
串行 Tiered 路径中属于内部不一致，处理方式见 3.5。校验失败时当前调用不修改簿。

`insert_only()` 的契约：

1. 仅接受剩余量大于 0 的活动订单，检测同一价档中的重复身份。
2. 根据累计量确定状态：cum=0 为 Booked，cum>0 为 PartialFill，不无条件覆盖为 Booked。
3. 按 `order.price` 选取 bids/asks 价格档，复用档内索引与 `push_back` 入档逻辑。
4. 返回实际入簿快照；Engine 用该快照同步 session 镜像后才发送确认。

`execute()` 的契约：

1. 按 side/key_price/session/id 定位并克隆订单；检查订单活动、
   `0 < qty <= remaining`、`fill_price > 0`，然后才能修改状态。
2. 在克隆上调用 `apply_fill`，更新 remaining/cum/avg/state；只有余量归零时为 Filled，
   50% 档执行完唯一一笔后仍为 PartialFill。
3. 将成交后快照及本笔量交给 `PriceLevel::update_fill`，维护档内订单与 `level.total`。
   **不能先覆盖原档内 remaining 再调用 update_fill**，否则其全成删除分支会扣错 total。
4. 若该档 `orders.is_empty()`，在释放档内可变借用后，从外层 bids/asks 删除整个价格档。
   `update_fill` 只删除档内订单，不会替调用者删除外层价档。
5. 返回成交后快照。不要再用会将活动订单标为 Cancelled 的外层撤单流程处理成交。

分档模式的档 key 必须用裸 `price`：现有真实入簿使用 `effective_price()`，
而撤单、改单、过期、断线清理均按记录的 `order.price` 移除。市价单内部价格
归一为 66.88 后，Tiered 的入簿、执行及移除均用同一个 key，不引入市价哨兵档。

保持不变量：`level.total` 等于档内所有订单剩余量之和；外层簿不存在空档；
簿与 session 镜像中的同一活动订单状态和成交数量一致。

### 3.3 Engine：模式、确认归属与行情发布

```rust
pub enum FillPolicy { Real, Tiered }
// Engine::new() 继续构造 Real，保留现有调用兼容性。
pub fn with_fill_policy(policy: FillPolicy) -> Engine
```

不提供运行期间修改模式的 setter。`exchange` 在配置校验成功后构造 Engine，
再载入品种并启动 REST/FIX，防止存量订单混用两种价格 key 和成交语义。

**新单 Tiered 分支**：

1. 完成 3.1 的预校验，再构造订单；市价单归一价格。
2. 建立 session/日期记录，取得品种簿并 `insert_only()`；用返回快照同步镜像。
3. 由 `create_order` 发送唯一一条 New。
4. 调用下述只负责成交的私有方法，然后统一完成行情发布并返回其执行结果。
5. 跳过 Real 分支的 `add_to_book`、`send_trade_reports`、`final_state` 同步及
   条件 `send_status`，防止真实撮合或双发 New。

**改单 Tiered 分支**（实际入口为 `modify_order_with_type`）：

1. 完成 3.1 的预校验后，移除旧簿单并沿用原来的逻辑行情版本推进。
2. 更新 ID、类型、数量及日期，保留累计成交量和均价；市价单先归一价格。R=0 时状态 Filled，
   R>0 且 cum>0 时 PartialFill，否则 Booked。
3. R>0 时 `insert_only()` 并同步返回快照；R=0 时只同步终态，不入簿。
4. 保留唯一的 `send_status_for(..., ExecType::Replaced, Some(orig_id))` 发送点，
   放在上述准备之后、分档成交之前。Real 的原有发送顺序保持不变。
5. R=0 时发布移除后的最终簿并返回；R>0 时仅执行计划中的成交并发布最终簿。
   跳过 Real 的 `add_to_book` 及其返回值处理，不重复发送确认。

私有方法 `tiered_fill_existing(order, plan) -> Result<(), EngineError>`：

- 只对已入簿订单执行预先校验的计划，不入簿、不发送 New/Replaced，也不重新按当前
  remaining 选择档位。计划中的每笔量固定，避免 50% 档被反复减半。
- 每笔 `book.execute(...)` 成功后，将返回快照交给 `send_fill`。该方法负责同步
  session 的成交字段并分配独立 ExecID。
- 簿执行错误立即中止后续成交并返回内部错误，不能静默跳过；详见 3.5。

外层操作为入簿/成交变更执行 `record_market_data(instrument_id, &[])`，再统一
`publish_market_data()`。改单 R=0 仅发布此前已记录的移除，不额外推进版本。
内部错误退出时，也须记录尚未登记的簿变更并发布已有待发布版本。应先保存执行结果，
再发布，最后返回结果，不能用提前 `?` 返回遗漏发布。覆盖无成交新单、部分/全部成交、
改单 R=0 以及内部错误退出。
预校验拒绝不得推进版本。空 trades 不增加成交统计，但更新深度及最佳买卖价；
某侧最后一单全成后，其价档必须消失，最佳价/量归零或切换到下一有效档。

### 3.4 回报容量与投递失败

当前每连接出站队列容量为 1024，管理消息和交易回报共用；发送为非阻塞
`try_send`。满载/接收端退出会设置连接失败标志，reader/writer 随后关闭连接。
现有 Engine 不回滚已经成交的订单，也没有持久化重放保证。

v3 明确沿用这一投递策略，不增加批量预留、阻塞等待、无界队列或自动重试：

- 最多 256 笔是**单次计算和回报生成上限**，不是 1024 队列的可用容量保证。
  已有积压、连续请求或慢客户端仍可能让一笔合法订单投递失败。
- 若确认或某笔成交回报无法入队，连接按现有规则进入失败状态；Engine 仍完成
  本次已经接受的有限成交计划并同步状态、发布行情，不回滚、不重发，也不再把
  此请求作为“未接受”发送业务拒绝。此时 API 成功只表示业务处理完成。
- 后续失败投递由现有失败标志快速返回；会话退出时按既有流程清理剩余挂单。
  writer 可能尚未写完已经排队的消息，客户端不能假定收到最终 Filled。
- 不在 Engine 锁内等待队列空位或做网络/磁盘 IO。保持其他会话能够在本次有界
  操作结束后继续处理；本方案不据此承诺具体时延。

这是保留的故障语义，不能用“257 < 1024”声称已解决可靠交付。如果后续要求
每个已接受订单的全部回报都可恢复，应另行设计持久化/重放与准入，不在本次扩展中暗含。

### 3.5 内部订单簿错误

Engine 的 `&mut self` 调用及 FIX 的全局 Mutex 将整次操作串行化，撤改和断线清理
不能在分档循环中并发移除订单。因此执行时找不到订单属于不变量破坏，不是正常竞争。

- `insert_only`/`execute` 的 `TieredBookError` 转为独立的
  `EngineError::BookInvariantViolation`，携带定位所需上下文；停止本次后续分笔。
- 保留已经完成的成交，不声称全成或回滚；同步已成功执行的快照，并按 3.3
  发布当前实际簿。失败调用本身必须在修改该笔状态之前返回错误。
- FIX `handle_business` 在把 Engine 错误转换为字符串之前区分这一错误，设置现有
  `tx.failure_flag()`，并把 session/order/instrument/分笔序号及原因带回 reader。
  reader 释放 Engine 锁后记录故障，再进入既有关闭与 `session_disconnect` 清理流程；
  不在持有 Engine 锁时进行会话日志 IO。落地时同步区分 reader 故障日志文案
（现固定为 "Outbound queue overflow/disconnected"，session.rs:965），
不变量故障不应复用满载文案。
- 不把该错误走普通新单 Rejected/改单 CancelReject 路径，避免已发确认/部分成交后
  又声称整次请求未生效；不返回 Ok、不重试失败笔。直接使用 Engine 的调用者必须
  将该错误视为会话故障并清理会话，不能继续把此订单当作正常接受结果。

### 3.6 配置

`FillPolicy` 是本项目的全局扩展键，**只允许显式写在 `[DEFAULT]`**：

```ini
[DEFAULT]
FillPolicy=Tiered
```

- `src/fix/config.rs` 的 `KNOWN_KEYS` 增加此键；共享解析器校验 DEFAULT 值，
  仅接受大小写精确的 `Real`/`Tiered`，缺省由 exchange 使用 Real，空值或其他值报错。
- 对原始 `config.sessions` 逐项检查；任何 SESSION 显式出现 `FillPolicy` 均报错，
  即使与 DEFAULT 相同也拒绝。不能检查继承后的映射，否则会错误拒绝正常继承。
- `src/bin/exchange.rs` 从已校验的 DEFAULT 读取模式，在启动监听器前注入 Engine，
  启动日志打印实际模式；Tiered 同时打印 `MAX_TIER_FILLS=256`。
- client/playback 共用解析器，合法 DEFAULT 值可接受但没有本地成交行为；
  值与作用域校验仍适用，不能静默接受非法值。这不允许客户端选择服务器成交模式。
  playback 能解析该配置不代表它支持向 Tiered 实例回放报价，适用范围见 3.7。

### 3.7 quote() 防线

Tiered 下 `quote()` 在任何撤旧报价、写簿或行情变更之前返回专用错误
`EngineError::QuoteNotSupportedInTiered`，不进入真实 `add_to_book`。

沿用现有 `MassQuote` 响应级别：level=2 始终回 ack，level=1 在出错时回 ack，
level=0 不回 ack。因此 Tiered 拒绝报价时 **level=1/2 都返回拒绝回报，只有 level=0 无回报**。

**playback 兼容性与运行要求**：现有 playback 将每行数据发送为 MassQuote，
默认响应级别为 0。连接 Tiered 后报价会被拒绝，但客户端可能没有拒绝反馈，
表现为进程正常结束而盘口没有变化；不得把正常退出视为回放成功。

需要报价回放时，启动独立的 `exchange` Real 实例，并将 playback 连接到该实例的
FIX 端口。与 Tiered 实例同时运行时，两个实例使用各自的 FIX/REST 端口和日志目录。
`FillPolicy` 是服务端实例级开关，在 playback 配置中填写 Real 不能改变所连接服务器的模式。
本次不将 playback 改造成分档报单工具，也不新增自动探测服务器模式的协议。

## 4. 改动清单

| 文件 | 计划改动 |
|---|---|
| `src/core/orderbook.rs` | 两个 Tiered 方法与错误类型；保留部分成交状态、维护 total、清理空档；相关单测 |
| `src/core/exchange/mod.rs` | 构造时选择模式、有界计划与预校验、两个明确分支、单次确认、行情收尾和内部错误（含新 `EngineError` 变体及 Display） |
| `src/core/exchange/tests.rs` | 分档边界、状态/簿一致性、撤改/过期/断线、超限无副作用和内部错误测试 |
| `src/fix/config.rs` | 白名单、DEFAULT 枚举值与原始 SESSION 作用域校验及单测 |
| `src/fix/session.rs` | 区分内部簿错误与业务拒绝，复用失败标志关闭连接；满载及错误路径单测 |
| `src/bin/exchange.rs` | 配置校验后构造 Engine、打印实际模式和上限 |
| `tests/tiered_fill.py` | 新增 Tiered 的 FIX TCP/字典与 REST 集成用例，复用现有黑盒测试设施 |
| `tests/README.md` | Tiered 集成测试运行方式 |

`src/queue.rs` 和 FIX 编码格式无需为此改动；不预估固定行数，以上行为与测试为实施边界。

## 5. 测试与验收

### 5.1 计划与前置校验

- 纯函数覆盖负数、0、99.5、100、100.01、1000、1001、2000、2000.5、2001、
  2500、2550、25599.5、25600、25600.01、102400 及 `Decimal::MAX`。
- 精确断言档位、每笔数量、计划总量与长度；超限在构造分笔数组之前拒绝。
- 超限新单不入簿、不写 session/日期、不推进行情版本、不发送 New/Fill；
  超限改单保留原 ID、簿单、日期、状态及 CumQty/AvgPx，不发送 Replaced/Fill。
- Real 不受 Tiered 上限影响；R 上限针对改单剩余量，不错误限制新总数量。

### 5.2 订单簿与业务回报

- 四档均验证确认先于成交、所有 ExecID 唯一、CumQty/LeavesQty/AvgPx 和末笔终态正确。
  R≤100 的无成交新单在 session 与簿内均为 Booked，撤单成功。
- 部分成交后改单分别覆盖 R≤100、单笔全成、50%、分批及 R=0；恰好一条 Replaced、
  零条 New，OrigClOrdID 正确，成交不超过新的剩余量，累计均价继承正确。
- 已有成交且改单后 R≤100 时，簿内与 session 均保留 PartialFill；R=0 不重新入簿。
- 市价买/卖的新单与限价改市价均验证内部价及 LastPx=66.88；覆盖无成交、部分成交，
  随后撤单、再次改单、日终过期和断线清理，不能留下幽灵单。
- 同价多单时 total 等于剩余量之和；某档最后一单全成后删除价档；有次优档时
  最佳价转移，无其他档时该侧最佳价/量归零。检查 REST 深度而非只检查内存数量。
- 交叉价格的两个 Tiered 挂单仍各自按分档规则处理，不发生相互真实撮合；
  volume/high/low 不变，快照版本及挂单可见性按操作更新。
- `execute` 找不到订单、非法数量、超量执行、非正成交价以及重复入簿均显式报错，
  当前失败调用不改变状态。通过模块内故障注入验证内部错误中止后续分笔、返回错误、
  发布已改变的簿，不误发普通拒单、不声称全部成交。
- quote 拒绝无簿/行情副作用；FIX level=1/2 返回拒绝 ack，level=0 无 ack。

### 5.3 投递边界与配置

- 空队列且暂停消费时，合法最大单应排入 257 条回报；恢复消费后逐条校验顺序和数量。
- 使用小容量或预填队列，分别在确认和中途成交处触发满载；验证失败标志、连接关闭、
  无阻塞重试、有限计划仍完成、实际簿和镜像一致。不能把引擎完成断言当作客户端收齐证明。
- 独立断开的 sink 覆盖相同边界；另一会话仍可继续处理。内部簿错误与投递失败分别测试：
  前者停止分笔并返回错误，后者沿用既有策略完成有界业务处理。
- 配置覆盖缺省、DEFAULT 的 Real/Tiered、空值、大小写错误、未知值；
  SESSION 单独写、覆盖 DEFAULT、重复相同值均拒绝，正常 DEFAULT 继承允许。
- 启动黑盒验证非法配置在监听之前退出，日志中的有效模式与实际成交行为一致。
- playback 适用范围验收：连接独立 Real 实例时正常回放报价并更新该实例盘口，
  同时运行的 Tiered 实例不受影响；误连 Tiered 时报价不改变簿或行情统计。
  校验实际盘口结果，不以 playback 正常退出作为业务成功依据。

### 5.4 回归与完成标准

运行 `cargo test`、现有 `fix42_wire.py` / `fix_delivery.py` / `deployment_smoke.py`，
再运行新增 Tiered 集成脚本；具体参数沿用 `tests/README.md` 并补充新脚本说明。
现有脚本验证默认 Real 未回归，新增脚本验证真实 FIX 字段、消息数、连接及 REST 结果。

实施完成须同时满足：边界数量与资源上限明确生效、单次确认、簿/镜像/行情一致、
非法配置不静默降级、两类故障按上述不同语义处理，且记录实际测试结果。
仅更新本方案或通过已有 Real 测试，都不等于 Tiered 已实现。

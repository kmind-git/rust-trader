# 模拟分档成交模式（FillPolicy=Tiered）实施方案 v2

> v2：按独立复核结论修订。修正四项阻塞：市价单簿 key 失配、手工更新破坏
> `PriceLevel.total`、改单分档基数错误、配置键会被白名单拒绝；并补 quote() 混用防线。

## 1. 目标与范围

在现有 `exchange` 二进制上新增可配置成交模式：报单不进入真实价格撮合，
按数量分档生成成交回报。真实撮合模式（现状）完全保留，默认不变。

**明确不做**：分批成交无时间间隔（单次请求内连发）；不新增 REST 端点；
分档成交不产生成交流水（volume/high/low 不变），但**簿快照照常发布**（REST `/api/book` 与 console `book` 可见挂单）；分档模式下 `quote()` 被拒绝（见 3.5）。

## 2. 行为定义

设分档基数 R = 报单**剩余量**（新单 R=Q；改单 R=新数量−已成交量）：

| 档位 | 条件 | 回报序列 |
|---|---|---|
| 不成交 | R ≤ 100 | 已报，订单留在簿内等待撤改 |
| 全成 | 100 < R ≤ 1000 | 已报 → 一笔全量成交 |
| 成交 50% | 1000 < R ≤ 2000 | 已报 → 一笔 50% 成交，余量可撤 |
| 分批成交 | R > 2000 | 已报 → 连续每笔 100，余量不足 100 时尾笔为余量 |

边界：R=100 不成交；R=1000 全成；R=2000 成交 1000；R=2001 → 20×100+1；
R=2550 → 25×100+50；R=2500 → 25×100。

回报细节：
- 分档模式**总是先发已报**（真实模式保持现状不发出独立 New 的行为）。
- 每笔回报独立 `ExecID`（沿用 `next_exec_id`），复用 `Report::Status`/`Report::Fill`。
- 成交价 `LastPx` = `order.price`。**市价单进入分档路径时 `order.price` 置为固定价
  66.88**（`Decimal::new(6688, 2)`，常量，不做配置；`order_type` 保持 Market），
  因此限价/市价行为统一，无特判。
- 部分成交订单留在簿内：可撤（回报带剩余量）、可改、受日终过期约束。

## 3. 设计

### 3.1 纯函数：分档计划

`src/core/exchange/mod.rs` 私有函数（带单测）：

```rust
fn tier_fill_plan(remaining: Decimal) -> Vec<Decimal>
```

### 3.2 OrderBook：只入簿不撮合 + 定点执行

新增两个公开方法（真实路径 `add()` 及其三个调用点不动）：

```rust
pub fn insert_only(&mut self, mut order: Order) {
    order.state = OrderState::Booked;          // 复核#5：add() 才设 Booked
    let price = order.price;                    // 档 key 用 price，不用 effective_price()
    // 复用现有私有 insert 的入档逻辑（按 price 取 bids/asks 档）
}

pub fn execute(&mut self, session: &str, id: OrderId, side: Side,
               key_price: Decimal, qty: Decimal, fill_price: Decimal) -> Option<Order>
// locate(key_price) → clone → apply_fill(order, qty, fill_price)
// → PriceLevel::update_fill（自动维护 level.total；余量归零自动移除）
// → 返回成交后订单快照（None = 未找到）
```

**为什么档 key 用 `price` 而非 `effective_price()`（复核#1/#6 的根因）**：
`insert()` 用 `effective_price()`，市价买=9999999999999、卖=0；而引擎全部移除
调用（session_disconnect mod.rs:208、expire :285、modify :428、cancel :540）
传的都是裸 `order.price`。市价单若按 effective_price 入档，四个移除路径全部
失配（expire 还会吞错照发 Expired，幽灵单永留簿内）。分档模式把市价单
`price` 归一为 66.88 后统一以 `price` 为档 key，所有现有移除路径零改动即兼容，
簿/统计也不会出现哨兵价。

**为什么用 `execute()` 而非手工改字段（复核#2）**：`avg_price/cum/state` 的算法
已存在为 `apply_fill`（orderbook.rs:279），`PriceLevel.total` 只由
`update_fill` 正确扣减，手工 locate_mut 更新会让 REST 簿深度和统计量失真。

### 3.3 Engine：模式字段与执行入口

```rust
pub enum FillPolicy { Real, Tiered }   // Engine 字段，默认 Real
pub fn set_fill_policy(&mut self, policy: FillPolicy)
```

在 `create_order` 与 `modify_order` 各自调用 `add_to_book` 处**显式分支**
（复核#7：不是一行替换）：

- `create_order`（mod.rs:345 后）：Real → 原逻辑原样；Tiered → 走 `tiered_execute`
  并**跳过**后续 `record_market_data/publish_market_data/send_trade_reports/
  条件 send_status` 块（`trades.is_empty()` 在分档路径恒真，不跳过会双发 New）。
- `modify_order`（mod.rs:493 后）：同理，并跳过依赖 `add_to_book` 返回值的
  `final_state` 同步块（Replaced ack 在 mod.rs:484 已先发出，顺序保持）。

`tiered_execute(order, ack)` 私有方法：
1. 市价单 `order.price = 66.88`；
2. 同步 session 镜像记录后 `book.insert_only(order)`；
3. 发 ack（create → `send_status(New)`；modify → 维持现有 Replaced 语义）；
4. `tier_fill_plan(order.remaining)` 逐笔：`book.execute(...)` 返回成交后快照 →
   `send_fill(&snapshot, order.price, chunk)`（内部已同步 session 镜像）；
   返回 None 视为并发移除，跳过该笔；
5. 状态顺序由 `execute()` 保证：先 `apply_fill` 置 Filled 再移除
   （复核#12：调换会使终态误报 Cancelled）；
6. 末尾调 `record_market_data(instrument_id, &[])` + `publish_market_data()`
   （空 trades：不产生成交统计，仅推进 sequence 并发布簿快照，使挂单在
   REST/console 可见；无成交档也执行，覆盖“≤100 挂单不成交”的可见性）。

改单语义：modify 已算好 `remaining = 新数量 − cum`（mod.rs:461），
分档基数即 `remaining`（复核#3：按新数量分档会超量成交）。

### 3.4 配置

- `src/fix/config.rs`：`KNOWN_KEYS` 白名单加 `FillPolicy`（复核#4：未知键
  触发 `ConfigError::UnknownKey`，启动即死）。白名单为 exchange/client/
  playback 三个二进制共享，后两者配置误带此键会被静默接受（无代码读取
  它，无行为分叉），可接受。
- `src/bin/exchange.rs`：`[DEFAULT]` 可选键 `FillPolicy = Real | Tiered`，
  缺省 Real，未知值沿用严格风格报错退出；启动日志打印当前模式。

### 3.5 quote() 防线（新增）

`quote()` 仍走真实 `add_to_book`（mod.rs:637）。若分档模式下允许报价，
报价腿会与分档挂单发生**真实撮合成交**，两种模式互相污染。因此
分档模式下 `quote()` 直接返回 `EngineError`（一条 match 分支），
这是不变量保护，不是新功能。已知可观测性限制：reject ack 仅在报价
   level=2（或 level=1 出错）时回发（session.rs:1287），level 0/1 的报价在
   分档模式下被拒但无反馈，属现状行为，不修。

## 4. 改动清单

| 文件 | 改动 | 估行数 |
|---|---|---|
| `src/core/orderbook.rs` | `insert_only()` + `execute()`（复用 `apply_fill`/`update_fill`） | ~45 |
| `src/core/exchange/mod.rs` | `FillPolicy`、`tier_fill_plan()`、`tiered_execute()`、两处分支、quote 防线 | ~120 |
| `src/fix/config.rs` | `KNOWN_KEYS` 加 `FillPolicy` | ~1 |
| `src/bin/exchange.rs` | 读 `FillPolicy` 并注入 Engine | ~15 |
| `src/core/exchange/tests.rs` | 分档单测 | ~90 |
| `configs/` | Tiered 示例配置（可选） | ~0 |

## 5. 测试计划

1. `tier_fill_plan` 边界：100 / 100 / 1000 / 1001 / 2000 / 2001 / 2500 / 2550 / 2000.5。
2. Engine 级单测（复用 tests.rs 的 sink 断言设施）：
   - R≤100：只有 New 回报，撤单成功；
   - 100<R≤1000：New + 单笔 Filled，ExecID 唯一；
   - 1000<R≤2000：New + PartialFill（余量正确），余量撤单成功；
   - R>2000：New + N×100 + 尾笔余量，累计=R，最终 Filled；
   - 改单：先 50% 成交再改单，分档基数=remaining，不超量；
   - 市价单：LastPx 与簿内价格均为 66.88，撤单路径正常（回归复核#1）；
   - 分档模式下 `quote()` 被拒；
   - `PriceLevel.total` 一致（分档成交后 REST 簿深度不失真，回归复核#2）。
3. 配置：`KNOWN_KEYS` 接受 `FillPolicy`；未知值报错。
4. 回归：`cargo test` 全量 + 三个 Python 黑盒脚本（默认 Real 行为不变）。

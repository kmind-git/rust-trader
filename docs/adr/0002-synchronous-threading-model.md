# 纯同步线程模型，不使用 async/tokio

网络与并发全部采用 std 同步设施（std::net 阻塞 IO、每连接一线程、Mutex、Condvar），REST 用阻塞式的 tiny_http，整个 crate 不出现 async。依据：Go 参照实现即为此形态（每连接一个 goroutine + 每订单簿一把锁），连接规模为个位数、撮合为轻负载，async 带来的复杂度（传染所有层、锁与 Future 的交互、Send/SendSync 约束）没有对应收益。

## Considered Options

- **tokio + axum 全异步**：生态事实标准、可扩展到数千连接——拒绝：当前与可预见规模用不到；心智负担与代码复杂度显著更高，且与"契约等价、结构对齐 Go 版"的目标相悖。
- **混合双运行时**（引擎线程 + REST tokio）：依赖与沟通成本最高——拒绝。
- **纯同步线程（选定）**：代价是将来若演进为高并发网关需要重构网络层；对外契约不受影响，且届时可仅替换 `fix/session.rs` 与 `rest.rs` 的 IO 层。

## Consequences

- 该决策限定的是 IO 层形态；领域核心（`core/`）本就不含 IO，不受影响。
- REST 层选型随之锁定为 tiny_http（若未来切 async，同步替换为 axum 即可，JSON/DTO 层复用）。

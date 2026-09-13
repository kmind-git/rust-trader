# 线协议采用 FIX 4.2 而非 FIX 4.4

本项目对外线协议按用户决策定为 FIX 4.2（BeginString=FIX.4.2）：ExecutionReport 的 ExecType(150) 使用 4.2 枚举 0=New / 1=Partial fill / 2=Fill / 4=Canceled（成交与状态语义直观分离），NewOrderSingle 携带必填的 HandlInst(21)，MassQuote 组携带 UnderlyingSymbol(311)/TotQuoteEntries(304)，SecurityDefinition 携带 TotalNumSecurities(393)。放弃 4.4 的原因：用户对 4.2 的回报语义（150 与 39 分离、0/1/2/4 生命周期）更熟悉且明确要求；且本项目消息集完全落在 4.2 字典内。

## Consequences

- FIX 4.4 的 `150=F(Trade)` / `150=I(Order Status)` 在本项目中不使用；订单状态一律看 OrdStatus(39)，其 0/1/2/4/8 枚举在 4.2 与 4.4 中相同。
- FIX 4.2 没有 SecurityListRequest(x)：品种查询/创建逐个通过 SecurityDefinitionRequest(c) 完成，客户端不再有"批量下载"流程。
- FIX 4.2 没有 SessionReject(3)：业务级拒绝以 BusinessMessageReject(j) + Text(58) 回复。
- 与参照实现 go-trader（FIX 4.4）的交叉验证随版本分叉退役；若需重启对照，需临时将 go-trader 的 BeginString 配置同步为 FIX.4.2（quickfixgo 支持多版本，但其应用层仍会发送 4.4 风格的 I/F 值，仅供参考不再作为 oracle）。

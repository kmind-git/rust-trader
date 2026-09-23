# 线协议采用 FIX 4.2 而非 FIX 4.4

本项目对外线协议按用户决策定为 FIX 4.2（BeginString=FIX.4.2）。当前实现只承诺已实现消息的 FIX 4.2 子集：Logon/Logout/Heartbeat/TestRequest/ResendRequest/SequenceReset、NewOrderSingle(D)、OrderCancelRequest(F)、OrderCancelReplaceRequest(G)、MassQuote(i)、ExecutionReport(8)、MassQuoteAcknowledgement(b) 和 SecurityDefinition(d)。其字段、必填项、枚举和重复组按内置 FIX 4.2 profile 校验；未实现的消息类型会得到明确拒绝或会话级拒绝。

配置中的 `UseDataDictionary=Y` 表示启用这个内置 FIX 4.2 profile，并不是一个只记录不生效的开关。`DataDictionary` 只接受内置 profile（`builtin` 或 `FIX42.xml` 逻辑名称）；其它路径和 `UseDataDictionary=N` 会在启动时报错。配置解析遵守 QuickFIX 的 `[DEFAULT]` 继承和 `[SESSION]` 覆盖规则，见 [README](../../README.md#fix-settings)。

`ExecutionReport` 的 FIX 4.2 回报使用 `ExecType(150)` 的 0=New / 1=Partial fill / 2=Fill / 4=Canceled 枚举以及 `OrdStatus(39)` 生命周期，`NewOrderSingle` 携带必填的 `HandlInst(21)`，`MassQuote` 组携带 `UnderlyingSymbol(311)`/`TotQuoteEntries(304)`，`SecurityDefinition` 携带 `TotalNumSecurities(393)`。放弃 4.4 的原因：用户明确要求 4.2 回报语义，且当前消息集完全落在 4.2 字典内。

## Consequences

- FIX 4.4 的 `150=F(Trade)` / `150=I(Order Status)` 在本项目中不使用；订单状态看 `OrdStatus(39)`，成交和状态回报仍按 FIX 4.2 的 ExecType/OrdStatus 组合表达。
- FIX 4.2 的完整字典消息集大于本项目实现范围。未实现的 SecurityListRequest(x)、动态品种创建以及其它应用消息会被明确拒绝，不会被当作已支持消息处理。
- `BusinessMessageReject(j)` 仅用于已定义的业务拒绝场景，并携带可关联原请求的标准引用字段；会话层格式错误使用 FIX 4.2 会话级拒绝流程。

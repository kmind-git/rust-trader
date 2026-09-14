# FIX 4.2 审计与实现范围

本轮目标是已支持消息正确、未支持请求明确拒绝；本项目仍是手写会话层，没有接入 QuickFIX 引擎，也不宣称通过第三方 FIX 认证。

## 已修正

- OrdType 使用 1=Market、2=Limit；成交回报包含 AvgPx、累计成交数量和独立 ExecID。改单保留累计成交及 OrderID，正确关联 OrigClOrdID；撤单回报关联撤单请求，失败使用 OrderCancelReject。
- 标准字符串 ClOrdID 在接入侧无损映射；非法数量、价格、重复请求 ID 和不支持的订单选项会拒绝。
- BodyLength 有大小限制，CheckSum 验证，分段报文遇到读取超时仍保留；高序号消息排队恢复，ResendRequest 使用 GapFill，校验 CompID、重复消息和心跳请求关联，完成 Logout 交换与线程退出。
- 配置按 DEFAULT 继承、SESSION 覆盖，逐会话选择日志；不支持的配置和值启动报错。

## 支持范围与项目扩展

会话消息 A/0/1/2/3/4/5；应用请求 D/F/G/i，回报 8/9/b/d/j。SecurityDefinition(d) 在登录后发送静态品种信息。其它请求明确拒绝。内置字段校验只支持这一 profile，不加载任意外部 XML。

订单支持 Market、Limit、HandlInst=1、TimeInForce=0（DAY，省略同义）。DAY 订单以 UTC 日期为边界到期，断连撤销本连接订单。MassQuote 只支持单 QuoteSet、单 QuoteEntry；缺省一侧撤销旧报价是项目的报价快照扩展。QuoteResponseLevel 使用 0/1/2 的标准响应语义。

当前只有内存会话状态：PersistMessages=N，ResetOnDisconnect=Y，ResetOnLogout=Y；重发请求用 GapFill，不重放历史应用消息。exchange 单进程只支持同一 SenderCompID 和监听端口，不兼容的多会话配置明确报错。没有自动重新连接与持久化订单恢复。

Logging、DynamicSessions、TargetCompIDs 是项目配置扩展。日志沿用 message/event 双文件；in/out 方向、UTC+8 展示时间、损坏帧 in-hex 明确属于扩展，报文时间仍是 UTC。无日志轮转。

## 性能审计结论

本轮消除了不可信 BodyLength 导致的无界帧分配，并保留增量读取缓冲，避免超时丢弃半帧。没有进行吞吐量基准，因此不报告性能提升比例。

后续已完成有界队列、REST 锁外序列化、价格档增量聚合及撤单索引，见 [性能优化验证](performance.md)。该文档给出本地微基准数据和多档撤单场景的权衡；全局锁、快照克隆和同步日志仍有进一步优化空间。

## 验证

Rust 单元测试覆盖撮合、累计成交、改单、DAY 到期、编码、配置、日志与分帧。独立 Python TCP 测试依据仓库内官方 QuickFIX FIX42.xml 校验实际回报，并验证交易所及两个客户端的关键交互。该测试不是完整 FIX 认证测试套件，也未包含生产负载压测。

参考：[QuickFIX 配置说明](https://quickfixengine.org/c/documentation/getting-started/configuration.html)、[QuickFIX Session 默认值实现](https://github.com/quickfix/quickfix/blob/master/src/C%2B%2B/Session.cpp)。PersistMessages 的网页默认值与当前 C++ 源码不同，本项目采用源码的 true（Y），要求显式配置 N。

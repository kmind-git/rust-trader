# 契约基线收缩：移除 (c) 动态建品种、控制台 watch 与 -props

2026-09-13 的功能评审（grilling 会话）决定：「契约等价」从硬约束降级为**可收缩基线**——逐项评估后，裁剪 SecurityDefinitionRequest (c) 运行期动态建品种（acceptor 处理 + initiator 封装，回包 (d) 因品种下载依赖而保留）、交易所控制台 watch/unwatch（每品种一线程秒级轮询）、client/playback 的 -props/got_settings 存在性检查（连同 configs/got_settings）。依据：这三者在工具链中均零调用（已 grep 实证），属纯表面积而非能力；保留项（撤单/改单 F/G、MassQuote ACK (b)、会话维护全套、REST 四端点、-speed）均有互通或功能价值。

## Considered Options

- **维持完全等价（D2 原状）**——拒绝：为两条从未走过的路径（外部 (c)、(b) 确认级别）与一个可用 curl 轮询替代的控制台功能，付出持续维护成本。
- **自由裁剪（连 F/G、ACK、会话维护一起删）**——拒绝：撤单是挂单唯一出口（断线撤单同路径）、ACK 是报价回执半边、会话维护是 quickfixgo 互通硬要求，删任何一项丧失基本能力。
- **可收缩基线（选定）**：只删双侧零使用的表面积，能力型功能全保留。

## Consequences

- 品种来源收敛为启动时的 instruments.txt，运行期不再新增；外部客户端发 (c) 会得到 "unsupported msg type" 日志并被忽略。
- 交易所控制台为 quit/sessions/book/list/help；盘口观察统一走 REST。
- 命令行不再接受 -props；configs/got_settings 已删除。
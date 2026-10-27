# 部署评审整改与统一验证记录

日期：2026-10-27。原评审：[deployment-review-2026-10-26.md](deployment-review-2026-10-26.md)。修改基于 `5940c0e9e2fdc962b5b731814491c58694d1e1cd`，本记录随整改提交；具体提交与后续构建身份以 GitHub 记录为准。

已完成 D1–D7 对应代码、配置、部署入口及文档修改，所有实现与测试先汇总，再统一运行验证。Windows 本地构建、91 项 Rust 测试、15 项启动检查和全部现有 Python FIX/撮合回归通过。Linux 归档验收已加入发布工作流，本机无可用 Linux/WSL，未执行真实 Linux 部署；不能据此宣称发布 CI、systemd 或 RHEL/CentOS 验收通过。

## 整改对照

| 编号 | 已实现变化 | 主要文件 |
|---|---|---|
| D1 | 品种加载采用原子校验：读取错误、空文件、错误列数、非法或重复 ID、重复 SYMBOL 均阻止启动。日志预检后先绑定 FIX，再启动 REST；REST 工作线程创建成功后返回，两个监听器成功后才写带 PID 的 ready 文件 | `src/core/instrument.rs`、`src/core/exchange/mod.rs`、`src/bin/exchange.rs`、`src/fix/session.rs`、`src/rest.rs` |
| D2 | 以二进制/控制入口位置定位安装根；配置默认值和相对日志目录不依赖调用者 cwd；普通用户 `traderctl check/start/stop/restart/status`，独立控制锁和由进程持有的实例锁，PID+启动时间+可执行路径核验；可生成实际用户/路径的 systemd unit | `src/runtime.rs`、三个 `src/bin/*.rs`、`traderctl`、`deploy/rust-trader.service` |
| D3 | 包内直接提供有效配置，由普通用户解压及编辑；默认部署不再要求 root 创建配置 | `scripts/package-linux.sh`、`DEPLOY.md` |
| D4 | `configs/` 有 exchange/connector/instruments/runtime 有效文件，`defaults/` 保留同版本原始副本；playback 优先使用包内 `examples/playback.txt`；升级步骤只更新程序和模板并保留既有配置 | `scripts/package-linux.sh`、`src/bin/playback.rs`、`configs/runtime.env`、`DEPLOY.md` |
| D5 | 运行诊断按本地日期落盘，默认 info，启动信息与错误接入统一日志；FIX messages/event 日志按本地日期和会话拆分，QuickFIX/J 时间前缀、完整原文、独立开关；两类日志分别配置保留期，默认 14 天，0 禁用清理 | `src/runtime.rs`、`src/fix/log.rs`、`configs/runtime.env`、`docs/adr/0005-fix-session-logs.md` |
| D6 | 部署测试改为完整打包脚本/真实 musl 归档，覆盖普通用户权限、带空格路径、非源码 cwd、有效配置修改、启停、PID 隔离、失败场景和升级；发布上传前加入实包门禁；工具链统一 1.98.1，目标兼容性保持待验收 | `tests/deployment_smoke.py`、`.github/workflows/release.yml`、`DEPLOY.md`、`README.md` |
| D7 | 默认忽略可安全识别的未消费标准可选单值字段及自定义单值扩展；对全部已支持消息执行消息/组层级校验，保留必填、业务值域、重复字段、组边界、帧与会话校验。未实现消息、未知标准字段、不支持的标准组及长度限定数据明确拒绝；原报文不改写 | `src/fix/codec.rs`、`tests/fix42_wire.py` |

`traderctl` 的 Linux 运行能力及 D3/D4/D6 的真实归档验收属于已实现、尚待运行验证；Bash 语法通过不等于 Linux 启停验收。现有 `FileLogPath=logs/exchange` 和 `logs/client` 保留，在各自目录下增加 `YYYY-MM-DD/`；connector 配置下 playback 同样写 `logs/client`。

## 统一验证结果

环境为 Windows 本机，Rust 固定工具链 1.98.1，debug 构建。协议回归仅使用本地独立模拟进程，没有连接外部 FIX 对端或发布版本。

| 检查 | 结果 |
|---|---|
| `cargo test --locked --offline` | 通过：88 项库单元测试、3 项行情集成测试，0 失败 |
| `cargo build --locked --offline --bins` | 通过：exchange/client/playback |
| `python tests/startup_validation.py` | 通过：15 项。缺失/空/错误/重复品种配置、日志路径不可写、check 不监听、异 cwd 默认配置、FIX/REST 冲突不发布 ready、stdin EOF 和 RUST_LOG=off |
| `python tests/fix42_wire.py` | 通过：独立 FIX42 字典、Logon/新改单撤单/报价/回报的默认兼容及负面结构检查、扩展原文日志、client/playback initiator、序号恢复、心跳与 Logout |
| `python tests/fix_delivery.py` | 通过：5 轮双会话回报投递、连续序号、ExecID 唯一、Logout 顺序；耗时仅是本地回归样本 |
| `python tests/real_boundary.py` | 通过：Real 撮合、扫单、市价余量、改单、拒单、报价及统计 |
| `python tests/tiered_fill.py` | 通过：分档成交、市价、超限拒绝、撤单及 REST |
| `python tests/tiered_boundary.py` | 通过：数量边界、改单、挂簿市价、断线清理与会话隔离 |
| 改动 Rust 文件的 `rustfmt --check --edition 2021 --config skip_children=true` | 通过 |
| 五个改动 Python 脚本的 `python -m py_compile` | 通过 |
| `bash -n traderctl scripts/package-linux.sh` | 通过；Git Bash 沙箱内无法创建信号管道，改用获准的沙箱外只读语法检查 |
| `git diff --check` | 通过 |
| `python tests/deployment_smoke.py` | Windows 明确 SKIP；没有验证 Linux 归档部署 |

统一验证期间同步调整测试：Real/Tiered 启动策略断言改读独立运行日志；Windows 端口冲突夹具独占绑定与服务相同的 `0.0.0.0` 地址。相关回归均已重跑通过。

可检查的本地证据（位于 Git 忽略的 `target/` 目录，不随源码提交或自动上传）：

- [15 项启动检查结果](../../target/test-runs/startup-validation/1793110877820711300/results.json)
- [FIX 线协议配置与日志](../../target/test-runs/fix42-wire/)
- [FIX 回报投递 diagnostic.json](../../target/test-runs/fix-delivery/1793110769034803700/diagnostic.json)
- [Real 边界产物](../../target/test-runs/real_boundary/1793110766855209600/)
- [Tiered 成交产物](../../target/test-runs/tiered_fill/1793110767791302300/)
- [Tiered 边界产物](../../target/test-runs/tiered_boundary/1793110768554209200/)

测试启动的本地 exchange/client/playback 进程均已结束。测试配置、日志和结果保留在仓库内 `target/test-runs/`。

## 保留的边界与待验收项

- Linux：尚未在本机执行完整 musl 构建、真实归档解压启停、普通用户权限、systemd、SELinux、RHEL/CentOS 目标运行。对应归档必须保留同一 SHA-256 进行目标验收；未触发或宣称发布 CI 已通过。
- 停止：`stop` 验证实例后发 SIGTERM 并等待退出。没有实现接入停止、队列排空、FIX Logout 的完整优雅停机状态机，也没有新增订单/会话持久化；重启仍丢失内存交易状态。
- FIX 自定义字段：`>=5000` 按单值扩展契约处理。没有私有字典元数据时，单条私有组与多个单值字段不可可靠区分，不承诺支持私有复杂组；重复自定义字段仍拒绝。不新增严格模式开关。
- 日志：跨日与保留期由可控日期单测验证，没有等待真实午夜。FIX 仅清理当前会话自己的过期文件；不再建立的历史会话需另行归档或清理。控制脚本的 console 兜底日志在启动时按保留期清理。

当前使用方式与完整操作步骤见 [DEPLOY.md](../../DEPLOY.md)。

## 提交前复核与构建环境确认

用户于 2026-10-27 确认沿用上一版环境，并先生成 GitHub Actions 候选包：Ubuntu 24.04 runner、Rust 1.98.1、`x86_64-unknown-linux-musl` 静态 tar.gz，RHEL 7.6 目标机兼容性继续待验收。上一版 `v0.1.1` 归档及其 `BUILD-INFO.json` 已重新下载核对，外层 SHA-256 为 `3fef9632c443fd12b661a24c9bc84aa9ad4f86c071ca0390e88b140adb1f8207`。

独立只读复核确认 D1–D7 均有对应实现；另发现 `STOP_TIMEOUT=0` 会阻断停止命令。提交前已修正为 `stop` 忽略无效超时并回退可用值，同时增加零值及非法文本配置下仍可停止的 Linux 归档用例，随本次候选包运行验证。

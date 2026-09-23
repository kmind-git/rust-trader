# RHEL 7.6 打包与部署方案

日期：2026-09-16。代码基线：`e44f1ff7be8dc018423cbc3aae1694730d951955`。状态：方案，尚未实施或生成 Linux 发布包。

用户指定操作系统为 Red Hat Enterprise Linux 7.6；CPU 暂沿用仓库现有的 x86_64 目标，实施前用 `uname -m` 确认。首版交付采用 **Linux CI 构建 musl 静态二进制、tar.gz 离线安装、systemd 托管、真实 RHEL 7.6 验收**。服务器运行时不需要安装 Rust、Cargo、Docker 或 Python。

## 1. 目标平台与兼容边界

| 项目 | 方案 |
|---|---|
| 运行环境 | RHEL 7.6 x86_64；验收记录实际内核、glibc、systemd 与 CPU |
| 编译目标 | `x86_64-unknown-linux-musl`，对最终 ELF 验证静态链接 |
| 构建环境 | GitHub Actions 的固定 `ubuntu-24.04` runner，固定 Rust 工具链和 Cargo.lock |
| 程序 | `exchange` 为服务；`client`、`playback` 为可选联调工具，随包交付但不自动运行 |
| 发布形式 | 首版 tar.gz；RPM 可在运维要求明确后复用同一份已验收二进制 |
| 升级方式 | 维护窗口停机切换，保留旧版本与配置备份 |

RHEL 7.6 发布基线包含 Linux `3.10.0-957`；RHEL 7 的 glibc 基于 `2.17`，Red Hat 的 7.6 更新镜像记录使用 systemd `219`。这些是版本资料，不代替目标服务器实测。[RHEL 7.6 内核](https://docs.redhat.com/zh-cn/documentation/red_hat_enterprise_linux/7/html/7.6_release_notes/new_features_kernel)、[RHEL 7 glibc](https://docs.redhat.com/en/documentation/red_hat_enterprise_linux/7/html/7.0_release_notes/sect-red_hat_enterprise_linux-7.0_release_notes-compiler_and_tools-glibc)、[RHEL 7.6 更新镜像包列表](https://access.redhat.com/articles/4297201)。

musl 方案用于减少对目标机 glibc 版本的依赖，不承诺消除内核、CPU、系统调用、SELinux 和服务管理差异。Rust 提供该 musl 编译目标，但没有据此保证本项目在每种旧系统上都能运行。[Rust 平台支持](https://doc.rust-lang.org/rustc/platform-support.html)。

构建时禁用 `target-cpu=native`，使用通用 x86-64 指令基线。RHEL 7.6 容器若运行在新内核宿主上，只能补充用户态检查；最终关卡必须是在对应旧内核上启动的虚拟机或物理机。

## 2. 现有基础与需要补齐的部分

| 已有文件/能力 | 当前状态 | 本次实施应补齐 |
|---|---|---|
| [.github/workflows/release.yml](../.github/workflows/release.yml) | tag/手动触发；构建 musl；tar 输出与上传路径已一致 | 固定构建环境、`--locked`、测试关卡、版本元数据、SHA-256、解包验收 |
| [deploy/rust-trader.service](../deploy/rust-trader.service) | 已使用 `--server`，非 root 用户运行 | 程序/配置/日志目录分离，绝对路径参数，RHEL 7 服务生命周期验证 |
| [DEPLOY.md](../DEPLOY.md) | 简易原地解压安装；泛称 CentOS 通用 | 改为明确的 RHEL 7.6 验收范围；加入配置保护、维护窗口与回滚流程 |
| [tests/deployment_smoke.py](../tests/deployment_smoke.py) | stdin EOF、REST/FIX 监听、tar 路径 fixture 检查 | 测真实发布包，不将 fixture 打包成功视为 Linux 兼容成功 |
| [tests/fix42_wire.py](../tests/fix42_wire.py)、[tests/fix_delivery.py](../tests/fix_delivery.py) | 独立字典与 TCP 回归，当前有源码目录/target 路径假设 | 支持指定解包后的 binary/config/output 路径，补充对现有服务的外部 peer 模式 |

现有代码提供部署需要的 CLI：`--server`、`-fix`、`-instruments`、`-port`。本轮打包不需要重写撮合、队列或 FIX 状态机。

## 3. 交付物

正式包名：`rust-trader-<版本>-linux-x86_64-musl.tar.gz`。正式版本来自经过校验的 `v<版本>` tag，要求对应 Cargo 包版本；手动构建使用 `dev-<短SHA>-<运行号>`，避免覆盖正式版或不同提交互相覆盖。发布附件包括压缩包、外层 `.sha256`、对应提交的源码归档和验收记录。

```text
rust-trader-<release-id>-linux-x86_64-musl/
├── bin/
│   ├── exchange
│   ├── client
│   └── playback
├── configs/
│   ├── qf_exchange_settings.example
│   ├── qf_connector_settings.example
│   └── instruments.txt.example
├── examples/playback.txt
├── systemd/rust-trader.service
├── scripts/
│   ├── install.sh
│   ├── upgrade.sh
│   ├── rollback.sh
│   └── check.sh
├── BUILD-INFO.json
├── SHA256SUMS
├── LICENSE
├── licenses/                     # 随包组件的许可证材料
└── DEPLOY.md
```

以上脚本为拟新增交付物，目前不存在。包只带配置示例，不带真实环境配置、账户凭据、日志、订单数据、Windows `.exe`、`.git` 或编译缓存。

`BUILD-INFO.json` 至少记录版本、完整 Git SHA、`rustc -Vv`、目标 triple、构建时间、runner、Cargo.lock 哈希及使用的编译选项。内部 `SHA256SUMS` 覆盖有效载荷文件，不包含自身；外层哈希用于核验整个 tar.gz。发布后不原地替换相同版本的二进制。

## 4. 构建与发布流水线

```mermaid
flowchart LR
    A[指定提交或版本 tag] --> B[固定 Linux 构建环境]
    B --> C[Rust 与 FIX 回归]
    C --> D[musl release 三个程序]
    D --> E[ELF 检查与归档]
    E --> F[解包后实际程序测试]
    F --> G[RHEL 7.6 虚拟机验收]
    G --> H[发布同一哈希的包及记录]
```

1. **可追溯构建**：保留 GitHub Actions 入口，runner 固定为 `ubuntu-24.04`，工具链通过拟新增 `rust-toolchain.toml` 固定。本机已验证工具链为 `rustc 1.98.1`，实施时先确认相同版本在 Linux 的构建与回归结果，再冻结到文件；不跟随浮动 `stable`。[GitHub runner 标签](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job)。
2. **准备编译环境**：CI 安装 musl target、musl 编译工具、binutils、Python 测试依赖；生产机不执行这些安装。固定关键工具/Action 版本并记录环境，不能仅凭固定 runner 标签宣称位级可复现。
3. **回归与构建**：运行已有 Rust/FIX 测试，使用 `cargo build --locked --release --target x86_64-unknown-linux-musl --bins` 构建。目标测试在 Linux 上以对应 `--target` 运行；网络接收缓冲、超时与 Logout 必须在 Linux 再验。
4. **ELF 门禁**：每个程序都用 `file`、`readelf -h -l -d` 检查 x86-64 ELF、无 `PT_INTERP` 和动态 `NEEDED` 共享库依赖。不能只凭 `.exe` 后缀消失或设置 musl target 就宣布静态产物正确。
5. **打包门禁**：脚本与服务文件统一 LF，程序/脚本具有执行权限；归档只有预定顶层目录，使用明确文件列表；校验解包文件与清单一致。
6. **测试真正的交付物**：解包至隔离目录，通过参数指定包内二进制运行测试；不能仍调用 `target/debug/exchange`。通过后保留该压缩包，不在验收后重新编译或重新打包。
7. **目标机验收后发布**：CI 构建附件先视为候选包；只有同一 SHA-256 的包通过 RHEL 7.6 关卡后，才标记为已验证 Release。没有目标机时保留“兼容性待验收”状态，不能自动发布兼容结论。

RHEL 验收由现代控制机通过 SSH 或人工命令驱动，目标机只需系统自带工具与包内程序；不要求在 RHEL 7.6 安装 GitHub Actions runner 或新 Python。功能测试所需 Python 运行在控制端；外部 peer 模式须在实施阶段补齐。

## 5. 安装目录与权限

```text
/opt/rust-trader/
├── releases/<release-id>/        # root 持有的不可变版本目录
└── current -> releases/<release-id>
/etc/rust-trader/
├── qf_exchange_settings               # 本机 FIX 配置，升级不覆盖
└── instruments.txt              # 本机品种表，升级不覆盖
/var/log/rust-trader/fix/         # rusttrader 可写的会话日志
/etc/systemd/system/rust-trader.service
```

运行账户使用 `rusttrader`，系统账户、无交互登录。程序与版本目录由 root 持有，运行账户只需读/执行；配置目录建议 root:rusttrader、目录 0750、文件 0640；日志目录由 rusttrader 写入。安装器只管理明确的项目路径，不递归修改整个 `/opt` 或 `/etc`。

安装器先检查系统/架构、包校验、运行账户、配置和日志权限。品种表必须存在、可读且格式正确：当前程序加载失败只打印错误并继续启动，因此不能只看进程存活就判定安装成功。

首次安装仅在配置缺失时从 `.example` 复制；已有配置保持原样。重复执行应返回明确状态，不重复创建用户、不覆盖本机配置、不自动重启正在交易的服务。安装与启动分开，由操作人员完成配置后启动。

## 6. FIX 配置与启动

以下为将来安装到 `/etc/rust-trader/qf_exchange_settings` 的示例，使用现有解析器已支持的键；CompID 与端口部署时替换为本机要求。

```ini
[DEFAULT]
ConnectionType=acceptor
BeginString=FIX.4.2
SenderCompID=GOX
SocketAcceptPort=5001
HeartBtInt=30
ResetOnLogout=Y
ResetOnDisconnect=Y
PersistMessages=N
UseDataDictionary=Y
FileLogPath=/var/log/rust-trader/fix
# Project extension: file logging switch.
Logging=Y
# Project extension: only declared SESSION clients are admitted.
DynamicSessions=N

[SESSION]
TargetCompID=CLIENT

[SESSION]
TargetCompID=PLAYBACK
```

生产准入按真实客户逐个声明 `[SESSION]`，不默认开启动态接入。DEFAULT 继承和 SESSION 覆盖照旧；当前同一 exchange 的会话须共享监听端口及服务端 SenderCompID。日志路径可由 SESSION 覆盖。不能向配置中添加尚未实现的队列容量、TLS 或绑定地址键。

拟更新的服务文件采用 RHEL 7 可用的基本选项，示例：

```ini
[Unit]
Description=Rust Trader Exchange (FIX 4.2 + REST)
After=network.target

[Service]
Type=simple
User=rusttrader
Group=rusttrader
WorkingDirectory=/opt/rust-trader/current
ExecStart=/opt/rust-trader/current/bin/exchange --server -fix /etc/rust-trader/qf_exchange_settings -instruments /etc/rust-trader/instruments.txt -port 8080
Environment=RUST_LOG=info
UMask=0027
Restart=on-failure
RestartSec=5
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

`Type=simple` 对应 `--server` 不 fork 的前台进程。服务退出重启只恢复进程，不恢复交易状态；`TimeoutStopSec` 也不会自动赋予程序排空回报的能力。RHEL 7 的服务创建、Restart 和 daemon-reload 用法可参考 [Red Hat systemd 管理指南](https://docs.redhat.com/en/documentation/red_hat_enterprise_linux/7/html/system_administrators_guide/chap-managing_services_with_systemd)。本方案不依赖较新 systemd 才有的 `DynamicUser`、`StateDirectory` 等选项。

目前没有专门的 SIGTERM 优雅停机处理，没有就绪通知，也没有配置热加载。安装后执行 `systemctl daemon-reload`，配置完成后分别执行 `systemctl enable rust-trader` 和 `systemctl start rust-trader`；运行 `systemctl is-active` 后仍须做应用检查。

FIX 与 REST 当前都绑定 `0.0.0.0`，REST 无鉴权，FIX CompID 白名单也不等同于加密或身份凭证。部署时按业务网络限制 FIX 5001 与 REST 8080 的访问来源，安装脚本不默认向所有来源开放端口；保留 SELinux 当前模式，在目标机检查目录标签与拒绝日志，而不是默认关闭 SELinux。

## 7. 日志与升级回滚

会话 message/event 文件继续保留项目定义的方向、展示时区与损坏帧标记；应用运行日志由 journald 收集。现有文件日志长期打开且没有 reopen/轮转接口，首版采用维护窗口停服务后归档的明确操作，不宣称直接 rename 就能在线轮转，也不默认采用可能丢行的 copytruncate。容量监控与留存周期写入部署说明。

升级流程：

1. 核验新包与 BUILD-INFO，解压到新的版本目录，记录当前链接目标；此时不动运行中的版本。
2. 检查配置兼容，备份 `/etc/rust-trader` 和已安装 unit；新包只提供配置示例，不覆盖生产配置。
3. 在维护窗口协调客户停止下单、核对成交与订单状态、完成注销，再停止服务。当前系统不能保证 SIGTERM 前自动排空所有回报。
4. 将 `current` 链接切换到新目录；若 unit 变更，安装对应版本并 `daemon-reload`；启动后执行应用检查。
5. 失败时停止新进程，将链接恢复到旧目录；有配置/unit 变更时同时恢复兼容版本，再启动并复核。每步失败应退出并给出恢复位置，不循环重试未知状态。

**回滚只能恢复程序与配置，不能恢复旧订单、会话序号、成交统计或未送达回报。** 当前状态仅在内存，重启后配置中的品种会重新加载；订单与交易统计重新开始。因此首版不是无停机升级，也没有自动恢复业务连续性的能力。

若目标机已经采用旧版 `/opt/rust-trader/exchange` 原地安装，先识别并备份旧程序、configs、unit 和日志；保持停止状态后再迁移到版本目录与 `/etc`，不能把已有根目录文件当作可清理的暂存内容。

## 8. 验收标准

| 关卡 | 必须取得的证据 |
|---|---|
| 平台 | `cat /etc/redhat-release`、`uname -r`、`uname -m`、`rpm -q glibc systemd`，对应真实 RHEL 7.6 环境 |
| 产物 | 三个 ELF 架构/静态依赖检查、内部与外部 SHA-256、LF/权限、版本与完整提交 SHA |
| 基础功能 | 包内三程序可执行；无 Rust/Python 运行依赖；关闭 stdin 后 exchange 持续服务 |
| FIX | 声明会话 Logon/Logout、下单/成交/改单/撤单、拒绝、序号恢复、空闲挂单方回报、合并和分段报文 |
| REST | 品种列表与预期一致，盘口/统计查询正确；已知品种初始空盘口正常 |
| systemd | 以 rusttrader 启停、重启、开机启动；配置/品种表可读、日志可写、退出状态可观察 |
| 安装升级 | 重复安装保留配置，新旧版本切换与失败回滚完成；旧安装可迁移 |
| 负载 | 在目标机测多 FIX 连接、集中做市商回报、慢接收方、并发 REST，记录日志开/关两组结果 |

首版目标机负载测试应报告吞吐、双方回报 p50/p95/p99、CPU、RSS、连接异常与外部可核对的丢回报情况；既有 1024 条队列限制与满载断连语义照旧。当前没有队列水位指标，不能报告未采集的内部水位或仅凭外部延迟反推精确积压。

Windows 的既有微基准不能代替 Linux/musl 性能验证。如果目标机实测 musl 版本在相关负载下表现不合适，再用 RHEL 7 ABI/构建 sysroot 生成 GNU 目标对照测试；不能直接拿新 Ubuntu 上的默认 GNU 二进制假定兼容 glibc 2.17。

## 9. 实施拆分与完成条件

| 阶段 | 拟新增或调整文件 | 完成条件 |
|---|---|---|
| P1 固定构建 | `rust-toolchain.toml`、`.github/workflows/release.yml`、`scripts/package-linux.sh` | 固定版本构建三个目标程序，生成可校验候选包 |
| P2 安装运维 | `deploy/` 下安装/升级/回滚/检查脚本与 unit、`DEPLOY.md` | 目录与权限正确，配置保护、旧安装迁移、回滚可演练 |
| P3 包级测试 | `tests/deployment_smoke.py`、FIX 测试入口、新包级检查 | Linux CI 测的是解包后的实际文件，产物 SHA 不变 |
| P4 目标验收 | RHEL 7.6 虚拟机/服务器及验收记录 | 通过上述关卡，再发布同一候选包为正式附件 |

实施前只需补齐目标 CPU 架构，以及 RHEL 7.6 测试机或由用户执行验收命令的安排；这些信息不阻止先完成 P1–P3。服务器访问与停机操作在实际部署时单独安排。本次仅新增此方案文件，没有修改工作流、服务配置、应用代码，没有创建 tag、触发构建或部署到服务器。

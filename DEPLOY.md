# Linux 用户部署手册（rust-trader）

发布包为 `x86_64-unknown-linux-musl` 静态链接候选产物。ELF 检查和 CI 的 Ubuntu 运行验收不能证明 RHEL 7.6、CentOS 7 或 Stream 8/9 的兼容性；这些系统仍须用同一归档 SHA-256 在目标环境验收，包含内核、CPU、权限、SELinux 和所选 systemd 版本。

## 普通用户安装

依赖 Linux、Bash、coreutils、util-linux 的 `flock`，解压时需要 tar/gzip。部署用户直接在自己有写权限的位置解压，不需要新建系统用户或修改系统目录。例如：

```bash
mkdir -p "$HOME/apps"
sha256sum -c rust-trader-<版本>-linux-x86_64-musl.tar.gz.sha256
tar -xzf rust-trader-<版本>-linux-x86_64-musl.tar.gz -C "$HOME/apps"
cd "$HOME/apps/rust-trader-<版本>-linux-x86_64-musl"
sha256sum -c SHA256SUMS
# 编辑 configs/qf_exchange_settings、configs/instruments.txt、configs/runtime.env
./traderctl check
./traderctl start
./traderctl status
curl http://127.0.0.1:8080/api/instruments/
./traderctl stop
```

`configs/` 已包含有效配置，由解压用户拥有并直接编辑；`defaults/` 保存同版本默认值供对照。无需从 `.example` 复制。关闭标准输入不影响 `start` 后的服务。控制入口总是以自身所在位置定位安装目录，因此从其他目录调用绝对路径也可用；移动安装目录前先停止实例。

```text
rust-trader-<版本>-linux-x86_64-musl/
├── bin/                        # exchange / client / playback
├── configs/                    # runtime.env、FIX acceptor/connector、instruments.txt
├── defaults/                   # 上述配置的同版本原始副本
├── examples/playback.txt
├── logs/                       # 启动时自动创建，见日志说明
├── run/                        # 自动创建的本实例 PID、ready 标记、锁
├── traderctl                   # check/start/stop/restart/status/run/systemd-unit
├── systemd/rust-trader.service.in
├── SHA256SUMS
├── BUILD-INFO.json
└── DEPLOY.md
```

`check` 检查品种配置、FIX 配置和日志可写性，不监听端口。`start` 先运行检查，再后台启动并等待程序确认 REST/FIX 两个端口均绑定成功；端口冲突或就绪超时返回非零。`status` 就绪返回 0，运行但未就绪返回 1，停止或失效 PID 返回 3。`stop` 比较 PID、Linux 进程启动时间和本安装目录的 executable 后发送 SIGTERM，等待退出；不会对失效、复用或不属于本实例的 PID 发信号，也不会超时强杀。实例锁持续由 exchange 持有，防止并发重复启动。运行配置被误改后，`stop/status` 仍可定位既有实例。

`run` 为 systemd 提供同样锁保护的前台入口。SIGTERM 停止目前仍是进程终止，不承诺停止接入、回报排空及 FIX Logout 的完整优雅停机。交易状态保存在内存，停止或重启会丢失订单等运行状态。

## 配置和日志

`configs/runtime.env` 使用逐行的字面 `KEY=value`，不支持引号、变量展开或 shell 命令。路径允许空格，以安装根目录为基准；绝对路径保持绝对含义。修改后重启生效。traderctl 和直接调用的二进制都会读取此文件，已有环境变量优先于文件值；二进制默认路径也从安装根目录定位。

| 配置 | 默认值 | 用途 |
|---|---|---|
| `REST_PORT` | `8080` | 只读 REST 端口；FIX 端口在 acceptor 配置中 |
| `RUST_LOG` | `info` | 运行诊断级别，支持 env_logger 过滤表达式 |
| `APP_LOG_PATH` | `logs/app` | 运行诊断日志目录 |
| `APP_LOG_RETENTION_DAYS` | `14` | 运行诊断保留天数，0 禁用清理 |
| `FIX_LOG_RETENTION_DAYS` | `14` | FIX 日志保留天数，0 禁用清理 |
| `START_TIMEOUT` / `STOP_TIMEOUT` | `15` | traderctl 启动就绪 / 停止等待秒数 |

运行诊断与 FIX 原文分别管理。运行日志按本地日期落盘；`traderctl start` 另将 stdout/stderr 兜底写入 `APP_LOG_PATH/console-YYYY-MM-DD.log`，该文件用于早期错误定位，程序启动后的诊断由应用日志负责。FIX `Logging=Y/N` 只控制 FIX 日志，`FileLogPath` 决定其目录，实际按本地日期/会话拆分，采用 QuickFIX/J FileLog 风格行前缀。FIX 报文的 52/60 仍为 UTC，`RUST_LOG=off` 不抑制 FIX 原文。

`configs/qf_exchange_settings` 中 `[SESSION]` 声明允许接入的 CompID，`DynamicSessions=Y` 才允许动态身份。默认 FIX 兼容行为忽略可以安全识别的未消费单值扩展字段，保留必填、枚举、重复字段、帧和重复组校验；这不实现对应账户或路由语义。

随包 client 和 playback 均有有效 connector 配置；playback 默认使用 `examples/playback.txt`。示例启动：

```bash
./bin/client
./bin/playback
```

## 升级并保留配置

先验证新归档的外层 checksum。停止旧实例，把新包解压到独立目录并验证内层 `SHA256SUMS`，再只复制程序、默认模板、控制脚本、服务模板和说明。不要直接覆盖解压到旧目录。

```bash
# OLD、NEW 分别为旧安装和已独立解压校验的新安装绝对路径
"$OLD/traderctl" stop
cp "$NEW/bin/"* "$OLD/bin/"
cp "$NEW/defaults/"* "$OLD/defaults/"
cp "$NEW/traderctl" "$NEW/DEPLOY.md" "$NEW/LICENSE" "$NEW/BUILD-INFO.json" "$OLD/"
cp "$NEW/systemd/"* "$OLD/systemd/"
# 对照 defaults 手动合并新增配置，保留 configs、logs、run
"$OLD/traderctl" check
"$OLD/traderctl" start
```

已有有效 `configs/` 文件不被复制步骤覆盖。升级后的目录包含用户配置，不再以旧 `SHA256SUMS` 验证整个目录；保留原始已验证的发布归档及其 checksum 作为产物证据，`BUILD-INFO.json` 记录当前程序版本。使用 systemd 时，停止/启动步骤改由 systemctl 完成，更新后重新生成 unit 并 daemon-reload。

## 可选 systemd 托管

先停止自行后台运行的实例。以下第一条以实际部署用户执行，自动带入当前用户、组及实际安装路径；生成结果在本安装目录内供审查，之后才由管理员安装：

```bash
./traderctl stop
./traderctl systemd-unit > systemd/rust-trader.service
# 管理员安装已生成文件
sudo install -m 644 systemd/rust-trader.service /etc/systemd/system/rust-trader.service
sudo systemctl daemon-reload
sudo systemctl enable --now rust-trader
sudo systemctl status rust-trader
sudo systemctl stop rust-trader
```

不得直接安装含 `@ROOT@/@USER@/@GROUP@` 的 `.in` 模板。systemd 直接托管 `traderctl run` 的前台进程；选择 systemd 后，日常启停用 systemctl，避免混用后台模式。默认日志仍落盘，早期 stderr 也可由 `journalctl -u rust-trader` 查看。含百分号、美元符号、反引号、双引号、反斜线或换行的安装路径不支持生成该 unit，请换用普通目录名。家目录是否允许 systemd 执行由目标机器 SELinux 策略决定，按实际拒绝记录配置本地策略并在目标环境复验，不能用静态链接推断通过。

FIX 默认监听 5001、REST 默认 8080，按实际接入范围配置网络和防火墙。这里的验证没有覆盖目标系统防火墙或 SELinux。

## 构建与发布验证

仓库固定 Rust **1.98.1**，具体 target 见 `rust-toolchain.toml`。Linux 候选包流程：

```bash
cargo test --locked --target x86_64-unknown-linux-musl
cargo build --locked --release --target x86_64-unknown-linux-musl --bins
bash scripts/package-linux.sh dev-local target dist
python3 tests/deployment_smoke.py --archive dist/rust-trader-dev-local-linux-x86_64-musl.tar.gz
```

真实归档验收要求普通 Linux 用户执行，测试在仓库 `target/test-runs/` 下解压到带空格的任意目录，从非源码工作目录修改有效配置、检查和启停，验证双监听器、PID 保护、失败路径及升级配置保留。发布 CI 在上传候选包前运行此门禁。Windows 本地运行同脚本时会明确跳过 Linux 归档验收；本机二进制启动回归由 `tests/startup_validation.py` 负责。

无网络构建需要提前准备与 1.98.1 匹配的完整工具链、target、链接器及依赖缓存，或自行执行 `cargo vendor --locked` 并带上其输出的 Cargo 源替换配置后使用 `cargo build --locked --offline`。当前发布流程不生成离线源码包，也不承诺某个固定依赖数量。musl 静态属性检查不能替代目标系统运行验收。

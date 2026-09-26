# CentOS 部署手册（rust-trader exchange）

二进制为 **musl 静态链接**，无 glibc 依赖，CentOS 7 / Stream 8 / Stream 9 通用（也适用于其他 x86_64 Linux）。

## 包内容

```
rust-trader-<版本>-linux-x86_64-musl/
├── bin/                          # musl 静态二进制
│   ├── exchange                  #   交易所主程序
│   ├── client                    #   自研客户端
│   └── playback                  #   报价回放
├── configs/                      # *.example 配置模板（qf_exchange_settings / qf_connector_settings / instruments.txt）
├── examples/playback.txt         # 回放样例
├── systemd/rust-trader.service   # systemd 服务文件
├── SHA256SUMS / BUILD-INFO.json  # 包内文件校验与构建信息
├── LICENSE
└── DEPLOY.md                     # 本手册
```

## 部署步骤

```bash
# 1. 创建运行用户（家目录即部署目录）
useradd -r -m -d /home/rust-trader -s /sbin/nologin rusttrader

# 2. 解压到 /home/rust-trader
mkdir -p /home/rust-trader
tar xzf rust-trader-*-linux-x86_64*.tar.gz -C /home/rust-trader --strip-components=1
chown -R rusttrader:rusttrader /home/rust-trader

# 3. 首次安装：从 .example 生成配置（已有配置保持原样）
cd /home/rust-trader/configs
cp -n qf_exchange_settings.example qf_exchange_settings
cp -n instruments.txt.example instruments.txt

# 4. 安装并启动服务
cp /home/rust-trader/systemd/rust-trader.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now rust-trader

# 5. 开放端口（FIX 5001 / REST 8080）
firewall-cmd --permanent --add-port=5001/tcp --add-port=8080/tcp && firewall-cmd --reload

# 6. 验证
curl http://localhost:8080/api/instruments/
journalctl -u rust-trader -f
```

服务文件使用 `exchange --server`，关闭标准输入不会导致退出。该模式不自行 fork 或脱离进程，由 systemd 管理启动和停止；手工交互运行仍可使用不带 `--server` 的命令。更新旧部署时需同时替换服务文件并执行 `systemctl daemon-reload`，然后重启服务。

部署在家目录时，SELinux enforcing 的系统（如 CentOS Stream）可能拒绝 systemd 从 `/home` 执行二进制：若启动报 `Permission denied`，执行 `chcon -t bin_t /home/rust-trader/bin/*` 后重启服务即可。

## 运行时目录与日志

| 路径 | 内容 |
|---|---|
| `/home/rust-trader/bin/exchange` | 主程序（包内二进制位于 `bin/` 目录） |
| `/home/rust-trader/configs/` | 配置（改 `TargetCompIDs` 准入白名单、`Logging` 日志开关后需重启） |
| `/home/rust-trader/logs/exchange/` | FIX 会话日志（messages/event，每接入会话一对文件，行前缀北京时间） |

## 准入控制

`configs/qf_exchange_settings` 的 `TargetCompIDs` 声明允许接入的客户端 CompID（如 `CLIENT,PLAYBACK`）；`DynamicSessions=Y` 可恢复"任意接入"模式。未声明客户端在 Logon 阶段即被拒绝。

## 停止 / 升级

```bash
systemctl stop rust-trader      # 停止
# 升级：替换 exchange 二进制后
systemctl restart rust-trader
```

注意：交易所状态为**内存态，无持久化**——重启即清零（品种、订单、统计）。

## 内网（离线）构建

目标机无外网时，用离线源码包 `rust-trader-offline-src.tar.gz`（在有网机器上 `cargo vendor` 生成，内含全部 105 个依赖源码与 `.cargo/config.toml`，已验证可纯离线编译）：

```bash
tar xzf rust-trader-offline-src.tar.gz
cd rust-trader
cargo build --release --offline          # 不访问 crates.io
./target/release/exchange                # 本机(gnu)构建，产物直接可跑
```

前置：rustc ≥ 1.74（rustup 安装）、gcc（链接器）。**离线机器建议直接本机 gnu 构建**——目标机就是部署机时 glibc 天然匹配；musl 静态需要 rustup 另行下载 target 组件，内网拿不到，除非从有网机器拷贝 `~/.rustup/toolchains/<版本>/lib/rustlib/x86_64-unknown-linux-musl/` 目录（须版本完全一致）。

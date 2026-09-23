# CentOS 部署手册（rust-trader exchange）

二进制为 **musl 静态链接**，无 glibc 依赖，CentOS 7 / Stream 8 / Stream 9 通用（也适用于其他 x86_64 Linux）。

## 包内容

```
rust-trader-<版本>-linux-x86_64/
├── exchange                      # 交易所主程序（musl 静态二进制）
├── configs/                      # FIX 配置、品种表、回放样例
│   ├── qf_exchange_settings           #   acceptor：端口/CompID/准入白名单 TargetCompIDs/Logging
│   ├── instruments.txt           #   品种表
│   └── ...
├── systemd/rust-trader.service   # systemd 服务文件
└── DEPLOY.md                     # 本手册
```

## 部署步骤

```bash
# 1. 创建运行用户
useradd -r -s /sbin/nologin rusttrader

# 2. 解压到 /opt/rust-trader
mkdir -p /opt/rust-trader
tar xzf rust-trader-*-linux-x86_64.tar.gz -C /opt/rust-trader --strip-components=1
chown -R rusttrader:rusttrader /opt/rust-trader

# 3. 安装并启动服务
cp /opt/rust-trader/systemd/rust-trader.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now rust-trader

# 4. 开放端口（FIX 5001 / REST 8080）
firewall-cmd --permanent --add-port=5001/tcp --add-port=8080/tcp && firewall-cmd --reload

# 5. 验证
curl http://localhost:8080/api/instruments/
journalctl -u rust-trader -f
```

服务文件使用 `exchange --server`，关闭标准输入不会导致退出。该模式不自行 fork 或脱离进程，由 systemd 管理启动和停止；手工交互运行仍可使用不带 `--server` 的命令。更新旧部署时需同时替换服务文件并执行 `systemctl daemon-reload`，然后重启服务。

## 运行时目录与日志

| 路径 | 内容 |
|---|---|
| `/opt/rust-trader/exchange` | 主程序 |
| `/opt/rust-trader/configs/` | 配置（改 `TargetCompIDs` 准入白名单、`Logging` 日志开关后需重启） |
| `/opt/rust-trader/logs/exchange/` | FIX 会话日志（messages/event，每接入会话一对文件，行前缀北京时间） |

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

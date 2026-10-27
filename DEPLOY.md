# 部署与配置步骤

以下以专属用户 `rusttrader` 为例，家目录为 `/home/rusttrader`，安装目录为 `/home/rusttrader/rust-trader`。使用其他用户名时，同步替换命令中的用户名、组名及 `/home/rusttrader` 路径。系统需有 Bash、tar、gzip 和 `flock`。

## 1. 创建专属用户

以有 sudo 权限的管理员执行：

```bash
sudo useradd --create-home --user-group --home-dir /home/rusttrader --shell /bin/bash rusttrader
```

## 2. 将安装文件交给专属用户

管理员进入已上传安装文件的目录，执行：

```bash
sudo install -o rusttrader -g rusttrader -m 0644 \
  rust-trader-dev-d1f0657-10-linux-x86_64-musl.tar.gz \
  /home/rusttrader/
sudo -iu rusttrader
```

切换后，以下解压、配置及普通启停命令均由 `rusttrader` 执行。

## 3. 在家目录下解压安装

```bash
cd /home/rusttrader
mkdir -p /home/rusttrader/rust-trader
tar -xzf /home/rusttrader/rust-trader-dev-d1f0657-10-linux-x86_64-musl.tar.gz \
  --strip-components=1 -C /home/rusttrader/rust-trader
cd /home/rusttrader/rust-trader
```

后续配置与启停命令均在 `/home/rusttrader/rust-trader` 中执行。文件由执行解压的 `rusttrader` 用户拥有，可直接修改 `configs/`。

## 4. 配置运行参数

编辑 `configs/runtime.env`：

```bash
vi configs/runtime.env
```

按需要修改：

```ini
REST_PORT=8080
RUST_LOG=info
APP_LOG_PATH=logs/app
APP_LOG_RETENTION_DAYS=14
FIX_LOG_RETENTION_DAYS=14
START_TIMEOUT=15
STOP_TIMEOUT=15
```

- `REST_PORT`：REST 端口。
- `RUST_LOG`：运行日志级别，日常用 `info`，排查问题时可改为 `debug`。
- `APP_LOG_PATH`：运行日志目录；相对路径以安装目录为基准。
- 两个 `RETENTION_DAYS`：对应日志保留天数，`0` 表示不自动清理。
- `START_TIMEOUT`、`STOP_TIMEOUT`：启动和停止等待秒数，填写正整数。

每行使用 `KEY=value`，不加引号，不使用变量展开。已有同名环境变量会覆盖文件值。

## 5. 配置交易品种

编辑 `configs/instruments.txt`：

```bash
vi configs/instruments.txt
```

每行填写一个数字 ID 和品种代码，以空格分隔，例如：

```text
1 IBM
2 AAPL
3 AMZN
```

ID 必须是正整数；ID 和品种代码分别保持唯一。至少保留一个有效品种；以 `#` 或 `//` 开头的行是注释。

## 6. 配置 FIX 接入

编辑 `configs/qf_exchange_settings`：

```bash
vi configs/qf_exchange_settings
```

在 `[DEFAULT]` 中按需要调整日志位置，保留以下运行设置：

```ini
[DEFAULT]
ConnectionType=acceptor
BeginString=FIX.4.2
HeartBtInt=30
ResetOnLogout=Y
ResetOnDisconnect=Y
PersistMessages=N
UseDataDictionary=Y
Logging=Y
FileLogPath=logs/exchange
DynamicSessions=N
```

在每个 `[SESSION]` 中设置服务端身份、允许连接的客户端身份和监听端口，例如：

```ini
[SESSION]
SenderCompID=GOX
TargetCompID=CLIENT
SocketAcceptPort=5001
```

- `SenderCompID`：本服务的 FIX 身份；所有会话使用相同值。
- `TargetCompID`：允许连接的客户端身份，应与对端发送的 `SenderCompID` 一致。
- `SocketAcceptPort`：FIX 监听端口；修改时同步修改所有会话。
- 新增客户端时，添加一个 `[SESSION]`；删除不使用的会话。
- `Logging=Y` 开启 FIX 原文日志，`N` 关闭；`FileLogPath` 是 FIX 日志目录。

确保目标机器网络允许实际使用的 FIX 和 REST 端口。

使用自带的 client 或 playback 时，再编辑 `configs/qf_connector_settings`，将 `SocketConnectHost`、`SocketConnectPort` 设置为服务地址和 FIX 端口，并确保双方 CompID 对应。

## 7. 检查配置并启动

```bash
./traderctl check
./traderctl start
./traderctl status
curl http://127.0.0.1:8080/api/instruments/
```

修改过 `REST_PORT` 时，将查询地址中的 `8080` 换成实际端口。若检查或启动失败，先查看安装目录下的日志：

```text
logs/app/                         运行日志
logs/exchange/YYYY-MM-DD/         FIX 会话日志
```

修改日志路径后，按配置中的位置查看。

## 8. 停止、重启及修改配置

```bash
./traderctl stop
./traderctl restart
```

修改配置后重启生效；需要修改多个文件时，可先停止，编辑完成后执行 `check` 和 `start`。停止或重启会清空内存中的订单状态。

## 9. 可选：使用 systemd 托管

先在 `rusttrader` 用户的安装目录中停止后台实例并生成服务文件：

```bash
./traderctl stop
./traderctl systemd-unit > systemd/rust-trader.service
exit
```

`exit` 返回之前的管理员终端。由管理员安装并启动服务，服务进程仍以 `rusttrader` 运行：

```bash
sudo install -m 644 /home/rusttrader/rust-trader/systemd/rust-trader.service /etc/systemd/system/rust-trader.service
sudo systemctl daemon-reload
sudo systemctl enable --now rust-trader
sudo systemctl status rust-trader
```

托管后由管理员使用以下命令启停，无需给 `rusttrader` 授予 sudo 权限：

```bash
sudo systemctl stop rust-trader
sudo systemctl start rust-trader
sudo systemctl restart rust-trader
```

选择 systemd 后，日常启停统一使用 `systemctl`。

# 部署、启停、配置、日志与 FIX 接入兼容性审查

审查日期：2026-10-26。源码基线：`5940c0e9e2fdc962b5b731814491c58694d1e1cd`。

整改跟进（2026-10-27）：已按 D1–D7 修改实现、配置、打包入口和相关文档；Windows 本地统一回归通过。下文保留原审查时的事实和建议，当前实现及验证边界见 [整改与统一验证记录](deployment-remediation-2026-10-27.md)。Linux 实包部署、systemd 和目标系统兼容性尚未完成运行验收。

目标：普通部署用户在自己有权限的目录解压，修改配置后即可启动，并能方便地停止、查看状态和定位日志；外部 FIX 接入默认容忍未使用的扩展字段。本次仅维护本报告，没有修改程序、配置、打包脚本或服务文件。

## 结论

现有包的 `bin/`、`configs/`、校验文件和构建信息是可保留的基础，但目前主要支持管理员安装 systemd 服务，尚未完整支持用户期望的自包含部署流程。优先解决品种配置失败仍启动的问题，然后补齐可移动路径、普通用户启停入口、有效配置和运行日志。FIX 字段兼容策略也需由当前严格白名单改为默认提取所需字段、忽略未使用的扩展字段，具体边界见 D7。

家目录部署可以作为用户级自包含部署方案。`/home/rust-trader` 本身不是本次问题的核心，固定用户、固定绝对路径、依赖调用者当前目录才是可移植性障碍。FHS 区分用户家目录与 `/opt` 系统级附加软件布局，并明确不应假定家目录的具体位置；本报告建议的是符合本项目使用目标的用户级目录约定，不宣称它等同于完整的系统级 FHS 安装。[FHS 3.0](https://www.debian.org/doc/packaging-manuals/fhs/fhs-3.0.html)

## 发现

### D1 · P1 · 品种配置加载失败后仍对外提供服务

- 位置：`src/bin/exchange.rs:76–78`、`155–168`；`src/core/instrument.rs:58–76`。
- 缺少或无法读取 `instruments.txt` 时，程序仅打印错误，随后照常启动 REST 和 FIX。部署手册用 REST 请求验证服务，但此时 REST 仍返回成功，不能据此判断品种配置已加载。
- 本次本地复现：指定不存在的品种文件，进程持续存活，FIX TCP 端口可连接，`/api/instruments/` 返回 HTTP 200 和 `[]`。
- 另经源码检查，品种文件列数不正确的行会被跳过，非法数字 ID 用 `0` 代替。当前配置检查不足以保证用户编辑后的品种文件有效；这部分没有另做运行复现。
- 建议：品种文件读取、格式和唯一性校验失败时，在开放监听端口前返回非零退出码；除非显式支持空品种模式，否则拒绝空品种配置。启动成功应包含配置加载成功和两个监听器均可用。

### D2 · P2 · 默认部署流程需要管理员，路径及用户写死

- 位置：`DEPLOY.md:24–43`；`deploy/rust-trader.service:8–11`；`src/bin/exchange.rs:11–28`。
- 手册要求 `useradd`、`chown`、写入 `/etc/systemd/system`、系统级 `systemctl`。服务固定使用 `rusttrader` 用户/组和 `/home/rust-trader` 路径。
- 解压到已有部署用户的任意目录后，不能直接复用该服务文件。程序本身允许普通用户运行，但没有对应的自包含启停入口。
- 默认配置路径、相对 `FileLogPath` 都相对于进程工作目录，而非二进制所在目录。从其他目录使用绝对路径调用二进制，仍可能找不到配置或把日志写到意外位置。
- 建议：以启动入口所在位置定位安装根目录，统一使用解析后的路径；以当前部署用户运行。systemd 作为可选托管方式，安装时生成实际路径与用户。

### D3 · P2 · 按手册安装会产生 root 拥有的有效配置

- 位置：`DEPLOY.md:30–35`；`scripts/package-linux.sh:62`。
- 按现有管理员流程执行，先 `chown -R`，再以 root 使用 `cp -n` 从模板创建有效配置。首次安装的新文件归 root 所有，普通部署用户对这些文件的常规原地写入会被拒绝。
- 因目录归部署用户所有，用户可能通过替换文件绕过文件级限制，但这不等同于配置正确归属部署用户，也不符合“修改配置即可启动”的正常操作方式。
- 建议：由部署用户完成解压和配置创建；管理员安装路径也应在生成配置后设置正确归属。该发现来自命令顺序和权限语义，本次未在 Linux 上执行安装。

### D4 · P2 · 包内模板、程序默认路径与文档流程尚未衔接完整

- 位置：`scripts/package-linux.sh:32–35`；`DEPLOY.md:32–35`；`src/bin/client.rs:53`；`src/bin/playback.rs:52–64`。
- 包内只有 `.example` 配置，程序默认读取无 `.example` 后缀的路径。仅修改模板内容不会使默认启动读取它，必须先复制或显式传 `-fix`。
- 手册只生成 exchange 与 instruments 配置，没有生成 `qf_connector_settings`，因此附带的 client/playback 仍不能按默认参数直接运行。
- 回放样例被打包到 `examples/playback.txt`，playback 默认读取 `configs/playback.txt`，需要传 `-file examples/playback.txt` 或统一布局。
- 建议：首次安装提供明确可编辑的有效配置；保留默认模板用于对照。升级流程只更新程序和模板，明确保留已有有效配置，不能用覆盖解压代替配置合并。

### D5 · P2 · 运行日志缺少用户级部署的统一落盘入口

- 位置：`src/bin/exchange.rs:44`、`76–87`、`156–160`；`deploy/rust-trader.service`；`src/fix/log.rs:77–84`、`184–189`。
- 运行诊断日志默认 `info`，可用 `RUST_LOG` 覆盖；默认输出到 stderr。systemd 通常由 journal 接收 stdout/stderr，程序本身没有独立的运行日志文件配置。
- 部分启动信息和错误直接用 `println!/eprintln!`，绕过日志级别和统一时间格式。本次验证 `RUST_LOG=off` 后，仍输出 `fill policy: Real` 和 REST 启动提示。
- `Logging=Y/N` 是 FIX 文件日志开关，不是运行日志级别。FIX 原文日志也不应随运行诊断级别下降而被过滤。
- 上一轮确认的 FIX 日志问题仍存在：没有按日切分，时间固定 UTC+8，命名及行格式未按用户要求对齐 QuickFIX/J。长期运行会持续追加同一组文件。
- 建议：把运行级别、运行日志位置、FIX 日志位置明确写入部署配置和手册；普通用户启动时自动创建日志目录并检查可写性。运行日志和 FIX 日志分别管理，按本地日期分文件，保留周期可配置。

### D6 · P2 · 发布验证没有覆盖真实解压部署，兼容性表述过强

- 位置：`tests/deployment_smoke.py:15`、`45`、`77–103`；`.github/workflows/release.yml:26–43`；`scripts/package-linux.sh:77`；`DEPLOY.md:3`、`78–87`。
- 当前部署测试运行仓库内的 `target/debug/exchange`，使用生成的配置和仓库工作目录。归档部分只提取脚本中的 tar 命令，手工构造带两个文件的测试目录，没有完整执行打包脚本或验证解压后的配置入口。
- 发布工作流执行 musl 测试、构建和打包，但没有从生成的真实归档中运行“解压、修改配置、启动、停止”的验证。
- 工作流和 BUILD-INFO 明确标注候选包、RHEL 7.6 待验收；部署手册却直接写 CentOS 7/Stream 8/9 通用。静态链接检查只能证明相关链接属性，不能替代目标系统运行证据。
- 离线构建说明写 `rustc >= 1.74`，当前仓库工具链固定为 `1.98.1`。仅准备旧版本 Rust 不能按现有 rustup 固定工具链流程离线构建；仓库当前发布流程也没有生成手册提及的离线源码包。
- 建议：增加真实发布归档在任意路径、普通用户、无源码工作目录下的验收；Linux 目标兼容性保持“待验收”直到有对应产物验证记录，并同步工具链说明。

### D7 · P2 · FIX 严格字段白名单阻碍外部系统接入；默认兼容模式已确认

**用户确认的整改要求：默认支持“只取需要的字段，忽略未使用的扩展字段”的兼容模式，无需额外开启。此处记录的是目标行为，当前代码尚未实现。**

当前证据：

- `src/fix/codec.rs:499–585` 在提取业务字段之前校验字段白名单；未知字段和未列入白名单的标准字段都会导致整个消息解码失败。
- 例如，新单 `35=D` 增加 `9001=abc`，返回 `unsupported tag 9001 for message D`；增加当前不使用的 `Account(1)` 也会被拒绝。
- `src/fix/session.rs:1040–1047` 对已登录会话中的此类错误发送 `Reject(35=3)`，不进入业务处理。首次登录的字段校验错误走 `src/fix/session.rs:841–854` 的握手失败路径，通常发送 Logout 并断开。
- 当前没有忽略未知字段的配置开关；`src/fix/config.rs:416–430` 要求使用内置字典规则，不支持通过关闭字典校验或替换外部 XML 达到该目的。

默认兼容模式的验收约定：

1. 对已支持消息，解析并使用当前业务需要的字段。新增的合法单值自定义字段、当前未消费的标准可选字段，在能够正确识别边界和所属层级时默认忽略；不因这些字段单独拒绝消息。适用范围包括首次 Logon、登录后的会话/业务消息，以及随附 initiator 接收的已支持回报。
2. 已支持字段仍按原有要求校验：必填字段不能缺失，字段类型、值域、枚举和条件必填规则继续生效。非法 `OrdType(40)`、当前不支持的 `TimeInForce(59)` 等不能通过忽略字段绕过。
3. 保留 BeginString、BodyLength、CheckSum、核心会话身份、序号、重复单值字段和已支持重复组结构校验。不能安全识别的未知重复组或长度限定数据字段应明确拒绝并说明原因；不能先全局删除未知 tag 再解析，以免改变组结构或字段归属。
4. FIX 原始日志完整保留收到的字段和原始报文；业务层忽略字段不等于改写线路报文，也不意味着实现了对应账户、路由或其他业务语义。
5. 此模式不增加消息类型或扩展现有业务能力。`c`、`H`、`V` 等未实现消息继续明确拒绝；MassQuote 仍按当前单组、单条目能力处理。
6. 如保留严格白名单模式，必须显式选择；未配置兼容选项时采用上述默认兼容行为。具体配置键尚未设计，不在现有配置中添加未实现的开关。

后续实现必须覆盖的验证：

- 合法新单增加 `9001`、`9002` 或未消费的标准可选字段后，业务处理结果与不带这些字段的基线一致，日志仍保留扩展字段。
- 首次 Logon、撤单、改单、报价和已支持回报携带可忽略扩展字段时均能正常处理；不能只放宽新单这一条路径。
- 扩展字段出现在合法位置时，不改变已支持字段的解释；未知复杂组或数据字段不可安全跳过时，应得到明确拒绝。
- 缺少必填字段、已支持字段取值非法、重复单值字段、错误 BodyLength/CheckSum 和错误组结构仍被拒绝。
- 未配置选项时即使用兼容模式；如实现严格模式，其显式开启后的拒绝行为另行验证。

上一轮已通过当前实现的 6 项 codec 单元测试，这只能证明现有严格规则的部分行为；本节新增兼容模式尚未实现，不能记作已通过验收。本次仅追加评审要求，未修改运行时默认值。

## 启停能力与边界

`exchange --server` 是前台服务进程，不读 stdin，不自行转入后台；现有本地测试确认关闭 stdin 不会结束服务。普通用户从安装根目录显式传入有效配置即可前台运行，后台化、PID 管理和状态查询目前由 systemd 或使用者自行承担。

`systemctl stop` 可以停止服务，即使没有 `ExecStop`，systemd 默认也会发送 SIGTERM；不能把缺少 `ExecStop` 视为无法停止。[systemd.kill](https://www.freedesktop.org/software/systemd/man/latest/systemd.kill.html)

目前代码没有应用层 SIGTERM 处理和停止状态机：`src/bin/exchange.rs:164–168` 直接进入 `src/fix/session.rs:801–823` 的接入循环，没有停止接入、等待回报队列、发送 FIX Logout、等待工作线程退出的协调步骤。因此当前停止属于进程终止，不能称为已实现优雅停机。本次未在 Linux/systemd 环境实测信号行为。

交易状态为内存态，停止或重启会丢失当前订单等运行状态。优雅停机也不会自动解决持久化问题。手册已经提示该限制，应保留。

## 当前配置和日志位置

以下相对路径以进程工作目录为基准，systemd 示例将其设为 `/home/rust-trader`。

| 内容 | 当前入口 / 位置 | 说明 |
|---|---|---|
| exchange FIX 配置 | `configs/qf_exchange_settings`；可用 `-fix` 覆盖 | 包内为 `.example`，需创建有效文件 |
| 品种配置 | `configs/instruments.txt`；可用 `-instruments` 覆盖 | 加载错误仍会启动，见 D1 |
| FIX 端口 | 配置内 `SocketAcceptPort=5001` | 配置值已校验，端口冲突会启动失败 |
| REST 端口 | 默认 `8080`，通过 `-port` 覆盖 | 不在现有 FIX 配置文件中；服务文件未提供 EnvironmentFile |
| 运行日志级别 | 默认 `info`；环境变量 `RUST_LOG` | 不控制 `println!` 或 FIX 原文文件日志 |
| 运行日志目录 | 程序未定义独立目录 | 控制台 / stderr；systemd 示例使用 journal；手工后台运行需自行重定向 |
| exchange FIX 日志 | 示例 `FileLogPath=logs/exchange` | 会话到来时才创建文件，不是启动时完成全部目录检查 |
| client FIX 日志 | 示例 `FileLogPath=logs/client` | 按会话分别命名 |
| playback FIX 日志 | 使用随包 connector 配置时也是 `logs/client` | `logs/playback` 只是未配置 FileLogPath 时的代码默认值；README 此处说明不完整 |

## 建议的最小交付形态（尚未实现）

保持自包含目录，以部署用户选择的路径为根，例如 `~/apps/rust-trader`：

```text
rust-trader/
├── bin/                         # exchange / client / playback
├── configs/
│   ├── runtime.env              # 新增：运行日志级别、REST 端口等
│   ├── qf_exchange_settings
│   ├── qf_connector_settings
│   └── instruments.txt
├── defaults/                    # 用于首次配置和升级对照的模板
├── examples/playback.txt
├── logs/
│   ├── app/                     # 按本地日期保存运行日志
│   └── fix/YYYY-MM-DD/           # 按会话拆分 messages.log / event.log
├── run/                         # PID 和运行锁；自动创建
├── traderctl                    # 新增：start / stop / restart / status / check
├── systemd/                     # 可选托管模板
├── SHA256SUMS
├── BUILD-INFO.json
└── DEPLOY.md
```

目录职责比 `configs` 是否改名为 `conf` 更重要，建议保留现有名字降低迁移成本。`logs`、`run` 是安装根目录下的私有子目录，不能误写到系统 `/run`。固定家目录和固定用户名应从默认用户级流程移除。

目标操作体验：解压到用户选择的目录，编辑 `configs/`，执行 `./traderctl start`。`start` 应完成路径解析、配置及权限检查、重复启动保护、后台进程启动和就绪检查；`stop` 只操作本实例，经 PID/进程身份验证后发送终止信号并等待退出；`status` 应识别失效 PID。systemd 托管与自带后台模式应择一使用，避免双方同时管理同一实例。

日志沿用用户已明确的方向：FIX 每行采用 QuickFIX/J FileLog 风格前缀，但时间取本地时区；按同一本地日期切分，报文字段 52/60 继续按协议采用 UTC。运行诊断默认 `info`，临时排障可调 `debug`；FIX 原文保留独立开关和保留策略。

上述目录、`runtime.env`、`defaults/` 和 `traderctl` 是建议目标，当前包里不存在，不能作为现有启动命令使用。

## 本次验证

- 通过：`cargo build --locked --offline --bin exchange`，Windows 本机 debug 构建。
- 通过：`python tests/deployment_smoke.py`，验证 server 模式不受 stdin EOF 影响、交互 EOF 退出、简化归档路径。
- 已复现：缺失品种文件仍提供 HTTP 200 空列表和 FIX TCP 监听。
- 已复现：`RUST_LOG=off` 仍保留直接打印的启动信息。
- 未进行：真实 Linux musl 包构建/解压运行、普通 Linux 用户权限验证、systemd 启停、SELinux、跨日轮转、目标 RHEL/CentOS 验收。
- 复现结果：[results.json](../../target/test-runs/deployment-review/1793024213135733300/results.json)。复现只请求本地 REST 和建立无报文的 TCP 连接，没有提交交易或回放行情；所启动进程已退出。

建议修复顺序：先使配置错误明确阻止启动，再补齐普通用户部署入口、配置衔接、日志落盘和 D7 默认 FIX 字段兼容模式，最后完成协议回归及真实归档的部署、启停验收。

# FIX 4.2 verification

Run from the project root:

```text
cargo test --locked --offline
cargo build --locked --offline
python tests/fix42_wire.py
```

`fix42_wire.py` uses Python's standard library and the unmodified official
QuickFIX dictionary in `fixtures/FIX42.xml`. It does not use this project's
encoder or decoder to construct/validate its peer messages.

The harness starts the built exchange on loopback ports selected by the OS,
keeps stdin open, and terminates that process after testing. It then acts as a
mock acceptor for the real client and playback binaries. Generated settings,
playback input, and captured process output remain in `target/fix42-wire/`.
No existing settings or logs are overwritten.

Checks cover required fields, enum values, repeating-group ordering, wire
BodyLength/CheckSum, standard market/limit mapping, string ClOrdID, partial-fill
replacement/cancellation quantities, average price, execution ID uniqueness,
reject responses, sequence recovery, heartbeats, and Logout.

This is a regression harness for the supported profile, not FIX certification
or an implementation of every conditional rule in the complete dictionary.

## 部署缺陷回归

运行 `cargo build --bin exchange` 后执行 `python tests/deployment_smoke.py`。
检查 `--server` 在 stdin EOF 下保持 REST/FIX 可用，默认交互模式 EOF 正常退出，以及 systemd 启动参数。
还会使用工作流中的实际 tar 命令打包本地 fixture，验证生成路径与 artifact/release 匹配；这不替代 Linux musl 构建、systemd 或 GitHub Actions 实跑。测试文件保存在 `target/deployment-smoke/`。

## 性能与队列回归

`cargo test` 包含有界队列溢出、关闭 TCP 连接、盘口增量总量/索引一致性及 REST 快照独立性检查。
`cargo run --release --example book_bench` 测量 20000 订单的插入、500 次盘口快照及逆序撤单。方法、前后结果与限制见 [性能优化验证](../docs/performance.md)。

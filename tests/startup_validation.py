"""Startup failure/readiness regression. Run after building all binaries."""
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "target/test-runs/startup-validation" / str(time.time_ns())
OUT.mkdir(parents=True)
EXE = ROOT / "target/debug" / ("exchange.exe" if os.name == "nt" else "exchange")
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def config(name, fix_port, log_dir=None):
    path = OUT / f"{name}.cfg"
    path.write_text(f"""[DEFAULT]
ConnectionType=acceptor
BeginString=FIX.4.2
SenderCompID=GOX
SocketAcceptPort={fix_port}
HeartBtInt=30
ResetOnLogout=Y
ResetOnDisconnect=Y
PersistMessages=N
UseDataDictionary=Y
Logging={'Y' if log_dir else 'N'}
{f'FileLogPath={log_dir}' if log_dir else ''}
[SESSION]
TargetCompID=CLIENT
""", encoding="utf-8")
    return path


def environment(name, **extra):
    return dict(os.environ, APP_LOG_PATH=str(OUT / name / "app"), **extra)


def args(cfg, rest, instruments=None, ready=None):
    values = [str(EXE), "--server", "-fix", str(cfg), "-port", str(rest)]
    if instruments:
        values += ["-instruments", str(instruments)]
    if ready:
        values += ["--ready-file", str(ready)]
    return values


def closed(number):
    with socket.socket() as sock:
        sock.settimeout(.5)
        assert sock.connect_ex(("127.0.0.1", number)) != 0, f"unexpected listener on {number}"


results = []
for index, content in enumerate([None, "", "# empty\n", "bad IBM\n", "0 IBM\n",
                                 "1 IBM extra\n", "1 IBM\n1 AAPL\n", "1 IBM\n2 IBM\n"]):
    name = f"bad-instruments-{index}"
    fix, rest = port(), port()
    instrument = OUT / f"{name}.txt"
    if content is not None:
        instrument.write_text(content, encoding="utf-8")
    ready = OUT / f"{name}.ready"
    result = subprocess.run(args(config(name, fix), rest, instrument, ready), cwd=OUT,
                            env=environment(name), capture_output=True, timeout=10)
    assert result.returncode != 0, name
    assert b"instruments" in result.stderr, result.stderr
    assert not ready.exists(), name
    closed(fix)
    closed(rest)
    results.append(name)

# --check validates logs/configs while neither port is opened.
fix, rest = port(), port()
cfg = config("check", fix)
result = subprocess.run(args(cfg, rest) + ["--check"], cwd=OUT,
                        env=environment("check"), capture_output=True, timeout=10)
assert result.returncode == 0, result.stderr
closed(fix)
closed(rest)
results.append("check-without-listeners")
result = subprocess.run([str(EXE), "--check"], cwd=OUT, env=environment("default-configs"),
                        capture_output=True, timeout=10)
assert result.returncode == 0, result.stderr
results.append("foreign-cwd-default-config-paths")

blocked = OUT / "not-a-log-directory"
blocked.write_text("file", encoding="utf-8")
for name, env, cfg in [
    ("app-log-unwritable", dict(os.environ, APP_LOG_PATH=str(blocked)), config("app-unwritable", port())),
    ("fix-log-unwritable", environment("fix-unwritable"), config("fix-unwritable", port(), blocked)),
]:
    result = subprocess.run(args(cfg, port()) + ["--check"], cwd=OUT,
                            env=env, capture_output=True, timeout=10)
    assert result.returncode != 0, name
    results.append(name)

for conflict in ["FIX", "REST"]:
    with socket.socket() as held:
        if os.name == "nt":
            held.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        held.bind(("0.0.0.0", 0))
        held.listen()
        fix, rest = port(), port()
        if conflict == "FIX": fix = held.getsockname()[1]
        else: rest = held.getsockname()[1]
        ready = OUT / f"conflict-{conflict}.ready"
        result = subprocess.run(args(config(f"conflict-{conflict}", fix), rest, ready=ready), cwd=OUT,
                                env=environment(f"conflict-{conflict}"), capture_output=True, timeout=10)
        assert result.returncode != 0, result.stderr
        assert not ready.exists()
        closed(rest if conflict == "FIX" else fix)
        results.append(f"{conflict}-port-conflict-no-readiness")

# Absolute invocation outside the source cwd still resolves default instruments.
fix, rest = port(), port()
ready = OUT / "live.ready"
with (OUT / "live.log").open("wb") as output:
    proc = subprocess.Popen(args(config("live", fix), rest, ready=ready), cwd=OUT,
                            env=environment("live", RUST_LOG="off"), stdin=subprocess.DEVNULL,
                            stdout=output, stderr=output)
    try:
        deadline = time.monotonic() + 10
        while not ready.exists():
            assert proc.poll() is None, "server exited before readiness"
            assert time.monotonic() < deadline, "readiness timed out"
            time.sleep(.05)
        # The file is created before writing, so wait for the PID write too.
        while not ready.read_text().strip():
            assert time.monotonic() < deadline
            time.sleep(.01)
        assert int(ready.read_text()) == proc.pid
        with OPENER.open(f"http://127.0.0.1:{rest}/api/instruments/", timeout=2) as response:
            assert "IBM" in json.load(response)
        with socket.create_connection(("127.0.0.1", fix), timeout=2): pass
        time.sleep(.1)
        assert proc.poll() is None, "stdin EOF ended service"
    finally:
        if proc.poll() is None:
            proc.terminate()
        proc.wait(timeout=5)
        ready.unlink(missing_ok=True)
assert (OUT / "live.log").read_bytes() == b"", "RUST_LOG=off must suppress startup diagnostics"
results.append("foreign-cwd-default-instruments-ready-stdin-eof-log-off")
(OUT / "results.json").write_text(json.dumps({"passed": results}, indent=2), encoding="utf-8")
print(f"PASS: {len(results)} startup checks; artifacts: {OUT}")

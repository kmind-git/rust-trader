"""Run after cargo build --bin exchange. Local service and archive smoke checks."""
import json
import os
from pathlib import Path
import shlex
import socket
import subprocess
import tarfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "target" / "deployment-smoke" / str(time.time_ns())
OUT.mkdir(parents=True)
EXE = ROOT / "target/debug" / ("exchange.exe" if os.name == "nt" else "exchange")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def launch(server):
    fix_port, rest_port = free_port(), free_port()
    while fix_port == rest_port:
        rest_port = free_port()
    cfg = OUT / f"{rest_port}.cfg"
    cfg.write_text(f"""[DEFAULT]
ConnectionType=acceptor
BeginString=FIX.4.2
SenderCompID=GOX
SocketAcceptPort={fix_port}
ResetOnLogout=Y
ResetOnDisconnect=Y
PersistMessages=N
Logging=N
[SESSION]
TargetCompID=CLIENT
""")
    args = [str(EXE), "-fix", str(cfg), "-port", str(rest_port)]
    if server:
        args.append("--server")
    with (OUT / f"{rest_port}.log").open("w") as log:
        proc = subprocess.Popen(args, cwd=ROOT, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        try:
            if not server:
                assert proc.wait(timeout=10) == 0
                return
            deadline = time.monotonic() + 10
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            while True:
                assert proc.poll() is None, "server exited with stdin closed"
                try:
                    with opener.open(f"http://127.0.0.1:{rest_port}/api/instruments/", timeout=.5) as response:
                        assert response.status == 200
                        json.load(response)
                    with socket.create_connection(("127.0.0.1", fix_port), timeout=.5):
                        pass
                    break
                except OSError:
                    assert time.monotonic() < deadline, "listeners did not start"
                    time.sleep(.05)
            time.sleep(.25)
            assert proc.poll() is None
        finally:
            if proc.poll() is None:
                proc.terminate()
                proc.wait(timeout=5)


launch(True)
launch(False)
assert "ExecStart=/opt/rust-trader/exchange --server" in (ROOT / "deploy/rust-trader.service").read_text()
print("PASS: --server survives stdin EOF with REST/FIX available; interactive EOF exits")

# Exercise the workflow's actual tar command with a local fixture, not a Linux build.
workflow = (ROOT / ".github/workflows/release.yml").read_text()
name = "rust-trader-smoke-linux-x86_64"
stage = OUT / "dist" / name
(stage / "systemd").mkdir(parents=True)
(stage / "exchange").write_text("packaging fixture")
(stage / "systemd/rust-trader.service").write_bytes((ROOT / "deploy/rust-trader.service").read_bytes())
command = next(line.strip() for line in workflow.splitlines() if line.strip().startswith("tar czf "))
subprocess.run(shlex.split(command.replace("${D}", name)), cwd=OUT, check=True)
pattern = next(line.strip().split(": ", 1)[1] for line in workflow.splitlines() if line.strip().startswith("path: "))
archives = list(OUT.glob(pattern))
assert len(archives) == 1, "artifact glob does not match the generated archive"
with tarfile.open(archives[0]) as archive:
    assert f"{name}/exchange" in archive.getnames()
    assert f"{name}/systemd/rust-trader.service" in archive.getnames()
assert "dist/*.tar.gz" in workflow.split("gh release create", 1)[1]
print("PASS: workflow tar output matches artifact/release path and contains expected files")

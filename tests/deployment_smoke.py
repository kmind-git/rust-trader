"""Real Linux release-archive acceptance; no synthetic tar or source-tree binary.

Without --archive, run the complete packager against target's musl release bins.
Windows skips explicitly; use startup_validation.py for native startup regression.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tarfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "target" / "test-runs" / "deployment-smoke" / str(time.time_ns())


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def extract(archive, destination):
    destination.mkdir(parents=True)
    with tarfile.open(archive) as bundle:
        entries = bundle.getmembers()
        top_names = {Path(entry.name).parts[0] for entry in entries}
        assert len(top_names) == 1, "one package root required"
        for entry in entries:
            name = Path(entry.name)
            assert not name.is_absolute() and ".." not in name.parts
            assert entry.isdir() or entry.isfile(), "links/devices are not payload files"
        # Paths and member kinds were checked above; supports Python 3.8+.
        bundle.extractall(destination)
    return destination / top_names.pop()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()
    if not sys.platform.startswith("linux"):
        print("SKIP: real Linux musl archive acceptance requires Linux; no archive deployment was validated")
        return
    import pwd

    assert os.geteuid() != 0, "run archive acceptance as an ordinary deployment user"
    OUT.mkdir(parents=True)
    archive = args.archive.resolve() if args.archive else None
    if archive is None:
        subprocess.run(["bash", str(ROOT / "scripts/package-linux.sh"), "smoke", str(ROOT / "target"), str(OUT / "dist")], cwd=ROOT, check=True)
        archive = OUT / "dist/rust-trader-smoke-linux-x86_64-musl.tar.gz"
    checksum_file = archive.with_name(archive.name + ".sha256")
    expected_hash = checksum_file.read_text().split()[0]
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == expected_hash
    install = extract(archive, OUT / "selected install path")
    caller = OUT / "caller without source files"
    caller.mkdir()
    subprocess.run(["sha256sum", "-c", "SHA256SUMS"], cwd=install, check=True, stdout=subprocess.DEVNULL)
    required = ["bin/exchange", "bin/client", "bin/playback", "traderctl", "configs/runtime.env", "configs/qf_exchange_settings", "configs/qf_connector_settings", "configs/instruments.txt", "examples/playback.txt", "systemd/rust-trader.service.in", "BUILD-INFO.json"]
    assert all((install / name).is_file() for name in required)
    for path in (install / "configs").iterdir():
        assert path.stat().st_uid == os.getuid() and os.access(path, os.W_OK)
        assert (install / "defaults" / path.name).read_bytes() == path.read_bytes()
    assert os.access(install / "traderctl", os.X_OK)
    assert "candidate" in json.loads((install / "BUILD-INFO.json").read_text())["acceptance"]
    fix_port, rest_port = free_port(), free_port()
    while fix_port == rest_port:
        rest_port = free_port()
    cfg = install / "configs/qf_exchange_settings"
    cfg.write_text(cfg.read_text().replace("SocketAcceptPort=5001", f"SocketAcceptPort={fix_port}"))
    runtime = install / "configs/runtime.env"
    runtime.write_text(runtime.read_text().replace("REST_PORT=8080", f"REST_PORT={rest_port}").replace("START_TIMEOUT=15", "START_TIMEOUT=4"))
    instruments = install / "configs/instruments.txt"
    instruments.write_text("101 DEPLOYMENT_SMOKE\n")
    ctl = install / "traderctl"
    results = []

    def run(action, success=True):
        result = subprocess.run([str(ctl), action], cwd=caller, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=25)
        results.append({"action": action, "exit_code": result.returncode, "output": result.stdout})
        assert (result.returncode == 0) == success, results[-1]
        return result

    def listeners_available():
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(f"http://127.0.0.1:{rest_port}/api/instruments/", timeout=3) as response:
            assert json.load(response) == ["DEPLOYMENT_SMOKE"]
        with socket.create_connection(("127.0.0.1", fix_port), timeout=3):
            pass

    def no_listeners():
        for port in (fix_port, rest_port):
            with socket.socket() as probe:
                assert probe.connect_ex(("127.0.0.1", port)) != 0, f"unexpected listener {port}"

    try:
        run("check")
        no_listeners()
        old_console = install / "logs/app/console-2000-01-01.log"
        old_console.write_text("expired controller log\n")
        unrelated_log = install / "logs/app/keep-me.txt"
        unrelated_log.write_text("unrelated user file\n")
        run("start")
        assert not old_console.exists() and unrelated_log.exists()
        listeners_available()
        first_identity = (install / "run/exchange.pid").read_text()
        run("status")
        run("start", success=False)
        assert (install / "run/exchange.pid").read_text() == first_identity
        run("restart")
        listeners_available()
        assert (install / "run/exchange.pid").read_text() != first_identity
        # Invalid edited runtime must not prevent status/stop of a live process.
        saved_runtime = runtime.read_text()
        for invalid_runtime in ("malformed\n", "STOP_TIMEOUT=0\n", "STOP_TIMEOUT=abc\n"):
            runtime.write_text(invalid_runtime)
            run("status")
            run("stop")
            runtime.write_text(saved_runtime)
            assert run("status", success=False).returncode == 3
            no_listeners()
            if invalid_runtime != "STOP_TIMEOUT=abc\n":
                run("start")

        # A live unrelated PID/starttime must never be signalled.
        sentinel = subprocess.Popen(["sleep", "30"], cwd=caller)
        try:
            start_time = Path(f"/proc/{sentinel.pid}/stat").read_text().split()[21]
            (install / "run/exchange.pid").write_text(f"{sentinel.pid} {start_time}\n")
            assert run("status", success=False).returncode == 3
            run("stop")
            assert sentinel.poll() is None
            run("start")
            listeners_available()
            run("stop")
            assert sentinel.poll() is None
        finally:
            sentinel.terminate()
            sentinel.wait(timeout=5)

        # Missing config is rejected before either listener opens.
        saved_instruments = instruments.read_text()
        instruments.unlink()
        run("check", success=False)
        run("start", success=False)
        no_listeners()
        instruments.write_text(saved_instruments)
        saved_fix = cfg.read_text()
        cfg.write_text("not a FIX config\n")
        run("start", success=False)
        no_listeners()
        cfg.write_text(saved_fix)

        # Real ordinary-user permissions: a non-writable FIX directory fails check.
        denied = install / "denied log directory"
        denied.mkdir()
        denied.chmod(0o500)
        try:
            cfg.write_text(saved_fix.replace("FileLogPath=logs/fix/exchange", f"FileLogPath={denied}").replace("FileLogPath=logs/exchange", f"FileLogPath={denied}"))
            assert str(denied) in cfg.read_text(), "fixture must override the real configured log path"
            run("check", success=False)
            run("start", success=False)
            no_listeners()
        finally:
            denied.chmod(0o700)
            cfg.write_text(saved_fix)

        # A bound FIX port prevents readiness and successful start.
        with socket.socket() as occupied:
            occupied.bind(("0.0.0.0", fix_port))
            occupied.listen()
            run("start", success=False)
        assert run("status", success=False).returncode == 3
        run("start")
        listeners_available()
        run("stop")
        assert any((install / "logs/app").glob("*.log")), "runtime output must be persisted"
        assert any(path.is_file() for path in (install / "logs").rglob("*event*")), "FIX connection event must be persisted"

        unit = run("systemd-unit").stdout
        assert f"User={pwd.getpwuid(os.getuid()).pw_name}" in unit
        assert str(install) in unit and "@ROOT@" not in unit
        assert 'traderctl" run' in unit
        # Emulate documented upgrade: binaries/templates/controller, never configs.
        fresh = extract(archive, OUT / "upgrade source")
        before = {p.name: p.read_bytes() for p in (install / "configs").iterdir()}
        for directory in ("bin", "defaults", "systemd"):
            for path in (fresh / directory).iterdir():
                shutil.copy2(path, install / directory / path.name)
        for name in ("traderctl", "DEPLOY.md", "LICENSE", "BUILD-INFO.json"):
            shutil.copy2(fresh / name, install / name)
        assert before == {p.name: p.read_bytes() for p in (install / "configs").iterdir()}
        run("check")
        run("start")
        listeners_available()
        run("stop")
        print(f"PASS: real musl archive deploy/check/start/stop/restart/identity/permissions/upgrade; {OUT}")
    finally:
        try:
            run("stop")
        finally:
            (OUT / "results.json").write_text(json.dumps({"archive": str(archive), "sha256": expected_hash, "ordinary_uid": os.getuid(), "checks": results}, indent=2))


if __name__ == "__main__":
    main()

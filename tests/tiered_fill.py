"""Tiered fill policy (FillPolicy=Tiered) black-box regression.

Run after cargo build:  python tests/tiered_fill.py
Starts an isolated Tiered exchange, drives one FIX session through every
quantity tier, checks the over-limit reject, wire-level LastPx for market
orders, cancel semantics and REST book visibility. All generated settings
and process output stay in target/test-runs/tiered-fill/.
"""
from __future__ import annotations

import json
import os
import socket
import subprocess
import time
import urllib.request
from pathlib import Path

try:
    from fix42_wire import ROOT, Peer, free_port, timestamp
except ImportError:  # useful when loaded as tests.tiered_fill
    from tests.fix42_wire import ROOT, Peer, free_port, timestamp

OUT = ROOT / "target/test-runs/tiered_fill".replace("/", os.sep) / str(time.time_ns())
EXE = ROOT / "target/debug" / ("exchange.exe" if os.name == "nt" else "exchange")


def rest(port, path):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=5) as response:
        return json.loads(response.read())


def wait_for_rest(port, log_path):
    for _ in range(100):
        try:
            rest(port, "/api/instruments")
            return
        except Exception:
            time.sleep(0.2)
    raise AssertionError(f"exchange REST never came up: {log_path.read_text(errors='replace')}")


def next_er(peer, ident):
    return peer.until(lambda m: m[35] == "8" and m.get(11) == ident)


def drain_quiet(peer, seconds=0.5):
    peer.socket.settimeout(seconds)
    try:
        peer.receive()
        raise AssertionError("unexpected extra report")
    except socket.timeout:
        pass
    finally:
        peer.socket.settimeout(5)


def main():
    OUT.mkdir(parents=True)
    fix_port, rest_port = free_port(), free_port()
    while fix_port == rest_port:
        rest_port = free_port()
    cfg = OUT / "tiered.cfg"
    cfg.write_text(f"""[DEFAULT]
ConnectionType=acceptor
BeginString=FIX.4.2
SenderCompID=GOX
SocketAcceptPort={fix_port}
ResetOnLogout=Y
ResetOnDisconnect=Y
PersistMessages=N
Logging=N
FillPolicy=Tiered
[SESSION]
TargetCompID=CLIENT
""", encoding="utf-8")

    with (OUT / "exchange.log").open("w", encoding="utf-8") as log:
        proc = subprocess.Popen(
            [str(EXE), "-fix", str(cfg), "-instruments", "configs/instruments.txt", "-port", str(rest_port)],
            cwd=ROOT, stdin=subprocess.PIPE, stdout=log, stderr=log, text=True,
        )
        try:
            wait_for_rest(rest_port, OUT / "exchange.log")
            startup = (OUT / "exchange.log").read_text(encoding="utf-8", errors="replace")
            assert "fill policy: Tiered" in startup, "startup log must state the policy"
            run_tiers(fix_port, rest_port)
            print("PASS: tiered tiers, market price, over-limit reject, cancel and REST book")
        finally:
            proc.terminate()
            proc.wait(timeout=10)


def run_tiers(fix_port, rest_port):
    peer = Peer(fix_port, "CLIENT")

    # --- tier 1: 100 rests, no fill; visible in REST book; cancel works ---
    new = peer.order("t1", "1", "100", "10.5")
    assert new[150] == "0" and new[39] == "0", "acked as new, not filled"
    assert new[151] == "100" and new[14] == "0", "leaves 100, cum 0"
    drain_quiet(peer)
    book = rest(rest_port, "/api/book/AAPL")
    assert [(l["price"], l["quantity"]) for l in book["bids"]] == [(10.5, 100.0)], book
    peer.send("F", [(11, "t1c"), (41, "t1"), (55, "AAPL"), (54, "1"), (60, timestamp())])
    cancelled = next_er(peer, "t1c")
    assert cancelled[39] == "4" and cancelled[151] == "0" and cancelled[14] == "0"

    # --- tier 2: 500 fills fully at the limit price ---
    new = peer.order("t2", "1", "500", "10.5")
    assert new[150] == "0" and new[39] == "0"
    fill = next_er(peer, "t2")
    assert fill[150] == "2" and fill[39] == "2", "fully filled"
    assert fill[32] == "500" and fill[31] == "10.5" and fill[6] == "10.5"
    assert fill[14] == "500" and fill[151] == "0"

    # --- tier 3: 1500 fills 50%, remainder cancels with cum preserved ---
    new = peer.order("t3", "1", "1500", "10")
    assert new[39] == "0"
    fill = next_er(peer, "t3")
    assert fill[150] == "1" and fill[39] == "1", "partially filled"
    assert fill[32] == "750" and fill[151] == "750" and fill[14] == "750"
    book = rest(rest_port, "/api/book/AAPL")
    assert [(l["price"], l["quantity"]) for l in book["bids"]] == [(10.0, 750.0)], book
    peer.send("F", [(11, "t3c"), (41, "t3"), (55, "AAPL"), (54, "1"), (60, timestamp())])
    cancelled = next_er(peer, "t3c")
    assert cancelled[39] == "4" and cancelled[14] == "750" and cancelled[151] == "0"

    # --- tier 4: 2550 = 25x100 + 50, terminal Filled, unique ExecIDs ---
    new = peer.order("t4", "1", "2550", "7")
    assert new[39] == "0"
    quantities = []
    for _ in range(26):
        fill = next_er(peer, "t4")
        quantities.append(fill[32])
        assert fill[31] == "7"
    assert quantities == ["100"] * 25 + ["50"], quantities
    assert fill[39] == "2" and fill[14] == "2550" and fill[151] == "0"

    # --- market order: LastPx is the fixed 66.88 ---
    new = peer.order("t5", "1", "500")
    assert new[39] == "0"
    fill = next_er(peer, "t5")
    assert fill[32] == "500" and fill[31] == "66.88" and fill[6] == "66.88"

    # --- over the limit: one business reject, no ack, no fill ---
    new = peer.order("t6", "1", "25600.01", "10")
    assert new[150] == "8" and new[39] == "8", "rejected, never acked"
    assert "tiered fill limit" in new.get(58, ""), new.get(58)
    drain_quiet(peer)

    # --- tiered fills never produce trade volume in statistics ---
    stats = rest(rest_port, "/api/stats/AAPL")
    assert stats["volume"] == 0, stats

    peer.close()


if __name__ == "__main__":
    main()

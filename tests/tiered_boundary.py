"""Tiered fill boundary matrix (black-box).

Run after cargo build --bin exchange:  python tests/tiered_boundary.py
Covers the tier edges end-to-end over a real FIX socket, modify/cancel
paths (including limit→market and R=0), the resting market order (the
price-key design path), disconnect cleanup and cross-session isolation.
Artifacts stay in target/test-runs/tiered-boundary/.
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
except ImportError:
    from tests.fix42_wire import ROOT, Peer, free_port, timestamp

OUT = ROOT / "target/test-runs/tiered_boundary".replace("/", os.sep) / str(time.time_ns())
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


def collect_fills(peer, ident, count):
    """Read `count` fills for `ident`; return (quantities, last_report)."""
    quantities, report = [], None
    for _ in range(count):
        report = next_er(peer, ident)
        assert report[150] in ("1", "2"), report
        quantities.append(report[32])
    return quantities, report


def replace(peer, new_id, orig_id, qty, order_type="2", price="10"):
    fields = [(11, new_id), (41, orig_id), (21, "1"), (55, "AAPL"), (54, "1"),
              (60, timestamp()), (40, order_type), (38, qty)]
    if order_type == "2":
        fields.append((44, price))
    peer.send("G", fields)
    return peer.until(lambda m: m[35] in ("8", "9") and m.get(11) == new_id)


def tier_case(peer, ident, qty, fill_count, expected_quantities, terminal="2"):
    new = peer.order(ident, "1", qty, "10")
    assert new[39] == "0", f"{ident}: expected ack first, got {new[39]}"
    quantities, last = collect_fills(peer, ident, fill_count)
    assert quantities == expected_quantities, f"{ident}: {quantities}"
    assert last[39] == terminal, f"{ident}: terminal {last[39]}"
    return quantities, last


def main():
    OUT.mkdir(parents=True)
    fix_port, rest_port = free_port(), free_port()
    while fix_port == rest_port:
        rest_port = free_port()
    cfg = OUT / "boundary.cfg"
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
[SESSION]
TargetCompID=OTHER
""", encoding="utf-8")

    with (OUT / "exchange.log").open("w", encoding="utf-8") as log:
        proc = subprocess.Popen(
            [str(EXE), "-fix", str(cfg), "-instruments", "configs/instruments.txt", "-port", str(rest_port)],
            cwd=ROOT, stdin=subprocess.PIPE, stdout=log, stderr=log, text=True,
        )
        try:
            wait_for_rest(rest_port, OUT / "exchange.log")
            run_matrix(fix_port, rest_port)
            print("PASS: tier edges, modify paths, resting market order, disconnect cleanup, cross-session isolation")
        finally:
            proc.terminate()
            proc.wait(timeout=10)


def run_matrix(fix_port, rest_port):
    peer = Peer(fix_port, "CLIENT")

    # --- tier edges: 101 / 1000 / 1001 / 2000 / 2001 / 2500 / 25600 ---
    tier_case(peer, "b101", "101", 1, ["101"])
    tier_case(peer, "b1000", "1000", 1, ["1000"])
    _, last = tier_case(peer, "b1001", "1001", 1, ["500.5"], terminal="1")
    assert last[151] == "500.5", last[151]
    peer.send("F", [(11, "b1001c"), (41, "b1001"), (55, "AAPL"), (54, "1"), (60, timestamp())])
    cancelled = next_er(peer, "b1001c")
    assert cancelled[39] == "4" and cancelled[14] == "500.5" and cancelled[151] == "0"
    tier_case(peer, "b2000", "2000", 1, ["1000"], terminal="1")
    peer.send("F", [(11, "b2000c"), (41, "b2000"), (55, "AAPL"), (54, "1"), (60, timestamp())])
    assert next_er(peer, "b2000c")[39] == "4"
    tier_case(peer, "b2001", "2001", 21, ["100"] * 20 + ["1"])
    tier_case(peer, "b2500", "2500", 25, ["100"] * 25)
    quantities, last = tier_case(peer, "b25600", "25600", 256, ["100"] * 256)
    assert last[14] == "25600"

    # --- far over the limit: single reject, nothing else ---
    rejected = peer.order("b102400", "1", "102400", "10")
    assert rejected[150] == "8" and rejected[39] == "8"

    # --- resting market order (price-key design path) ---
    new = peer.order("m100", "1", "100")  # market, qty 100: rests at 66.88
    assert new[39] == "0"
    peer.socket.settimeout(0.5)
    try:
        peer.receive()
        raise AssertionError("resting market order must not fill")
    except socket.timeout:
        pass
    finally:
        peer.socket.settimeout(5)
    book = rest(rest_port, "/api/book/AAPL")
    assert [(l["price"], l["quantity"]) for l in book["bids"]] == [(66.88, 100.0)], book
    peer.send("F", [(11, "m100c"), (41, "m100"), (55, "AAPL"), (54, "1"), (60, timestamp())])
    assert next_er(peer, "m100c")[39] == "4"
    assert rest(rest_port, "/api/book/AAPL")["bids"] == []

    # --- modify to cum (R=0): one Replaced, terminal Filled ---
    peer.order("r0", "1", "1500", "10")
    next_er(peer, "r0")  # 750 fill
    replaced = replace(peer, "r0m", "r0", "750")
    assert replaced[35] == "8" and replaced[150] == "5" and replaced[39] == "2", replaced
    assert replaced[14] == "750"

    # --- modify up: 1500 (750 filled) -> total 2200: R=1450 -> 50% = 725 ---
    peer.order("up", "1", "1500", "10")
    next_er(peer, "up")
    replaced = replace(peer, "upm", "up", "2200")
    assert replaced[150] == "5", replaced
    fill = next_er(peer, "upm")
    assert fill[32] == "725", fill[32]
    assert fill[14] == "1475" and fill[151] == "725"
    peer.send("F", [(11, "upc"), (41, "upm"), (55, "AAPL"), (54, "1"), (60, timestamp())])
    assert next_er(peer, "upc")[39] == "4"

    # --- modify over the limit: CancelReplaceReject, order intact ---
    peer.order("ovl", "1", "1500", "10")
    next_er(peer, "ovl")
    reject = replace(peer, "ovlm", "ovl", "26500")
    assert reject[35] == "9", f"expected OrderCancelReject, got {reject[35]}"
    book = rest(rest_port, "/api/book/AAPL")
    assert [(l["price"], l["quantity"]) for l in book["bids"]] == [(10.0, 750.0)], book

    # --- limit -> market modify: fills at fixed 66.88 ---
    peer.order("l2m", "1", "1500", "10")
    next_er(peer, "l2m")
    replaced = replace(peer, "l2mm", "l2m", "1000", order_type="1")
    assert replaced[150] == "5", replaced
    fill = next_er(peer, "l2mm")
    assert fill[31] == "66.88" and fill[32] == "250", fill
    assert fill[39] == "2" and fill[14] == "1000"

    # --- disconnect cleanup for resting tiered orders ---
    peer.order("disc", "1", "100", "12")
    peer.socket.close()  # no Logout: ResetOnDisconnect must clean the book
    deadline = time.time() + 10
    while time.time() < deadline:
        if rest(rest_port, "/api/book/AAPL")["bids"] == []:
            break
        time.sleep(0.2)
    assert rest(rest_port, "/api/book/AAPL")["bids"] == [], "resting order survived disconnect"

    # --- cross-session isolation: crossing tiered orders never trade ---
    a = Peer(fix_port, "OTHER")
    b = Peer(fix_port, "CLIENT")
    a.order("xa", "1", "100", "100.5")   # bid rests
    b.order("xb", "2", "100", "99")      # ask would cross in Real mode
    book = rest(rest_port, "/api/book/AAPL")
    prices = {("bid", l["price"]) for l in book["bids"]} | {("ask", l["price"]) for l in book["asks"]}
    assert ("bid", 100.5) in prices and ("ask", 99.0) in prices, book
    stats = rest(rest_port, "/api/stats/AAPL")
    assert stats["volume"] == 0, stats
    a.close()
    b.close()


if __name__ == "__main__":
    main()

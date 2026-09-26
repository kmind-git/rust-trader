"""Real (price-crossing) mode boundary matrix (black-box).

Run after cargo build --bin exchange:  python tests/real_boundary.py
Default policy (no FillPolicy key). Covers crossing semantics (no New ack
for crossing orders, trades at the resting price), multi-level sweeps,
market-order remainder cancel, replace re-queue, cancel/duplicate rejects,
MassQuote integration and trade volume statistics over a real FIX socket.
Artifacts stay in target/test-runs/real-boundary/.
"""
from __future__ import annotations

import json
import os
import subprocess
import time
import urllib.request
from pathlib import Path

try:
    from fix42_wire import ROOT, Peer, free_port, timestamp
except ImportError:
    from tests.fix42_wire import ROOT, Peer, free_port, timestamp

OUT = ROOT / "target/test-runs/real_boundary".replace("/", os.sep) / str(time.time_ns())
EXE = ROOT / "target/debug" / ("exchange.exe" if os.name == "nt" else "exchange")
TRADED = 0  # expected /api/stats volume, accumulated per case


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


def collect_ers(peer, ids, count):
    """Collect ERs whose tag 11 is in `ids` (reports arrive buyer-first per
    trade, so a single-order until() would swallow the maker's fill)."""
    out = []
    while len(out) < count:
        out.append(peer.until(lambda m: m[35] == "8" and m.get(11) in ids))
    return out


def trade(qty):
    global TRADED
    TRADED += int(float(qty))


def main():
    OUT.mkdir(parents=True)
    fix_port, rest_port = free_port(), free_port()
    while fix_port == rest_port:
        rest_port = free_port()
    cfg = OUT / "real.cfg"
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
""", encoding="utf-8")

    with (OUT / "exchange.log").open("w", encoding="utf-8") as log:
        proc = subprocess.Popen(
            [str(EXE), "-fix", str(cfg), "-instruments", "configs/instruments.txt", "-port", str(rest_port)],
            cwd=ROOT, stdin=subprocess.PIPE, stdout=log, stderr=log, text=True,
        )
        try:
            wait_for_rest(rest_port, OUT / "exchange.log")
            startup = (OUT / "exchange.log").read_text(encoding="utf-8", errors="replace")
            assert "fill policy: Real" in startup, "default policy must be Real"
            run_matrix(fix_port, rest_port)
            print("PASS: real crossing semantics, sweeps, market remainder, replace, rejects, quotes, stats")
        finally:
            proc.terminate()
            proc.wait(timeout=10)


def run_matrix(fix_port, rest_port):
    peer = Peer(fix_port, "CLIENT")

    # --- simple cross: taker gets a fill directly (no New ack), at maker price ---
    new = peer.order("m1", "1", "100", "10.5")
    assert new[150] == "0" and new[39] == "0", "resting maker is acked"
    book = rest(rest_port, "/api/book/AAPL")
    assert [(l["price"], l["quantity"]) for l in book["bids"]] == [(10.5, 100.0)], book
    peer.send("D", [(11, "t1"), (21, "1"), (55, "AAPL"), (54, "2"), (60, timestamp()),
                    (40, "2"), (38, "50"), (44, "10")])
    reports = {m[11]: m for m in collect_ers(peer, {"m1", "t1"}, 2)}
    taker_fill, maker_fill = reports["t1"], reports["m1"]
    # a crossing order gets its fill directly, at the resting maker's price
    assert taker_fill[150] == "2" and taker_fill[39] == "2", taker_fill
    assert taker_fill[31] == "10.5" and taker_fill[32] == "50", "trade at the resting price"
    assert maker_fill[39] == "1" and maker_fill[151] == "50" and maker_fill[14] == "50"
    trade(50)
    peer.send("F", [(11, "m1c"), (41, "m1"), (55, "AAPL"), (54, "1"), (60, timestamp())])
    cancelled = next_er(peer, "m1c")
    assert cancelled[39] == "4" and cancelled[14] == "50" and cancelled[151] == "0"
    assert rest(rest_port, "/api/book/AAPL")["bids"] == []

    # --- two-level sweep: best level trades first, remainder rests ---
    peer.order("m2", "1", "100", "10")     # lower bid
    peer.order("m3", "1", "100", "10.5")   # better bid
    peer.send("D", [(11, "t2"), (21, "1"), (55, "AAPL"), (54, "2"), (60, timestamp()),
                    (40, "2"), (38, "250"), (44, "9")])
    reports = collect_ers(peer, {"m2", "m3", "t2"}, 4)
    taker_fills = [m for m in reports if m[11] == "t2"]
    assert [(m[31], m[32]) for m in taker_fills] == [("10.5", "100"), ("10", "100")],         "best level trades first"
    for maker in ("m3", "m2"):
        mf = next(m for m in reports if m[11] == maker)
        assert mf[39] == "2", f"{maker} fully filled"
    trade(200)
    book = rest(rest_port, "/api/book/AAPL")
    assert [(l["price"], l["quantity"]) for l in book["asks"]] == [(9.0, 50.0)], book
    peer.send("F", [(11, "t2c"), (41, "t2"), (55, "AAPL"), (54, "2"), (60, timestamp())])
    assert next_er(peer, "t2c")[39] == "4"

    # --- market order: fills what it can, cancels the remainder ---
    peer.order("m4", "1", "100", "10.5")
    peer.send("D", [(11, "t3"), (21, "1"), (55, "AAPL"), (54, "2"), (60, timestamp()),
                    (40, "1"), (38, "250")])
    reports = collect_ers(peer, {"m4", "t3"}, 3)
    fill = next(m for m in reports if m[11] == "t3" and m[150] in ("1", "2"))
    assert fill[31] == "10.5" and fill[32] == "100"
    assert next(m for m in reports if m[11] == "m4")[39] == "2"
    remainder = next(m for m in reports if m[11] == "t3" and m[39] == "4")
    assert remainder[151] == "0", "market remainder is cancelled"
    trade(100)

    # --- market order on an empty book: immediate cancel, no fill ---
    empty = peer.order("t4", "2", "100")
    assert empty[39] == "4" and empty[151] == "0", empty

    # --- replace re-queues at the new price, then crosses there ---
    new = peer.order("r1", "1", "100", "10")
    assert new[39] == "0"
    peer.send("G", [(11, "r1m"), (41, "r1"), (21, "1"), (55, "AAPL"), (54, "1"),
                    (60, timestamp()), (40, "2"), (38, "100"), (44, "10.8")])
    replaced = peer.until(lambda m: m[35] == "8" and m.get(11) == "r1m")
    assert replaced[150] == "5", replaced
    book = rest(rest_port, "/api/book/AAPL")
    assert [(l["price"], l["quantity"]) for l in book["bids"]] == [(10.8, 100.0)], book
    fill = peer.order("t5", "2", "100", "10.8")
    assert fill[31] == "10.8" and fill[39] == "2"
    trade(100)

    # --- cancel an unknown order: OrderCancelReject ---
    peer.send("F", [(11, "uc"), (41, "nope"), (55, "AAPL"), (54, "1"), (60, timestamp())])
    reject = peer.until(lambda m: m[35] == "9" and m.get(11) == "uc")
    assert reject.get(39) in ("8", "0"), reject

    # --- duplicate ClOrdID (reusing m1): business reject ---
    dup = peer.order("m1", "1", "100", "10")
    assert dup[150] == "8" and "duplicate" in dup.get(58, "").lower(), dup

    # --- MassQuote: ack at level 2, book depth, and a cross against the bid ---
    peer.send("i", [(117, "q1"), (301, "2"), (296, "1"), (302, "1"), (311, "1"),
                    (304, "1"), (295, "1"), (299, "AAPL"), (55, "AAPL"),
                    (132, "99.5"), (133, "100.5"), (134, "20"), (135, "10")])
    ack = peer.until(lambda m: m[35] == "b")
    assert ack[297] == "0", f"quote accepted, got {ack}"
    book = rest(rest_port, "/api/book/AAPL")
    assert {"bid": 99.5} in [{"bid": l["price"]} for l in book["bids"]], book
    assert {"ask": 100.5} in [{"ask": l["price"]} for l in book["asks"]], book
    fill = peer.order("t7", "2", "5", "99.5")
    assert fill[31] == "99.5" and fill[32] == "5", "crossed the quote bid"
    trade(5)

    # --- statistics accumulate real trade volume ---
    stats = rest(rest_port, "/api/stats/AAPL")
    assert stats["volume"] == float(TRADED), (stats["volume"], TRADED)

    peer.close()


if __name__ == "__main__":
    main()

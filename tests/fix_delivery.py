"""Small FIX delivery and session-order regression.

Run after building the exchange binary::

    python tests/fix_delivery.py
    python tests/fix_delivery.py --release

The script starts an isolated local exchange and writes every generated file
under ``target/fix-delivery/<run-id>/``.  It intentionally sends no heartbeat
from the maker while the maker is waiting for the taker's order.  The two
second receive deadline is only a bounded regression guard; reported timings
include Python, socket, and message-validation work and are not a production
SLO.
"""

from __future__ import annotations

import argparse
from decimal import Decimal
import json
import os
from pathlib import Path
import selectors
import socket
import subprocess
import time
from typing import Callable, Iterable

try:
    # Running ``python tests/fix_delivery.py`` puts tests/ on sys.path.
    from fix42_wire import ROOT, SOH, free_port, timestamp, validate, wire
except ImportError:  # pragma: no cover - useful when loaded as tests.fix_delivery
    from tests.fix42_wire import ROOT, SOH, free_port, timestamp, validate, wire


OUT_ROOT = ROOT / "target" / "fix-delivery"
DELIVERY_TIMEOUT = 2.0
LOGOUT_CLOSE_TIMEOUT = 1.0
CYCLES = 5
HEART_BT_INT = 30


def order_fields(ident: str, side: str, quantity: str = "4", price: str = "100"):
    """Return a dictionary-valid limit NewOrderSingle body."""

    return [
        (11, ident),
        (21, "1"),
        (55, "AAPL"),
        (54, side),
        (60, timestamp()),
        (40, "2"),
        (38, quantity),
        (44, price),
    ]


class DeliveryPeer:
    """A tiny FIX peer with counted writes and an incremental frame buffer."""

    def __init__(self, sock: socket.socket, sender: str):
        self.socket = sock
        self.sender = sender
        self.remote = "GOX"
        self.out_seq = 0
        self.in_seq = 0
        self.buffer = bytearray()
        self.received: list[tuple[dict[int, str], int]] = []
        self.exec_ids: set[str] = set()
        self.send_calls = 0
        self.first_send_count: int | None = None
        self.closed = False
        self.saw_logon = False
        self.socket.settimeout(5.0)

    @classmethod
    def connect(cls, port: int, sender: str) -> "DeliveryPeer":
        sock = socket.create_connection(("127.0.0.1", port), timeout=5.0)
        return cls(sock, sender)

    def send_many(self, messages: Iterable[tuple[str, list[tuple[int, str]]]]) -> None:
        """Send all supplied messages with exactly one socket sendall call."""

        frames = []
        count = 0
        for msg_type, fields in messages:
            self.out_seq += 1
            count += 1
            frames.append(wire(msg_type, self.out_seq, self.sender, fields, self.remote))
        if not frames:
            return
        self.socket.sendall(b"".join(frames))
        self.send_calls += 1
        if self.first_send_count is None:
            self.first_send_count = count

    def send(self, msg_type: str, fields: list[tuple[int, str]] | None = None) -> None:
        self.send_many([(msg_type, fields or [])])

    def _take_frame(self) -> bytes | None:
        begin_end = self.buffer.find(SOH)
        if begin_end < 0:
            return None
        length_end = self.buffer.find(SOH, begin_end + 1)
        if length_end < 0:
            return None
        length_field = bytes(self.buffer[begin_end + 1 : length_end])
        if not length_field.startswith(b"9="):
            raise AssertionError("server sent a malformed FIX length field")
        body_length = int(length_field[2:])
        total = length_end + 1 + body_length + 7
        if len(self.buffer) < total:
            return None
        raw = bytes(self.buffer[:total])
        del self.buffer[:total]
        return raw

    def read_ready(self) -> list[tuple[dict[int, str], int]]:
        """Read once after selector readiness and parse every complete frame."""

        if self.closed:
            return []
        try:
            chunk = self.socket.recv(65536)
        except socket.timeout:
            return []
        except OSError:
            self.closed = True
            return []
        if chunk:
            self.buffer.extend(chunk)
        else:
            self.closed = True

        result: list[tuple[dict[int, str], int]] = []
        while True:
            raw = self._take_frame()
            if raw is None:
                break
            msg = validate(raw)
            self._record(msg)
            result.append((msg, time.perf_counter_ns()))
        if self.closed and self.buffer:
            raise AssertionError("server closed with a truncated FIX frame")
        return result

    def _record(self, msg: dict[int, str]) -> None:
        assert msg.get(49) == self.remote, f"unexpected SenderCompID: {msg.get(49)!r}"
        assert msg.get(56) == self.sender, f"unexpected TargetCompID: {msg.get(56)!r}"
        sequence = int(msg[34])
        assert sequence == self.in_seq + 1, (
            f"{self.sender} outbound SeqNum discontinuity: "
            f"expected {self.in_seq + 1}, got {sequence}"
        )
        self.in_seq = sequence
        msg_type = msg[35]
        if msg_type == "A":
            self.saw_logon = True
        if msg_type == "8":
            assert self.saw_logon, f"{self.sender} received business report before Logon"
            exec_id = msg[17]
            assert exec_id not in self.exec_ids, f"{self.sender} reused ExecID {exec_id}"
            self.exec_ids.add(exec_id)
        self.received.append((msg, time.perf_counter_ns()))

    def close(self) -> None:
        try:
            self.socket.close()
        finally:
            self.closed = True


class DeliveryHub:
    """Read both peers from one selector and enforce global ExecID uniqueness."""

    def __init__(self, peers: Iterable[DeliveryPeer] = ()):
        self.selector = selectors.DefaultSelector()
        self.peers: set[DeliveryPeer] = set()
        self.all_exec_ids: set[str] = set()
        for peer in peers:
            self.add(peer)

    def add(self, peer: DeliveryPeer) -> None:
        self.peers.add(peer)
        self.selector.register(peer.socket, selectors.EVENT_READ, data=peer)

    def remove(self, peer: DeliveryPeer) -> None:
        if peer not in self.peers:
            return
        self.peers.remove(peer)
        try:
            self.selector.unregister(peer.socket)
        except (KeyError, OSError):
            pass

    def poll(self, timeout: float) -> list[tuple[DeliveryPeer, dict[int, str], int]]:
        events = self.selector.select(max(0.0, timeout))
        result: list[tuple[DeliveryPeer, dict[int, str], int]] = []
        for key, _ in events:
            peer: DeliveryPeer = key.data
            for msg, seen_ns in peer.read_ready():
                if msg[35] == "8":
                    exec_id = msg[17]
                    assert exec_id not in self.all_exec_ids, f"ExecID reused globally: {exec_id}"
                    self.all_exec_ids.add(exec_id)
                result.append((peer, msg, seen_ns))
            if peer.closed:
                self.remove(peer)
        return result

    def close(self) -> None:
        for peer in list(self.peers):
            self.remove(peer)
        self.selector.close()


def wait_for(
    hub: DeliveryHub,
    predicate: Callable[[DeliveryPeer, dict[int, str]], bool],
    timeout: float = DELIVERY_TIMEOUT,
) -> tuple[DeliveryPeer, dict[int, str], int]:
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(f"timed out after {timeout:.3f}s waiting for FIX message")
        for peer, msg, seen_ns in hub.poll(remaining):
            if predicate(peer, msg):
                return peer, msg, seen_ns


def assert_fill(msg: dict[int, str], ident: str, quantity: Decimal, price: Decimal) -> None:
    assert msg[35] == "8" and msg[11] == ident
    assert msg[150] == "2" and msg[39] == "2", f"expected full fill, got {msg}"
    assert Decimal(msg[32]) == quantity
    assert Decimal(msg[14]) == quantity
    assert Decimal(msg[151]) == Decimal("0")
    assert Decimal(msg[31]) == price
    assert Decimal(msg[6]) == price
    assert Decimal(msg[38]) == quantity


def wait_for_cycle(
    hub: DeliveryHub,
    maker: DeliveryPeer,
    taker: DeliveryPeer,
    maker_id: str,
    taker_id: str,
    sent_ns: int,
) -> dict[str, float]:
    fills: dict[str, int] = {}
    deadline = time.monotonic() + DELIVERY_TIMEOUT
    quantity, price = Decimal("4"), Decimal("100")
    while len(fills) < 2:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(
                f"timed out after {DELIVERY_TIMEOUT:.3f}s waiting for both fills "
                f"maker={maker_id!r}, taker={taker_id!r}"
            )
        for peer, msg, seen_ns in hub.poll(remaining):
            if msg[35] != "8" or msg.get(150) != "2":
                continue
            if peer is maker and msg.get(11) == maker_id:
                key = "maker"
                assert key not in fills, "maker fill duplicated"
                assert_fill(msg, maker_id, quantity, price)
            elif peer is taker and msg.get(11) == taker_id:
                key = "taker"
                assert key not in fills, "taker fill duplicated"
                assert_fill(msg, taker_id, quantity, price)
            else:
                continue
            fills[key] = seen_ns
    return {
        "maker_fill_ms": (fills["maker"] - sent_ns) / 1_000_000,
        "taker_fill_ms": (fills["taker"] - sent_ns) / 1_000_000,
        "both_fills_ms": (max(fills.values()) - sent_ns) / 1_000_000,
    }


def logout_and_verify(hub: DeliveryHub, peer: DeliveryPeer) -> None:
    """The peer initiates Logout; no report may follow the Logout response."""

    peer.send("5")
    wait_for(hub, lambda p, m: p is peer and m[35] == "5")
    post_types: list[str] = []
    deadline = time.monotonic() + LOGOUT_CLOSE_TIMEOUT
    while time.monotonic() < deadline and not peer.closed:
        for current, msg, _ in hub.poll(deadline - time.monotonic()):
            if current is peer:
                post_types.append(msg[35])
                assert msg[35] != "8", "business report arrived after Logout"
    assert peer.closed, "server did not close the connection after Logout within the deadline"
    assert not post_types, f"messages followed Logout: {post_types}"
    assert peer.received[-1][0][35] == "5", "last normal message was not Logout"


def wait_for_startup(proc: subprocess.Popen, port: int, log_path: Path) -> None:
    deadline = time.monotonic() + 10.0
    while True:
        if proc.poll() is not None:
            raise AssertionError(
                f"exchange exited during startup with code {proc.returncode}:\n"
                f"{log_path.read_text(encoding='utf-8', errors='replace')}"
            )
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            if time.monotonic() >= deadline:
                raise AssertionError("exchange startup timed out")
            time.sleep(0.05)


def stop_process(proc: subprocess.Popen | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=5.0)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5.0)


def run(release: bool) -> None:
    run_dir = OUT_ROOT / str(time.time_ns())
    run_dir.mkdir(parents=True, exist_ok=False)
    fix_port, rest_port = free_port(), free_port()
    while rest_port == fix_port:
        rest_port = free_port()

    config = run_dir / "acceptor.cfg"
    config.write_text(
        f"""[DEFAULT]
ConnectionType=acceptor
BeginString=FIX.4.2
SenderCompID=GOX
SocketAcceptPort={fix_port}
HeartBtInt={HEART_BT_INT}
ResetOnLogout=Y
ResetOnDisconnect=Y
PersistMessages=N
Logging=N

[SESSION]
TargetCompID=MAKER

[SESSION]
TargetCompID=TAKER
""",
        encoding="utf-8",
    )
    log_path = run_dir / "exchange.log"
    binary_dir = "target/release" if release else "target/debug"
    binary = ROOT / binary_dir / ("exchange.exe" if os.name == "nt" else "exchange")
    if not binary.is_file():
        raise AssertionError(f"exchange binary not found: {binary}; build it first")

    proc: subprocess.Popen | None = None
    log_handle = log_path.open("w", encoding="utf-8")
    maker: DeliveryPeer | None = None
    taker: DeliveryPeer | None = None
    hub: DeliveryHub | None = None
    cycle_timings: list[dict[str, object]] = []
    try:
        proc = subprocess.Popen(
            [str(binary), "-fix", str(config), "-port", str(rest_port), "--server"],
            cwd=ROOT,
            stdin=subprocess.DEVNULL,
            stdout=log_handle,
            stderr=subprocess.STDOUT,
        )
        wait_for_startup(proc, fix_port, log_path)

        maker = DeliveryPeer.connect(fix_port, "MAKER")
        hub = DeliveryHub([maker])
        maker_id = "maker-0"
        # Logon and the first legal order are deliberately one sendall call.
        maker.send_many(
            [
                ("A", [(98, "0"), (108, str(HEART_BT_INT))]),
                ("D", order_fields(maker_id, "2")),
            ]
        )
        assert maker.first_send_count == 2
        wait_for(
            hub,
            lambda peer, msg: peer is maker
            and msg[35] == "8"
            and msg.get(11) == maker_id
            and msg.get(150) == "0",
        )

        taker = DeliveryPeer.connect(fix_port, "TAKER")
        hub.add(taker)
        taker_id = "taker-0"
        taker_first_sent_ns = time.perf_counter_ns()
        # The taker's first Logon and first legal order also share one sendall.
        taker.send_many(
            [
                ("A", [(98, "0"), (108, str(HEART_BT_INT))]),
                ("D", order_fields(taker_id, "1")),
            ]
        )
        assert taker.first_send_count == 2
        maker_sends_before_fill = maker.send_calls
        first_timing = wait_for_cycle(
            hub, maker, taker, maker_id, taker_id, taker_first_sent_ns
        )
        assert maker.send_calls == maker_sends_before_fill, (
            "maker emitted bytes while idle waiting for the taker; "
            "the peer must not feed the reader with heartbeat messages"
        )
        cycle_timings.append(
            {"cycle": 0, "maker_id": maker_id, "taker_id": taker_id, **first_timing}
        )

        for cycle in range(1, CYCLES):
            maker_id = f"maker-{cycle}"
            taker_id = f"taker-{cycle}"
            maker.send("D", order_fields(maker_id, "2"))
            wait_for(
                hub,
                lambda peer, msg, ident=maker_id: peer is maker
                and msg[35] == "8"
                and msg.get(11) == ident
                and msg.get(150) == "0",
            )
            maker_sends_before_fill = maker.send_calls
            sent_ns = time.perf_counter_ns()
            taker.send("D", order_fields(taker_id, "1"))
            timing = wait_for_cycle(hub, maker, taker, maker_id, taker_id, sent_ns)
            assert maker.send_calls == maker_sends_before_fill, (
                f"maker emitted bytes while idle in cycle {cycle}"
            )
            cycle_timings.append(
                {"cycle": cycle, "maker_id": maker_id, "taker_id": taker_id, **timing}
            )

        # There are no active orders left, so the final normal message on each
        # connection must be the server's Logout response.
        logout_and_verify(hub, maker)
        hub.remove(maker)
        logout_and_verify(hub, taker)

        all_ids = {
            ident
            for cycle in range(CYCLES)
            for ident in (f"maker-{cycle}", f"taker-{cycle}")
        }
        assert len(all_ids) == 2 * CYCLES
        assert maker.saw_logon and taker.saw_logon
        print("PASS: FIX delivery, paired fills, sequence continuity, ExecID uniqueness, and Logout ordering")
        diagnostic = {
            "pass": True,
            "binary": str(binary),
            "cycles": cycle_timings,
            "delivery_timeout_seconds": DELIVERY_TIMEOUT,
            "logout_close_timeout_seconds": LOGOUT_CLOSE_TIMEOUT,
            "maker_inbound_messages": len(maker.received),
            "taker_inbound_messages": len(taker.received),
            "global_exec_ids": len(hub.all_exec_ids),
            "timing_scope": "loopback sample; includes Python, socket, and validation cost; not a production SLO",
            "output_dir": str(run_dir),
        }
        diagnostic_json = json.dumps(diagnostic, indent=2, sort_keys=True)
        (run_dir / "diagnostic.json").write_text(diagnostic_json + "\n", encoding="utf-8")
        print("DIAGNOSTIC_JSON=" + json.dumps(diagnostic, sort_keys=True))
    finally:
        if hub is not None:
            hub.close()
        for peer in (maker, taker):
            if peer is not None:
                peer.close()
        stop_process(proc)
        log_handle.close()


def main() -> None:
    parser = argparse.ArgumentParser(description="FIX 4.2 delivery regression")
    parser.add_argument(
        "--release",
        action="store_true",
        help="use target/release/exchange instead of target/debug/exchange",
    )
    args = parser.parse_args()
    run(args.release)


if __name__ == "__main__":
    main()

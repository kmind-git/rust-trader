"""Independent FIX 4.2 socket regression; stdlib only.
Run after cargo build: python tests/fix42_wire.py
All generated settings and process output stay in target/fix42-wire/.
"""
from pathlib import Path
import datetime as dt
from decimal import Decimal
import os
import socket
import subprocess
import time
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
SOH = b"\x01"
XML = ET.parse(ROOT / "tests/fixtures/FIX42.xml").getroot()
FIELDS = {f.attrib["name"]: f for f in XML.find("fields")}
NUM = {name: int(f.attrib["number"]) for name, f in FIELDS.items()}
BY_NUM = {int(f.attrib["number"]): f for f in FIELDS.values()}
MESSAGES = {m.attrib["msgtype"]: m for m in XML.find("messages")}
HEADER = {NUM[n.attrib["name"]] for n in XML.find("header")}

def check_nodes(nodes, pairs, ordered=False):
    allowed = {NUM[n.attrib["name"]]: (i, n) for i, n in enumerate(nodes)}
    present = set()
    previous = -1
    i = 0
    while i < len(pairs):
        tag, value = pairs[i]
        assert tag in allowed, f"tag {tag} not defined in message/group"
        position, node = allowed[tag]
        assert tag not in present, f"duplicate tag {tag}"
        assert not ordered or position > previous, f"group tag {tag} out of dictionary order"
        previous = position
        present.add(tag)
        if node.tag == "group":
            count = int(value)
            i += 1
            children = list(node)
            child_tags = {NUM[n.attrib["name"]] for n in children}
            delimiter = NUM[children[0].attrib["name"]]
            for _ in range(count):
                assert i < len(pairs) and pairs[i][0] == delimiter, "missing group delimiter"
                start = i
                i += 1
                # These fixtures use only one level inside NoQuoteSets; nested
                # groups are handled by the recursive parser below via counts.
                depth_tags = set(child_tags)
                for child in children:
                    depth_tags.update(NUM[n.attrib["name"]] for n in child.iter() if n is not child)
                while i < len(pairs) and pairs[i][0] in depth_tags and pairs[i][0] != delimiter:
                    i += 1
                check_nodes(children, pairs[start:i], True)
        else:
            i += 1
    required = {NUM[n.attrib["name"]] for n in nodes if n.attrib.get("required") == "Y"}
    assert required <= present, f"missing required fields {required - present}"

def validate(raw):
    fields = [(int(f.split(b"=", 1)[0]), f.split(b"=", 1)[1].decode("ascii"))
              for f in raw.rstrip(SOH).split(SOH)]
    assert [t for t, _ in fields[:3]] == [8, 9, 35]
    assert fields[-1][0] == 10 and len(fields[-1][1]) == 3
    first_end = raw.index(SOH)
    body_start = raw.index(SOH, first_end + 1) + 1
    assert int(fields[1][1]) == len(raw) - 7 - body_start
    assert int(fields[-1][1]) == sum(raw[:-7]) % 256
    assert fields[0][1] == "FIX.4.2"
    for tag, value in fields:
        assert tag in BY_NUM, f"unknown tag {tag}"
        enums = {v.attrib["enum"] for v in BY_NUM[tag]}
        if enums:
            assert value in enums, f"invalid enum {tag}={value}"
    head = [(t, v) for t, v in fields[:-1] if t in HEADER]
    body = [(t, v) for t, v in fields[:-1] if t not in HEADER]
    check_nodes(list(XML.find("header")), head)
    msg = dict(fields)
    check_nodes(list(MESSAGES[msg[35]]), body)
    return msg

def timestamp():
    return dt.datetime.now(dt.timezone.utc).strftime("%Y%m%d-%H:%M:%S.%f")[:-3]

def wire(kind, seq, sender, fields=(), target="GOX"):
    pairs = [(35, kind), (49, sender), (56, target), (34, str(seq)), (52, timestamp()), *fields]
    body = SOH.join(f"{t}={v}".encode("ascii") for t, v in pairs) + SOH
    head = b"8=FIX.4.2\x019=" + str(len(body)).encode() + SOH
    data = head + body
    return data + f"10={sum(data) % 256:03}".encode() + SOH

class Peer:
    def __init__(self, port, sender):
        self.sender, self.seq, self.buffer = sender, 0, b""
        self.remote = "GOX"
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.socket.settimeout(5)
        self.exec_ids = set()
        self.send("A", [(98, "0"), (108, "30")])
        self.until(lambda m: m[35] == "A")

    def send(self, kind, fields=()):
        self.seq += 1
        self.socket.sendall(wire(kind, self.seq, self.sender, fields, self.remote))

    def receive(self):
        while True:
            if self.buffer.count(SOH) >= 2:
                begin_end = self.buffer.index(SOH)
                len_end = self.buffer.index(SOH, begin_end + 1)
                total = len_end + 1 + int(self.buffer[begin_end + 3:len_end]) + 7
                if len(self.buffer) >= total:
                    raw, self.buffer = self.buffer[:total], self.buffer[total:]
                    msg = validate(raw)
                    assert msg[49] == self.remote and msg[56] == self.sender
                    if msg[35] == "8":
                        assert msg[17] not in self.exec_ids, "ExecID reused"
                        self.exec_ids.add(msg[17])
                    return msg
            chunk = self.socket.recv(65536)
            assert chunk, "unexpected EOF"
            self.buffer += chunk

    def until(self, predicate):
        for _ in range(100):
            msg = self.receive()
            if predicate(msg):
                return msg
        raise AssertionError("expected message never arrived")

    def order(self, ident, side, qty, price=None):
        f = [(11, ident), (21, "1"), (55, "AAPL"), (54, side), (60, timestamp()),
             (40, "1" if price is None else "2"), (38, qty)]
        if price is not None:
            f.append((44, price))
        self.send("D", f)
        return self.until(lambda m: m[35] == "8" and m.get(11) == ident)

    def close(self):
        self.send("5")
        self.until(lambda m: m[35] == "5")
        self.socket.close()

def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]

def check_outbound(binary):
    """Use an independent mock acceptor to inspect our client/playback bytes."""
    out = ROOT / "target/fix42-wire"
    with socket.socket() as listener, (out / f"{binary}.log").open("w") as log:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(10)
        port = listener.getsockname()[1]
        ident = "CLIENT" if binary == "client" else "PLAYBACK"
        cfg = out / f"{binary}.cfg"
        cfg.write_text(f"""[DEFAULT]
ConnectionType=initiator
BeginString=FIX.4.2
SenderCompID={ident}
TargetCompID=GOX
SocketConnectHost=127.0.0.1
SocketConnectPort={port}
HeartBtInt=30
ResetOnLogout=Y
ResetOnDisconnect=Y
PersistMessages=N
Logging=N
[SESSION]
""")
        exe = ROOT / "target/debug" / (binary + (".exe" if os.name == "nt" else ""))
        args = [str(exe), "-fix", str(cfg)]
        if binary == "playback":
            quotes = out / "quotes.txt"
            quotes.write_text("+0s AAPL 10 99 10 101\n")
            args += ["-file", str(quotes)]
        proc = subprocess.Popen(args, cwd=ROOT, stdin=subprocess.PIPE, stdout=log, stderr=log, text=True)
        peer = None
        try:
            conn, _ = listener.accept()
            conn.settimeout(5)
            peer = Peer.__new__(Peer)
            peer.sender, peer.remote, peer.seq = "GOX", ident, 0
            peer.socket, peer.buffer, peer.exec_ids = conn, b"", set()
            assert peer.receive()[35] == "A"
            peer.send("A", [(98, "0"), (108, "30")])
            peer.send("d", [(320, "bootstrap"), (322, "1"), (323, "4"),
                            (393, "1"), (55, "AAPL"), (48, "2"), (22, "8")])
            if binary == "client":
                proc.stdin.write("buy AAPL 10 100\n")
                proc.stdin.flush()
                order = peer.until(lambda m: m[35] == "D")
                assert order[40] == "2", "client reversed limit OrdType"
                # Ack to give the client a standard active order record.
                peer.send("8", [(37, "ex1"), (17, "ack1"), (20, "0"), (150, "0"), (39, "0"),
                                (11, order[11]), (55, "AAPL"), (54, "1"), (38, "10"),
                                (40, "2"), (44, "100"), (151, "10"), (14, "0"), (6, "0")])
                proc.stdin.write("buy AAPL 2\n")
                proc.stdin.flush()
                market = peer.until(lambda m: m[35] == "D")
                assert market[40] == "1", "client reversed market OrdType"
                proc.stdin.write("modify 1 101 10\n")
                proc.stdin.flush()
                replace = peer.until(lambda m: m[35] == "G")
                assert replace[40] == "2" and replace[41] == order[11]
                proc.stdin.write("quit\n")
                proc.stdin.flush()
            else:
                quote = peer.until(lambda m: m[35] == "i")
                assert quote[132] == "99" or Decimal(quote[132]) == 99
            peer.until(lambda m: m[35] == "5")
            peer.send("5")
            proc.wait(timeout=5)
            assert proc.returncode == 0, (out / f"{binary}.log").read_text()
            print(f"PASS: {binary} outbound messages pass independent FIX42 dictionary validation")
        finally:
            if peer:
                peer.socket.close()
            if proc.poll() is None:
                proc.kill()
                proc.wait()

def run():
    out = ROOT / "target/fix42-wire"
    out.mkdir(parents=True, exist_ok=True)
    port, http = free_port(), free_port()
    session_log = out / f"session-log-{time.time_ns()}"
    cfg = out / "acceptor.cfg"
    cfg.write_text(f"""[DEFAULT]
ConnectionType=acceptor
BeginString=FIX.4.2
SenderCompID=GOX
SocketAcceptPort={port}
ResetOnLogout=Y
ResetOnDisconnect=Y
PersistMessages=N
Logging=N
[SESSION]
TargetCompID=MAKER
Logging=Y
FileLogPath={session_log.as_posix()}
[SESSION]
TargetCompID=TAKER
""")
    exe = ROOT / "target/debug" / ("exchange.exe" if os.name == "nt" else "exchange")
    with (out / "exchange.log").open("w") as log:
        proc = subprocess.Popen([str(exe), "-fix", str(cfg), "-port", str(http)], cwd=ROOT,
                                stdin=subprocess.PIPE, stdout=log, stderr=log, text=True)
        try:
            deadline = time.monotonic() + 10
            while True:
                assert proc.poll() is None, (out / "exchange.log").read_text()
                try:
                    probe = socket.create_connection(("127.0.0.1", port), timeout=.2)
                    probe.close()
                    break
                except OSError:
                    assert time.monotonic() < deadline, "exchange startup timed out"
                    time.sleep(.05)
            maker, taker = Peer(port, "MAKER"), Peer(port, "TAKER")
            r = maker.order("ask-alpha", "2", "4", "100")
            assert r[150] == "0" and Decimal(r[6]) == 0
            r = taker.order("buy-alpha", "1", "10", "100")
            if r[150] == "0":
                r = taker.until(lambda m: m[35] == "8" and m.get(150) == "1")
            assert Decimal(r[14]) == 4 and Decimal(r[151]) == 6 and Decimal(r[6]) == 100
            order_id = r[37]
            taker.send("G", [(41, "buy-alpha"), (11, "buy-beta"), (21, "1"), (55, "AAPL"),
                             (54, "1"), (60, timestamp()), (40, "2"), (38, "10"), (44, "100")])
            r = taker.until(lambda m: m[35] == "8" and m.get(150) == "5")
            assert r[11] == "buy-beta" and r[41] == "buy-alpha"
            assert r[37] == order_id and r[39] == "1"
            assert Decimal(r[14]) == 4 and Decimal(r[151]) == 6 and Decimal(r[6]) == 100
            taker.send("F", [(41, "buy-beta"), (11, "cancel-beta"), (55, "AAPL"),
                             (54, "1"), (60, timestamp()), (38, "10")])
            r = taker.until(lambda m: m[35] == "8" and m.get(150) == "4")
            assert r[11] == "cancel-beta" and r[41] == "buy-beta"
            assert Decimal(r[14]) == 4 and Decimal(r[151]) == 0
            taker.send("F", [(41, "missing"), (11, "cancel-missing"), (55, "AAPL"),
                             (54, "1"), (60, timestamp()), (38, "1")])
            r = taker.until(lambda m: m[35] == "9")
            assert r[11] == "cancel-missing" and r[41] == "missing"
            taker.send("F", [(41, "buy-beta"), (11, "cancel-again"), (55, "AAPL"),
                             (54, "1"), (60, timestamp()), (38, "10")])
            r = taker.until(lambda m: m[35] == "9")
            assert r[37] == order_id and r[39] == "4"
            maker.order("ask-market", "2", "2", "101")
            r = taker.order("market-alpha", "1", "2")
            if r[150] == "0":
                r = taker.until(lambda m: m[35] == "8" and m.get(11) == "market-alpha" and m[150] == "2")
            assert r[150] == "2" and Decimal(r[6]) == 101
            maker.send("i", [(117, "quote-standard"), (301, "2"), (296, "1"),
                             (302, "set-1"), (311, "IBM"), (304, "1"), (295, "1"),
                             (299, "entry-1"), (55, "IBM"), (132, "99"), (133, "101"),
                             (134, "10"), (135, "10")])
            r = maker.until(lambda m: m[35] == "b")
            assert r[117] == "quote-standard" and r[297] == "0"
            taker.send("Z")
            r = taker.until(lambda m: m[35] in ("3", "j"))
            assert r.get(372) == "Z"
            taker.send("2", [(7, "1"), (16, "0")])
            r = taker.until(lambda m: m[35] == "4")
            assert r[123] == "Y" and int(r[36]) > int(r[34])
            # Deliver a higher sequence first, then fill the missing slot.
            missing = taker.seq + 1
            taker.seq += 1
            taker.send("1", [(112, "queued-probe")])
            r = taker.until(lambda m: m[35] == "2")
            assert int(r[7]) == missing
            taker.socket.sendall(wire("4", missing, taker.sender,
                                     [(43, "Y"), (122, timestamp()), (123, "Y"), (36, str(missing + 1))]))
            taker.until(lambda m: m[35] == "0" and m.get(112) == "queued-probe")
            taker.send("1", [(112, "probe-unique")])
            r = taker.until(lambda m: m[35] == "0" and m.get(112) == "probe-unique")
            maker.close()
            taker.close()
            assert list(session_log.glob("*MAKER.messages.current.log"))
            assert not list(session_log.glob("*TAKER*"))
            print("PASS: dictionary-valid reports; string IDs; standard market/limit; partial fill; replace chain; cancel/reject; unique ExecID; GapFill/recovery; TestRequest; Logout")
        finally:
            if proc.poll() is None:
                try:
                    proc.communicate("quit\n", timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()

if __name__ == "__main__":
    run()
    check_outbound("client")
    check_outbound("playback")

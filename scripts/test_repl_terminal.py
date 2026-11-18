#!/usr/bin/env python3
"""Real terminal regression: idle Rust demo, foreground Ctrl+C, shared host."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import socket
import struct
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--cq", type=Path, required=True)
parser.add_argument("--source-store", type=Path, required=True)
parser.add_argument("--project", required=True)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--idle-seconds", type=float, default=125)
args = parser.parse_args()
assert args.idle_seconds > 120, "exercise the actual former 120-second cutoff"
binary = args.cq.resolve()
args.output.mkdir(parents=True, exist_ok=True)
store = (args.output / "store").resolve()
source = args.source_store.resolve()
shutil.copytree(source, store, ignore=lambda directory, names:
    set(names) & {"runs", "targets", "mirrors"} if Path(directory) == source else set())
environment = os.environ | {"CQ_STORE_HOST": str(binary.with_name("cq-store-host"))}
events = args.output / "events.jsonl"
cases = []
peers = []
host_pid = None
stopped = False


def log(value):
    with events.open("a") as stream:
        stream.write(json.dumps(value, ensure_ascii=False) + "\n")


def ping():
    address = "/tmp/codequest-%s-%s.sock" % (
        os.geteuid(), hashlib.sha256(os.fsencode(store)).hexdigest()[:24])
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(10)
        connection.connect(address)
        payload = b'{"op":"ping"}'
        connection.sendall(struct.pack(">I", len(payload)) + payload)

        def read(n):
            data = b""
            while len(data) < n:
                part = connection.recv(n - len(data))
                if not part:
                    raise EOFError("host disconnected")
                data += part
            return data

        response = json.loads(read(struct.unpack(">I", read(4))[0]))
        assert "error" not in response, response
        return response["result"]["pid"]


class Terminal:
    def __init__(self):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execve(str(binary), [str(binary), "repl", "--store", str(store),
                "--project", args.project], environment)
        self.buffer = b""
        self.exit = None
        peers.append(self)
        deadline = time.monotonic() + 5
        while os.tcgetpgrp(self.fd) != self.pid or os.getpgid(self.pid) != self.pid:
            assert time.monotonic() < deadline, "not a foreground PTY"
            time.sleep(.01)

    def send(self, text):
        log({"pid": self.pid, "input": text})
        os.write(self.fd, text.encode() + b"\n")

    def response(self, timeout=30):
        deadline = time.monotonic() + timeout
        while True:
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                # Terminal echo can precede JSON, including an echoed ^C.
                start = line.find(b"{")
                if start < 0:
                    continue
                try:
                    value = json.loads(line[start:])
                except ValueError:
                    continue
                if "request" in value:
                    log({"pid": self.pid, "response": value})
                    return value
            left = deadline - time.monotonic()
            assert left > 0, "REPL response timed out"
            ready, _, _ = select.select([self.fd], [], [], left)
            assert ready, "REPL response timed out"
            part = os.read(self.fd, 65536)
            assert part, "REPL closed unexpectedly"
            self.buffer += part

    def cmd(self, text):
        self.send(text)
        value = self.response()
        assert "error" not in value, value
        return value

    def interrupt(self):
        log({"pid": self.pid, "terminal_input": "Ctrl+C"})
        os.write(self.fd, b"\x03")

    def wait(self, timeout=5):
        deadline = time.monotonic() + timeout
        while self.exit is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.exit = os.waitstatus_to_exitcode(status)
                return self.exit
            assert time.monotonic() < deadline, "REPL did not exit"
            time.sleep(.02)
        return self.exit


def passed(name, **evidence):
    cases.append({"case": name, "passed": True, **evidence})
    (args.output / "results.json").write_text(json.dumps({
        "passed": len(cases), "complete": False, "cases": cases}, indent=2))


def wait_for_client_ipc(pid):
    """Linux child syscall state is a barrier, rather than a scheduler sleep."""
    root = Path("/proc") / str(pid)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        for task in (root / "task").iterdir():
            try:
                call = (task / "syscall").read_text().split()
                if len(call) > 1:
                    fd = int(call[1], 16)
                    if os.readlink(root / "fd" / str(fd)).startswith("socket:["):
                        log({"ipc_barrier": {"pid": pid, "thread": task.name, "syscall": call}})
                        return
            except (OSError, ValueError):
                pass
        time.sleep(.01)
    raise AssertionError("client did not reach an IPC syscall while host was stopped")


try:
    first = Terminal()
    initial = first.cmd("overview")
    revision = initial["revision"]
    host_pid = ping()
    assert os.getpgid(host_pid) != os.getpgid(first.pid), "host shares terminal interrupt group"
    passed("daemon_is_outside_client_foreground_group", host_pid=host_pid,
        host_pgid=os.getpgid(host_pid), client_pgid=os.getpgid(first.pid))
    other = Terminal()
    assert other.cmd("overview")["revision"] == revision
    search = 'search {"text":"^run$","regex":true,"kind":"Function","file":"src/lib.rs"}'
    found = first.cmd(search)
    assert len(found["items"]) == 1 and found["items"][0]["name"] == "run", found
    subject = found["items"][0]["id"]

    # A stopped, task-owned daemon holds begin_read pending. The explicit
    # cancellation acknowledgement proves the REPL saw an active request.
    os.kill(host_pid, signal.SIGSTOP)
    stopped = True
    first.send("trace " + json.dumps({"subject": subject, "depth": 4096}))
    wait_for_client_ipc(first.pid)
    first.interrupt()
    ack = first.response()
    assert ack.get("state") == "cancellation_requested", ack
    os.kill(host_pid, signal.SIGCONT)
    stopped = False
    cancelled = first.response()
    assert cancelled.get("error", {}).get("code") == "Cancelled", cancelled
    assert cancelled["request"] == ack["request"]
    assert ping() == host_pid, "terminal Ctrl+C killed or restarted the shared host"
    assert first.cmd("overview")["revision"] == revision
    assert other.cmd("overview")["revision"] == revision
    passed("active_terminal_interrupt_cancels_one_request_and_connection_recovers")

    start = time.monotonic()
    log({"idle_begin": start, "seconds": args.idle_seconds, "client_traffic": "none"})
    time.sleep(args.idle_seconds)
    found = first.cmd(search)
    assert found["revision"] == revision and found["items"][0]["id"] == subject, found
    assert other.cmd("overview")["revision"] == revision
    passed("idle_session_survives_real_120_second_cutoff",
        idle_seconds=time.monotonic() - start, revision=revision)

    first.interrupt()
    assert first.wait() == 0
    passed("idle_terminal_interrupt_exits_promptly")
    assert ping() == host_pid
    assert other.cmd(search)["items"][0]["id"] == subject
    passed("other_client_and_shared_host_survive_client_interrupt")
    other.send("quit")
    assert other.wait() == 0
    passed("quit_exits_promptly")
    eof = Terminal()
    eof.cmd("overview")
    os.write(eof.fd, b"\x04")
    assert eof.wait() == 0
    passed("terminal_eof_exits_promptly")
    (args.output / "results.json").write_text(json.dumps({"passed": len(cases),
        "complete": True, "cases": cases, "binary": str(binary),
        "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}, indent=2))
    print(json.dumps({"passed": len(cases), "complete": True}), flush=True)
finally:
    if stopped and host_pid:
        os.kill(host_pid, signal.SIGCONT)
    for peer in peers:
        if peer.exit is None:
            try:
                os.kill(peer.pid, signal.SIGTERM)
                peer.wait()
            except ProcessLookupError:
                pass
        os.close(peer.fd)
    if host_pid:
        try:
            os.kill(host_pid, signal.SIGTERM)
        except ProcessLookupError:
            pass

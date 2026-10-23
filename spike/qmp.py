#!/usr/bin/env python3
"""Minimal QMP and qemu-guest-agent client for the spike (stdlib only).

  qmp.py qmp  SOCK CMD [ARGS_JSON]       one QMP command; prints its "return" as JSON
  qmp.py wait-migrate SOCK [TIMEOUT]     poll query-migrate until it is "completed";
                                         prints the final query-migrate result
  qmp.py qga  SOCK CMD [ARGS_JSON]       one guest-agent command; prints its "return"
  qmp.py ping SOCK [TIMEOUT]             wait until the guest agent answers
  qmp.py exec SOCK SHELL_CMD [TIMEOUT]   run `sh -c SHELL_CMD` in the guest; prints its
                                         stdout (stderr to stderr), exits with its status

Every command and its reply are appended to $QMP_LOG (if set), one line each, with a
host timestamp, so the logs show the exact QMP sequences that were run.
"""

import base64
import json
import os
import random
import socket
import sys
import time


def log(sock_path, arrow, obj):
    path = os.environ.get("QMP_LOG")
    if not path:
        return
    name = os.path.basename(os.path.dirname(sock_path)) + "/" + os.path.basename(sock_path)
    stamp = time.strftime("%H:%M:%S", time.gmtime()) + f".{int(time.time() * 1000) % 1000:03d}"
    with open(path, "a") as f:
        f.write(f"{stamp} {name} {arrow} {json.dumps(obj, separators=(',', ':'))}\n")


class Conn:
    def __init__(self, path, timeout):
        self.path = path
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self.buf = b""

    def send(self, obj, raw_prefix=b""):
        self.sock.sendall(raw_prefix + json.dumps(obj).encode() + b"\n")

    def recv(self):
        while b"\n" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError(f"{self.path}: connection closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        # The guest agent prefixes a delimited sync reply with 0xff.
        line = line.lstrip(b"\xff").strip()
        return json.loads(line) if line else self.recv()


def qmp_connect(path, timeout=600):
    c = Conn(path, timeout)
    c.recv()  # greeting
    c.send({"execute": "qmp_capabilities"})
    c.recv()
    return c


def qmp_call(c, cmd, args=None, quiet=False):
    req = {"execute": cmd}
    if args is not None:
        req["arguments"] = args
    if not quiet:
        log(c.path, "->", req)
    c.send(req)
    while True:
        resp = c.recv()
        if "event" in resp:
            log(c.path, "<- event", resp)
            continue
        if not quiet or "error" in resp:
            log(c.path, "<-", resp)
        if "error" in resp:
            raise RuntimeError(f"{cmd}: {resp['error'].get('desc')}")
        return resp.get("return")


def qga_connect(path, timeout):
    """Connect and resynchronise with guest-sync-delimited (stale replies are skipped)."""
    c = Conn(path, timeout)
    sync_id = random.randint(1, 2**31)
    c.send({"execute": "guest-sync-delimited", "arguments": {"id": sync_id}}, raw_prefix=b"\xff")
    while True:
        resp = c.recv()
        if resp.get("return") == sync_id:
            return c


def main():
    op, sock = sys.argv[1], sys.argv[2]
    rest = sys.argv[3:]
    if op == "qmp":
        args = json.loads(rest[1]) if len(rest) > 1 else None
        print(json.dumps(qmp_call(qmp_connect(sock), rest[0], args)))
    elif op == "wait-migrate":
        deadline = time.time() + (float(rest[0]) if rest else 1800)
        c = qmp_connect(sock)
        while True:
            info = qmp_call(c, "query-migrate", quiet=True)
            status = info.get("status")
            if status == "completed":
                log(sock, "<- query-migrate", info)
                print(json.dumps(info))
                return
            if status in ("failed", "cancelled") or time.time() > deadline:
                log(sock, "<- query-migrate", info)
                sys.exit(f"migration {status}: {info.get('error-desc', '')}")
            time.sleep(0.02)
    elif op == "qga":
        args = json.loads(rest[1]) if len(rest) > 1 else None
        print(json.dumps(qmp_call(qga_connect(sock, 600), rest[0], args)))
    elif op == "ping":
        deadline = time.time() + (float(rest[0]) if rest else 300)
        while True:
            try:
                qmp_call(qga_connect(sock, 1), "guest-ping", quiet=True)
                return
            except (OSError, ConnectionError, ValueError):
                if time.time() > deadline:
                    sys.exit(f"{sock}: guest agent did not answer")
                time.sleep(0.1)
    elif op == "exec":
        timeout = float(rest[1]) if len(rest) > 1 else 600
        c = qga_connect(sock, timeout)
        pid = qmp_call(c, "guest-exec", {"path": "/bin/sh", "arg": ["-c", rest[0]], "capture-output": True})["pid"]
        deadline = time.time() + timeout
        while True:
            st = qmp_call(c, "guest-exec-status", {"pid": pid}, quiet=True)
            if st.get("exited"):
                break
            if time.time() > deadline:
                sys.exit(f"guest command timed out: {rest[0]}")
            time.sleep(0.02)
        out = base64.b64decode(st.get("out-data", "")).decode(errors="replace")
        err = base64.b64decode(st.get("err-data", "")).decode(errors="replace")
        log(sock, "<- exit", {"exitcode": st.get("exitcode"), "out": out[-400:], "err": err[-400:]})
        sys.stdout.write(out)
        sys.stderr.write(err)
        sys.exit(st.get("exitcode", 1))
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()

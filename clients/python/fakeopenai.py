"""python client and process manager for the fake-openai mock binary.

shared by the ecosystem's end-to-end tests: spawn the real binary, point an app
at base_url, drive a scenario, and inspect what was captured over the http admin
api. the tests run as plain python3, so this module is stdlib-only -- no third
party dependency to install before `make test`.

two ways to drive a running mock, sharing one implementation:

  - the `FakeOpenAI` context manager handles spawn + teardown and exposes the
    admin api as methods (preferred for new tests):

        with fakeopenai.FakeOpenAI("--consume-only-with-tools") as fake:
            fake.program([{"tool_calls": [...]}, {"content": "done"}])
            ...drive the app at fake.base_url...
            caps = fake.chat_captures()

  - module-level functions keyed on an admin url, for a test that just holds the
    url: fakeopenai.captures(admin_url), fakeopenai.program(admin_url, specs),
    fakeopenai.reset(admin_url), fakeopenai.behavior(admin_url, **flags).

clients reference this module by the same ../fake-openai/ sibling path they use
for the binary:

    sys.path.insert(0, str(ROOT.parent / "fake-openai" / "clients" / "python"))
    import fakeopenai
    if not fakeopenai.available():
        print("SKIP: build ../fake-openai"); sys.exit(0)
"""

import base64
import json
import os
import socket
import struct
import subprocess
import time
import urllib.request
from pathlib import Path

# the binary lives in this checkout (clients/python/ -> repo root is parents[2]).
# FAKE_OPENAI_BIN overrides, e.g. to point at a release build.
_REPO_ROOT = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("FAKE_OPENAI_BIN") or _REPO_ROOT / "target" / "debug" / "fake-openai")

# the binary announces its chosen url on this stdout line; the admin api shares
# the port under /__admin (without the /v1 suffix).
BASE_URL_PREFIX = "base_url="
STARTUP_TIMEOUT_S = 10
ADMIN_TIMEOUT_S = 5

# default sentinel the binary uses to exempt heartbeat traffic; is_heartbeat_request
# mirrors the rust is_heartbeat predicate so tests can split heartbeat captures.
HEARTBEAT_SENTINEL = "[heartbeat]"


def available():
    """true when the binary is built; tests skip themselves when it is not."""
    return BIN.exists()


def is_heartbeat_request(body, sentinel=HEARTBEAT_SENTINEL):
    """detect a heartbeat by the sentinel in any user message content, matching
    the binary's predicate (a string, or a list of parts each with a text field)."""
    for m in body.get("messages", []) or []:
        c = m.get("content")
        if isinstance(c, str) and sentinel in c:
            return True
        if isinstance(c, list):
            for p in c:
                if isinstance(p, dict) and sentinel in (p.get("text") or ""):
                    return True
    return False


def _admin(admin_url, path, method="GET", payload=None):
    """one admin request; returns the parsed json body (or {} when empty)."""
    data = json.dumps(payload).encode() if payload is not None else None
    headers = {"Content-Type": "application/json"} if data is not None else {}
    req = urllib.request.Request(admin_url + path, data=data, method=method, headers=headers)
    with urllib.request.urlopen(req, timeout=ADMIN_TIMEOUT_S) as r:
        raw = r.read().decode()
    return json.loads(raw) if raw else {}


# --- admin api keyed on an admin url ---

def captures(admin_url):
    """every non-admin request recorded as {path, headers, body}."""
    return _admin(admin_url, "/captures").get("captures", [])


def chat_captures(admin_url):
    """just the chat-completions captures, the common assertion target."""
    return [c for c in captures(admin_url) if "chat/completions" in c.get("path", "")]


def program(admin_url, specs):
    """append one spec or a list of specs to the response queue."""
    _admin(admin_url, "/responses", "POST", specs)


def set_responses(admin_url, specs):
    """replace the whole response queue."""
    _admin(admin_url, "/responses", "PUT", specs)


def set_default(admin_url, spec):
    """set the empty-queue fallback response."""
    _admin(admin_url, "/default", "PUT", spec)


def set_models(admin_url, ids):
    """set the model list reported by /v1/models.

    accepts bare id strings or full model objects (see llamaswap_model), which
    are reported verbatim so a caller can mirror a real server's metadata.
    """
    _admin(admin_url, "/models", "PUT", list(ids))


def llamaswap_model(model_id, aliases=(), modsi="text", modso="text"):
    """build a /v1/models entry carrying llama-swap metadata.

    `aliases` are the stable names the model also answers to; `modsi`/`modso`
    are its comma-separated input/output modality tags, which clients read to
    filter a model picker down to the models a given task can use. serving the
    alias itself needs the llamaswap behavior enabled.
    """
    meta = {"modsi": modsi, "modso": modso}
    if aliases:
        meta["aliases"] = list(aliases)
    return {"id": model_id, "meta": {"llamaswap": meta}}


def behavior(admin_url, **kw):
    """merge behavior flags at runtime, e.g. behavior(url, consume_only_with_tools=True)."""
    _admin(admin_url, "/behavior", "POST", kw)


def reset(admin_url):
    """clear captures and the queue and re-arm the stall latch."""
    _admin(admin_url, "/reset", "POST", {})


def health(admin_url):
    return _admin(admin_url, "/health")


class FakeOpenAI:
    """a running fake-openai subprocess plus its admin api. spawn it with cli
    flags (e.g. "--stall-first-with-tools"); use as a context manager so the
    process is always terminated. the admin methods delegate to the module-level
    functions above against self.admin_url."""

    def __init__(self, *args, timeout=STARTUP_TIMEOUT_S):
        self.args = args
        self.timeout = timeout
        self.proc = None
        self.base_url = None
        self.admin_url = None

    def start(self):
        """launch on a free port and block until base_url is announced."""
        self.proc = subprocess.Popen(
            [str(BIN), "--port", "0", *self.args],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        deadline = time.time() + self.timeout
        while time.time() < deadline:
            line = self.proc.stdout.readline()
            if not line:
                break
            if line.startswith(BASE_URL_PREFIX):
                self.base_url = line.strip()[len(BASE_URL_PREFIX):]
                self.admin_url = self.base_url.rsplit("/v1", 1)[0] + "/__admin"
                return self
        self.stop()
        raise RuntimeError("fake-openai did not announce base_url")

    def stop(self):
        """terminate the subprocess if still running (idempotent)."""
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()

    def __enter__(self):
        return self.start()

    def __exit__(self, *exc):
        self.stop()
        return False

    def captures(self):
        return captures(self.admin_url)

    def chat_captures(self):
        return chat_captures(self.admin_url)

    def health(self):
        return health(self.admin_url)

    def program(self, specs):
        program(self.admin_url, specs)

    def set_responses(self, specs):
        set_responses(self.admin_url, specs)

    def set_default(self, spec):
        set_default(self.admin_url, spec)

    def set_models(self, ids):
        set_models(self.admin_url, ids)

    def behavior(self, **kw):
        behavior(self.admin_url, **kw)

    def reset(self):
        reset(self.admin_url)


# --- websocket client -------------------------------------------------------
# the pi-* web servers (pi-webui, pi-omni) stream over a websocket, so their
# black-box tests need a client. python has no stdlib websocket client, and this
# module stays dependency-free, so this is a minimal RFC6455 text-frame client:
# masked client frames, ping/pong, close. not a general implementation (no
# continuation frames or extensions), which is all the test protocols use.

_WS_TEXT = 0x1
_WS_BINARY = 0x2
_WS_CLOSE = 0x8
_WS_PING = 0x9
_WS_PONG = 0xA


class WSClient:
    """minimal websocket text client for driving the pi-* web servers in tests."""

    def __init__(self, host, port, path="/ws", timeout=30):
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.sock.settimeout(timeout)
        self._buf = b""
        self._open(host, port, path)

    def _open(self, host, port, path):
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(req.encode())
        resp = b""
        while b"\r\n\r\n" not in resp:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("websocket handshake closed early")
            resp += chunk
        head, _, rest = resp.partition(b"\r\n\r\n")
        if b" 101 " not in head.split(b"\r\n", 1)[0]:
            raise ConnectionError(f"websocket handshake failed: {head.splitlines()[0]!r}")
        self._buf = rest  # bytes after the headers are the start of the frame stream

    def _send(self, opcode, payload):
        mask = os.urandom(4)
        header = bytearray([0x80 | opcode])
        n = len(payload)
        if n < 126:
            header.append(0x80 | n)
        elif n < 65536:
            header.append(0x80 | 126)
            header += struct.pack(">H", n)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", n)
        header += mask
        self.sock.sendall(bytes(header) + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def send_json(self, obj):
        self._send(_WS_TEXT, json.dumps(obj).encode())

    def send_bytes(self, data):
        """send a binary frame (e.g. raw PCM audio for the omni voice server)."""
        self._send(_WS_BINARY, bytes(data))

    def _need(self, n):
        while len(self._buf) < n:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("websocket closed")
            self._buf += chunk

    def _frame(self):
        self._need(2)
        b1 = self._buf[1]
        opcode = self._buf[0] & 0x0F
        length = b1 & 0x7F
        off = 2
        if length == 126:
            self._need(4)
            length = struct.unpack(">H", self._buf[2:4])[0]
            off = 4
        elif length == 127:
            self._need(10)
            length = struct.unpack(">Q", self._buf[2:10])[0]
            off = 10
        mask = b""
        if b1 & 0x80:
            self._need(off + 4)
            mask = self._buf[off:off + 4]
            off += 4
        self._need(off + length)
        data = bytearray(self._buf[off:off + length])
        self._buf = self._buf[off + length:]
        if mask:
            for i in range(len(data)):
                data[i] ^= mask[i % 4]
        return opcode, bytes(data)

    def recv_json(self, timeout=None):
        """return the next text frame parsed as json; answer pings transparently."""
        if timeout is not None:
            self.sock.settimeout(timeout)
        while True:
            opcode, data = self._frame()
            if opcode == _WS_TEXT:
                return json.loads(data.decode())
            if opcode == _WS_CLOSE:
                raise ConnectionError("websocket closed by server")
            if opcode == _WS_PING:
                self._control(_WS_PONG, data)

    def _control(self, opcode, data=b""):
        mask = os.urandom(4)
        header = bytes([0x80 | opcode, 0x80 | len(data)]) + mask
        self.sock.sendall(header + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def close(self):
        try:
            self._control(_WS_CLOSE)
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

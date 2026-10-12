"""Real gateway, local upstream and recording proxy; CI-only repeatable routing evidence."""

import base64
import http.client as http_client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import select
import socket
import socketserver
from urllib.parse import urlsplit

from common import free_port, stop_process, suite_cli, wait_ready


class ProxyUpstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def respond(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.server.captures.append({
            "path": self.path,
            "via": self.headers.get("X-Fixture-Proxy", "direct"),
            "body": body.decode(),
            "proxy_auth_leaked": "Proxy-Authorization" in self.headers,
        })
        assert "Proxy-Authorization" not in self.headers
        payload = (
            {"object": "list", "data": [{"id": "fixture", "object": "model"}]}
            if self.command == "GET"
            else {"usage": {"input_tokens": 10, "output_tokens": 2}, "content": []}
        )
        stream = self.command == "POST" and json.loads(body).get("stream")
        wire = (
            b'data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}\n\ndata: [DONE]\n\n'
            if stream
            else json.dumps(payload).encode()
        )
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream" if stream else "application/json")
        self.send_header("Content-Length", str(len(wire)))
        self.end_headers()
        self.wfile.write(wire)

    do_GET = respond
    do_POST = respond


class RecordingProxy(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def forward(self):
        target = urlsplit(self.path)
        assert target.hostname == self.server.target_host and target.port == self.server.target_port, target
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        headers = {
            key: value for key, value in self.headers.items()
            if key.lower() not in ("proxy-authorization", "proxy-connection", "connection")
        }
        headers["X-Fixture-Proxy"] = "proxy"
        auth = self.headers.get("Proxy-Authorization")
        if self.server.require_auth:
            assert auth == "Basic " + base64.b64encode(b"fixture:secret").decode(), auth
        connection = http_client.HTTPConnection(self.server.target_host, self.server.target_port, timeout=10)
        try:
            connection.request(self.command, target.path, body, headers)
            response = connection.getresponse()
            wire = response.read()
            self.send_response(response.status)
            self.send_header("Content-Type", response.getheader("Content-Type"))
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)
        finally:
            connection.close()

    do_GET = forward
    do_POST = forward


class SocksHandler(socketserver.BaseRequestHandler):
    def handle(self):
        def read(length):
            data = b""
            while len(data) < length:
                chunk = self.request.recv(length - len(data))
                assert chunk, "incomplete SOCKS handshake"
                data += chunk
            return data

        version, methods = read(2)
        assert version == 5 and 0 in read(methods)
        self.request.sendall(b"\x05\x00")
        version, command, _, address_type = read(4)
        assert version == 5 and command == 1
        target = socket.inet_ntoa(read(4)) if address_type == 1 else read(read(1)[0]).decode()
        target_port = int.from_bytes(read(2), "big")
        assert target == self.server.target_host and target_port == self.server.target_port
        self.server.socks_connections.append({"host": target, "port": target_port})
        with socket.create_connection((target, target_port), timeout=10) as connection:
            self.request.sendall(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
            peers = [self.request, connection]
            while ready := select.select(peers, [], [], 10)[0]:
                for peer in ready:
                    data = peer.recv(65536)
                    if not data:
                        return
                    (connection if peer is self.request else self.request).sendall(data)


class ProxyTestContext:
    def __init__(self, binary, output, test, port, host, upstream, proxy, socks, env, base, settings_path, log):
        self.binary = binary
        self.output = output
        self.test = test
        self.port = port
        self.host = host
        self.upstream = upstream
        self.proxy = proxy
        self.socks = socks
        self.env = env
        self.base = base
        self.settings_path = settings_path
        self.log = log
        self.process = None
        self.registry = []

    def passed(self, name):
        self.test.passed(name)

    def launch(self):
        self.process = self.test.spawn([str(self.binary), "--headless"], env=self.env, stdout=self.log, stderr=self.log)
        wait_ready(self.process, self.port)

    def stop(self):
        if self.process is not None and self.process.poll() is None:
            stop_process(self.process)
            self.process = None

    def http(self, method, path, payload=None):
        connection = http_client.HTTPConnection("127.0.0.1", self.port, timeout=15)
        connection.request(
            method, path, json.dumps(payload) if payload is not None else None, {"Content-Type": "application/json"}
        )
        response = connection.getresponse()
        try:
            return response.status, response.read()
        finally:
            connection.close()

    def status(self):
        code, wire = self.http("GET", "/ui/status")
        assert code == 200
        return json.loads(wire)

    def update(self, **changes):
        assert self.http("POST", "/ui/settings", {"upstream_base_url": self.base, **changes})[0] == 204

    def request(self, model, via, endpoint="messages", stream=False):
        before = len(self.upstream.captures)
        code, wire = self.http("POST", f"/v1/{endpoint}", {"model": model, "messages": [], "stream": stream})
        assert code == 200, wire
        assert len(self.upstream.captures) == before + 1 and self.upstream.captures[-1]["via"] == via, self.upstream.captures[-1:]
        if stream:
            assert wire.endswith(b"data: [DONE]\n\n")

    def catalog(self, expected):
        before = len(self.upstream.captures)
        code, wire = self.http("GET", "/v1/models")
        assert code == 200 and len(json.loads(wire)["data"]) == 2, wire
        assert sorted(row["via"] for row in self.upstream.captures[before:]) == sorted(expected)


def run_proxy_routing_and_switching_cases(ctx):
    """System proxy defaults, per-provider routing toggles, custom proxies, and address schemas."""
    ctx.launch()
    state = ctx.status()
    assert state["proxy"] == "" and state["stepfun_use_proxy"] and state["opencode_use_proxy"]
    ctx.request("stepfun/fixture", "proxy")
    ctx.request("opencode/fixture", "proxy", "chat/completions", True)
    ctx.catalog(["proxy", "proxy"])
    ctx.passed("old-settings-default-to-system-and-proxy-stream-and-model-list")

    ctx.update(stepfun_use_proxy=False)
    ctx.request("stepfun/fixture", "direct")
    ctx.request("opencode/fixture", "proxy", "responses")
    ctx.catalog(["direct", "proxy"])
    ctx.update(stepfun_use_proxy=True, opencode_use_proxy=False)
    ctx.request("stepfun/fixture", "proxy")
    ctx.request("opencode/fixture", "direct")
    ctx.catalog(["proxy", "direct"])
    ctx.passed("per-provider-switches-change-routing-without-restart-or-pool-leakage")

    ctx.update(proxy="direct", opencode_use_proxy=True)
    ctx.request("fixture", "direct")
    ctx.request("opencode/fixture", "direct")
    ctx.update(proxy=f"http://fixture:secret@127.0.0.1:{ctx.proxy.server_port}")
    ctx.proxy.require_auth = True
    ctx.request("fixture", "proxy")
    ctx.request("opencode/fixture", "proxy")
    ctx.passed("global-direct-and-custom-authenticated-proxy-override-system")

    preserved = ctx.settings_path.read_bytes()
    for invalid in ("file:///tmp/proxy", "http://", "bad-proxy", "ftp://127.0.0.1:7890"):
        assert ctx.http("POST", "/ui/settings", {"upstream_base_url": ctx.base, "proxy": invalid})[0] == 400
        assert ctx.settings_path.read_bytes() == preserved
    ctx.request("fixture", "proxy")
    ctx.passed("invalid-proxy-preserves-settings-and-working-clients")

    for scheme in ("socks5", "socks5h"):
        ctx.update(proxy=f"{scheme}://127.0.0.1:{ctx.socks.server_address[1]}")
        before = len(ctx.socks.socks_connections)
        ctx.request("fixture", "direct")
        assert len(ctx.socks.socks_connections) == before + 1
    ctx.update(proxy=f"127.0.0.1:{ctx.proxy.server_port}")
    ctx.proxy.require_auth = False
    ctx.request("fixture", "proxy")
    ctx.passed("socks5-socks5h-and-http-host-port-proxy-addresses")


def run_proxy_persistence_and_platform_cases(ctx):
    """Proxy restart persistence, loopback dead-proxy bypass, and Windows internet options."""
    ctx.stop()
    ctx.env["HTTP_PROXY"] = "http://127.0.0.1:1"
    ctx.launch()
    ctx.request("fixture", "proxy")
    ctx.update(stepfun_use_proxy=False)
    ctx.request("fixture", "direct")
    ctx.update(proxy="", opencode_use_proxy=False)
    ctx.request("opencode/fixture", "direct")
    ctx.update(upstream_base_url=f"http://127.0.0.1:{ctx.upstream.server_port}/v1", stepfun_use_proxy=True)
    ctx.request("fixture", "direct")
    ctx.passed("restart-preserves-custom-and-provider-direct-and-loopback-bypasses-dead-proxy")
    ctx.stop()

    if os.name == "nt":
        import winreg
        with winreg.CreateKey(winreg.HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Internet Settings") as key:
            for name, value, kind in (
                ("ProxyEnable", 1, winreg.REG_DWORD),
                ("ProxyServer", f"127.0.0.1:{ctx.proxy.server_port}", winreg.REG_SZ),
                ("ProxyOverride", "localhost;127.0.0.1", winreg.REG_SZ),
            ):
                try:
                    old_value, old_kind = winreg.QueryValueEx(key, name)
                    ctx.registry.append((name, old_value, old_kind))
                except FileNotFoundError:
                    ctx.registry.append((name, None, None))
                winreg.SetValueEx(key, name, 0, kind, value)
        ctx.env.pop("HTTP_PROXY", None)
        ctx.proxy.require_auth = False
        ctx.launch()
        ctx.update(upstream_base_url=ctx.base, opencode_use_proxy=True)
        ctx.request("fixture", "proxy")
        ctx.request("opencode/fixture", "proxy")
        ctx.passed("windows-internet-options-used-without-proxy-environment")


def run(binary, output, test):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.connect(("192.0.2.1", 80))
        host = sock.getsockname()[0]

    upstream = ThreadingHTTPServer(("0.0.0.0", 0), ProxyUpstream)
    upstream.captures = []
    test.server(upstream)

    proxy = ThreadingHTTPServer(("127.0.0.1", 0), RecordingProxy)
    proxy.require_auth = False
    proxy.target_host = host
    proxy.target_port = upstream.server_port
    test.server(proxy)

    socks = socketserver.ThreadingTCPServer(("127.0.0.1", 0), SocksHandler)
    socks.daemon_threads = True
    socks.socks_connections = []
    socks.target_host = host
    socks.target_port = upstream.server_port
    test.server(socks)

    port = free_port()
    settings_path = output.resolve() / "settings.json"
    base = f"http://{host}:{upstream.server_port}/v1"
    settings = {"upstream_base_url": base, "opencode_base_url": base, "enabled": True}
    settings_path.write_text(json.dumps(settings), encoding="utf-8")
    database = output.resolve() / "requests.sqlite3"
    database.unlink(missing_ok=True)
    env = {key: value for key, value in os.environ.items() if "proxy" not in key.lower() and not key.startswith("GATEWAY_")}
    env.update(
        GATEWAY_LISTEN=f"127.0.0.1:{port}",
        GATEWAY_CONFIG=str(settings_path),
        GATEWAY_DB=str(database),
        HTTP_PROXY=f"http://127.0.0.1:{proxy.server_port}",
        NO_PROXY="localhost,127.0.0.1",
    )
    log = test.log("gateway.log")

    ctx = ProxyTestContext(binary, output, test, port, host, upstream, proxy, socks, env, base, settings_path, log)
    try:
        run_proxy_routing_and_switching_cases(ctx)
        run_proxy_persistence_and_platform_cases(ctx)
    finally:
        ctx.stop()
        if ctx.registry:
            import winreg
            with winreg.CreateKey(winreg.HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Internet Settings") as key:
                for name, value, kind in ctx.registry:
                    if kind is None:
                        winreg.DeleteValue(key, name)
                    else:
                        winreg.SetValueEx(key, name, 0, kind, value)
        (output / "routes.json").write_text(json.dumps(upstream.captures, indent=2), encoding="utf-8")
        (output / "socks-connections.json").write_text(json.dumps(socks.socks_connections, indent=2), encoding="utf-8")


if __name__ == "__main__":
    suite_cli("proxy", run)

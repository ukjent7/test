"""Real gateway, local upstream and recording proxy; CI-only repeatable routing evidence."""

import argparse
import base64
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import select
import socket
import socketserver
import subprocess
import threading
import time
from urllib.parse import urlsplit


def run(binary, output):
    output.mkdir(parents=True, exist_ok=True)
    report = {"status": "running", "cases": [], "commit": os.environ.get("GITHUB_SHA"),
              "gateway_sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
    captures = []
    socks_connections = []
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.connect(("192.0.2.1", 80))
        host = sock.getsockname()[0]

    class Upstream(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def respond(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            captures.append({"path": self.path, "via": self.headers.get("X-Fixture-Proxy", "direct"),
                             "body": body.decode(), "proxy_auth_leaked": "Proxy-Authorization" in self.headers})
            assert "Proxy-Authorization" not in self.headers
            payload = {"object": "list", "data": [{"id": "fixture", "object": "model"}]} if self.command == "GET" else {"usage": {"input_tokens": 10, "output_tokens": 2}, "content": []}
            stream = self.command == "POST" and json.loads(body).get("stream")
            wire = b'data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}\n\ndata: [DONE]\n\n' if stream else json.dumps(payload).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream" if stream else "application/json")
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)

        do_GET = respond
        do_POST = respond

    upstream = ThreadingHTTPServer(("0.0.0.0", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    class Proxy(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def forward(self):
            target = urlsplit(self.path)
            assert target.hostname == host and target.port == upstream.server_port, target
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            headers = {key: value for key, value in self.headers.items()
                       if key.lower() not in ("proxy-authorization", "proxy-connection", "connection")}
            headers["X-Fixture-Proxy"] = "proxy"
            auth = self.headers.get("Proxy-Authorization")
            if self.server.require_auth:
                assert auth == "Basic " + base64.b64encode(b"fixture:secret").decode(), auth
            connection = http.client.HTTPConnection(host, upstream.server_port, timeout=10)
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

    proxy = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
    proxy.require_auth = False
    threading.Thread(target=proxy.serve_forever, daemon=True).start()

    class Socks(socketserver.BaseRequestHandler):
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
            assert target == host and target_port == upstream.server_port
            socks_connections.append({"host": target, "port": target_port})
            with socket.create_connection((target, target_port), timeout=10) as connection:
                self.request.sendall(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
                peers = [self.request, connection]
                while ready := select.select(peers, [], [], 10)[0]:
                    for peer in ready:
                        data = peer.recv(65536)
                        if not data:
                            return
                        (connection if peer is self.request else self.request).sendall(data)

    socks = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Socks)
    socks.daemon_threads = True
    threading.Thread(target=socks.serve_forever, daemon=True).start()
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    settings_path = output.resolve() / "settings.json"
    base = f"http://{host}:{upstream.server_port}/v1"
    settings = {"upstream_base_url": base, "opencode_base_url": base, "enabled": True}
    settings_path.write_text(json.dumps(settings), encoding="utf-8")
    database = output.resolve() / "requests.sqlite3"
    database.unlink(missing_ok=True)
    env = {key: value for key, value in os.environ.items() if "proxy" not in key.lower() and not key.startswith("GATEWAY_")}
    env.update(GATEWAY_LISTEN=f"127.0.0.1:{port}", GATEWAY_CONFIG=str(settings_path), GATEWAY_DB=str(database),
               HTTP_PROXY=f"http://127.0.0.1:{proxy.server_port}", NO_PROXY="localhost,127.0.0.1")
    process = None
    registry = []
    log = (output / "gateway.log").open("wb")

    def http(method, path, payload=None):
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
        connection.request(method, path, json.dumps(payload) if payload is not None else None,
                           {"Content-Type": "application/json"})
        response = connection.getresponse()
        try:
            return response.status, response.read()
        finally:
            connection.close()

    def status():
        code, wire = http("GET", "/ui/status")
        assert code == 200
        return json.loads(wire)

    def launch():
        nonlocal process
        process = subprocess.Popen([str(binary), "--headless"], env=env, stdout=log, stderr=log)
        for _ in range(200):
            assert process.poll() is None, "gateway exited"
            try:
                if http("GET", "/health")[0] == 200:
                    return
            except OSError:
                time.sleep(0.05)
        raise RuntimeError("gateway did not become ready")

    def stop():
        process.terminate()
        process.wait(timeout=10)

    def update(**changes):
        assert http("POST", "/ui/settings", {"upstream_base_url": base, **changes})[0] == 204

    def request(model, via, endpoint="messages", stream=False):
        before = len(captures)
        code, wire = http("POST", f"/v1/{endpoint}", {"model": model, "messages": [], "stream": stream})
        assert code == 200, wire
        assert len(captures) == before + 1 and captures[-1]["via"] == via, captures[-1:]
        if stream:
            assert wire.endswith(b"data: [DONE]\n\n")

    def catalog(expected):
        before = len(captures)
        code, wire = http("GET", "/v1/models")
        assert code == 200 and len(json.loads(wire)["data"]) == 2, wire
        assert sorted(row["via"] for row in captures[before:]) == sorted(expected)

    def passed(name):
        report["cases"].append({"name": name, "status": "passed"})

    try:
        launch()
        state = status()
        assert state["proxy"] == "" and state["stepfun_use_proxy"] and state["opencode_use_proxy"]
        request("stepfun/fixture", "proxy")
        request("opencode/fixture", "proxy", "chat/completions", True)
        catalog(["proxy", "proxy"])
        passed("old-settings-default-to-system-and-proxy-stream-and-model-list")
        update(stepfun_use_proxy=False)
        request("stepfun/fixture", "direct")
        request("opencode/fixture", "proxy", "responses")
        catalog(["direct", "proxy"])
        update(stepfun_use_proxy=True, opencode_use_proxy=False)
        request("stepfun/fixture", "proxy")
        request("opencode/fixture", "direct")
        catalog(["proxy", "direct"])
        passed("per-provider-switches-change-routing-without-restart-or-pool-leakage")
        update(proxy="direct", opencode_use_proxy=True)
        request("fixture", "direct")
        request("opencode/fixture", "direct")
        update(proxy=f"http://fixture:secret@127.0.0.1:{proxy.server_port}")
        proxy.require_auth = True
        request("fixture", "proxy")
        request("opencode/fixture", "proxy")
        passed("global-direct-and-custom-authenticated-proxy-override-system")
        preserved = settings_path.read_bytes()
        for invalid in ("file:///tmp/proxy", "http://", "bad-proxy", "ftp://127.0.0.1:7890"):
            assert http("POST", "/ui/settings", {"upstream_base_url": base, "proxy": invalid})[0] == 400
            assert settings_path.read_bytes() == preserved
        request("fixture", "proxy")
        passed("invalid-proxy-preserves-settings-and-working-clients")
        for scheme in ("socks5", "socks5h"):
            update(proxy=f"{scheme}://127.0.0.1:{socks.server_address[1]}")
            before = len(socks_connections)
            request("fixture", "direct")
            assert len(socks_connections) == before + 1
        update(proxy=f"127.0.0.1:{proxy.server_port}")
        proxy.require_auth = False
        request("fixture", "proxy")
        passed("socks5-socks5h-and-http-host-port-proxy-addresses")
        stop()
        env["HTTP_PROXY"] = "http://127.0.0.1:1"
        launch()
        request("fixture", "proxy")
        update(stepfun_use_proxy=False)
        request("fixture", "direct")
        update(proxy="", opencode_use_proxy=False)
        request("opencode/fixture", "direct")
        update(upstream_base_url=f"http://127.0.0.1:{upstream.server_port}/v1", stepfun_use_proxy=True)
        request("fixture", "direct")
        passed("restart-preserves-custom-and-provider-direct-and-loopback-bypasses-dead-proxy")
        stop()
        if os.name == "nt":
            import winreg
            with winreg.CreateKey(winreg.HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Internet Settings") as key:
                for name, value, kind in (("ProxyEnable", 1, winreg.REG_DWORD),
                                          ("ProxyServer", f"127.0.0.1:{proxy.server_port}", winreg.REG_SZ),
                                          ("ProxyOverride", "localhost;127.0.0.1", winreg.REG_SZ)):
                    try:
                        old_value, old_kind = winreg.QueryValueEx(key, name)
                        registry.append((name, old_value, old_kind))
                    except FileNotFoundError:
                        registry.append((name, None, None))
                    winreg.SetValueEx(key, name, 0, kind, value)
            env.pop("HTTP_PROXY")
            proxy.require_auth = False
            launch()
            update(upstream_base_url=base, opencode_use_proxy=True)
            request("fixture", "proxy")
            request("opencode/fixture", "proxy")
            passed("windows-internet-options-used-without-proxy-environment")
        report["status"] = "passed"
    except BaseException as error:
        report.update(status="failed", error=repr(error))
        raise
    finally:
        if process is not None and process.poll() is None:
            stop()
        if registry:
            import winreg
            with winreg.CreateKey(winreg.HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Internet Settings") as key:
                for name, value, kind in registry:
                    if kind is None:
                        winreg.DeleteValue(key, name)
                    else:
                        winreg.SetValueEx(key, name, 0, kind, value)
        for server in (proxy, upstream, socks):
            server.shutdown()
            server.server_close()
        log.close()
        (output / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        (output / "routes.json").write_text(json.dumps(captures, indent=2), encoding="utf-8")
        (output / "socks-connections.json").write_text(json.dumps(socks_connections, indent=2), encoding="utf-8")
        manifest = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                    for path in output.iterdir() if path.is_file() and path.name != "sha256.json"}
        (output / "sha256.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(f"Proxy E2E passed: {len(report['cases'])} cases; evidence: {output}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--gateway", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("artifacts/proxy"))
    args = parser.parse_args()
    run(args.gateway.resolve(), args.output)

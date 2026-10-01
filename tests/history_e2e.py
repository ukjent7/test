"""Real gateway -> real HTTP fixtures -> SQLite and byte-exact history evidence; CI only."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import http.client as http_client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import threading
import time


def run(binary, output):
    output.mkdir(parents=True, exist_ok=True)
    report = {"status": "running", "cases": [], "commit": os.environ.get("GITHUB_SHA"),
              "gateway_sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
    captures = {}
    release = threading.Event()
    error_wire = b'{ "error" : {"message": "fixture error \\t \\n"} }\r\n'
    normal_wire = b'{ "type": "message", "content": [] } \t\r\n'
    sse_wire = b': heartbeat\r\ndata: { "type": "content_block_start", "index": 0, "content_block": {"type": "thinking"} }\r\n\r\n'

    class Upstream(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            body = self.rfile.read(int(self.headers["content-length"]))
            model = json.loads(body)["model"]
            captures[model] = {"target": self.path, "headers": dict(self.headers), "body": list(body)}
            status, wire = (429, error_wire) if model == "history-error" else (200, normal_wire)
            if model.startswith("history-sse"):
                wire = sse_wire
            if model == "history-binary":
                wire = b"\xff\x00\xfe \t\r\n"
            self.send_response(status)
            self.send_header("Content-Type", "text/event-stream" if model.startswith("history-sse") else "application/json")
            self.send_header("Content-Length", str(len(wire) + (100 if model in ("history-sse-truncated", "history-sse-cancel") else 0)))
            self.send_header("X-Fixture", "header  spaces")
            self.send_header("Set-Cookie", "PRIVATE_RESPONSE_COOKIE")
            self.end_headers()
            if model == "history-sse-cancel":
                self.wfile.write(wire)
                self.wfile.flush()
                release.wait(10)
            else:
                for offset in range(0, len(wire), 3):
                    self.wfile.write(wire[offset:offset + 3])
                    self.wfile.flush()
            if model in ("history-sse-truncated", "history-sse-cancel"):
                self.close_connection = True

    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    db = output.resolve() / "requests.sqlite3"
    db.unlink(missing_ok=True)
    (output / "settings.json").unlink(missing_ok=True)
    env = {**os.environ, "GATEWAY_LISTEN": f"127.0.0.1:{port}",
           "GATEWAY_CONFIG": str(output.resolve() / "settings.json"), "GATEWAY_DB": str(db),
           "GATEWAY_UPSTREAM_BASE_URL": f"http://127.0.0.1:{upstream.server_port}/v1",
           "GATEWAY_STEPFUN_API_KEY": "PRIVATE_CONFIGURED_KEY"}
    log = (output / "gateway.log").open("wb")
    process = None

    def http(method, path, body=None):
        connection = http_client.HTTPConnection("127.0.0.1", port, timeout=10)
        connection.request(method, path, body, {"Content-Type": "application/json",
            "Authorization": "Bearer PRIVATE_CLIENT_KEY", "Cookie": "PRIVATE_CLIENT_COOKIE",
            "Connection": "keep-alive, x-remove", "x-remove": "remove  spaces"})
        response = connection.getresponse()
        try:
            wire = response.read()
            return response.status, wire
        finally:
            connection.close()

    def get(path):
        status, wire = http("GET", path)
        assert status == 200, wire
        return json.loads(wire)

    def launch():
        nonlocal process
        process = subprocess.Popen([str(binary), "--headless"], env=env, stdout=log, stderr=log)
        for _ in range(200):
            if process.poll() is not None:
                raise RuntimeError("gateway exited during startup")
            try:
                if http("GET", "/health")[0] == 200:
                    return
            except OSError:
                time.sleep(0.05)
        raise RuntimeError("gateway did not become ready")

    def settled():
        for _ in range(200):
            state = get("/ui/status")
            if state["stats"]["active"] == 0:
                assert state["history_error"] is None, state
                return state
            time.sleep(0.02)
        raise RuntimeError("requests did not complete")

    def detail(model):
        state = settled()
        call = next(call for call in state["calls"] if call["model"] == model)
        value = get(f"/ui/calls/{call['id']}")
        assert all(key not in call for key in ("exchange", "diff"))
        for secret in ("PRIVATE_CLIENT_KEY", "PRIVATE_CONFIGURED_KEY", "PRIVATE_CLIENT_COOKIE", "PRIVATE_RESPONSE_COOKIE"):
            assert secret not in json.dumps(value), secret
        (output / f"{model}.json").write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")
        return value

    def passed(name):
        report["cases"].append({"name": name, "status": "passed"})

    try:
        process = subprocess.Popen([str(binary), "--headless"], env={**env, "GATEWAY_DB": str(output.resolve())},
                                   stdout=log, stderr=log)
        assert process.wait(timeout=10) != 0, "invalid database path was silently ignored"
        passed("database-open-failure-is-explicit")
        launch()
        with sqlite3.connect(db) as connection:
            connection.execute("BEGIN IMMEDIATE")
            assert http("POST", "/v1/messages", b'{"model":"history-storage-failed"}')[0] == 200
            assert get("/ui/status")["history_error"], "failed persistence must be visible"
            connection.rollback()
        assert http("POST", "/v1/messages", b'{"model":"history-storage-recovered"}')[0] == 200
        assert settled()["history_error"] is None
        passed("database-write-failure-visible-and-recoverable")
        original = b'{ \t"model" : "stepfun/history-whitespace", "stream": false, "messages": [] }\r\n'
        status, wire = http("POST", "/v1/messages?space=a%20b", original)
        assert status == 200 and wire == normal_wire
        value = detail("stepfun/history-whitespace")
        exchange = value["exchange"]
        assert bytes(exchange["request"]["body"]) == original
        attempt = exchange["attempts"][0]
        assert attempt["request"]["body"] == captures["history-whitespace"]["body"]
        assert attempt["request"]["target"].endswith("/v1/messages?space=a%20b")
        assert attempt["response"]["body"] == list(normal_wire)
        assert exchange["response"]["body"] == list(wire)
        assert exchange["response"]["complete"] and attempt["response"]["complete"]
        before_headers, after_headers = dict(exchange["request"]["headers"]), dict(attempt["request"]["headers"])
        assert "x-remove" in before_headers and "x-remove" not in after_headers
        assert before_headers["authorization"] != after_headers["authorization"]
        assert "已隐藏" in before_headers["authorization"]
        (output / "whitespace-before.bin").write_bytes(original)
        (output / "whitespace-after.bin").write_bytes(bytes(attempt["request"]["body"]))
        passed("byte-exact-whitespace-url-headers-and-auth-changes")

        status, wire = http("POST", "/v1/messages", b"{ invalid JSON \t\r\n")
        assert status == 400
        value = detail("未知模型")
        assert value["call"]["status"] == 400 and not value["exchange"]["attempts"]
        assert value["exchange"]["request"]["body"] == list(b"{ invalid JSON \t\r\n")
        assert value["exchange"]["response"]["body"] == list(wire)
        assert value["exchange"]["error"]
        passed("invalid-client-json-is-retained")

        status, wire = http("POST", "/v1/messages", b'{"model":"history-error"}')
        assert status == 429 and wire == error_wire
        value = detail("history-error")
        assert value["exchange"]["response"]["body"] == list(error_wire)
        assert value["exchange"]["attempts"][0]["response"]["status"] == 429
        passed("http-error-body-and-status-retained")

        status, wire = http("POST", "/v1/chat/completions", b'{"model":"history-binary"}')
        assert status == 200
        assert detail("history-binary")["exchange"]["response"]["body"] == list(wire)
        (output / "binary.bin").write_bytes(wire)
        passed("binary-body-is-lossless")

        status, wire = http("POST", "/v1/messages", b'{"model":"history-sse"}')
        assert status == 200
        value = detail("history-sse")
        assert value["exchange"]["attempts"][0]["response"]["body"] == list(sse_wire)
        assert value["exchange"]["response"]["body"] == list(wire) and wire != sse_wire
        assert json.loads(wire.split(b"data: ", 1)[1])["content_block"]["signature"] == ""
        passed("fragmented-sse-preserves-all-original-and-forwarded-bytes")

        try:
            http("POST", "/v1/messages", b'{"model":"history-sse-truncated"}')
        except (http_client.HTTPException, OSError):
            pass
        else:
            raise AssertionError("truncated stream appeared complete")
        value = detail("history-sse-truncated")
        assert value["call"]["result"] == "失败"
        assert not value["exchange"]["attempts"][0]["response"]["complete"]
        assert not value["exchange"]["response"]["complete"] and value["exchange"]["error"]
        passed("truncated-stream-partial-bytes-and-error")

        connection = http_client.HTTPConnection("127.0.0.1", port, timeout=10)
        connection.request("POST", "/v1/messages", b'{"model":"history-sse-cancel"}',
                           {"Content-Type": "application/json"})
        response = connection.getresponse()
        first = b""
        while not first.endswith(b"\r\n\r\n"):
            first += response.read(1)
        response.close()
        connection.close()
        release.set()
        value = detail("history-sse-cancel")
        assert value["call"]["result"] in ("取消", "失败")
        assert value["exchange"]["response"]["body"] and not value["exchange"]["response"]["complete"]
        passed("cancelled-stream-retains-partial-history")

        http("POST", "/ui/settings", json.dumps({"upstream_base_url": "http://127.0.0.1:1/v1"}).encode())
        assert http("POST", "/v1/messages", b'{"model":"history-connect-error"}')[0] == 502
        value = detail("history-connect-error")
        assert value["exchange"]["error"] and len(value["exchange"]["attempts"]) == 1
        assert value["exchange"]["attempts"][0]["response"] is None
        passed("connection-failure-retains-request-and-error")
        http("POST", "/ui/settings", json.dumps({"upstream_base_url": env["GATEWAY_UPSTREAM_BASE_URL"]}).encode())

        first_id = get("/ui/status")["calls"][-1]["id"]
        for index in range(103):
            assert http("POST", "/v1/messages", json.dumps({"model": f"history-fill-{index}"}).encode())[0] == 200
        with ThreadPoolExecutor(max_workers=8) as pool:
            statuses = list(pool.map(lambda index: http("POST", "/v1/messages",
                json.dumps({"model": f"history-concurrent-{index}"}).encode())[0], range(12)))
        assert statuses == [200] * 12
        assert http("POST", "/v1/messages", b'{"model":"history-error"}')[0] == 429
        before_restart = settled()
        assert len(before_restart["calls"]) == 100
        assert len({call["id"] for call in before_restart["calls"]}) == 100
        assert http("GET", f"/ui/calls/{first_id}")[0] == 404
        with sqlite3.connect(db) as connection:
            assert connection.execute("select count(*) from calls").fetchone()[0] == 100
            assert connection.execute("pragma integrity_check").fetchone()[0] == "ok"
        passed("concurrent-records-and-transactional-100-row-retention")
        process.terminate()
        process.wait(timeout=10)
        launch()
        after_restart = settled()
        assert after_restart["calls"] == before_restart["calls"]
        value = detail("history-error")
        assert value["exchange"]["response"]["body"] == list(error_wire)
        assert http("POST", "/v1/messages", b'{"model":"history-after-restart"}')[0] == 200
        assert settled()["calls"][0]["id"] > before_restart["calls"][0]["id"]
        passed("restart-preserves-details-and-monotonic-ids")
        with sqlite3.connect(db) as source, sqlite3.connect(output / "history-backup.sqlite3") as backup:
            source.backup(backup)
        (output / "upstream-captures.json").write_text(json.dumps(captures, indent=2), encoding="utf-8")
        report["status"] = "passed"
    except BaseException as error:
        report.update(status="failed", error=repr(error))
        raise
    finally:
        release.set()
        if process is not None and process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
        upstream.shutdown()
        upstream.server_close()
        log.close()
        (output / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        manifest = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                    for path in sorted(output.iterdir()) if path.is_file() and path.name != "sha256.json"}
        (output / "sha256.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(f"History E2E passed: {len(report['cases'])} cases; evidence: {output}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--gateway", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("artifacts/history"))
    args = parser.parse_args()
    run(args.gateway.resolve(), args.output)

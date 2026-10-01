"""Real gateway -> real HTTP fixtures -> SQLite and byte-exact history evidence; CI only."""

from concurrent.futures import ThreadPoolExecutor
import http.client as http_client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import shutil
import sqlite3
import threading
import time

from common import free_port, stop_process, suite_cli, wait_ready


def run(binary, output, test):
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
            if model.startswith("usage-"):
                usage = {
                    "usage-shared": {"input_tokens": 100, "output_tokens": 40,
                                     "cache_read_input_tokens": 700, "cache_creation_input_tokens": 200},
                    "usage-zero": {"input_tokens": 0, "output_tokens": 0,
                                   "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
                    "usage-unknown": {"input_tokens": 500},
                    "usage-other": {"input_tokens": 500, "output_tokens": 30,
                                    "input_tokens_details": {"cached_tokens": 100}},
                }.get(model, {})
                if self.path.endswith("chat/completions"):
                    usage = {"prompt_tokens": 2000, "completion_tokens": 60,
                             "prompt_tokens_details": {"cached_tokens": 500}}
                wire = json.dumps({"usage": usage}).encode()
                if model == "usage-stream":
                    wire = b"".join(b"data: " + json.dumps(event).encode() + b"\n\n" for event in [
                        {"type": "message_start", "message": {"usage": {"input_tokens": 50,
                         "output_tokens": 0, "cache_read_input_tokens": 150, "cache_creation_input_tokens": 0}}},
                        {"type": "message_delta", "usage": {"output_tokens": 25}},
                        {"type": "message_delta", "usage": {"output_tokens": 25}},
                    ])
            self.send_response(status)
            self.send_header("Content-Type", "text/event-stream" if model.startswith("history-sse") or model == "usage-stream" else "application/json")
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
    test.server(upstream)
    port = free_port()
    db = output.resolve() / "requests.sqlite3"
    db.unlink(missing_ok=True)
    (output / "settings.json").unlink(missing_ok=True)
    env = {**os.environ, "GATEWAY_LISTEN": f"127.0.0.1:{port}",
           "GATEWAY_CONFIG": str(output.resolve() / "settings.json"), "GATEWAY_DB": str(db),
           "GATEWAY_UPSTREAM_BASE_URL": f"http://127.0.0.1:{upstream.server_port}/v1",
           "GATEWAY_OPENCODE_BASE_URL": f"http://127.0.0.1:{upstream.server_port}/v1",
           "GATEWAY_STEPFUN_API_KEY": "PRIVATE_CONFIGURED_KEY"}
    log = test.log("gateway.log")
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

    def launch(executable=binary, environment=env, cwd=None):
        nonlocal process
        process = test.spawn([str(executable), "--headless"], env=environment, cwd=cwd, stdout=log, stderr=log)
        wait_ready(process, port)

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
        (output / f"{model.replace('/', '-')}.json").write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")
        return value

    passed = test.passed

    try:
        process = test.spawn([str(binary), "--headless"], env={**env, "GATEWAY_DB": str(output.resolve())},
                                   stdout=log, stderr=log)
        assert process.wait(timeout=10) != 0, "invalid database path was silently ignored"
        passed("database-open-failure-is-explicit")
        launch()
        empty_usage = get("/ui/usage")
        assert empty_usage["total"]["requests"] == 0 and empty_usage["total"]["cache_hit_rate"] is None
        assert empty_usage["providers"] == [] and empty_usage["models"] == []
        with sqlite3.connect(db) as connection:
            connection.execute("BEGIN IMMEDIATE")
            assert http("POST", "/v1/messages", b'{"model":"history-storage-failed"}')[0] == 200
            assert get("/ui/status")["history_error"], "failed persistence must be visible"
            connection.rollback()
        assert http("POST", "/v1/messages", b'{"model":"history-storage-recovered"}')[0] == 200
        assert settled()["history_error"] is None
        passed("database-write-failure-visible-and-recoverable")
        baseline = get("/ui/usage")["total"]["requests"]
        for endpoint, model in [("messages", "stepfun/usage-shared"),
                                ("chat/completions", "opencode/usage-shared"),
                                ("responses", "opencode/usage-other"),
                                ("messages", "usage-unknown"), ("messages", "usage-zero"),
                                ("messages", "usage-stream")]:
            assert http("POST", f"/v1/{endpoint}", json.dumps({"model": model, "stream": model == "usage-stream"}).encode())[0] == 200
        settled()
        usage = get("/ui/usage")
        total = usage["total"]
        assert total["requests"] == baseline + 6
        assert (total["input_tokens"], total["output_tokens"], total["cache_read_tokens"], total["cache_write_tokens"]) == (4200, 155, 1450, 200)
        assert abs(total["cache_hit_rate"] - 1450 / 3700 * 100) < 1e-9
        assert total["cache_reported_requests"] == 5
        shared = [row for row in usage["models"] if row["model"] == "usage-shared"]
        assert {row["provider"] for row in shared} == {"stepfun", "opencode"}
        unknown = next(row for row in usage["models"] if row["model"] == "usage-unknown")
        assert unknown["cache_hit_rate"] is None and unknown["output_tokens"] is None
        zero = next(row for row in usage["models"] if row["model"] == "usage-zero")
        assert zero["cache_read_tokens"] == 0 and zero["cache_reported_requests"] == 1 and zero["cache_hit_rate"] is None
        stream = next(row for row in usage["models"] if row["model"] == "usage-stream")
        assert stream["input_tokens"] == 200 and stream["output_tokens"] == 25
        assert sum(row["requests"] for row in usage["providers"]) == total["requests"]
        assert get("/ui/usage?since=9999999999999")["total"]["requests"] == 0
        assert http("GET", "/ui/usage?since=invalid")[0] == 400
        (output / "usage-protocols.json").write_text(json.dumps(usage, indent=2), encoding="utf-8")
        passed("usage-three-levels-native-protocols-stream-cumulative-zero-and-unknown")
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
        usage_before_restart = get("/ui/usage")
        assert usage_before_restart["total"]["input_tokens"] == 4200
        assert usage_before_restart["total"]["requests"] > 100
        assert len(before_restart["calls"]) == 100
        assert len({call["id"] for call in before_restart["calls"]}) == 100
        assert http("GET", f"/ui/calls/{first_id}")[0] == 404
        with sqlite3.connect(db) as connection:
            assert connection.execute("select count(*) from calls").fetchone()[0] == 100
            assert connection.execute("pragma integrity_check").fetchone()[0] == "ok"
        passed("concurrent-records-and-transactional-100-row-retention")
        stop_process(process)
        launch()
        after_restart = settled()
        assert get("/ui/usage") == usage_before_restart
        (output / "usage-retention-restart.json").write_text(json.dumps(usage_before_restart, indent=2), encoding="utf-8")
        passed("usage-survives-100-row-retention-and-restart-without-double-counting")
        assert after_restart["calls"] == before_restart["calls"]
        value = detail("history-error")
        assert value["exchange"]["response"]["body"] == list(error_wire)
        assert http("POST", "/v1/messages", b'{"model":"history-after-restart"}')[0] == 200
        assert settled()["calls"][0]["id"] > before_restart["calls"][0]["id"]
        passed("restart-preserves-details-and-monotonic-ids")
        with sqlite3.connect(db) as source, sqlite3.connect(output / "history-backup.sqlite3") as backup:
            source.backup(backup)
        (output / "upstream-captures.json").write_text(json.dumps(captures, indent=2), encoding="utf-8")
        stop_process(process)
        anchor, day = 1700000000000, 86400000
        with sqlite3.connect(db) as connection:
            connection.execute("DROP TABLE usage")
            connection.execute("UPDATE calls SET summary = json_set(summary, '$.timestamp', 0)")
            ids = [row[0] for row in connection.execute("SELECT id FROM calls ORDER BY id DESC LIMIT 4")]
            for call_id, days in zip(ids, (0, 6, 29, 30)):
                connection.execute("UPDATE calls SET summary = json_remove(json_set(summary, '$.timestamp', ?, '$.cache.input_tokens', 100, '$.cache.cache_read_tokens', 50), '$.cache.output_tokens') WHERE id = ?",
                                   (anchor - days * day, call_id))
        launch()
        migration = get("/ui/usage")
        assert migration["total"]["requests"] == 100 and migration["total"]["output_tokens"] is None
        windows = {str(days): get(f"/ui/usage?since={anchor - days * day}") for days in (0, 6, 29)}
        assert [value["total"]["requests"] for value in windows.values()] == [1, 2, 3]
        assert all(value["total"]["cache_hit_rate"] == 50 for value in windows.values())
        stop_process(process)
        launch()
        assert get("/ui/usage") == migration
        (output / "usage-upgrade-and-windows.json").write_text(json.dumps({"anchor": anchor, "windows": windows, "all": migration}, indent=2), encoding="utf-8")
        passed("legacy-history-backfill-idempotent-local-day-window-boundaries")
        stop_process(process)
        portable = output.resolve() / "便携 网关"
        portable.mkdir(exist_ok=True)
        portable_binary = portable / binary.name
        shutil.copy2(binary, portable_binary)
        launch_cwd = output.resolve() / "other-launch-directory"
        launch_cwd.mkdir(exist_ok=True)
        outside = output.resolve() / "unused-user-directory"
        portable_env = {**env, "APPDATA": str(outside), "LOCALAPPDATA": str(outside),
                        "XDG_CONFIG_HOME": str(outside), "XDG_DATA_HOME": str(outside)}
        for name in ("GATEWAY_CONFIG", "GATEWAY_DB"):
            portable_env.pop(name, None)
        for name in ("settings.json", "requests.sqlite3"):
            (portable / name).unlink(missing_ok=True)
        launch(portable_binary, portable_env, launch_cwd)
        assert (portable / "requests.sqlite3").is_file()
        assert http("POST", "/ui/settings", json.dumps({"upstream_base_url": env["GATEWAY_UPSTREAM_BASE_URL"]}).encode())[0] == 204
        assert (portable / "settings.json").is_file()
        assert http("POST", "/v1/messages", b'{"model":"portable-history"}')[0] == 200
        portable_calls = settled()["calls"]
        assert len(portable_calls) == 1
        stop_process(process)
        launch(portable_binary, portable_env, launch_cwd)
        assert settled()["calls"] == portable_calls
        assert not list(launch_cwd.iterdir()) and not outside.exists()
        (output / "portable-paths.json").write_text(json.dumps({"program": str(portable_binary),
            "cwd": str(launch_cwd), "settings": str(portable / "settings.json"),
            "database": str(portable / "requests.sqlite3"), "calls": portable_calls}, indent=2), encoding="utf-8")
        passed("portable-defaults-ignore-cwd-and-user-directories-and-survive-restart")
    finally:
        release.set()


if __name__ == "__main__":
    suite_cli("history", run)

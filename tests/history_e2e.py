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

ERROR_WIRE = b'{ "error" : {"message": "fixture error \\t \\n"} }\r\n'
NORMAL_WIRE = b'{ "type": "message", "content": [] } \t\r\n'
SSE_WIRE = b': heartbeat\r\ndata: { "type": "content_block_start", "index": 0, "content_block": {"type": "thinking"} }\r\n\r\n'


class HistoryUpstream(BaseHTTPRequestHandler):
    """Mock upstream server recording requests and producing test responses for history verification."""

    def log_message(self, *_args):
        pass

    @property
    def captures(self):
        return self.server.captures

    @property
    def release(self):
        return self.server.release

    def do_POST(self):
        body = self.rfile.read(int(self.headers["content-length"]))
        model = json.loads(body)["model"]
        self.captures[model] = {"target": self.path, "headers": dict(self.headers), "body": list(body)}
        status, wire = (429, ERROR_WIRE) if model == "history-error" else (200, NORMAL_WIRE)
        if model.startswith("history-sse"):
            wire = SSE_WIRE
        if model == "history-binary":
            wire = b"\xff\x00\xfe \t\r\n"
        if model.startswith("usage-"):
            usage = {
                "usage-shared": {"input_tokens": 100, "output_tokens": 40, "cache_read_input_tokens": 700, "cache_creation_input_tokens": 200},
                "usage-zero": {"input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
                "usage-unknown": {"input_tokens": 500},
                "usage-other": {"input_tokens": 500, "output_tokens": 30, "input_tokens_details": {"cached_tokens": 100}},
            }.get(model, {})
            if self.path.endswith("chat/completions"):
                usage = {"prompt_tokens": 2000, "completion_tokens": 60, "prompt_tokens_details": {"cached_tokens": 500}}
            wire = json.dumps({"usage": usage}).encode()
            if model == "usage-stream":
                wire = b"".join(b"data: " + json.dumps(event).encode() + b"\n\n" for event in [
                    {"type": "message_start", "message": {"usage": {"input_tokens": 50, "output_tokens": 0, "cache_read_input_tokens": 150, "cache_creation_input_tokens": 0}}},
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
            self.release.wait(10)
        else:
            for offset in range(0, len(wire), 3):
                self.wfile.write(wire[offset:offset + 3])
                self.wfile.flush()
        if model in ("history-sse-truncated", "history-sse-cancel"):
            self.close_connection = True


class HistoryTestContext:
    """Manages history test state, gateway process lifecycle, and test assertions."""

    def __init__(self, binary, output, test, port, upstream, env, db, log):
        self.binary = binary
        self.output = output
        self.test = test
        self.port = port
        self.upstream = upstream
        self.env = env
        self.db = db
        self.log = log
        self.process = None

    def passed(self, name):
        self.test.passed(name)

    def launch(self, executable=None, environment=None, cwd=None):
        binary = executable or self.binary
        env = environment or self.env
        self.process = self.test.spawn([str(binary), "--headless"], env=env, cwd=cwd, stdout=self.log, stderr=self.log)
        wait_ready(self.process, self.port)

    def stop(self):
        if self.process is not None and self.process.poll() is None:
            stop_process(self.process)
            self.process = None

    def http(self, method, path, body=None):
        connection = http_client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        connection.request(
            method,
            path,
            body,
            {
                "Content-Type": "application/json",
                "Authorization": "Bearer PRIVATE_CLIENT_KEY",
                "Cookie": "PRIVATE_CLIENT_COOKIE",
                "Connection": "keep-alive, x-remove",
                "x-remove": "remove  spaces",
            },
        )
        response = connection.getresponse()
        try:
            wire = response.read()
            return response.status, wire
        finally:
            connection.close()

    def get(self, path):
        status, wire = self.http("GET", path)
        assert status == 200, wire
        return json.loads(wire)

    def settled(self):
        for _ in range(200):
            state = self.get("/ui/status")
            if state["stats"]["active"] == 0:
                assert state["history_error"] is None, state
                return state
            time.sleep(0.02)
        raise RuntimeError("requests did not complete")

    def detail(self, model):
        state = self.settled()
        call = next(call for call in state["calls"] if call["model"] == model)
        value = self.get(f"/ui/calls/{call['id']}")
        assert all(key not in call for key in ("exchange", "diff"))
        for secret in ("PRIVATE_CLIENT_KEY", "PRIVATE_CONFIGURED_KEY", "PRIVATE_CLIENT_COOKIE", "PRIVATE_RESPONSE_COOKIE"):
            assert secret not in json.dumps(value), secret
        (self.output / f"{model.replace('/', '-')}.json").write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")
        return value


def run_database_lifecycle_cases(ctx):
    """Database initialization failures and transactional write error visibility."""
    ctx.process = ctx.test.spawn(
        [str(ctx.binary), "--headless"], env={**ctx.env, "GATEWAY_DB": str(ctx.output.resolve())}, stdout=ctx.log, stderr=ctx.log
    )
    assert ctx.process.wait(timeout=10) != 0, "invalid database path was silently ignored"
    ctx.passed("database-open-failure-is-explicit")

    ctx.launch()
    empty_usage = ctx.get("/ui/usage")
    assert empty_usage["total"]["requests"] == 0 and empty_usage["total"]["cache_hit_rate"] is None
    assert empty_usage["providers"] == [] and empty_usage["models"] == []
    with sqlite3.connect(ctx.db) as connection:
        connection.execute("BEGIN IMMEDIATE")
        assert ctx.http("POST", "/v1/messages", b'{"model":"history-storage-failed"}')[0] == 200
        assert ctx.get("/ui/status")["history_error"], "failed persistence must be visible"
        connection.rollback()
    assert ctx.http("POST", "/v1/messages", b'{"model":"history-storage-recovered"}')[0] == 200
    assert ctx.settled()["history_error"] is None
    ctx.passed("database-write-failure-visible-and-recoverable")


def run_history_capture_and_usage_cases(ctx):
    """Token usage tracking, byte-exact payload fidelity, and streaming truncation/cancellation."""
    baseline = ctx.get("/ui/usage")["total"]["requests"]
    for endpoint, model in [
        ("messages", "stepfun/usage-shared"),
        ("chat/completions", "opencode/usage-shared"),
        ("responses", "opencode/usage-other"),
        ("messages", "usage-unknown"),
        ("messages", "usage-zero"),
        ("messages", "usage-stream"),
    ]:
        assert ctx.http("POST", f"/v1/{endpoint}", json.dumps({"model": model, "stream": model == "usage-stream"}).encode())[0] == 200
    ctx.settled()
    usage = ctx.get("/ui/usage")
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
    assert ctx.get("/ui/usage?since=9999999999999")["total"]["requests"] == 0
    assert ctx.http("GET", "/ui/usage?since=invalid")[0] == 400
    (ctx.output / "usage-protocols.json").write_text(json.dumps(usage, indent=2), encoding="utf-8")
    ctx.passed("usage-three-levels-native-protocols-stream-cumulative-zero-and-unknown")

    original = b'{ \t"model" : "stepfun/history-whitespace", "stream": false, "messages": [] }\r\n'
    status, wire = ctx.http("POST", "/v1/messages?space=a%20b", original)
    assert status == 200 and wire == NORMAL_WIRE
    value = ctx.detail("stepfun/history-whitespace")
    exchange = value["exchange"]
    assert bytes(exchange["request"]["body"]) == original
    attempt = exchange["attempts"][0]
    assert attempt["request"]["body"] == ctx.upstream.captures["history-whitespace"]["body"]
    assert attempt["request"]["target"].endswith("/v1/messages?space=a%20b")
    assert attempt["response"]["body"] == list(NORMAL_WIRE)
    assert exchange["response"]["body"] == list(wire)
    assert exchange["response"]["complete"] and attempt["response"]["complete"]
    before_headers, after_headers = dict(exchange["request"]["headers"]), dict(attempt["request"]["headers"])
    assert "x-remove" in before_headers and "x-remove" not in after_headers
    assert before_headers["authorization"] != after_headers["authorization"]
    assert "已隐藏" in before_headers["authorization"]
    (ctx.output / "whitespace-before.bin").write_bytes(original)
    (ctx.output / "whitespace-after.bin").write_bytes(bytes(attempt["request"]["body"]))
    ctx.passed("byte-exact-whitespace-url-headers-and-auth-changes")

    status, wire = ctx.http("POST", "/v1/messages", b"{ invalid JSON \t\r\n")
    assert status == 400
    value = ctx.detail("未知模型")
    assert value["call"]["status"] == 400 and not value["exchange"]["attempts"]
    assert value["exchange"]["request"]["body"] == list(b"{ invalid JSON \t\r\n")
    assert value["exchange"]["response"]["body"] == list(wire)
    assert value["exchange"]["error"]
    ctx.passed("invalid-client-json-is-retained")

    status, wire = ctx.http("POST", "/v1/messages", b'{"model":"history-error"}')
    assert status == 429 and wire == ERROR_WIRE
    value = ctx.detail("history-error")
    assert value["exchange"]["response"]["body"] == list(ERROR_WIRE)
    assert value["exchange"]["attempts"][0]["response"]["status"] == 429
    ctx.passed("http-error-body-and-status-retained")

    status, wire = ctx.http("POST", "/v1/chat/completions", b'{"model":"history-binary"}')
    assert status == 200
    assert ctx.detail("history-binary")["exchange"]["response"]["body"] == list(wire)
    (ctx.output / "binary.bin").write_bytes(wire)
    ctx.passed("binary-body-is-lossless")

    status, wire = ctx.http("POST", "/v1/messages", b'{"model":"history-sse"}')
    assert status == 200
    value = ctx.detail("history-sse")
    assert value["exchange"]["attempts"][0]["response"]["body"] == list(SSE_WIRE)
    assert value["exchange"]["response"]["body"] == list(wire) and wire != SSE_WIRE
    assert json.loads(wire.split(b"data: ", 1)[1])["content_block"]["signature"] == ""
    ctx.passed("fragmented-sse-preserves-all-original-and-forwarded-bytes")

    try:
        ctx.http("POST", "/v1/messages", b'{"model":"history-sse-truncated"}')
    except (http_client.HTTPException, OSError):
        pass
    else:
        raise AssertionError("truncated stream appeared complete")
    value = ctx.detail("history-sse-truncated")
    assert value["call"]["result"] == "失败"
    assert not value["exchange"]["attempts"][0]["response"]["complete"]
    assert not value["exchange"]["response"]["complete"] and value["exchange"]["error"]
    ctx.passed("truncated-stream-partial-bytes-and-error")

    connection = http_client.HTTPConnection("127.0.0.1", ctx.port, timeout=10)
    connection.request("POST", "/v1/messages", b'{"model":"history-sse-cancel"}', {"Content-Type": "application/json"})
    response = connection.getresponse()
    first = b""
    while not first.endswith(b"\r\n\r\n"):
        first += response.read(1)
    response.close()
    connection.close()
    ctx.upstream.release.set()
    value = ctx.detail("history-sse-cancel")
    assert value["call"]["result"] in ("取消", "失败")
    assert value["exchange"]["response"]["body"] and not value["exchange"]["response"]["complete"]
    ctx.passed("cancelled-stream-retains-partial-history")

    ctx.http("POST", "/ui/settings", json.dumps({"upstream_base_url": "http://127.0.0.1:1/v1"}).encode())
    assert ctx.http("POST", "/v1/messages", b'{"model":"history-connect-error"}')[0] == 502
    value = ctx.detail("history-connect-error")
    assert value["exchange"]["error"] and len(value["exchange"]["attempts"]) == 1
    assert value["exchange"]["attempts"][0]["response"] is None
    ctx.passed("connection-failure-retains-request-and-error")
    ctx.http("POST", "/ui/settings", json.dumps({"upstream_base_url": ctx.env["GATEWAY_UPSTREAM_BASE_URL"]}).encode())


def run_history_retention_and_migration_cases(ctx):
    """100-row retention, restart durability, legacy database migration, and portable paths."""
    first_id = ctx.get("/ui/status")["calls"][-1]["id"]
    for index in range(103):
        assert ctx.http("POST", "/v1/messages", json.dumps({"model": f"history-fill-{index}"}).encode())[0] == 200
    with ThreadPoolExecutor(max_workers=8) as pool:
        statuses = list(pool.map(lambda index: ctx.http("POST", "/v1/messages", json.dumps({"model": f"history-concurrent-{index}"}).encode())[0], range(12)))
    assert statuses == [200] * 12
    assert ctx.http("POST", "/v1/messages", b'{"model":"history-error"}')[0] == 429
    before_restart = ctx.settled()
    usage_before_restart = ctx.get("/ui/usage")
    assert usage_before_restart["total"]["input_tokens"] == 4200
    assert usage_before_restart["total"]["requests"] > 100
    assert len(before_restart["calls"]) == 100
    assert len({call["id"] for call in before_restart["calls"]}) == 100
    assert ctx.http("GET", f"/ui/calls/{first_id}")[0] == 404
    with sqlite3.connect(ctx.db) as connection:
        assert connection.execute("select count(*) from calls").fetchone()[0] == 100
        assert connection.execute("pragma integrity_check").fetchone()[0] == "ok"
    ctx.passed("concurrent-records-and-transactional-100-row-retention")

    ctx.stop()
    ctx.launch()
    after_restart = ctx.settled()
    assert ctx.get("/ui/usage") == usage_before_restart
    (ctx.output / "usage-retention-restart.json").write_text(json.dumps(usage_before_restart, indent=2), encoding="utf-8")
    ctx.passed("usage-survives-100-row-retention-and-restart-without-double-counting")
    assert after_restart["calls"] == before_restart["calls"]
    value = ctx.detail("history-error")
    assert value["exchange"]["response"]["body"] == list(ERROR_WIRE)
    assert ctx.http("POST", "/v1/messages", b'{"model":"history-after-restart"}')[0] == 200
    assert ctx.settled()["calls"][0]["id"] > before_restart["calls"][0]["id"]
    ctx.passed("restart-preserves-details-and-monotonic-ids")

    with sqlite3.connect(ctx.db) as source, sqlite3.connect(ctx.output / "history-backup.sqlite3") as backup:
        source.backup(backup)
    (ctx.output / "upstream-captures.json").write_text(json.dumps(ctx.upstream.captures, indent=2), encoding="utf-8")
    ctx.stop()

    anchor, day = 1700000000000, 86400000
    with sqlite3.connect(ctx.db) as connection:
        connection.execute("DROP TABLE usage")
        connection.execute("UPDATE calls SET summary = json_set(summary, '$.timestamp', 0)")
        ids = [row[0] for row in connection.execute("SELECT id FROM calls ORDER BY id DESC LIMIT 4")]
        for call_id, days in zip(ids, (0, 6, 29, 30)):
            connection.execute(
                "UPDATE calls SET summary = json_remove(json_set(summary, '$.timestamp', ?, '$.cache.input_tokens', 100, '$.cache.cache_read_tokens', 50), '$.cache.output_tokens') WHERE id = ?",
                (anchor - days * day, call_id),
            )
    ctx.launch()
    migration = ctx.get("/ui/usage")
    assert migration["total"]["requests"] == 100 and migration["total"]["output_tokens"] is None
    windows = {str(days): ctx.get(f"/ui/usage?since={anchor - days * day}") for days in (0, 6, 29)}
    assert [value["total"]["requests"] for value in windows.values()] == [1, 2, 3]
    assert all(value["total"]["cache_hit_rate"] == 50 for value in windows.values())
    ctx.stop()
    ctx.launch()
    assert ctx.get("/ui/usage") == migration
    (ctx.output / "usage-upgrade-and-windows.json").write_text(json.dumps({"anchor": anchor, "windows": windows, "all": migration}, indent=2), encoding="utf-8")
    ctx.passed("legacy-history-backfill-idempotent-local-day-window-boundaries")
    ctx.stop()

    portable = ctx.output.resolve() / "便携 网关"
    portable.mkdir(exist_ok=True)
    portable_binary = portable / ctx.binary.name
    shutil.copy2(ctx.binary, portable_binary)
    launch_cwd = ctx.output.resolve() / "other-launch-directory"
    launch_cwd.mkdir(exist_ok=True)
    outside = ctx.output.resolve() / "unused-user-directory"
    portable_env = {
        **ctx.env,
        "APPDATA": str(outside),
        "LOCALAPPDATA": str(outside),
        "XDG_CONFIG_HOME": str(outside),
        "XDG_DATA_HOME": str(outside),
    }
    for name in ("GATEWAY_CONFIG", "GATEWAY_DB"):
        portable_env.pop(name, None)
    for name in ("settings.json", "requests.sqlite3"):
        (portable / name).unlink(missing_ok=True)
    ctx.launch(portable_binary, portable_env, launch_cwd)
    assert (portable / "requests.sqlite3").is_file()
    assert ctx.http("POST", "/ui/settings", json.dumps({"upstream_base_url": ctx.env["GATEWAY_UPSTREAM_BASE_URL"]}).encode())[0] == 204
    assert (portable / "settings.json").is_file()
    assert ctx.http("POST", "/v1/messages", b'{"model":"portable-history"}')[0] == 200
    portable_calls = ctx.settled()["calls"]
    assert len(portable_calls) == 1
    ctx.stop()
    ctx.launch(portable_binary, portable_env, launch_cwd)
    assert ctx.settled()["calls"] == portable_calls
    assert not list(launch_cwd.iterdir()) and not outside.exists()
    (ctx.output / "portable-paths.json").write_text(
        json.dumps(
            {
                "program": str(portable_binary),
                "cwd": str(launch_cwd),
                "settings": str(portable / "settings.json"),
                "database": str(portable / "requests.sqlite3"),
                "calls": portable_calls,
            },
            indent=2,
        ),
        encoding="utf-8",
    )
    ctx.passed("portable-defaults-ignore-cwd-and-user-directories-and-survive-restart")


def run(binary, output, test):
    upstream = ThreadingHTTPServer(("127.0.0.1", 0), HistoryUpstream)
    upstream.captures = {}
    upstream.release = threading.Event()
    test.server(upstream)

    port = free_port()
    db = output.resolve() / "requests.sqlite3"
    db.unlink(missing_ok=True)
    (output / "settings.json").unlink(missing_ok=True)
    env = {
        **os.environ,
        "GATEWAY_LISTEN": f"127.0.0.1:{port}",
        "GATEWAY_CONFIG": str(output.resolve() / "settings.json"),
        "GATEWAY_DB": str(db),
        "GATEWAY_UPSTREAM_BASE_URL": f"http://127.0.0.1:{upstream.server_port}/v1",
        "GATEWAY_OPENCODE_BASE_URL": f"http://127.0.0.1:{upstream.server_port}/v1",
        "GATEWAY_STEPFUN_API_KEY": "PRIVATE_CONFIGURED_KEY",
    }
    log = test.log("gateway.log")

    ctx = HistoryTestContext(binary, output, test, port, upstream, env, db, log)
    try:
        run_database_lifecycle_cases(ctx)
        run_history_capture_and_usage_cases(ctx)
        run_history_retention_and_migration_cases(ctx)
    finally:
        ctx.stop()
        upstream.release.set()


if __name__ == "__main__":
    suite_cli("history", run)

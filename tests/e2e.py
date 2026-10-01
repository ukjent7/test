"""Real HTTP upstream -> gateway binary -> Grok production wire decoder.

Run only in CI or against an already built binary. No provider credentials needed.
"""

import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = Path(__file__).resolve().parent
MISSING = json.loads((ROOT / "fixtures/missing-signature.json").read_text())
MESSAGE = {
    "id": "msg_e2e", "type": "message", "role": "assistant",
    "model": "step-5-preview", "content": [], "stop_reason": None,
    "stop_sequence": None, "usage": {"input_tokens": 411, "output_tokens": 0,
    "cache_creation_input_tokens": 0, "cache_read_input_tokens": 13824},
}
START = {"type": "message_start", "message": MESSAGE}
END = [
    {"type": "message_delta", "delta": {"stop_reason": "tool_use",
     "stop_sequence": None}, "usage": {"output_tokens": 23}},
    {"type": "message_stop"},
]
NATIVE_SSE = b'data: {"delta":"native stream"}\n\ndata: [DONE]\n\n'
ZEN_WIRE = (ROOT / "fixtures/zen-chat.sse").read_bytes()
CACHE_FIXTURES = {
    "cache-messages-hit": ("messages", {"input_tokens": 100, "cache_read_input_tokens": 700,
        "cache_creation_input_tokens": 200}, {"input_tokens": 1000, "cache_read_tokens": 700, "cache_write_tokens": 200}),
    "cache-messages-zero": ("messages", {"input_tokens": 100, "cache_read_input_tokens": 0,
        "cache_creation_input_tokens": 0}, {"input_tokens": 100, "cache_read_tokens": 0, "cache_write_tokens": 0}),
    "cache-messages-unknown": ("messages", {"input_tokens": 100},
        {"input_tokens": 100, "cache_read_tokens": None, "cache_write_tokens": None}),
    "cache-chat-hit": ("chat/completions", {"prompt_tokens": 1000,
        "prompt_tokens_details": {"cached_tokens": 700, "cache_write_tokens": 200}},
        {"input_tokens": 1000, "cache_read_tokens": 700, "cache_write_tokens": 200}),
    "cache-chat-zero": ("chat/completions", {"prompt_tokens": 1000, "prompt_tokens_details": {"cached_tokens": 0}},
        {"input_tokens": 1000, "cache_read_tokens": 0, "cache_write_tokens": None}),
    "cache-chat-unknown": ("chat/completions", {"prompt_tokens": 1000},
        {"input_tokens": 1000, "cache_read_tokens": None, "cache_write_tokens": None}),
    "cache-chat-deepseek": ("chat/completions", {"prompt_tokens": 1000, "prompt_cache_hit_tokens": 700,
        "prompt_cache_miss_tokens": 300}, {"input_tokens": 1000, "cache_read_tokens": 700, "cache_write_tokens": None}),
    "cache-responses-hit": ("responses", {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 700}},
        {"input_tokens": 1000, "cache_read_tokens": 700, "cache_write_tokens": None}),
    "cache-responses-zero": ("responses", {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 0}},
        {"input_tokens": 1000, "cache_read_tokens": 0, "cache_write_tokens": None}),
    "cache-responses-unknown": ("responses", {"input_tokens": 1000},
        {"input_tokens": 1000, "cache_read_tokens": None, "cache_write_tokens": None}),
}


def delta(index, kind, **fields):
    return {"type": "content_block_delta", "index": index,
            "delta": {"type": kind, **fields}}


def start(index, kind, **fields):
    return {"type": "content_block_start", "index": index,
            "content_block": {"type": kind, **fields}}


def stop(index):
    return {"type": "content_block_stop", "index": index}


EVENTS = [
    START, MISSING, delta(0, "thinking_delta", thinking="检查中文与工具调用"),
    delta(0, "signature_delta", signature="signed-"),
    delta(0, "signature_delta", signature="payload"), stop(0),
    start(1, "thinking", signature=None),
    delta(1, "thinking_delta", thinking="unsigned reasoning"), stop(1),
    start(2, "redacted_thinking", data="opaque-redacted"), stop(2),
    start(3, "text", text=""), delta(3, "text_delta", text="正在读取文件"), stop(3),
    start(4, "tool_use", id="tool_1", name="read_file", input={}),
    delta(4, "input_json_delta", partial_json='{"path":"'),
    delta(4, "input_json_delta", partial_json='README.md"}'), stop(4),
    {"type": "ping"}, *END,
]


def frame(event, newline="\n", multiline=False):
    payload = json.dumps(event, ensure_ascii=False, indent=2 if multiline else None)
    lines = [": upstream heartbeat", "id: event-id", "retry: 1500",
             "event: " + event["type"]]
    lines += ["data: " + line for line in payload.splitlines()]
    return (newline.join(lines) + newline * 2).encode()


def decode_sse(wire):
    values = []
    for block in wire.decode().replace("\r\n", "\n").split("\n\n"):
        data = "\n".join(line[5:].removeprefix(" ") for line in block.splitlines()
                         if line.startswith("data:"))
        if data:
            values.append(json.loads(data))
    return values


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def run(binary, checker, output):
    output.mkdir(parents=True, exist_ok=True)
    report = {"cases": [], "wire_checks": [], "status": "running",
              "commit": os.environ.get("GITHUB_SHA"),
              "gateway_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "grok_decoder_sha256": hashlib.sha256(checker.read_bytes()).hexdigest()}
    captures = {}
    attempts = {}
    model_errors = {}
    release = threading.Event()

    class Upstream(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def do_GET(self):
            provider = "opencode" if "/zen/v1/models" in self.path else "stepfun"
            captures[f"models-{provider}"] = {"path": self.path, "headers": dict(self.headers)}
            status = model_errors.get(provider, 200)
            payload = {"object": "list", "data": [{"id": "mimo-v2.5-free" if provider == "opencode" else "step-5-preview",
                       "object": "model", "created": 7, "owned_by": provider}]}
            wire = json.dumps(payload if status == 200 else {"error": "fixture"}).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)

        def do_POST(self):
            body = self.rfile.read(int(self.headers["content-length"]))
            case = self.headers.get("x-e2e-case") or json.loads(body)["model"]
            captures[case] = {"path": self.path, "headers": dict(self.headers),
                              "body": json.loads(body)}
            attempts.setdefault(case, []).append(captures[case])
            if case in CACHE_FIXTURES or case in ("cache-chat-live", "cache-messages-delta"):
                fixture = "cache-chat-hit" if case == "cache-chat-live" else (
                    "cache-messages-hit" if case == "cache-messages-delta" else case)
                endpoint, usage, _ = CACHE_FIXTURES[fixture]
                if endpoint == "messages":
                    payload = {**MESSAGE, "usage": usage}
                    events = [{"type": "message_start", "message": payload}, *json.loads(json.dumps(END))]
                    if case == "cache-messages-delta":
                        events[1]["usage"].update(input_tokens=50, cache_read_input_tokens=800,
                                                  cache_creation_input_tokens=200)
                elif endpoint == "responses":
                    payload = {"id": "resp_cache", "object": "response", "output": [], "usage": usage}
                    events = [{"type": "response.created", "response": payload},
                              {"type": "response.completed", "response": payload}]
                else:
                    payload = {"id": "chat_cache", "choices": [], "usage": usage}
                    events = [{"choices": []}, payload]
                streaming = json.loads(body).get("stream", False)
                wire = (b"".join(b"data: " + json.dumps(event, ensure_ascii=False, indent=2).encode().replace(
                    b"\n", b"\ndata: ") + b"\r\n\r\n" for event in events)
                    + (b"data: [DONE]\n\n" if endpoint == "chat/completions" else b"")) if streaming else json.dumps(payload).encode()
                captures[case]["response_hex"] = wire.hex()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream" if streaming else "application/json")
                self.send_header("Content-Length", str(len(wire)))
                self.end_headers()
                if case == "cache-chat-live":
                    first = wire.index(b"\r\n\r\n") + 4
                    self.wfile.write(wire[:first])
                    self.wfile.flush()
                    assert release.wait(10)
                    wire = wire[first:]
                for offset in range(0, len(wire), 7):
                    self.wfile.write(wire[offset:offset + 7])
                    self.wfile.flush()
                return
            if self.path.split("?", 1)[0].endswith(("/chat/completions", "/responses")):
                request = json.loads(body)
                if case == "mimo-v2.5-free" or case.startswith("zen-free-"):
                    names = {tool.get("function", tool).get("name") for tool in request.get("tools", [])}
                    if not request.get("stream") or not set(("bash", "edit", "glob", "grep", "read")) <= names or case == "zen-free-always-denied":
                        wire = b'{"type":"error","error":{"type":"FreeTierError","message":"fixture"}}'
                        self.send_response(403)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(wire)))
                        self.end_headers()
                        self.wfile.write(wire)
                        return
                    wire = ZEN_WIRE
                    if case == "zen-free-tools":
                        chunks = [{"id": "tools-e2e", "object": "chat.completion.chunk", "model": case,
                                   "choices": [{"index": 0, "delta": {"role": "assistant", "reasoning_content": "reason ",
                                       "tool_calls": [{"index": 0, "id": "tool_1", "type": "function",
                                           "function": {"name": "read_file", "arguments": '{"path":'}}]}}]},
                                  {"choices": [{"index": 0, "delta": {"reasoning_content": "done",
                                       "tool_calls": [{"index": 0, "function": {"arguments": '"README.md"}'}}]},
                                       "finish_reason": "tool_calls"},
                                       {"index": 1, "delta": {"content": "second choice"}, "finish_reason": "stop"}],
                                   "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}}]
                        wire = b"".join(b"data: " + json.dumps(chunk).encode() + b"\n\n" for chunk in chunks) + b"data: [DONE]\n\n"
                    elif case == "zen-free-responses":
                        completed = {"type": "response.completed", "response": {"id": "resp_free", "object": "response",
                            "status": "completed", "model": case, "output": [{"type": "message", "content": [
                                {"type": "output_text", "text": "hello"}]}], "usage": {"input_tokens": 5, "output_tokens": 2}}}
                        wire = b"data: " + json.dumps(completed).encode() + b"\n\n"
                    elif case == "zen-free-invalid":
                        wire = b"data: invalid\n\ndata: [DONE]\n\n"
                    elif case == "zen-free-missing-done":
                        wire = ZEN_WIRE.replace(b"data: [DONE]\r\n\r\n", b"").replace(b"data: [DONE]\n\n", b"")
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    if case == "zen-free-live":
                        first = b": upstream heartbeat\n\n"
                        self.send_header("Content-Length", str(len(first) + len(wire)))
                        self.end_headers()
                        self.wfile.write(first)
                        self.wfile.flush()
                        assert release.wait(10)
                    else:
                        self.send_header("Content-Length", str(len(wire) + (4096 if case == "zen-free-truncated" else 0)))
                        self.end_headers()
                    self.wfile.write(wire)
                    self.wfile.flush()
                    if case == "zen-free-truncated":
                        self.close_connection = True
                    return
                payload = {"id": "native-e2e", "model": json.loads(body)["model"],
                           "content": [{"type": "thinking", "thinking": "native passthrough"}],
                           "choices": [{"message": {"role": "assistant", "content": "hello"}}]}
                streaming = json.loads(body).get("stream", False)
                wire = NATIVE_SSE if streaming else json.dumps(payload).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream" if streaming else "application/json")
                self.send_header("Content-Length", str(len(wire)))
                self.end_headers()
                self.wfile.write(wire)
                return
            if case.startswith(("error-", "zen-error-")) or case == "redirect":
                status = int(case.rsplit("-", 1)[1]) if case != "redirect" else 307
                wire = b'{"type":"error","error":{"type":"api_error","message":"fixture"}}'
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Retry-After", "7")
                self.send_header("Location", "/must-not-follow")
            elif case.startswith(("json", "zen-json")) or case == "followup":
                message = {**MESSAGE, "stop_reason": "end_turn", "content": [
                    {"type": "thinking", "thinking": "分析"},
                    {"type": "thinking", "thinking": "signed", "signature": "valid"},
                    {"type": "redacted_thinking", "data": "opaque"},
                    {"type": "text", "text": "完成"}]}
                wire = json.dumps(message, ensure_ascii=False).encode()
                if case == "json-invalid":
                    wire = b"invalid JSON"
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
            else:
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream; charset=utf-8")
                self.send_header("Connection", "keep-alive, x-upstream-hop")
                self.send_header("X-Upstream-Hop", "remove-me")
                self.send_header("X-Request-Id", "req-e2e")
                if case == "sse-invalid":
                    wire = b": comment\ndata: not-json\n\n"
                elif case == "sse-start-content":
                    wire = frame({"type": "message_start", "message": {
                        **MESSAGE, "content": [{"type": "thinking", "thinking": "seed"}]}})
                else:
                    newline = "\r\n" if case == "sse-crlf" else "\n"
                    wire = b"".join(frame(event, newline, multiline=(event == MISSING))
                                    for event in EVENTS)
                if case == "sse-truncated":
                    self.send_header("Content-Length", str(len(wire) + 4096))
                    self.end_headers()
                    self.wfile.write(wire)
                    self.wfile.flush()
                    self.close_connection = True
                    return
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()

                def send_chunk(chunk):
                    self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
                    self.wfile.flush()

                if case in ("sse-live", "zen-sse-live"):
                    first = frame(START)
                    send_chunk(first)
                    if not release.wait(10):
                        self.close_connection = True
                        return
                    wire = wire[len(first):]
                # Boundaries deliberately split UTF-8, CRLF and JSON tokens.
                for offset in range(0, len(wire), 7):
                    send_chunk(wire[offset:offset + 7])
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
                return
            self.send_header("Content-Length", str(len(wire)))
            self.send_header("X-Request-Id", "req-e2e")
            self.end_headers()
            self.wfile.write(wire)

    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    port = free_port()
    database = output.resolve() / "requests.sqlite3"
    database.unlink(missing_ok=True)
    env = {**os.environ, "GATEWAY_CONFIG": str(output.resolve() / "settings.json"),
           "GATEWAY_DB": str(database),
           "GATEWAY_LISTEN": f"127.0.0.1:{port}",
           "HTTP_PROXY": f"http://127.0.0.1:{upstream.server_port}",
           "http_proxy": f"http://127.0.0.1:{upstream.server_port}",
           "NO_PROXY": "127.0.0.1,localhost", "no_proxy": "127.0.0.1,localhost",
           "GATEWAY_UPSTREAM_BASE_URL": f"http://127.0.0.1:{upstream.server_port}/step_plan/v1/"}
    for name in ("GATEWAY_OPENCODE_BASE_URL", "GATEWAY_STEPFUN_API_KEY", "GATEWAY_OPENCODE_API_KEY"):
        env.pop(name, None)
    # The old schema also proves migration works; previous runs cannot supply stale keys.
    (output / "settings.json").write_text(json.dumps({"upstream_base_url": env["GATEWAY_UPSTREAM_BASE_URL"],
                                                    "enabled": True}))
    log = (output / "gateway.log").open("wb")
    process = subprocess.Popen([str(binary), "--headless"], env=env, stdout=log, stderr=log)

    def check_wire(data, kind="event", expected="ok"):
        envelope = json.dumps({"kind": kind, "data": data}) + "\n"
        result = subprocess.run([str(checker)], input=envelope, text=True,
                                capture_output=True, check=True).stdout.strip()
        report["wire_checks"].append({"kind": kind, "data": data, "result": result})
        assert expected in result, result

    def post(case, payload=None, path="/v1/messages?beta=true", live=False,
             truncated=False, api_key=False, extra_headers=None, no_key=False, first_events=None):
        payload = payload or {"model": "step-5-preview", "max_tokens": 512,
                              "stream": not case.startswith("json"),
                              "messages": [{"role": "user", "content": "你好"}]}
        headers = {"Content-Type": "application/json", "x-e2e-case": case,
                   "anthropic-version": "2023-06-01", "anthropic-beta": "test-beta",
                   "Connection": "keep-alive, x-client-hop", "x-client-hop": "remove-me"}
        if not no_key:
            headers.update({"x-api-key": "dummy-api-key"} if api_key
                           else {"Authorization": "Bearer dummy-token"})
        headers.update(extra_headers or {})
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
        connection.request("POST", path, json.dumps(payload).encode(), headers)
        response = connection.getresponse()
        response_headers = dict(response.getheaders())
        wire = b""
        try:
            if live:
                started = time.monotonic()
                while not wire.endswith((b"\n\n", b"\r\n\r\n")):
                    wire += response.read(1)
                report["first_event_seconds"] = time.monotonic() - started
                assert decode_sse(wire) == ([START] if first_events is None else first_events)
                assert not release.is_set()
                release.set()
            if truncated:
                try:
                    response.read()
                except (http.client.HTTPException, OSError) as error:
                    report["truncation_error"] = type(error).__name__
                else:
                    raise AssertionError("truncated upstream appeared successful")
            else:
                wire += response.read()
        finally:
            connection.close()
            (output / f"{case}.response.bin").write_bytes(wire)
            (output / f"{case}.json").write_text(json.dumps({"request": payload,
                "response_status": response.status, "response_headers": response_headers,
                "upstream": captures.get(case)}, ensure_ascii=False, indent=2), encoding="utf-8")
        return response.status, {k.lower(): v for k, v in response_headers.items()}, wire

    def passed(case):
        report["cases"].append({"name": case, "status": "passed"})

    try:
        for _ in range(100):
            if process.poll() is not None:
                raise RuntimeError("gateway exited during startup")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                    break
            except OSError:
                time.sleep(0.05)
        else:
            raise RuntimeError("gateway did not listen")

        # Prove the exact production decoder rejects the original logged event.
        check_wire(MISSING, expected="missing field `signature`")
        passed("original-log-reproduces-error")
        for case in ("sse-lf", "sse-crlf", "sse-live"):
            status, headers, wire = post(case, live=case == "sse-live")
            assert status == 200
            assert headers["x-request-id"] == "req-e2e"
            assert "x-upstream-hop" not in headers
            values = decode_sse(wire)
            expected = json.loads(json.dumps(EVENTS))
            expected[1]["content_block"]["signature"] = ""
            expected[6]["content_block"].update(signature="", thinking="")
            assert values == expected
            for value in values:
                check_wire(value)
            newline = "\r\n" if case == "sse-crlf" else "\n"
            for event in EVENTS:
                if event["type"] != "content_block_start" or event["content_block"]["type"] != "thinking":
                    assert frame(event, newline) in wire
            assert wire.count(b"id: event-id") == len(EVENTS)
            assert wire.count(b"retry: 1500") == len(EVENTS)
            capture = captures[case]
            assert capture["path"] == "/step_plan/v1/messages?beta=true"
            forwarded = {k.lower(): v for k, v in capture["headers"].items()}
            assert forwarded["authorization"] == "Bearer dummy-token"
            assert forwarded["anthropic-beta"] == "test-beta"
            assert forwarded["anthropic-version"] == "2023-06-01"
            assert forwarded["accept-encoding"] == "identity"
            assert "x-client-hop" not in forwarded
            passed(case)

        for case in ("json", "sse-start-content"):
            status, headers, wire = post(case, api_key=True, path="/messages")
            assert status == 200
            if case == "json":
                message = json.loads(wire)
                assert message["content"][0]["signature"] == ""
                assert message["content"][1]["signature"] == "valid"
                assert message["content"][2]["data"] == "opaque"
                check_wire(message, kind="message")
            else:
                event = decode_sse(wire)[0]
                assert event["message"]["content"][0]["signature"] == ""
                check_wire(event)
            assert captures[case]["headers"]["x-api-key"] == "dummy-api-key"
            if "content-length" in headers:
                assert int(headers["content-length"]) == len(wire)
            passed(case)

        unsigned = {"type": "thinking", "thinking": "retain reasoning", "signature": "",
                    "cache_control": {"type": "ephemeral"}}
        signed = {"type": "thinking", "thinking": "signed", "signature": "valid"}
        tool = {"type": "tool_use", "id": "tool_1", "name": "read_file", "input": {"path": "README.md"}}
        redacted = {"type": "redacted_thinking", "data": "opaque"}
        tool_result = {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool_1", "content": "done"}]}
        request = {"model": "step-5-preview", "max_tokens": 512, "stream": False,
                   "system": "retain system", "tools": [{"name": "read_file", "input_schema": {"type": "object"}}],
                   "messages": [{"role": "assistant", "content": [unsigned, signed, redacted, tool]},
                                tool_result, {"role": "assistant", "content": [{"type": "thinking", "thinking": ""}]}]}
        assert post("followup", request)[0] == 200
        upstream_request = captures["followup"]["body"]
        expected = json.loads(json.dumps(request))
        expected["messages"].pop()
        expected["messages"][0]["content"][0] = {"type": "text", "text": "retain reasoning", "cache_control": {"type": "ephemeral"}}
        assert upstream_request == expected
        passed("followup")

        for code in (401, 429, 500):
            case = f"error-{code}"
            status, headers, wire = post(case)
            assert status == code and headers["retry-after"] == "7"
            assert wire == b'{"type":"error","error":{"type":"api_error","message":"fixture"}}'
            passed(case)
        assert post("redirect")[0] == 307
        assert "/must-not-follow" not in [item["path"] for item in captures.values()]
        passed("redirect")
        assert post("json-invalid")[0] == 502
        passed("json-invalid")
        assert post("sse-invalid")[2] == b": comment\ndata: not-json\n\n"
        passed("sse-invalid")
        post("sse-truncated", truncated=True)
        passed("sse-truncated")
        for path in ("/v1/unsupported", "/v1/embeddings"):
            assert post(path.rsplit("/", 1)[1], path=path)[0] == 404
            passed(path)

        def set_upstream(base, **settings):
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            try:
                connection.request("POST", "/ui/settings", json.dumps({"upstream_base_url": base, **settings}),
                                   {"Content-Type": "application/json"})
                response = connection.getresponse()
                assert response.status == 204, response.read()
                response.read()
            finally:
                connection.close()

        # Real hostname through a local HTTP proxy, with no production test switch.
        set_upstream("http://opencode.ai/zen/v1/")
        dirty = {"User-Agent": "grok-build/test", "Cookie": "client-cookie",
                 "Forwarded": "for=192.0.2.1", "X-Forwarded-For": "192.0.2.1",
                 "X-Stainless-Lang": "rust", "X-Stainless-Package-Version": "test",
                 "X-Request-Id": "private-request", "Traceparent": "private-trace",
                 "X-Session-Id": "foreign-session", "x-opencode-request": "foreign-request",
                 "x-opencode-project": "foreign-project", "x-opencode-client": "grok",
                 "x-session-affinity": "foreign-session"}

        def zen_post(case, extra=None, streaming=False, content="first turn"):
            payload = {"model": case, "max_tokens": 512, "stream": streaming,
                       "system": "retain system", "tools": [{"name": "read_file",
                       "input_schema": {"type": "object"}}],
                       "messages": [{"role": "user", "content": content}]}
            status, _, wire = post(case, payload, live=case == "zen-sse-live",
                                   extra_headers={**dirty, **(extra or {})})
            assert status == 200
            capture = captures[case]
            assert capture["body"] == payload, capture
            assert capture["path"] == "http://opencode.ai/zen/v1/messages?beta=true"
            forwarded = {k.lower(): v for k, v in capture["headers"].items()}
            assert forwarded["user-agent"] == "opencode/1.18.31"
            assert re.fullmatch(r"ses_[0-9a-f]{12}[0-9A-Za-z]{14}", forwarded["x-opencode-session"])
            assert forwarded["x-api-key"] == "dummy-token"
            assert forwarded["anthropic-version"] == "2023-06-01"
            assert forwarded["anthropic-beta"] == "test-beta"
            assert set(forwarded) <= {"host", "content-length", "content-type", "accept",
                                      "accept-encoding", "user-agent", "x-opencode-session",
                                      "x-api-key", "anthropic-version", "anthropic-beta"}, forwarded
            passed(case)
            return forwarded["x-opencode-session"], wire

        session, _ = zen_post("zen-json-headers")
        again, _ = zen_post("zen-json-followup", content="different turn")
        assert session == again, "explicit session must survive history changes"
        other, _ = zen_post("zen-json-other-session", {"X-Session-Id": "another-session",
                                                      "x-session-affinity": "another-session"})
        assert session != other
        canonical = "ses_abcdef123456aB0Cd1Ef2Gh3Ij"
        preserved, _ = zen_post("zen-json-canonical", {"x-opencode-session": canonical})
        assert preserved == canonical
        release.clear()
        _, wire = zen_post("zen-sse-live", streaming=True)
        assert decode_sse(wire)[1]["content_block"]["signature"] == ""

        for code in (401, 429, 500):
            case = f"zen-error-{code}"
            payload = {"model": case, "max_tokens": 64, "stream": False,
                       "messages": [{"role": "user", "content": "hello"}]}
            status, headers, wire = post(case, payload)
            assert status == code and headers["retry-after"] == "7"
            assert json.loads(wire)["error"]["message"] == "fixture"
            passed(case)

        # Without an explicit identity, only the first user turn seeds the session.
        seeds = []
        for case, first in (("zen-json-seed", "same first"), ("zen-json-seed-followup", "same first"),
                            ("zen-json-seed-other", "other first")):
            payload = {"model": case, "max_tokens": 64, "stream": False,
                       "messages": [{"role": "user", "content": first}]}
            if case.endswith("followup"):
                payload["messages"] += [{"role": "assistant", "content": "reply"},
                                        {"role": "user", "content": "next turn"}]
            assert post(case, payload, api_key=True)[0] == 200
            forwarded = {k.lower(): v for k, v in captures[case]["headers"].items()}
            assert forwarded["x-api-key"] == "dummy-api-key"
            seeds.append(forwarded["x-opencode-session"])
            passed(case)
        assert seeds[0] == seeds[1] and seeds[0] != seeds[2]

        defaults = {"model": "zen-json-defaults", "max_tokens": 64,
                    "stream": False, "messages": [{"role": "user", "content": "hello"}]}
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
        try:
            connection.request("POST", "/messages", json.dumps(defaults),
                               {"Content-Type": "application/json", "Authorization": "Bearer ignored",
                                "x-api-key": "preferred-key"})
            response = connection.getresponse()
            assert response.status == 200
            response.read()
        finally:
            connection.close()
        forwarded = {k.lower(): v for k, v in captures["zen-json-defaults"]["headers"].items()}
        assert forwarded["x-api-key"] == "preferred-key"
        assert forwarded["anthropic-version"] == "2023-06-01"
        assert "authorization" not in forwarded
        (output / "zen-defaults.json").write_text(json.dumps(captures["zen-json-defaults"], indent=2))
        passed("zen-json-defaults")

        for i, base in enumerate(("http://opencode.ai.example/zen/v1", "http://sub.opencode.ai/zen/v1",
                                  "http://opencode.ai/zen/v10", "http://opencode.ai/zen/go/v1")):
            set_upstream(base)
            case = f"json-not-zen-{i}"
            assert post(case, extra_headers={"User-Agent": "original-client", "Cookie": "keep"})[0] == 200
            forwarded = {k.lower(): v for k, v in captures[case]["headers"].items()}
            assert forwarded["user-agent"] == "original-client" and forwarded["cookie"] == "keep"
            assert "x-opencode-session" not in forwarded
            passed(case)
        set_upstream(env["GATEWAY_UPSTREAM_BASE_URL"])
        assert post("json-switch-back", extra_headers={"User-Agent": "original-client"})[0] == 200
        assert captures["json-switch-back"]["headers"]["user-agent"] == "original-client"
        passed("json-switch-back")

        set_upstream(env["GATEWAY_UPSTREAM_BASE_URL"], opencode_base_url="http://opencode.ai/zen/v1",
                     stepfun_api_key="configured-step-key", opencode_api_key="configured-zen-key")

        def get(path):
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            try:
                connection.request("GET", path)
                response = connection.getresponse()
                return response.status, json.loads(response.read())
            finally:
                connection.close()

        def latest_call(model):
            for _ in range(100):
                _, state = get("/ui/status")
                if state["stats"]["active"] == 0 and state["calls"] and state["calls"][0]["model"] == model:
                    return state["calls"][0]
                time.sleep(0.02)
            raise AssertionError(f"missing completed call: {model}")

        identities = []
        for index, (endpoint, headers, fields, source, group) in enumerate([
            ("messages", {"x-grok-conv-id": "PRIVATE_MAIN", "x-grok-session-id": "PRIVATE_PARENT"}, {}, "x-grok-conv-id", "main"),
            ("messages", {"x-grok-conv-id": "PRIVATE_SIDE", "x-grok-session-id": "PRIVATE_PARENT"}, {}, "x-grok-conv-id", "side"),
            ("messages", {"x-grok-conv-id": "PRIVATE_MAIN", "x-grok-req-id": "new-request"}, {}, "x-grok-conv-id", "main"),
            ("responses", {"x-grok-conv-id": "btw-1"}, {"prompt_cache_key": "PRIVATE_MAIN"}, "prompt_cache_key", "main"),
            ("responses", {"x-grok-conv-id": "btw-2"}, {"prompt_cache_key": "PRIVATE_MAIN"}, "prompt_cache_key", "main"),
            ("responses", {"x-grok-conv-id": "PRIVATE_SIDE"}, {"prompt_cache_key": ""}, "x-grok-conv-id", "side"),
            ("chat/completions", {"session-id": "PRIVATE_MAIN"}, {}, "session-id", "main"),
            ("chat/completions", {"session_id": "PRIVATE_MAIN"}, {}, "session_id", "main"),
            ("responses", {"x-opencode-session": canonical}, {"prompt_cache_key": "PRIVATE_MAIN"}, "x-opencode-session", "canonical"),
            ("messages", {"x-session-affinity": "PRIVATE_MAIN", "x-grok-conv-id": "PRIVATE_SIDE"}, {}, "x-session-affinity", "main"),
            ("messages", {"x-grok-conv-id": "", "session-id": ""}, {"metadata": {"session_id": "PRIVATE_MAIN"}}, "metadata.session_id", "main"),
        ]):
            bare = f"zen-json-identity-{index}"
            payload = {"model": f"opencode/{bare}", "max_tokens": 64, "stream": False, **fields}
            payload["input" if endpoint == "responses" else "messages"] = (
                f"changed input {index}" if endpoint == "responses" else
                [{"role": "user", "content": "same first" if index < 2 else f"compressed {index}"}])
            assert post(bare, payload, path=f"/v1/{endpoint}", extra_headers=headers, no_key=True)[0] == 200
            identity = captures[bare]["headers"]["x-opencode-session"]
            call = latest_call(payload["model"])
            assert call["routing"]["source"] == source
            assert re.fullmatch(r"[0-9a-f]{16}", call["routing"]["fingerprint"])
            identities.append({"group": group, "identity": identity, "call": call})
            passed(bare)
        for group in ("main", "side", "canonical"):
            matching = [item for item in identities if item["group"] == group]
            assert len({item["identity"] for item in matching}) == 1
            assert len({item["call"]["routing"]["fingerprint"] for item in matching}) == 1
        assert identities[0]["identity"] != identities[1]["identity"]
        assert identities[8]["identity"] == canonical
        assert all(private not in json.dumps([item["call"] for item in identities])
                   for private in ("PRIVATE_MAIN", "PRIVATE_SIDE", "PRIVATE_PARENT", "configured-zen-key"))
        (output / "cache-identities.json").write_text(json.dumps(identities, ensure_ascii=False, indent=2), encoding="utf-8")

        observations = []
        for bare, (endpoint, _, expected) in CACHE_FIXTURES.items():
            for streaming in (False, True):
                payload = {"model": bare, "max_tokens": 64, "stream": streaming,
                           "input": "PRIVATE_CACHE_INPUT"} if endpoint == "responses" else {
                           "model": bare, "max_tokens": 64, "stream": streaming,
                           "messages": [{"role": "user", "content": "PRIVATE_CACHE_INPUT"}]}
                status, _, wire = post(bare, payload, path=f"/v1/{endpoint}",
                    extra_headers={"x-grok-conv-id": "PRIVATE_CACHE_SESSION"})
                assert status == 200 and wire == bytes.fromhex(captures[bare]["response_hex"])
                call = latest_call(bare)
                assert call["cache"] == {**expected, "output_tokens": 23 if endpoint == "messages" and streaming else None}, call
                assert call["routing"]["source"] == "x-grok-conv-id"
                assert "PRIVATE_CACHE" not in json.dumps(call)
                observations.append({"model": bare, "stream": streaming, "call": call})
                (output / f"{bare}-{streaming}.response.bin").write_bytes(wire)
                passed(f"{bare}-{streaming}")
        payload = {"model": "cache-messages-delta", "stream": True,
                   "messages": [{"role": "user", "content": "hi"}]}
        assert post("cache-messages-delta", payload)[0] == 200
        assert latest_call(payload["model"])["cache"] == {
            "input_tokens": 1050, "output_tokens": 23, "cache_read_tokens": 800, "cache_write_tokens": 200}
        passed("cache-messages-delta-replaces-cumulative-usage")
        release.clear()
        payload = {"model": "cache-chat-live", "stream": True, "messages": [{"role": "user", "content": "hi"}]}
        assert post(payload["model"], payload, path="/v1/chat/completions", live=True,
                    first_events=[{"choices": []}])[0] == 200
        assert latest_call(payload["model"])["cache"] == {**CACHE_FIXTURES["cache-chat-hit"][2], "output_tokens": None}
        passed("cache-native-sse-remains-live")
        (output / "cache-observations.json").write_text(json.dumps(observations, ensure_ascii=False, indent=2), encoding="utf-8")

        status, catalog = get("/v1/models")
        assert status == 200
        assert [model["id"] for model in catalog["data"]] == ["stepfun/step-5-preview", "opencode/mimo-v2.5-free"]
        assert all(model["created"] == 7 for model in catalog["data"])
        assert catalog["upstream_errors"] == []
        assert captures["models-stepfun"]["path"] == "/step_plan/v1/models"
        assert captures["models-opencode"]["path"] == "http://opencode.ai/zen/v1/models"
        for provider, key in (("stepfun", "configured-step-key"), ("opencode", "configured-zen-key")):
            forwarded = {k.lower(): v for k, v in captures[f"models-{provider}"]["headers"].items()}
            assert forwarded["authorization"] == f"Bearer {key}"
            assert "x-api-key" not in forwarded
        (output / "models.json").write_text(json.dumps({"catalog": catalog,
            "upstreams": {k: v for k, v in captures.items() if k.startswith("models-")}}, indent=2))
        assert get("/models")[1] == catalog
        passed("prefixed-live-model-catalog-with-configured-keys")

        model_errors["stepfun"] = 401
        status, partial = get("/v1/models")
        assert status == 200 and [m["id"] for m in partial["data"]] == ["opencode/mimo-v2.5-free"]
        assert "stepfun" in partial["upstream_errors"][0] and "401" in partial["upstream_errors"][0]
        (output / "models-partial.json").write_text(json.dumps(partial, indent=2))
        passed("models-one-upstream-fails")
        model_errors["opencode"] = 500
        assert get("/v1/models")[0] == 502
        passed("models-both-upstreams-fail")
        model_errors.clear()

        for provider, case in (("stepfun", "json-stepfun-route"), ("opencode", "zen-json-route")):
            bare = "step-5-preview" if provider == "stepfun" else case
            payload = {"model": f"{provider}/{bare}", "max_tokens": 64, "stream": False,
                       "messages": [{"role": "user", "content": "hello"}]}
            assert post(case, payload, no_key=True)[0] == 200
            assert captures[case]["body"]["model"] == bare
            forwarded = {k.lower(): v for k, v in captures[case]["headers"].items()}
            assert forwarded["authorization" if provider == "stepfun" else "x-api-key"] == (
                "Bearer configured-step-key" if provider == "stepfun" else "configured-zen-key")
            assert captures[case]["path"] == ("/step_plan/v1/messages?beta=true" if provider == "stepfun"
                                             else "http://opencode.ai/zen/v1/messages?beta=true")
            passed(case)

        for endpoint in ("chat/completions", "responses"):
            for streaming in (False, True):
                case = f"zen-native-{endpoint.replace('/', '-')}-{streaming}"
                bare = "paid-chat-model" if endpoint == "chat/completions" else "responses-model"
                payload = {"model": f"opencode/{bare}", "stream": streaming,
                           "messages": [{"role": "user", "content": "hello"}]} if endpoint == "chat/completions" else {
                           "model": f"opencode/{bare}", "stream": streaming, "input": "hello"}
                # Zen removes the case header, so the proxy identifies native fixtures by the bare model.
                status, _, wire = post(bare, payload, path=f"/v1/{endpoint}?beta=true", no_key=not streaming,
                                       extra_headers={"User-Agent": "original-client", "Cookie": "must-remove"})
                assert status == 200
                forwarded = {k.lower(): v for k, v in captures[bare]["headers"].items()}
                assert forwarded["authorization"] == "Bearer configured-zen-key"
                assert "x-api-key" not in forwarded and "cookie" not in forwarded
                assert "anthropic-version" not in forwarded and "anthropic-beta" not in forwarded
                assert captures[bare]["body"] == {**payload, "model": bare}
                assert captures[bare]["path"] == f"http://opencode.ai/zen/v1/{endpoint}?beta=true"
                if streaming:
                    assert wire == NATIVE_SSE
                else:
                    assert "signature" not in json.loads(wire)["content"][0]
                (output / f"{case}.response.bin").write_bytes(wire)
                passed(case)

        def free_post(bare, streaming=False, **fields):
            payload = {"model": f"opencode/{bare}", "max_tokens": 64, "stream": streaming,
                       "messages": [{"role": "user", "content": "PRIVATE_FREE_MESSAGE"}], **fields}
            endpoint = "responses" if bare == "zen-free-responses" else "chat/completions"
            if endpoint == "responses":
                payload.pop("messages")
                payload["input"] = "PRIVATE_FREE_MESSAGE"
            before = len(attempts.get(bare, []))
            status, headers, wire = post(bare, payload, path=f"/v1/{endpoint}", no_key=True,
                live=bare == "zen-free-live", first_events=[])
            seen = attempts[bare][before:]
            (output / f"{bare}.attempts.json").write_text(json.dumps(seen, indent=2))
            return status, headers, wire, seen

        status, headers, wire, seen = free_post("mimo-v2.5-free")
        assert status == 200 and headers["content-type"].startswith("application/json")
        message = json.loads(wire)
        assert message["object"] == "chat.completion" and message["model"] == "mimo-v2.5-free"
        assert message["choices"][0]["message"]["content"] == "OK"
        assert message["choices"][0]["finish_reason"] == "stop"
        reasoning = message["choices"][0]["message"]["reasoning"]
        assert reasoning == message["choices"][0]["message"]["reasoning_details"][0]["text"]
        assert message["choices"][0]["message"]["reasoning_details"][0]["format"] == "unknown"
        assert message["usage"]["total_tokens"] == 244 and message["cost"] == "0"
        assert len(seen) == 2 and seen[0]["body"]["stream"] is False
        assert seen[1]["body"]["stream"] is True and seen[1]["body"]["tool_choice"] == "none"
        assert {tool["function"]["name"] for tool in seen[1]["body"]["tools"]} == {"bash", "edit", "glob", "grep", "read"}
        assert seen[0]["headers"]["x-opencode-session"] == seen[1]["headers"]["x-opencode-session"]
        _, state = get("/ui/status")
        _, detail = get(f"/ui/calls/{state['calls'][0]['id']}")
        assert any(change["reason"] == "Zen 免费层要求流式与基础工具" for change in detail["diff"]["request"])
        assert "PRIVATE_FREE_MESSAGE" not in json.dumps(detail["diff"])
        exchange = detail["exchange"]
        assert len(exchange["attempts"]) == 2
        assert exchange["attempts"][0]["response"]["status"] == 403
        assert exchange["attempts"][1]["response"]["status"] == 200
        assert bytes(exchange["attempts"][1]["response"]["body"]) == ZEN_WIRE
        assert bytes(exchange["response"]["body"]) == wire
        assert "PRIVATE_FREE_MESSAGE" in bytes(exchange["request"]["body"]).decode()
        assert detail["call"]["cache"] == {"input_tokens": 225, "output_tokens": 19, "cache_read_tokens": 0, "cache_write_tokens": 0}
        (output / "free-request-diff.json").write_text(json.dumps(detail, ensure_ascii=False, indent=2), encoding="utf-8")
        passed("free-json-collapses-real-mimo-stream")

        tool = {"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}}
        choice = {"type": "function", "function": {"name": "read_file"}}
        status, _, wire, seen = free_post("zen-free-tools", tools=[tool], tool_choice=choice)
        message = json.loads(wire)
        assert status == 200 and len(message["choices"]) == 2
        assert message["choices"][0]["message"]["reasoning_content"] == "reason done"
        call = message["choices"][0]["message"]["tool_calls"][0]
        assert call["id"] == "tool_1" and call["function"] == {"name": "read_file", "arguments": '{"path":"README.md"}'}
        assert message["choices"][0]["finish_reason"] == "tool_calls"
        assert message["choices"][1]["message"]["content"] == "second choice"
        assert message["usage"]["total_tokens"] == 12
        assert seen[1]["body"]["tools"][0] == tool and seen[1]["body"]["tool_choice"] == choice
        passed("free-tools-reasoning-usage-and-multiple-choices")
        status, _, wire, _ = free_post("zen-free-responses")
        assert status == 200 and json.loads(wire)["output"][0]["content"][0]["text"] == "hello"
        assert json.loads(wire)["usage"]["input_tokens"] == 5
        passed("free-responses-terminal-json")
        release.clear()
        status, headers, wire, _ = free_post("zen-free-live", streaming=True)
        assert status == 200 and headers["content-type"].startswith("text/event-stream")
        assert wire == b": upstream heartbeat\n\n" + ZEN_WIRE
        passed("free-sse-first-event-arrives-without-buffering")
        for bare in ("zen-free-invalid", "zen-free-missing-done", "zen-free-truncated"):
            assert free_post(bare)[0] == 502
            passed(bare)
        core_tools = [{"type": "function", "function": {"name": name}} for name in ("bash", "edit", "glob", "grep", "read")]
        status, _, _, seen = free_post("zen-free-always-denied", streaming=True, tools=core_tools)
        assert status == 403 and len(seen) == 1
        passed("fully-shaped-free-error-is-not-retried")
        case = "zen-error-403"
        payload = {"model": f"opencode/{case}", "stream": False, "messages": [{"role": "user", "content": "hi"}]}
        assert post(case, payload)[0] == 403 and len(attempts[case]) == 1
        passed("ordinary-zen-403-is-not-retried")

        _, state = get("/ui/status")
        assert state["stepfun_key_configured"] and state["opencode_key_configured"]
        assert "configured-step-key" not in json.dumps(state) and "configured-zen-key" not in json.dumps(state)
        _, detail = get(f"/ui/calls/{state['calls'][0]['id']}")
        assert any(change["reason"] == "模型前缀路由" for change in detail["diff"]["request"])
        assert "configured-zen-key" not in json.dumps(detail)
        assert detail["call"]["cache"] == {"input_tokens": None, "output_tokens": None, "cache_read_tokens": None, "cache_write_tokens": None}
        (output / "routed-diff.json").write_text(json.dumps(detail, ensure_ascii=False, indent=2), encoding="utf-8")
        passed("configured-keys-private-and-routing-visible")
        report["status"] = "passed"
    except BaseException as error:
        report["status"] = "failed"
        report["error"] = repr(error)
        raise
    finally:
        release.set()
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        upstream.shutdown()
        upstream.server_close()
        log.close()
        (output / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        manifest = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                    for path in sorted(output.iterdir()) if path.is_file() and path.name != "sha256.json"}
        (output / "sha256.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(f"E2E passed: {len(report['cases'])} cases; evidence: {output}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--gateway", type=Path, required=True)
    parser.add_argument("--checker", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("artifacts/e2e"))
    args = parser.parse_args()
    run(args.gateway.resolve(), args.checker.resolve(), args.output)

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
            if self.path.split("?", 1)[0].endswith(("/chat/completions", "/responses")):
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
    env = {**os.environ, "GATEWAY_CONFIG": str(output.resolve() / "settings.json"),
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
             truncated=False, api_key=False, extra_headers=None, no_key=False):
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
                while not wire.endswith(b"\n\n"):
                    wire += response.read(1)
                report["first_event_seconds"] = time.monotonic() - started
                assert decode_sse(wire) == [START]
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
                bare = "mimo-v2.5-free" if endpoint == "chat/completions" else "responses-model"
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

        _, state = get("/ui/status")
        assert state["stepfun_key_configured"] and state["opencode_key_configured"]
        assert "configured-step-key" not in json.dumps(state) and "configured-zen-key" not in json.dumps(state)
        _, detail = get(f"/ui/calls/{state['calls'][0]['id']}")
        assert any(change["reason"] == "模型前缀路由" for change in detail["diff"]["request"])
        assert "configured-zen-key" not in json.dumps(detail)
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

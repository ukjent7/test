"""Real HTTP upstream -> gateway binary -> Grok production wire decoder.

Run only in CI or against an already built binary. No provider credentials needed.
"""

import http.client
import json
import os
import re
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from common import free_port, suite_cli, wait_ready
from wire_fixtures import (
    CACHE_FIXTURES,
    END,
    EVENTS,
    MESSAGE,
    MISSING,
    NATIVE_SSE,
    START,
    ZEN_WIRE,
    decode_sse,
    frame,
)

ZEN_MESSAGES_EVENTS = [
    {
        "type": "message_start",
        "message": {
            "id": "msg_free",
            "type": "message",
            "role": "assistant",
            "model": "mimo-v2.5-free",
            "content": [],
            "stop_reason": None,
            "stop_sequence": None,
            "usage": {"input_tokens": 9, "output_tokens": 1},
        },
    },
    {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
    {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "免费回复"}},
    {"type": "content_block_stop", "index": 0},
    {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 7}},
    {"type": "message_stop"},
]


class MockUpstream(BaseHTTPRequestHandler):
    """Mock upstream server providing StepFun, Zen, cache, and free tier responses."""

    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    @property
    def captures(self):
        return self.server.captures

    @property
    def attempts(self):
        return self.server.attempts

    @property
    def model_errors(self):
        return self.server.model_errors

    @property
    def release(self):
        return self.server.release

    def do_GET(self):
        provider = "opencode" if "/zen/v1/models" in self.path else "stepfun"
        self.captures[f"models-{provider}"] = {"path": self.path, "headers": dict(self.headers)}
        status = self.model_errors.get(provider, 200)
        payload = {
            "object": "list",
            "data": [
                {"id": "mimo-v2.5-free" if provider == "opencode" else "step-5-preview", "object": "model", "created": 7, "owned_by": provider}
            ],
        }
        wire = json.dumps(payload if status == 200 else {"error": "fixture"}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(wire)))
        self.end_headers()
        self.wfile.write(wire)

    def do_POST(self):
        body = self.rfile.read(int(self.headers["content-length"]))
        case = self.headers.get("x-e2e-case") or json.loads(body)["model"]
        if case == "mimo-v2.5-free" and "/zen/v1/messages" in self.path:
            case = "mimo-v2.5-free-messages"

        self.captures[case] = {"path": self.path, "headers": dict(self.headers), "body": json.loads(body)}
        self.attempts.setdefault(case, []).append(self.captures[case])

        if case in CACHE_FIXTURES or case in ("cache-chat-live", "cache-messages-delta"):
            self._handle_cache_post(case, body)
            return

        request = json.loads(body)
        if case in ("mimo-v2.5-free", "mimo-v2.5-free-messages") or case.startswith("zen-free-"):
            self._handle_zen_free_post(case, body, request)
            return

        if self.path.split("?", 1)[0].endswith(("/chat/completions", "/responses")):
            self._handle_native_post(request, body)
            return

        self._handle_messages_post(case, body)

    def _handle_cache_post(self, case, body):
        fixture = "cache-chat-hit" if case == "cache-chat-live" else (
            "cache-messages-hit" if case == "cache-messages-delta" else case
        )
        endpoint, usage, _ = CACHE_FIXTURES[fixture]
        if endpoint == "messages":
            payload = {**MESSAGE, "usage": usage}
            events = [{"type": "message_start", "message": payload}, *json.loads(json.dumps(END))]
            if case == "cache-messages-delta":
                events[1]["usage"].update(input_tokens=50, cache_read_input_tokens=800, cache_creation_input_tokens=200)
        elif endpoint == "responses":
            payload = {"id": "resp_cache", "object": "response", "output": [], "usage": usage}
            events = [{"type": "response.created", "response": payload}, {"type": "response.completed", "response": payload}]
        else:
            payload = {"id": "chat_cache", "choices": [], "usage": usage}
            events = [{"choices": []}, payload]

        streaming = json.loads(body).get("stream", False)
        wire = (
            b"".join(
                b"data: " + json.dumps(event, ensure_ascii=False, indent=2).encode().replace(b"\n", b"\ndata: ") + b"\r\n\r\n"
                for event in events
            )
            + (b"data: [DONE]\n\n" if endpoint == "chat/completions" else b"")
        ) if streaming else json.dumps(payload).encode()
        self.captures[case]["response_hex"] = wire.hex()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream" if streaming else "application/json")
        self.send_header("Content-Length", str(len(wire)))
        self.end_headers()
        if case == "cache-chat-live":
            first = wire.index(b"\r\n\r\n") + 4
            self.wfile.write(wire[:first])
            self.wfile.flush()
            assert self.release.wait(10)
            wire = wire[first:]
        for offset in range(0, len(wire), 7):
            self.wfile.write(wire[offset:offset + 7])
        self.wfile.flush()

    def _handle_zen_free_post(self, case, _body, request):
        names = {tool.get("function", tool).get("name") for tool in request.get("tools", [])}
        if not request.get("stream") or not set(("bash", "read")) <= names or case == "zen-free-always-denied":
            wire = b'{"type":"error","error":{"type":"FreeTierError","message":"fixture"}}'
            self.send_response(403)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)
            return

        if case == "mimo-v2.5-free-messages":
            wire = b"".join(b"data: " + json.dumps(event).encode() + b"\r\n\r\n" for event in ZEN_MESSAGES_EVENTS)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)
            return

        wire = ZEN_WIRE
        if case == "zen-free-tools":
            chunks = [
                {
                    "id": "tools-e2e",
                    "object": "chat.completion.chunk",
                    "model": case,
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "reasoning_content": "reason ",
                                "tool_calls": [{"index": 0, "id": "tool_1", "type": "function", "function": {"name": "read_file", "arguments": '{"path":'}}],
                            },
                        }
                    ],
                },
                {
                    "choices": [
                        {"index": 0, "delta": {"reasoning_content": "done", "tool_calls": [{"index": 0, "function": {"arguments": '"README.md"}'}}]}, "finish_reason": "tool_calls"},
                        {"index": 1, "delta": {"content": "second choice"}, "finish_reason": "stop"},
                    ],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12},
                },
            ]
            wire = b"".join(b"data: " + json.dumps(chunk).encode() + b"\n\n" for chunk in chunks) + b"data: [DONE]\n\n"
        elif case == "zen-free-responses":
            completed = {
                "type": "response.completed",
                "response": {
                    "id": "resp_free",
                    "object": "response",
                    "status": "completed",
                    "model": case,
                    "output": [{"type": "message", "content": [{"type": "output_text", "text": "hello"}]}],
                    "usage": {"input_tokens": 5, "output_tokens": 2},
                },
            }
            wire = b"data: " + json.dumps(completed).encode() + b"\n\n"
        elif case in ("zen-free-stub", "zen-free-client-tool"):
            tool_name = "bash" if case == "zen-free-stub" else "run_terminal_command"
            chunks = [
                {"choices": [{"index": 0, "delta": {"role": "assistant", "content": "" if case == "zen-free-stub" else None}}]} if case == "zen-free-stub" else None,
                {"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1" if case == "zen-free-client-tool" else "call_stub", "type": "function", "function": {"name": tool_name, "arguments": '{"command":'}}]}}]},
                {"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": '"ls"}'}}]}}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ]
            chunks = [c for c in chunks if c is not None]
            wire = b"".join(b"data: " + json.dumps(chunk).encode() + b"\n\n" for chunk in chunks) + b"data: [DONE]\n\n"
        elif case == "zen-free-reasoning-alias":
            details = [{"type": "reasoning.text", "text": "thinking", "format": "unknown", "index": 0}]
            chunks = [
                {"choices": [{"index": 0, "delta": {"role": "assistant", "reasoning": "thinking", "reasoning_details": details}}]},
                {"choices": [{"index": 0, "delta": {"reasoning": ""}}]},
                {"choices": [{"index": 0, "delta": {"reasoning": "upstream-dup", "reasoning_content": "kept"}}]},
                {"choices": [{"index": 0, "delta": {"content": "visible"}, "finish_reason": "stop"}]},
            ]
            wire = b"".join(b"data: " + json.dumps(chunk).encode() + b"\n\n" for chunk in chunks) + b"data: [DONE]\n\n"
        elif case == "zen-free-tool-markup":
            def call_fragment(args, index=0):
                return {"choices": [{"index": 0, "delta": {"tool_calls": [{"index": index, "function": {"arguments": args}}]}}]}
            chunks = [
                {"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_ls", "type": "function", "function": {"name": "list_dir", "arguments": ""}}]}}]},
                call_fragment(""),
                call_fragment('{"target_directory": "/home/kk"}'),
                {"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 1, "id": "call_hg", "type": "function", "function": {"name": "hashline_grep", "arguments": ""}}]}}]},
                call_fragment("", index=1),
                call_fragment('{"target_directory": "/home/kk/.grok"}', index=1),
                {"choices": [{"index": 0, "delta": {"content": '<tool_call><function=hashline_grep><parameter=-i>true</parameter><parameter=limit>20</parameter><parameter=glob>*.{json,toml}</parameter><parameter=pattern>model|grok</parameter></function></tool_call>'}}]},
                call_fragment('{"limit": ', index=1),
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ]
            wire = b"".join(b"data: " + json.dumps(chunk).encode() + b"\n\n" for chunk in chunks) + b"data: [DONE]\n\n"
        elif case == "zen-free-multi-tool-concurrency":
            def call_fragment(args, index=0):
                return {"choices": [{"index": 0, "delta": {"tool_calls": [{"index": index, "function": {"arguments": args}}]}}]}
            chunks = [
                {"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_ls", "type": "function", "function": {"name": "list_dir", "arguments": ""}}]}}]},
                call_fragment(""),
                call_fragment('{"target_directory": "/mnt/d/work/pi"}'),
                {"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 1, "id": "call_hg_1", "type": "function", "function": {"name": "hashline_grep", "arguments": ""}}]}}]},
                call_fragment("", index=1),
                call_fragment('{"glob": ', index=1),
                {"choices": [{"index": 0, "delta": {"content": '<tool_call><function=hashline_grep><parameter=glob>*.{rs,ts,go}</parameter><parameter=head_limit>40</parameter><parameter=path>/mnt/d/work</parameter><parameter=pattern>fn main|async fn</parameter></function></tool_call>'}}]},
                call_fragment('40}', index=1),
                {"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 2, "id": "call_cmd_1", "type": "function", "function": {"name": "run_terminal_command", "arguments": ""}}]}}]},
                call_fragment("", index=2),
                call_fragment('{"block_until_ms": 10000', index=2),
                {"choices": [{"index": 0, "delta": {"content": '<tool_call><function=run_terminal_command><parameter=block_until_ms>10000</parameter><parameter=command>echo hello</parameter><parameter=description>test cmd</parameter></function></tool_call>'}}]},
                call_fragment('}', index=2),
                {"choices": [{"index": 0, "delta": {"content": '<tool_call><function=search_tool><parameter=limit>10</parameter><parameter=query>task create list</parameter></function></tool_call>'}}]},
                {"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 3, "id": "call_hg_2", "type": "function", "function": {"name": "hashline_grep", "arguments": ""}}]}}]},
                call_fragment("", index=3),
                call_fragment('{"head_limit": 15', index=3),
                {"choices": [{"index": 0, "delta": {"content": '<tool_call><function=hashline_grep><parameter=head_limit>15</parameter><parameter=path>/mnt/d/work/README.md</parameter><parameter=pattern>^# |^## </parameter></function></tool_call>'}}]},
                call_fragment('}', index=3),
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ]
            wire = b"".join(b"data: " + json.dumps(chunk).encode() + b"\n\n" for chunk in chunks) + b"data: [DONE]\n\n"
        elif case == "zen-free-invalid":
            wire = b"data: invalid\n\ndata: [DONE]\n\n"
        elif case in ("zen-free-tool-restart", "zen-free-tool-truncated"):
            def call_fragment(args):
                return {"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": args}}]}}]}
            if case == "zen-free-tool-restart":
                chunks = [
                    {"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_restart", "type": "function", "function": {"name": "read_file", "arguments": ""}}]}}]},
                    call_fragment(""), call_fragment('{"path": '), call_fragment(""),
                    call_fragment('{"path": '), call_fragment(""),
                    call_fragment('{"path": '), call_fragment('"README'), call_fragment('.md"}'),
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                ]
            else:
                chunks = [
                    {"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_cut", "type": "function", "function": {"name": "read_file", "arguments": '{"path": "un'}}]}}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                ]
            wire = b"".join(b"data: " + json.dumps(chunk).encode() + b"\n\n" for chunk in chunks) + b"data: [DONE]\n\n"
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
            assert self.release.wait(10)
        else:
            self.send_header("Content-Length", str(len(wire) + (4096 if case == "zen-free-truncated" else 0)))
            self.end_headers()
        self.wfile.write(wire)
        self.wfile.flush()
        if case == "zen-free-truncated":
            self.close_connection = True

    def _handle_native_post(self, request, body):
        payload = {
            "id": "native-e2e",
            "model": request["model"],
            "content": [{"type": "thinking", "thinking": "native passthrough"}],
            "choices": [{"message": {"role": "assistant", "content": "hello"}}],
        }
        streaming = json.loads(body).get("stream", False)
        wire = NATIVE_SSE if streaming else json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream" if streaming else "application/json")
        self.send_header("Content-Length", str(len(wire)))
        self.end_headers()
        self.wfile.write(wire)

    def _handle_messages_post(self, case, _body):
        if case.startswith(("error-", "zen-error-")) or case == "redirect":
            status = int(case.rsplit("-", 1)[1]) if case != "redirect" else 307
            wire = b'{"type":"error","error":{"type":"api_error","message":"fixture"}}'
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Retry-After", "7")
            self.send_header("Location", "/must-not-follow")
        elif case.startswith(("json", "zen-json")) or case == "followup":
            message = {
                **MESSAGE,
                "stop_reason": "end_turn",
                "content": [
                    {"type": "thinking", "thinking": "分析"},
                    {"type": "thinking", "thinking": "signed", "signature": "valid"},
                    {"type": "redacted_thinking", "data": "opaque"},
                    {"type": "text", "text": "完成"},
                ],
            }
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
                wire = frame({"type": "message_start", "message": {**MESSAGE, "content": [{"type": "thinking", "thinking": "seed"}]}})
            else:
                newline = "\r\n" if case == "sse-crlf" else "\n"
                wire = b"".join(frame(event, newline, multiline=(event == MISSING)) for event in EVENTS)
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
                if not self.release.wait(10):
                    self.close_connection = True
                    return
                wire = wire[len(first):]
            for offset in range(0, len(wire), 7):
                send_chunk(wire[offset:offset + 7])
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            return

        self.send_header("Content-Length", str(len(wire)))
        self.send_header("X-Request-Id", "req-e2e")
        self.end_headers()
        self.wfile.write(wire)


class HttpTestContext:
    """Encapsulates test runner execution context, assertions, and evidence records."""

    def __init__(self, binary, checker, output, test, port, upstream, env):
        self.binary = binary
        self.checker = checker
        self.output = output
        self.test = test
        self.port = port
        self.upstream = upstream
        self.env = env
        self.captures = upstream.captures
        self.attempts = upstream.attempts
        self.model_errors = upstream.model_errors
        self.release = upstream.release
        self.report = test.report

    def passed(self, name):
        self.test.passed(name)

    def check_wire(self, data, kind="event", expected="ok"):
        envelope = json.dumps({"kind": kind, "data": data}) + "\n"
        result = subprocess.run(
            [str(self.checker)], input=envelope, text=True, capture_output=True, check=True, timeout=30
        ).stdout.strip()
        self.report["wire_checks"].append({"kind": kind, "data": data, "result": result})
        assert expected in result, result

    def get(self, path):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        try:
            connection.request("GET", path)
            response = connection.getresponse()
            return response.status, json.loads(response.read())
        finally:
            connection.close()

    def set_upstream(self, base, **settings):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        try:
            connection.request(
                "POST", "/ui/settings", json.dumps({"upstream_base_url": base, **settings}), {"Content-Type": "application/json"}
            )
            response = connection.getresponse()
            assert response.status == 204, response.read()
            response.read()
        finally:
            connection.close()

    def latest_call(self, model):
        for _ in range(100):
            _, state = self.get("/ui/status")
            if state["stats"]["active"] == 0 and state["calls"] and state["calls"][0]["model"] == model:
                return state["calls"][0]
            time.sleep(0.02)
        raise AssertionError(f"missing completed call: {model}")

    def post(self, case, payload=None, path="/v1/messages?beta=true", live=False,
             truncated=False, api_key=False, extra_headers=None, no_key=False, first_events=None):
        payload = payload or {
            "model": "step-5-preview",
            "max_tokens": 512,
            "stream": not case.startswith("json"),
            "messages": [{"role": "user", "content": "你好"}],
        }
        headers = {
            "Content-Type": "application/json",
            "x-e2e-case": case,
            "anthropic-version": "2023-06-01",
            "anthropic-beta": "test-beta",
            "Connection": "keep-alive, x-client-hop",
            "x-client-hop": "remove-me",
        }
        if not no_key:
            headers.update({"x-api-key": "dummy-api-key"} if api_key else {"Authorization": "Bearer dummy-token"})
        headers.update(extra_headers or {})
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        connection.request("POST", path, json.dumps(payload).encode(), headers)
        response = connection.getresponse()
        response_headers = dict(response.getheaders())
        wire = b""
        try:
            if live:
                started = time.monotonic()
                while not wire.endswith((b"\n\n", b"\r\n\r\n")):
                    wire += response.read(1)
                self.report["first_event_seconds"] = time.monotonic() - started
                assert decode_sse(wire) == ([START] if first_events is None else first_events)
                assert not self.release.is_set()
                self.release.set()
            if truncated:
                try:
                    response.read()
                except (http.client.HTTPException, OSError) as error:
                    self.report["truncation_error"] = type(error).__name__
                else:
                    raise AssertionError("truncated upstream appeared successful")
            else:
                wire += response.read()
        finally:
            connection.close()
            (self.output / f"{case}.response.bin").write_bytes(wire)
            (self.output / f"{case}.json").write_text(
                json.dumps(
                    {
                        "request": payload,
                        "response_status": response.status,
                        "response_headers": response_headers,
                        "upstream": self.captures.get(case),
                    },
                    ensure_ascii=False,
                    indent=2,
                ),
                encoding="utf-8",
            )
        return response.status, {k.lower(): v for k, v in response_headers.items()}, wire


def run_messages_protocol_cases(ctx):
    """StepFun messages protocol, thinking/signature repairs, errors and unsupported endpoints."""
    ctx.check_wire(MISSING, expected="missing field `signature`")
    ctx.passed("original-log-reproduces-error")

    for case in ("sse-lf", "sse-crlf", "sse-live"):
        status, headers, wire = ctx.post(case, live=case == "sse-live")
        assert status == 200
        assert headers["x-request-id"] == "req-e2e"
        assert "x-upstream-hop" not in headers
        values = decode_sse(wire)
        expected = json.loads(json.dumps(EVENTS))
        expected[1]["content_block"]["signature"] = ""
        expected[6]["content_block"].update(signature="", thinking="")
        assert values == expected
        for value in values:
            ctx.check_wire(value)
        newline = "\r\n" if case == "sse-crlf" else "\n"
        for event in EVENTS:
            if event["type"] != "content_block_start" or event["content_block"]["type"] != "thinking":
                assert frame(event, newline) in wire
        assert wire.count(b"id: event-id") == len(EVENTS)
        assert wire.count(b"retry: 1500") == len(EVENTS)
        capture = ctx.captures[case]
        assert capture["path"] == "/step_plan/v1/messages?beta=true"
        forwarded = {k.lower(): v for k, v in capture["headers"].items()}
        assert forwarded["authorization"] == "Bearer dummy-token"
        assert forwarded["anthropic-beta"] == "test-beta"
        assert forwarded["anthropic-version"] == "2023-06-01"
        assert forwarded["accept-encoding"] == "identity"
        assert "x-client-hop" not in forwarded
        ctx.passed(case)

    for case in ("json", "sse-start-content"):
        status, headers, wire = ctx.post(case, api_key=True, path="/messages")
        assert status == 200
        if case == "json":
            message = json.loads(wire)
            assert message["content"][0]["signature"] == ""
            assert message["content"][1]["signature"] == "valid"
            assert message["content"][2]["data"] == "opaque"
            ctx.check_wire(message, kind="message")
        else:
            event = decode_sse(wire)[0]
            assert event["message"]["content"][0]["signature"] == ""
            ctx.check_wire(event)
        assert ctx.captures[case]["headers"]["x-api-key"] == "dummy-api-key"
        if "content-length" in headers:
            assert int(headers["content-length"]) == len(wire)
        ctx.passed(case)

    unsigned = {"type": "thinking", "thinking": "retain reasoning", "signature": "", "cache_control": {"type": "ephemeral"}}
    signed = {"type": "thinking", "thinking": "signed", "signature": "valid"}
    tool = {"type": "tool_use", "id": "tool_1", "name": "read_file", "input": {"path": "README.md"}}
    redacted = {"type": "redacted_thinking", "data": "opaque"}
    tool_result = {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool_1", "content": "done"}]}
    request = {
        "model": "step-5-preview",
        "max_tokens": 512,
        "stream": False,
        "system": "retain system",
        "tools": [{"name": "read_file", "input_schema": {"type": "object"}}],
        "messages": [{"role": "assistant", "content": [unsigned, signed, redacted, tool]}, tool_result, {"role": "assistant", "content": [{"type": "thinking", "thinking": ""}]}],
    }
    assert ctx.post("followup", request)[0] == 200
    upstream_request = ctx.captures["followup"]["body"]
    expected = json.loads(json.dumps(request))
    expected["messages"].pop()
    expected["messages"][0]["content"][0] = {"type": "text", "text": "retain reasoning", "cache_control": {"type": "ephemeral"}}
    assert upstream_request == expected
    ctx.passed("followup")

    for code in (401, 429, 500):
        case = f"error-{code}"
        status, headers, wire = ctx.post(case)
        assert status == code and headers["retry-after"] == "7"
        assert wire == b'{"type":"error","error":{"type":"api_error","message":"fixture"}}'
        ctx.passed(case)

    assert ctx.post("redirect")[0] == 307
    assert "/must-not-follow" not in [item["path"] for item in ctx.captures.values()]
    ctx.passed("redirect")
    assert ctx.post("json-invalid")[0] == 502
    ctx.passed("json-invalid")
    assert ctx.post("sse-invalid")[2] == b": comment\ndata: not-json\n\n"
    ctx.passed("sse-invalid")
    ctx.post("sse-truncated", truncated=True)
    ctx.passed("sse-truncated")

    for path in ("/v1/unsupported", "/v1/embeddings"):
        assert ctx.post(path.rsplit("/", 1)[1], path=path)[0] == 404
        ctx.passed(path)


def run_zen_session_cases(ctx):
    """Zen session preservation, header normalization, and routing switchbacks."""
    ctx.set_upstream("http://opencode.ai/zen/v1/")
    dirty = {
        "User-Agent": "grok-build/test",
        "Cookie": "client-cookie",
        "Forwarded": "for=192.0.2.1",
        "X-Forwarded-For": "192.0.2.1",
        "X-Stainless-Lang": "rust",
        "X-Stainless-Package-Version": "test",
        "X-Request-Id": "private-request",
        "Traceparent": "private-trace",
        "X-Session-Id": "foreign-session",
        "x-opencode-request": "foreign-request",
        "x-opencode-project": "foreign-project",
        "x-opencode-client": "grok",
        "x-session-affinity": "foreign-session",
    }

    def zen_post(case, extra=None, streaming=False, content="first turn"):
        payload = {
            "model": case,
            "max_tokens": 512,
            "stream": streaming,
            "system": "retain system",
            "tools": [{"name": "read_file", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": content}],
        }
        status, _, wire = ctx.post(case, payload, live=case == "zen-sse-live", extra_headers={**dirty, **(extra or {})})
        assert status == 200
        capture = ctx.captures[case]
        assert capture["body"] == payload, capture
        assert capture["path"] == "http://opencode.ai/zen/v1/messages?beta=true"
        forwarded = {k.lower(): v for k, v in capture["headers"].items()}
        assert forwarded["user-agent"] == "opencode/1.18.34"
        assert re.fullmatch(r"ses_[0-9a-f]{12}[0-9A-Za-z]{14}", forwarded["x-opencode-session"])
        assert re.fullmatch(r"msg_[0-9a-f]{12}[0-9A-Za-z]{14}", forwarded["x-opencode-request"])
        assert forwarded["x-opencode-client"] == "cli"
        assert forwarded["x-opencode-project"] == "global"
        assert forwarded["x-api-key"] == "dummy-token"
        assert forwarded["anthropic-version"] == "2023-06-01"
        assert forwarded["anthropic-beta"] == "test-beta"
        assert set(forwarded) <= {
            "host", "content-length", "content-type", "accept", "accept-encoding", "user-agent",
            "x-opencode-session", "x-opencode-request", "x-opencode-client", "x-opencode-project",
            "x-api-key", "anthropic-version", "anthropic-beta",
        }, forwarded
        ctx.passed(case)
        return forwarded["x-opencode-session"], wire

    session, _ = zen_post("zen-json-headers")
    again, _ = zen_post("zen-json-followup", content="different turn")
    assert session == again, "explicit session must survive history changes"
    other, _ = zen_post("zen-json-other-session", {"X-Session-Id": "another-session", "x-session-affinity": "another-session"})
    assert session != other
    canonical = "ses_abcdef123456aB0Cd1Ef2Gh3Ij"
    preserved, _ = zen_post("zen-json-canonical", {"x-opencode-session": canonical})
    assert preserved == canonical
    ctx.release.clear()
    _, wire = zen_post("zen-sse-live", streaming=True)
    assert decode_sse(wire)[1]["content_block"]["signature"] == ""

    for code in (401, 429, 500):
        case = f"zen-error-{code}"
        payload = {"model": case, "max_tokens": 64, "stream": False, "messages": [{"role": "user", "content": "hello"}]}
        status, headers, wire = ctx.post(case, payload)
        assert status == code and headers["retry-after"] == "7"
        assert json.loads(wire)["error"]["message"] == "fixture"
        ctx.passed(case)

    seeds = []
    for case, first in (("zen-json-seed", "same first"), ("zen-json-seed-followup", "same first"), ("zen-json-seed-other", "other first")):
        payload = {"model": case, "max_tokens": 64, "stream": False, "messages": [{"role": "user", "content": first}]}
        if case.endswith("followup"):
            payload["messages"] += [{"role": "assistant", "content": "reply"}, {"role": "user", "content": "next turn"}]
        assert ctx.post(case, payload, api_key=True)[0] == 200
        forwarded = {k.lower(): v for k, v in ctx.captures[case]["headers"].items()}
        assert forwarded["x-api-key"] == "dummy-api-key"
        seeds.append(forwarded["x-opencode-session"])
        ctx.passed(case)
    assert seeds[0] == seeds[1] and seeds[0] != seeds[2]

    defaults = {"model": "zen-json-defaults", "max_tokens": 64, "stream": False, "messages": [{"role": "user", "content": "hello"}]}
    connection = http.client.HTTPConnection("127.0.0.1", ctx.port, timeout=10)
    try:
        connection.request("POST", "/messages", json.dumps(defaults),
                           {"Content-Type": "application/json", "Authorization": "Bearer ignored", "x-api-key": "preferred-key"})
        response = connection.getresponse()
        assert response.status == 200
        response.read()
    finally:
        connection.close()
    forwarded = {k.lower(): v for k, v in ctx.captures["zen-json-defaults"]["headers"].items()}
    assert forwarded["x-api-key"] == "preferred-key"
    assert forwarded["anthropic-version"] == "2023-06-01"
    assert "authorization" not in forwarded
    (ctx.output / "zen-defaults.json").write_text(json.dumps(ctx.captures["zen-json-defaults"], indent=2))
    ctx.passed("zen-json-defaults")

    for i, base in enumerate(("http://opencode.ai.example/zen/v1", "http://sub.opencode.ai/zen/v1",
                              "http://opencode.ai/zen/v10", "http://opencode.ai/zen/go/v1")):
        ctx.set_upstream(base)
        case = f"json-not-zen-{i}"
        assert ctx.post(case, extra_headers={"User-Agent": "original-client", "Cookie": "keep"})[0] == 200
        forwarded = {k.lower(): v for k, v in ctx.captures[case]["headers"].items()}
        assert forwarded["user-agent"] == "original-client" and forwarded["cookie"] == "keep"
        assert "x-opencode-session" not in forwarded
        ctx.passed(case)
    ctx.set_upstream(ctx.env["GATEWAY_UPSTREAM_BASE_URL"])
    assert ctx.post("json-switch-back", extra_headers={"User-Agent": "original-client"})[0] == 200
    assert ctx.captures["json-switch-back"]["headers"]["user-agent"] == "original-client"
    ctx.passed("json-switch-back")


def run_zen_identity_cases(ctx):
    """Dual-upstream configuration and Zen routing identity stability."""
    ctx.set_upstream(
        ctx.env["GATEWAY_UPSTREAM_BASE_URL"],
        opencode_base_url="http://opencode.ai/zen/v1",
        stepfun_api_key="configured-step-key",
        opencode_api_key="configured-zen-key",
    )
    canonical = "ses_abcdef123456aB0Cd1Ef2Gh3Ij"
    identities = []
    cases_spec = [
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
    ]
    for index, (endpoint, headers, fields, source, group) in enumerate(cases_spec):
        bare = f"zen-json-identity-{index}"
        payload = {"model": f"opencode/{bare}", "max_tokens": 64, "stream": False, **fields}
        payload["input" if endpoint == "responses" else "messages"] = (
            f"changed input {index}" if endpoint == "responses" else
            [{"role": "user", "content": "same first" if index < 2 else f"compressed {index}"}]
        )
        assert ctx.post(bare, payload, path=f"/v1/{endpoint}", extra_headers=headers, no_key=True)[0] == 200
        identity = ctx.captures[bare]["headers"]["x-opencode-session"]
        call = ctx.latest_call(payload["model"])
        assert call["routing"]["source"] == source
        assert re.fullmatch(r"[0-9a-f]{16}", call["routing"]["fingerprint"])
        identities.append({"group": group, "identity": identity, "call": call})
        ctx.passed(bare)

    for group in ("main", "side", "canonical"):
        matching = [item for item in identities if item["group"] == group]
        assert len({item["identity"] for item in matching}) == 1
        assert len({item["call"]["routing"]["fingerprint"] for item in matching}) == 1
    assert identities[0]["identity"] != identities[1]["identity"]
    assert identities[8]["identity"] == canonical
    assert all(private not in json.dumps([item["call"] for item in identities])
               for private in ("PRIVATE_MAIN", "PRIVATE_SIDE", "PRIVATE_PARENT", "configured-zen-key"))
    (ctx.output / "cache-identities.json").write_text(json.dumps(identities, ensure_ascii=False, indent=2), encoding="utf-8")


def run_cache_accounting_cases(ctx):
    """Token cache statistics and hit accounting across all endpoints."""
    observations = []
    for bare, (endpoint, _, expected) in CACHE_FIXTURES.items():
        for streaming in (False, True):
            payload = {
                "model": bare,
                "max_tokens": 64,
                "stream": streaming,
                "input": "PRIVATE_CACHE_INPUT",
            } if endpoint == "responses" else {
                "model": bare,
                "max_tokens": 64,
                "stream": streaming,
                "messages": [{"role": "user", "content": "PRIVATE_CACHE_INPUT"}],
            }
            status, _, wire = ctx.post(bare, payload, path=f"/v1/{endpoint}", extra_headers={"x-grok-conv-id": "PRIVATE_CACHE_SESSION"})
            assert status == 200 and wire == bytes.fromhex(ctx.captures[bare]["response_hex"])
            call = ctx.latest_call(bare)
            assert call["cache"] == {**expected, "output_tokens": 23 if endpoint == "messages" and streaming else None}, call
            assert call["routing"]["source"] == "x-grok-conv-id"
            assert "PRIVATE_CACHE" not in json.dumps(call)
            observations.append({"model": bare, "stream": streaming, "call": call})
            (ctx.output / f"{bare}-{streaming}.response.bin").write_bytes(wire)
            ctx.passed(f"{bare}-{streaming}")

    payload = {"model": "cache-messages-delta", "stream": True, "messages": [{"role": "user", "content": "hi"}]}
    assert ctx.post("cache-messages-delta", payload)[0] == 200
    assert ctx.latest_call(payload["model"])["cache"] == {
        "input_tokens": 1050, "output_tokens": 23, "cache_read_tokens": 800, "cache_write_tokens": 200,
    }
    ctx.passed("cache-messages-delta-replaces-cumulative-usage")

    ctx.release.clear()
    payload = {"model": "cache-chat-live", "stream": True, "messages": [{"role": "user", "content": "hi"}]}
    assert ctx.post(payload["model"], payload, path="/v1/chat/completions", live=True, first_events=[{"choices": []}])[0] == 200
    assert ctx.latest_call(payload["model"])["cache"] == {**CACHE_FIXTURES["cache-chat-hit"][2], "output_tokens": None}
    ctx.passed("cache-native-sse-remains-live")
    (ctx.output / "cache-observations.json").write_text(json.dumps(observations, ensure_ascii=False, indent=2), encoding="utf-8")


def run_model_catalog_and_routing_cases(ctx):
    """Model catalog merging, upstream failure modes, and native protocol routes."""
    status, catalog = ctx.get("/v1/models")
    assert status == 200
    assert [model["id"] for model in catalog["data"]] == ["stepfun/step-5-preview", "opencode/mimo-v2.5-free"]
    assert all(model["created"] == 7 for model in catalog["data"])
    assert catalog["upstream_errors"] == []
    assert ctx.captures["models-stepfun"]["path"] == "/step_plan/v1/models"
    assert ctx.captures["models-opencode"]["path"] == "http://opencode.ai/zen/v1/models"
    for provider, key in (("stepfun", "configured-step-key"), ("opencode", "configured-zen-key")):
        forwarded = {k.lower(): v for k, v in ctx.captures[f"models-{provider}"]["headers"].items()}
        assert forwarded["authorization"] == f"Bearer {key}"
        assert "x-api-key" not in forwarded
    (ctx.output / "models.json").write_text(
        json.dumps({"catalog": catalog, "upstreams": {k: v for k, v in ctx.captures.items() if k.startswith("models-")}}, indent=2)
    )
    assert ctx.get("/models")[1] == catalog
    ctx.passed("prefixed-live-model-catalog-with-configured-keys")

    ctx.model_errors["stepfun"] = 401
    status, partial = ctx.get("/v1/models")
    assert status == 200 and [m["id"] for m in partial["data"]] == ["opencode/mimo-v2.5-free"]
    assert "stepfun" in partial["upstream_errors"][0] and "401" in partial["upstream_errors"][0]
    (ctx.output / "models-partial.json").write_text(json.dumps(partial, indent=2))
    ctx.passed("models-one-upstream-fails")
    ctx.model_errors["opencode"] = 500
    assert ctx.get("/v1/models")[0] == 502
    ctx.passed("models-both-upstreams-fail")
    ctx.model_errors.clear()

    for provider, case in (("stepfun", "json-stepfun-route"), ("opencode", "zen-json-route")):
        bare = "step-5-preview" if provider == "stepfun" else case
        payload = {"model": f"{provider}/{bare}", "max_tokens": 64, "stream": False, "messages": [{"role": "user", "content": "hello"}]}
        assert ctx.post(case, payload, no_key=True)[0] == 200
        assert ctx.captures[case]["body"]["model"] == bare
        forwarded = {k.lower(): v for k, v in ctx.captures[case]["headers"].items()}
        assert forwarded["authorization" if provider == "stepfun" else "x-api-key"] == (
            "Bearer configured-step-key" if provider == "stepfun" else "configured-zen-key"
        )
        assert ctx.captures[case]["path"] == (
            "/step_plan/v1/messages?beta=true" if provider == "stepfun" else "http://opencode.ai/zen/v1/messages?beta=true"
        )
        ctx.passed(case)

    for endpoint in ("chat/completions", "responses"):
        for streaming in (False, True):
            case = f"zen-native-{endpoint.replace('/', '-')}-{streaming}"
            bare = "paid-chat-model" if endpoint == "chat/completions" else "responses-model"
            payload = (
                {"model": f"opencode/{bare}", "stream": streaming, "messages": [{"role": "user", "content": "hello"}]}
                if endpoint == "chat/completions"
                else {"model": f"opencode/{bare}", "stream": streaming, "input": "hello"}
            )
            status, _, wire = ctx.post(
                bare,
                payload,
                path=f"/v1/{endpoint}?beta=true",
                no_key=not streaming,
                extra_headers={"User-Agent": "original-client", "Cookie": "must-remove"},
            )
            assert status == 200
            forwarded = {k.lower(): v for k, v in ctx.captures[bare]["headers"].items()}
            assert forwarded["authorization"] == "Bearer configured-zen-key"
            assert "x-api-key" not in forwarded and "cookie" not in forwarded
            assert "anthropic-version" not in forwarded and "anthropic-beta" not in forwarded
            assert ctx.captures[bare]["body"] == {**payload, "model": bare}
            assert ctx.captures[bare]["path"] == f"http://opencode.ai/zen/v1/{endpoint}?beta=true"
            if streaming:
                assert wire == NATIVE_SSE
            else:
                assert "signature" not in json.loads(wire)["content"][0]
            (ctx.output / f"{case}.response.bin").write_bytes(wire)
            ctx.passed(case)


def run_zen_free_tier_cases(ctx):
    """Zen free-tier emulation, tool renaming, restart recovery, markup repair, and 403 handling."""
    def free_post(bare, streaming=False, **fields):
        payload = {
            "model": f"opencode/{bare}",
            "max_tokens": 64,
            "stream": streaming,
            "messages": [{"role": "user", "content": "PRIVATE_FREE_MESSAGE"}],
            **fields,
        }
        endpoint = "responses" if bare == "zen-free-responses" else "chat/completions"
        if endpoint == "responses":
            payload.pop("messages")
            payload["input"] = "PRIVATE_FREE_MESSAGE"
        before = len(ctx.attempts.get(bare, []))
        status, headers, wire = ctx.post(
            bare, payload, path=f"/v1/{endpoint}", no_key=True, live=bare == "zen-free-live", first_events=[]
        )
        seen = ctx.attempts[bare][before:]
        (ctx.output / f"{bare}.attempts.json").write_text(json.dumps(seen, indent=2))
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
    assert len(seen) == 1 and seen[0]["body"]["stream"] is True
    assert "tool_choice" not in seen[0]["body"]
    assert {tool["function"]["name"] for tool in seen[0]["body"]["tools"]} == {"bash", "read"}
    _, state = ctx.get("/ui/status")
    _, detail = ctx.get(f"/ui/calls/{state['calls'][0]['id']}")
    assert any(change["reason"] == "Zen 免费层要求流式与基础工具" for change in detail["diff"]["request"])
    assert "PRIVATE_FREE_MESSAGE" not in json.dumps(detail["diff"])
    exchange = detail["exchange"]
    assert len(exchange["attempts"]) == 1
    assert exchange["attempts"][0]["response"]["status"] == 200
    assert bytes(exchange["attempts"][0]["response"]["body"]) == ZEN_WIRE
    assert bytes(exchange["response"]["body"]) == wire
    assert "PRIVATE_FREE_MESSAGE" in bytes(exchange["request"]["body"]).decode()
    assert detail["call"]["cache"] == {"input_tokens": 225, "output_tokens": 19, "cache_read_tokens": 0, "cache_write_tokens": 0}
    (ctx.output / "free-request-diff.json").write_text(json.dumps(detail, ensure_ascii=False, indent=2), encoding="utf-8")
    ctx.passed("free-json-collapses-real-mimo-stream")

    payload = {
        "model": "opencode/mimo-v2.5-free",
        "max_tokens": 64,
        "stream": False,
        "tools": [{"name": "bash", "input_schema": {"type": "object"}}],
        "messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "tool_1", "name": "bash", "input": {}}]},
            {"role": "user", "content": "PRIVATE_FREE_MESSAGE"},
        ],
    }
    before = len(ctx.attempts.get("mimo-v2.5-free-messages", []))
    status, headers, wire = ctx.post("mimo-v2.5-free-messages", payload)
    assert status == 200 and headers["content-type"].startswith("application/json")
    message = json.loads(wire)
    assert message["type"] == "message" and message["role"] == "assistant"
    assert message["content"] == [{"type": "text", "text": "免费回复"}]
    assert message["stop_reason"] == "end_turn"
    assert message["usage"]["input_tokens"] == 9 and message["usage"]["output_tokens"] == 7
    seen = ctx.attempts["mimo-v2.5-free-messages"][before:]
    assert len(seen) == 1
    tools = seen[0]["body"]["tools"]
    assert tools[0] == {"name": "bash", "input_schema": {"type": "object"}}
    assert tools[1]["name"] == "read" and tools[1]["input_schema"]["type"] == "object"
    assert seen[0]["body"]["messages"][0]["content"][0]["name"] == "bash"
    (ctx.output / "free-messages-collapse.json").write_text(wire.decode(), encoding="utf-8")
    ctx.passed("free-messages-collapse-anthropic-wire")

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
    ctx.passed("free-tools-reasoning-usage-and-multiple-choices")

    status, _, wire, _ = free_post("zen-free-responses")
    assert status == 200 and json.loads(wire)["output"][0]["content"][0]["text"] == "hello"
    assert json.loads(wire)["usage"]["input_tokens"] == 5
    ctx.passed("free-responses-terminal-json")

    status, _, wire, seen = free_post("zen-free-stub", streaming=True)
    assert status == 200
    events = decode_sse(wire)
    assert len(events) == 2 and "bash" not in json.dumps(events)
    assert events[0]["choices"][0]["delta"] == {"role": "assistant", "content": ""}
    assert events[1]["choices"][0]["finish_reason"] == "stop"
    assert len(seen) == 2
    assert {tool["function"]["name"] for tool in seen[1]["body"]["tools"]} == {"bash", "read"}
    ctx.passed("free-stub-call-dropped-and-stop-reclassified")

    status, _, wire, seen = free_post(
        "zen-free-client-tool",
        streaming=True,
        tools=[{"type": "function", "function": {"name": "run_terminal_command"}}],
    )
    assert status == 200
    events = decode_sse(wire)
    assert [event["choices"][0]["delta"]["tool_calls"][0]["function"] for event in events[:2]] == [
        {"name": "run_terminal_command", "arguments": ""},
        {"arguments": '{"command":"ls"}'},
    ]
    assert events[2]["choices"][0]["finish_reason"] == "tool_calls"
    assert len(seen) == 2
    tools = seen[1]["body"]["tools"]
    assert {tool["function"]["name"] for tool in tools} == {"run_terminal_command", "bash", "read"}
    ctx.passed("free-client-tools-preserved-with-stubs")

    status, _, wire, _ = free_post("zen-free-tool-restart", streaming=True)
    assert status == 200
    events = decode_sse(wire)
    tool_events = [e for e in events if "tool_calls" in (e["choices"][0].get("delta") or {})]
    assert len(tool_events) == 2, f"expected only declaration and complete args, got {len(tool_events)}"
    assert tool_events[0]["choices"][0]["delta"]["tool_calls"][0]["function"] == {"name": "read_file", "arguments": ""}
    assert tool_events[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] == '{"path": "README.md"}'
    assert events[-1]["choices"][0]["finish_reason"] == "tool_calls"
    assert len(events) == 3, f"expected 3 events in total, got {len(events)}"
    _, detail = ctx.get(f"/ui/calls/{ctx.latest_call('opencode/zen-free-tool-restart')['id']}")
    assert len([change for change in detail["diff"]["response"] if change["reason"] == "丢弃重试前残缺的工具参数"]) == 2
    (ctx.output / "zen-free-tool-restart.sse").write_text(wire.decode(), encoding="utf-8")
    ctx.passed("free-tool-args-restart-drops-aborted-fragments")

    status, _, wire, _ = free_post("zen-free-tool-truncated", streaming=True)
    assert status == 200
    events = decode_sse(wire)
    assert events[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] == ""
    injected = events[1]["choices"][0]["delta"]["tool_calls"][0]
    assert injected["function"]["arguments"] == '{"path": "un'
    assert events[1]["choices"][0]["finish_reason"] == "tool_calls"
    (ctx.output / "zen-free-tool-truncated.sse").write_text(wire.decode(), encoding="utf-8")
    ctx.passed("free-tool-args-truncated-sent-with-finish")

    status, _, wire, seen = free_post(
        "zen-free-tool-markup",
        streaming=True,
        tools=[
            {
                "type": "function",
                "function": {
                    "name": "list_dir",
                    "parameters": {"type": "object", "required": ["target_directory"], "properties": {"target_directory": {"type": "string"}}},
                },
            },
            {
                "type": "function",
                "function": {
                    "name": "hashline_grep",
                    "parameters": {
                        "type": "object",
                        "required": ["pattern"],
                        "properties": {
                            "-i": {"type": "boolean"},
                            "glob": {"type": "string"},
                            "limit": {"type": ["integer", "null"]},
                            "pattern": {"type": "string"},
                        },
                    },
                },
            },
        ],
    )
    assert status == 200
    events = decode_sse(wire)
    calls = {}
    for event in events:
        delta = event["choices"][0].get("delta") or {}
        assert "<tool_call" not in (delta.get("content") or "")
        for call in delta.get("tool_calls") or []:
            calls[call["index"]] = calls.get(call["index"], "") + (call["function"].get("arguments") or "")
    assert json.loads(calls[0]) == {"target_directory": "/home/kk"}
    assert json.loads(calls[1]) == {"-i": True, "glob": "*.{json,toml}", "limit": 20, "pattern": "model|grok"}
    assert events[-1]["choices"][0]["finish_reason"] == "tool_calls"
    assert len(seen) == 2
    _, detail = ctx.get(f"/ui/calls/{ctx.latest_call('opencode/zen-free-tool-markup')['id']}")
    reasons = [change["reason"] for change in detail["diff"]["response"]]
    assert reasons.count("从文本工具标记重建工具调用") == 1
    assert reasons.count("丢弃完整参数后的多余片段") == 1
    assert reasons.count("从正文剔除文本工具标记") == 1
    (ctx.output / "zen-free-tool-markup.sse").write_text(wire.decode(), encoding="utf-8")
    ctx.passed("free-tool-markup-rebuilds-broken-native-args")

    status, _, wire, _ = free_post(
        "zen-free-multi-tool-concurrency",
        streaming=True,
        tools=[
            {
                "type": "function",
                "function": {
                    "name": "list_dir",
                    "parameters": {"type": "object", "required": ["target_directory"], "properties": {"target_directory": {"type": "string"}}},
                },
            },
            {
                "type": "function",
                "function": {
                    "name": "hashline_grep",
                    "parameters": {
                        "type": "object",
                        "required": ["pattern"],
                        "properties": {
                            "glob": {"type": "string"},
                            "head_limit": {"type": ["integer", "null"]},
                            "path": {"type": "string"},
                            "pattern": {"type": "string"},
                        },
                    },
                },
            },
            {
                "type": "function",
                "function": {
                    "name": "run_terminal_command",
                    "parameters": {
                        "type": "object",
                        "required": ["command"],
                        "properties": {
                            "block_until_ms": {"type": "integer"},
                            "command": {"type": "string"},
                            "description": {"type": "string"},
                        },
                    },
                },
            },
            {
                "type": "function",
                "function": {
                    "name": "search_tool",
                    "parameters": {
                        "type": "object",
                        "required": ["query"],
                        "properties": {
                            "limit": {"type": ["integer", "null"]},
                            "query": {"type": "string"},
                        },
                    },
                },
            },
        ],
    )
    assert status == 200
    events = decode_sse(wire)
    calls = {}
    call_names = {}
    for event in events:
        delta = event["choices"][0].get("delta") or {}
        assert "<tool_call" not in (delta.get("content") or "")
        for call in delta.get("tool_calls") or []:
            idx = call["index"]
            fn = call.get("function") or {}
            if fn.get("name"):
                call_names[idx] = fn["name"]
            if fn.get("arguments"):
                calls[idx] = calls.get(idx, "") + fn["arguments"]
    assert len(call_names) == 5
    assert call_names[0] == "list_dir"
    assert json.loads(calls[0]) == {"target_directory": "/mnt/d/work/pi"}
    assert call_names[1] == "hashline_grep"
    assert json.loads(calls[1]) == {"glob": "*.{rs,ts,go}", "head_limit": 40, "path": "/mnt/d/work", "pattern": "fn main|async fn"}
    assert call_names[2] == "run_terminal_command"
    assert json.loads(calls[2]) == {"block_until_ms": 10000, "command": "echo hello", "description": "test cmd"}
    assert call_names[3] == "search_tool"
    assert json.loads(calls[3]) == {"limit": 10, "query": "task create list"}
    assert call_names[4] == "hashline_grep"
    assert json.loads(calls[4]) == {"head_limit": 15, "path": "/mnt/d/work/README.md", "pattern": "^# |^## "}
    assert events[-1]["choices"][0]["finish_reason"] == "tool_calls"
    (ctx.output / "zen-free-multi-tool-concurrency.sse").write_text(wire.decode(), encoding="utf-8")
    ctx.passed("free-multi-tool-concurrency-preserves-indices-and-names")

    align_payload_tools = [
        {"type": "function", "function": {"name": "list_dir", "parameters": {"type": "object"}}},
        {"type": "function", "function": {"name": "run_terminal_command", "parameters": {"type": "object"}}},
        {"type": "function", "function": {"name": "hashline_grep", "parameters": {"type": "object"}}},
    ]
    align_messages = [
        {"role": "user", "content": "多工具并发执行"},
        {
            "role": "assistant",
            "content": "发起调用",
            "tool_calls": [
                {"id": "call_list", "type": "function", "function": {"name": "list_dir", "arguments": "{\"path\":\".\"}"}},
                {"id": "call_git", "type": "function", "function": {"name": "run_terminal_command", "arguments": "{\"command\":\"git status\"}"}},
                {"id": "call_grep", "type": "function", "function": {"name": "hashline_grep", "arguments": "{\"pattern\":\"main\"}"}},
            ],
        },
        {"role": "tool", "tool_call_id": "call_git", "content": "fatal: not a git repo"},
        {"role": "tool", "tool_call_id": "call_grep", "content": "found: main.rs"},
        {"role": "tool", "tool_call_id": "call_list", "content": "file1\nfile2"},
    ]
    status, _, wire, seen = free_post(
        "zen-free-tool-align",
        tools=align_payload_tools,
        messages=align_messages,
    )
    assert status == 200
    assert len(seen) == 1
    sent_msgs = seen[0]["body"]["messages"]
    tool_msgs = [m for m in sent_msgs if m.get("role") == "tool"]
    assert [m["tool_call_id"] for m in tool_msgs] == ["call_list", "call_git", "call_grep"]
    assert [m["name"] for m in tool_msgs] == ["list_dir", "run_terminal_command", "hashline_grep"]
    assert [m["content"] for m in tool_msgs] == ["file1\nfile2", "fatal: not a git repo", "found: main.rs"]
    _, detail = ctx.get(f"/ui/calls/{ctx.latest_call('opencode/zen-free-tool-align')['id']}")
    assert any("对齐并发工具响应顺序与工具名" in change["reason"] for change in detail["diff"]["request"])
    ctx.passed("free-tool-responses-aligned-with-call-order")

    status, _, wire, _ = free_post("zen-free-reasoning-alias", streaming=True)
    assert status == 200
    events = decode_sse(wire)
    delta = events[0]["choices"][0]["delta"]
    assert delta["reasoning_content"] == "thinking" and "reasoning" not in delta
    assert delta["reasoning_details"][0]["text"] == "thinking"
    assert events[1]["choices"][0]["delta"] == {"reasoning": ""}
    assert events[2]["choices"][0]["delta"] == {"reasoning": "upstream-dup", "reasoning_content": "kept"}
    assert events[3]["choices"][0]["delta"] == {"content": "visible"}
    assert events[3]["choices"][0]["finish_reason"] == "stop"
    (ctx.output / "zen-free-reasoning-alias.sse").write_text(wire.decode(), encoding="utf-8")
    ctx.passed("free-reasoning-alias-renamed-to-reasoning-content")

    ctx.release.clear()
    status, headers, wire, _ = free_post("zen-free-live", streaming=True)
    assert status == 200 and headers["content-type"].startswith("text/event-stream")
    assert wire.startswith(b": upstream heartbeat\n\n")
    expected = decode_sse(ZEN_WIRE)
    for event in expected:
        for choice in event.get("choices") or []:
            delta = choice.get("delta") or {}
            if isinstance(delta.get("reasoning"), str) and delta["reasoning"] and delta.get("reasoning_content") is None:
                delta["reasoning_content"] = delta.pop("reasoning")
    assert decode_sse(wire) == expected
    ctx.passed("free-sse-first-event-arrives-without-buffering")

    for bare in ("zen-free-invalid", "zen-free-missing-done", "zen-free-truncated"):
        assert free_post(bare)[0] == 502
        ctx.passed(bare)

    core_tools = [{"type": "function", "function": {"name": name}} for name in ("bash", "edit", "glob", "grep", "read")]
    status, _, _, seen = free_post("zen-free-always-denied", streaming=True, tools=core_tools)
    assert status == 403 and len(seen) == 1
    ctx.passed("fully-shaped-free-error-is-not-retried")

    case = "zen-error-403"
    payload = {"model": f"opencode/{case}", "stream": False, "messages": [{"role": "user", "content": "hi"}]}
    assert ctx.post(case, payload)[0] == 403 and len(ctx.attempts[case]) == 1
    ctx.passed("ordinary-zen-403-is-not-retried")

    _, state = ctx.get("/ui/status")
    assert state["stepfun_key_configured"] and state["opencode_key_configured"]
    assert "configured-step-key" not in json.dumps(state) and "configured-zen-key" not in json.dumps(state)
    _, detail = ctx.get(f"/ui/calls/{state['calls'][0]['id']}")
    assert any(change["reason"] == "模型前缀路由" for change in detail["diff"]["request"])
    assert "configured-zen-key" not in json.dumps(detail)
    assert detail["call"]["cache"] == {"input_tokens": None, "output_tokens": None, "cache_read_tokens": None, "cache_write_tokens": None}
    (ctx.output / "routed-diff.json").write_text(json.dumps(detail, ensure_ascii=False, indent=2), encoding="utf-8")
    ctx.passed("configured-keys-private-and-routing-visible")


def run(binary, checker, output, test):
    test.report["wire_checks"] = []
    upstream = ThreadingHTTPServer(("127.0.0.1", 0), MockUpstream)
    upstream.captures = {}
    upstream.attempts = {}
    upstream.model_errors = {}
    upstream.release = threading.Event()
    test.server(upstream)

    port = free_port()
    database = output.resolve() / "requests.sqlite3"
    database.unlink(missing_ok=True)
    env = {
        **os.environ,
        "GATEWAY_CONFIG": str(output.resolve() / "settings.json"),
        "GATEWAY_DB": str(database),
        "GATEWAY_LISTEN": f"127.0.0.1:{port}",
        "HTTP_PROXY": f"http://127.0.0.1:{upstream.server_port}",
        "http_proxy": f"http://127.0.0.1:{upstream.server_port}",
        "NO_PROXY": "127.0.0.1,localhost",
        "no_proxy": "127.0.0.1,localhost",
        "GATEWAY_UPSTREAM_BASE_URL": f"http://127.0.0.1:{upstream.server_port}/step_plan/v1/",
    }
    for name in ("GATEWAY_OPENCODE_BASE_URL", "GATEWAY_STEPFUN_API_KEY", "GATEWAY_OPENCODE_API_KEY"):
        env.pop(name, None)
    (output / "settings.json").write_text(json.dumps({"upstream_base_url": env["GATEWAY_UPSTREAM_BASE_URL"], "enabled": True}))
    log = test.log("gateway.log")
    process = test.spawn([str(binary), "--headless"], env=env, stdout=log, stderr=log)

    ctx = HttpTestContext(binary, checker, output, test, port, upstream, env)
    try:
        wait_ready(process, port)
        run_messages_protocol_cases(ctx)
        run_zen_session_cases(ctx)
        run_zen_identity_cases(ctx)
        run_cache_accounting_cases(ctx)
        run_model_catalog_and_routing_cases(ctx)
        run_zen_free_tier_cases(ctx)
    finally:
        upstream.release.set()


if __name__ == "__main__":
    suite_cli("http", run, checker=True)

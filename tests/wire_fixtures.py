"""Protocol fixtures shared by HTTP and desktop E2E."""

import json
from pathlib import Path

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



#!/usr/bin/env python3
"""A tiny, deterministic stand-in for the OpenAI Chat Completions API.

It exists only so the demo is fully self-contained: no API key, no network, no
secrets. replaykit records the traffic between the agent and *this* server, then
replays it offline — at which point this server can be turned off entirely.

It implements just enough of `/v1/chat/completions` for the openai-python SDK:
  * if the conversation does not yet contain a tool result, it asks the agent to
    call the `get_weather` tool;
  * once the tool result is present, it returns a final natural-language answer.
Both streaming (SSE) and non-streaming responses are supported.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def wants_tool_result(messages):
    return any(m.get("role") == "tool" for m in messages)


def final_answer(messages):
    for m in messages:
        if m.get("role") == "tool":
            return f"The weather report came back: {m.get('content', '')}. Dress accordingly!"
    return "I could not determine the weather."


def build_message(messages):
    """Return the assistant message dict for this turn (tool call or answer)."""
    if wants_tool_result(messages):
        return {"role": "assistant", "content": final_answer(messages)}
    return {
        "role": "assistant",
        "content": None,
        "tool_calls": [
            {
                "id": "call_demo_0",
                "type": "function",
                "function": {"name": "get_weather", "arguments": json.dumps({"city": "Paris"})},
            }
        ],
    }


def completion_body(messages):
    msg = build_message(messages)
    finish = "tool_calls" if msg.get("tool_calls") else "stop"
    return {
        "id": "chatcmpl-demo",
        "object": "chat.completion",
        "created": 0,
        "model": "gpt-4o-mini",
        "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
    }


def sse_chunks(messages):
    """Yield Server-Sent-Event byte chunks for a streamed completion."""
    msg = build_message(messages)
    base = {"id": "chatcmpl-demo", "object": "chat.completion.chunk", "created": 0, "model": "gpt-4o-mini"}

    def event(delta, finish=None):
        payload = dict(base)
        payload["choices"] = [{"index": 0, "delta": delta, "finish_reason": finish}]
        return f"data: {json.dumps(payload)}\n\n".encode()

    yield event({"role": "assistant"})
    if msg.get("tool_calls"):
        tc = msg["tool_calls"][0]
        yield event({"tool_calls": [{"index": 0, "id": tc["id"], "type": "function",
                                     "function": {"name": tc["function"]["name"], "arguments": ""}}]})
        yield event({"tool_calls": [{"index": 0, "function": {"arguments": tc["function"]["arguments"]}}]})
        yield event({}, finish="tool_calls")
    else:
        for word in (msg["content"] or "").split(" "):
            yield event({"content": word + " "})
        yield event({}, finish="stop")
    yield b"data: [DONE]\n\n"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):  # silence default logging
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            req = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            req = {}
        messages = req.get("messages", [])
        stream = bool(req.get("stream"))

        if stream:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.end_headers()
            for chunk in sse_chunks(messages):
                self.wfile.write(chunk)
                self.wfile.flush()
        else:
            body = json.dumps(completion_body(messages)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9000
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"mock OpenAI listening on http://127.0.0.1:{port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()

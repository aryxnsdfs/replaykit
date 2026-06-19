#!/usr/bin/env python3
"""A tiny demo agent built on the official OpenAI SDK + one fake local tool.

Nothing here knows about replaykit. The agent just talks to an OpenAI-compatible
endpoint via its `base_url` — which is pointed at the replaykit proxy. That is
the whole idea: the tool sits *below* the framework, at the network boundary, so
the agent code is unchanged whether we are recording or replaying.

Run it through replaykit (see run_demo.py) — or against any OpenAI-compatible
server by setting OPENAI_BASE_URL.
"""

import json
import os
import sys

from openai import OpenAI

MODEL = "gpt-4o-mini"

TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get the current weather for a city.",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
            },
        },
    }
]


def get_weather(city: str) -> str:
    """The one fake tool. Deterministic so runs are reproducible."""
    table = {"Paris": "18°C and partly cloudy", "Tokyo": "24°C and sunny"}
    return table.get(city, "weather unavailable")


def run(stream: bool = False) -> str:
    client = OpenAI(
        base_url=os.environ.get("OPENAI_BASE_URL", "http://127.0.0.1:8080/v1"),
        api_key=os.environ.get("OPENAI_API_KEY", "sk-replaykit-demo"),
        max_retries=0,
    )

    # DEMO_PROMPT lets run_demo.py force the agent down a different branch to
    # demonstrate divergence detection.
    user_prompt = os.environ.get("DEMO_PROMPT", "What's the weather in Paris? Use the tool.")
    messages = [
        {"role": "system", "content": "You are a helpful weather assistant."},
        {"role": "user", "content": user_prompt},
    ]

    # First call: the model asks to call the tool.
    first = client.chat.completions.create(model=MODEL, messages=messages, tools=TOOLS)
    choice = first.choices[0].message
    messages.append(choice.model_dump(exclude_none=True))

    for call in choice.tool_calls or []:
        args = json.loads(call.function.arguments)
        result = get_weather(**args)
        messages.append(
            {"role": "tool", "tool_call_id": call.id, "name": call.function.name, "content": result}
        )

    # Second call: with the tool result, the model produces the final answer.
    if stream:
        final_text = ""
        for chunk in client.chat.completions.create(model=MODEL, messages=messages, stream=True):
            if not chunk.choices:
                continue
            delta = chunk.choices[0].delta
            if delta and delta.content:
                final_text += delta.content
        return final_text.strip()
    else:
        second = client.chat.completions.create(model=MODEL, messages=messages)
        return (second.choices[0].message.content or "").strip()


def main():
    stream = "--stream" in sys.argv
    answer = run(stream=stream)
    print(answer)


if __name__ == "__main__":
    main()

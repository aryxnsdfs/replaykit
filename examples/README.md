# replaykit demo

A self-contained demo that proves the headline guarantees **with no API key and
no network**:

1. **Record** a tiny tool-using agent (built on the official OpenAI SDK) talking
   to a local *mock* OpenAI server.
2. **Replay** it with the mock turned **off** — the agent's output is
   byte-for-byte identical.
3. **Diverge** — force the agent down a path that was never recorded and watch
   replaykit catch it at the right step.

## Run it

```bash
# from the repo root
cargo build --release
pip install -r examples/requirements.txt
python examples/run_demo.py
```

Expected tail:

```
plain  identical: ✅
stream identical: ✅
divergence detected: ✅
ALL CHECKS PASSED
```

## Files

| File | Purpose |
|---|---|
| `demo_agent.py` | The agent. Knows nothing about replaykit — it just uses the OpenAI SDK with `base_url` pointed at the proxy, plus one fake `get_weather` tool. |
| `mock_openai.py` | A deterministic stand-in for `/v1/chat/completions` (tool call → final answer; supports streaming). Exists only so the demo needs no real API. |
| `run_demo.py` | Orchestrates record → offline replay → divergence and checks the results. Doubles as the integration test run in CI. |

## Point it at a real provider instead

`demo_agent.py` is a normal OpenAI-SDK program. To record against the real API:

```bash
replaykit setup                                   # once
replaykit record --preset openai --out runs/real --port 8080 &
OPENAI_API_KEY=sk-...                              \
OPENAI_BASE_URL=http://127.0.0.1:8080/v1          \
python examples/demo_agent.py
```

Then replay it offline with `replaykit replay --run runs/real`.

## Inspect a recorded run

```bash
replaykit inspect examples/runs/demo
replaykit diff   examples/runs/demo --step 0
replaykit dashboard --run examples/runs/demo
```

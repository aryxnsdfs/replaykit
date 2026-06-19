#!/usr/bin/env python3
"""End-to-end demo / acceptance check for replaykit.

It proves the headline guarantees, fully offline and without any API key:

  1. RECORD the demo agent talking to a local mock OpenAI server.
  2. Turn the mock OFF (simulate "disconnect the internet") and REPLAY — the
     agent's output is byte-for-byte identical.
  3. Force the agent down a different branch and show replaykit reports a
     DIVERGENCE at the right step.

Usage:
    python examples/run_demo.py
Environment:
    REPLAYKIT_BIN   path to the replaykit binary (default: auto-detect)
"""

import os
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

# Windows consoles default to cp1252, which can't encode the ✅/° characters in
# this script's output. Force UTF-8 so the demo prints cleanly everywhere.
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, ValueError):
        pass

ROOT = Path(__file__).resolve().parent.parent
EXAMPLES = ROOT / "examples"
RUN_DIR = EXAMPLES / "runs" / "demo"
PROXY_PORT = 8080
MOCK_PORT = 9000


def find_binary() -> str:
    if os.environ.get("REPLAYKIT_BIN"):
        return os.environ["REPLAYKIT_BIN"]
    exe = "replaykit.exe" if os.name == "nt" else "replaykit"
    for cand in [ROOT / "target" / "release" / exe, ROOT / "target" / "debug" / exe]:
        if cand.exists():
            return str(cand)
    found = shutil.which("replaykit")
    if found:
        return found
    sys.exit("could not find replaykit binary; build it with `cargo build --release` "
             "or set REPLAYKIT_BIN")


def wait_port(port: int, timeout: float = 15.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.3)
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return
        time.sleep(0.1)
    raise TimeoutError(f"port {port} did not open in {timeout}s")


def run_agent(stream: bool = False, prompt: str | None = None) -> subprocess.CompletedProcess:
    env = dict(os.environ)
    env["OPENAI_BASE_URL"] = f"http://127.0.0.1:{PROXY_PORT}/v1"
    env["OPENAI_API_KEY"] = "sk-replaykit-demo"
    env["PYTHONUTF8"] = "1"
    env["PYTHONIOENCODING"] = "utf-8"
    if prompt:
        env["DEMO_PROMPT"] = prompt
    args = [sys.executable, str(EXAMPLES / "demo_agent.py")]
    if stream:
        args.append("--stream")
    return subprocess.run(args, env=env, capture_output=True, text=True, encoding="utf-8")


def stop(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()


def banner(text: str) -> None:
    print(f"\n\033[1m=== {text} ===\033[0m", flush=True)


def main() -> int:
    binary = find_binary()
    print(f"replaykit binary: {binary}")
    if RUN_DIR.exists():
        shutil.rmtree(RUN_DIR)
    RUN_DIR.parent.mkdir(parents=True, exist_ok=True)

    # ---- 1. RECORD --------------------------------------------------------
    banner("RECORD (agent -> replaykit -> mock OpenAI)")
    mock = subprocess.Popen([sys.executable, str(EXAMPLES / "mock_openai.py"), str(MOCK_PORT)])
    wait_port(MOCK_PORT)
    rec = subprocess.Popen([
        binary, "record", "--preset", "custom",
        "--upstream", f"http://127.0.0.1:{MOCK_PORT}",
        "--out", str(RUN_DIR), "--port", str(PROXY_PORT),
    ])
    wait_port(PROXY_PORT)

    rec_plain = run_agent(stream=False)
    rec_stream = run_agent(stream=True)
    print("recorded (plain) :", rec_plain.stdout.strip() or rec_plain.stderr.strip())
    print("recorded (stream):", rec_stream.stdout.strip() or rec_stream.stderr.strip())
    # Let the recorder's async append for the streamed interaction flush to disk.
    time.sleep(0.7)
    stop(rec)
    stop(mock)
    time.sleep(0.5)

    if rec_plain.returncode != 0:
        print(rec_plain.stderr)
        return 1

    # ---- 2. REPLAY OFFLINE ------------------------------------------------
    banner("REPLAY (mock is OFF — fully offline)")
    rep = subprocess.Popen([
        binary, "replay", "--run", str(RUN_DIR), "--port", str(PROXY_PORT),
        "--on-divergence", "fail-fast",
    ])
    wait_port(PROXY_PORT)
    rep_plain = run_agent(stream=False)
    rep_stream = run_agent(stream=True)
    print("replayed (plain) :", rep_plain.stdout.strip() or rep_plain.stderr.strip())
    print("replayed (stream):", rep_stream.stdout.strip() or rep_stream.stderr.strip())
    stop(rep)

    ok_plain = rec_plain.stdout.strip() == rep_plain.stdout.strip() and rec_plain.stdout.strip() != ""
    ok_stream = rec_stream.stdout.strip() == rep_stream.stdout.strip() and rec_stream.stdout.strip() != ""
    print(f"\nplain  identical: {'✅' if ok_plain else '❌'}")
    print(f"stream identical: {'✅' if ok_stream else '❌'}")

    # ---- 3. DIVERGENCE ----------------------------------------------------
    banner("DIVERGENCE (agent forced down a different branch)")
    rep2 = subprocess.Popen([
        binary, "replay", "--run", str(RUN_DIR), "--port", str(PROXY_PORT),
        "--on-divergence", "fail-fast", "--min-tier", "normalized",
    ])
    wait_port(PROXY_PORT)
    diverged = run_agent(stream=False, prompt="Completely different request that was never recorded.")
    stop(rep2)
    saw_divergence = diverged.returncode != 0 or "divergence" in (diverged.stdout + diverged.stderr).lower()
    print("agent output:", (diverged.stdout + diverged.stderr).strip()[:200])
    print(f"divergence detected: {'✅' if saw_divergence else '❌'}")

    print()
    if ok_plain and ok_stream and saw_divergence:
        print("\033[32mALL CHECKS PASSED\033[0m")
        return 0
    print("\033[31mSOME CHECKS FAILED\033[0m")
    return 1


if __name__ == "__main__":
    sys.exit(main())

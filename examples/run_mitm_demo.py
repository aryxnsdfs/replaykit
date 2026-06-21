#!/usr/bin/env python3
"""End-to-end MITM acceptance check for replaykit.

The plain demo (`run_demo.py`) exercises the reverse-proxy / HTTP path. This
script proves the **HTTPS MITM / forward-proxy** path is healthy end-to-end:

  1. Mint a CA + leaf cert for `localhost`.
  2. Start a localhost HTTPS mock server using that leaf.
  3. `replaykit setup` -> create the proxy's own CA.
  4. RECORD: agent uses replaykit as `HTTPS_PROXY` and talks to
     `https://localhost:<mock_port>` through CONNECT + TLS interception.
     Both the test client and replaykit's upstream client trust the mock CA
     (the client via `SSL_CERT_FILE`, replaykit via `REPLAYKIT_EXTRA_ROOTS`).
  5. REPLAY: kill the mock; same client request must succeed offline and
     return a byte-identical body.

The CA-bundle approach (instead of an `--insecure-upstream` flag) keeps the
production `record` path safe: there is no way to disable verification at the
CLI level, only to add additional trusted roots from a file you own.

Usage:
    python examples/run_mitm_demo.py
"""

from __future__ import annotations

import http.server
import os
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

# UTF-8 console (Windows CI default codepage can't encode ✅).
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, ValueError):
        pass

ROOT = Path(__file__).resolve().parent.parent
RUN_DIR = ROOT / "examples" / "runs" / "mitm-demo"
PROXY_PORT = 18080
MOCK_PORT = 19443
MOCK_HOST = "localhost"
MOCK_BODY = b'{"id":"mock-1","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"hello from mitm mock"}}]}'


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
    sys.exit("could not find replaykit binary; build it or set REPLAYKIT_BIN")


def wait_port(port: int, timeout: float = 15.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.3)
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return
        time.sleep(0.1)
    raise TimeoutError(f"port {port} did not open in {timeout}s")


def mint_localhost_cert(out_dir: Path) -> tuple[Path, Path]:
    """Mint a self-signed cert+key for `localhost` and return (cert_pem, key_pem)."""
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import rsa
    from cryptography.x509.oid import NameOID

    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")])
    now = datetime.now(timezone.utc)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - timedelta(minutes=5))
        .not_valid_after(now + timedelta(days=30))
        .add_extension(
            x509.SubjectAlternativeName([x509.DNSName("localhost")]),
            critical=False,
        )
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .sign(private_key=key, algorithm=hashes.SHA256())
    )
    cert_pem = out_dir / "mock-cert.pem"
    key_pem = out_dir / "mock-key.pem"
    cert_pem.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    key_pem.write_bytes(
        key.private_bytes(
            encoding=serialization.Encoding.PEM,
            format=serialization.PrivateFormat.TraditionalOpenSSL,
            encryption_algorithm=serialization.NoEncryption(),
        )
    )
    return cert_pem, key_pem


class _MockHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802 (BaseHTTPRequestHandler interface)
        length = int(self.headers.get("Content-Length", "0"))
        _ = self.rfile.read(length)
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(MOCK_BODY)))
        self.end_headers()
        self.wfile.write(MOCK_BODY)

    def log_message(self, *_args):  # silence stderr noise
        return


def start_mock(cert_pem: Path, key_pem: Path) -> http.server.ThreadingHTTPServer:
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", MOCK_PORT), _MockHandler)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(str(cert_pem), str(key_pem))
    httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    wait_port(MOCK_PORT)
    return httpd


def client_request(replaykit_ca_pem: Path, mock_cert_pem: Path) -> tuple[int, bytes]:
    """Send one HTTPS POST through replaykit as a forward proxy."""
    # The client must trust *both*: replaykit's minted leaf (signed by the
    # replaykit CA — the cert it actually presents during MITM) and the mock
    # cert (in case CONNECT bypass ever short-circuits).
    bundle = tempfile.NamedTemporaryFile(delete=False, suffix=".pem")
    bundle.write(replaykit_ca_pem.read_bytes() + b"\n" + mock_cert_pem.read_bytes())
    bundle.flush()
    bundle.close()
    ctx = ssl.create_default_context(cafile=bundle.name)
    proxy = f"http://127.0.0.1:{PROXY_PORT}"
    handler = urllib.request.ProxyHandler({"https": proxy, "http": proxy})
    opener = urllib.request.build_opener(handler, urllib.request.HTTPSHandler(context=ctx))
    req = urllib.request.Request(
        f"https://{MOCK_HOST}:{MOCK_PORT}/v1/chat/completions",
        data=b'{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}',
        headers={"Content-Type": "application/json", "Authorization": "Bearer sk-mitm-demo"},
        method="POST",
    )
    with opener.open(req, timeout=10) as resp:
        return resp.getcode(), resp.read()


def banner(text: str) -> None:
    print(f"\n\033[1m=== {text} ===\033[0m", flush=True)


def stop(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()


def main() -> int:
    try:
        import cryptography  # noqa: F401
    except ImportError:
        print("cryptography not installed; install with: pip install -r examples/requirements.txt")
        return 0  # don't fail CI if optional dep missing — script is best-effort

    binary = find_binary()
    print(f"replaykit binary: {binary}")
    if RUN_DIR.exists():
        shutil.rmtree(RUN_DIR)
    RUN_DIR.parent.mkdir(parents=True, exist_ok=True)
    RUN_DIR.mkdir(parents=True, exist_ok=True)

    cert_dir = RUN_DIR / "_certs"
    cert_dir.mkdir(exist_ok=True)
    ca_dir = RUN_DIR / "_replaykit-ca"

    banner("setup replaykit CA")
    subprocess.run([binary, "setup", "--ca-dir", str(ca_dir)], check=True)
    replaykit_ca_pem = ca_dir / "ca-cert.pem"
    if not replaykit_ca_pem.exists():
        pems = [p for p in ca_dir.glob("*.pem") if "key" not in p.name.lower()]
        if not pems:
            print(f"no CA pem produced under {ca_dir}")
            return 1
        replaykit_ca_pem = pems[0]

    banner("mint localhost cert + start TLS mock")
    mock_cert, mock_key = mint_localhost_cert(cert_dir)
    httpd = start_mock(mock_cert, mock_key)

    env = dict(os.environ)
    env["REPLAYKIT_EXTRA_ROOTS"] = str(mock_cert)

    try:
        banner("RECORD (HTTPS through CONNECT + MITM)")
        rec = subprocess.Popen(
            [
                binary, "record",
                "--preset", "custom",
                "--upstream", f"https://{MOCK_HOST}:{MOCK_PORT}",
                "--out", str(RUN_DIR / "run"),
                "--port", str(PROXY_PORT),
                "--ca-dir", str(ca_dir),
            ],
            env=env,
        )
        wait_port(PROXY_PORT)
        status_rec, body_rec = client_request(replaykit_ca_pem, mock_cert)
        print(f"recorded: status={status_rec} bytes={len(body_rec)}")
        time.sleep(0.5)
        stop(rec)
    finally:
        httpd.shutdown()

    banner("REPLAY (mock is OFF — pure offline MITM)")
    rep = subprocess.Popen(
        [
            binary, "replay",
            "--run", str(RUN_DIR / "run"),
            "--port", str(PROXY_PORT),
            "--ca-dir", str(ca_dir),
            "--on-divergence", "fail-fast",
        ],
    )
    wait_port(PROXY_PORT)
    try:
        status_rep, body_rep = client_request(replaykit_ca_pem, mock_cert)
        print(f"replayed: status={status_rep} bytes={len(body_rep)}")
    finally:
        stop(rep)

    ok = status_rec == status_rep == 200 and body_rec == body_rep and body_rec
    print(f"\nMITM record == replay: {'✅' if ok else '❌'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())

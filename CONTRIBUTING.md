# Contributing to replaykit

Thanks for your interest! replaykit is a small, focused tool and contributions
are very welcome — bug reports, docs, new provider presets, and matcher
improvements especially.

## Development setup

You need a recent stable Rust toolchain (1.74+) and, for the demo, Python 3.10+.

```bash
git clone https://github.com/aryxnsdfs/replaykit
cd replaykit
cargo build
cargo test
```

The end-to-end demo doubles as an integration test and needs no API key:

```bash
cargo build --release
pip install -r examples/requirements.txt
python examples/run_demo.py
```

## Before opening a PR

Please make sure these pass — CI runs the same checks:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

## Project layout

| Path | What lives there |
|---|---|
| `src/proxy/` | The hyper proxy: dispatch, TLS MITM, record & replay engines, SSE. |
| `src/cassette/` | Content-addressed storage: chunker, blob store, manifest, reader/writer. |
| `src/matcher.rs` | Tiered semantic request matching + key computation. |
| `src/divergence.rs` | Divergence detection, diffing, policies, replay cursor. |
| `src/ca.rs` | Local CA generation, per-host leaf minting, OS trust install. |
| `src/dashboard.rs` | Embedded single-page web UI. |
| `src/commands.rs` | CLI subcommand implementations. |
| `assets/` | Dashboard HTML/CSS/JS (embedded into the binary via `rust-embed`). |
| `examples/` | Self-contained demo agent + mock server + acceptance runner. |

## Design principles

- **Record at the egress boundary, verbatim.** No judgement about what's
  "important"; never inspect the agent's internals.
- **Never silently return a wrong entry on replay.** A mismatch is a divergence,
  surfaced loudly.
- **Flat memory.** Recordings stream to disk; a run is never buffered in RAM.

## Adding a provider preset

Presets live in `src/config.rs` (`Preset`). A preset just supplies a default
upstream and whether it's local (HTTP, no CA). Add the variant, its
`default_upstream()`, `is_local()`, and a line to the README's preset list.

## Reporting bugs

Please include the `replaykit` version (`replaykit --version`), the command you
ran, and — if you can share it — the output of `replaykit inspect <run>`.

By contributing you agree your contributions are licensed under the MIT license.

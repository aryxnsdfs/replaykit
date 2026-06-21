# replaykit fuzz targets

Coverage-guided fuzzers for the request matcher. The matcher is the security-
adjacent surface of replaykit: it decides which recorded response a replayed
request is allowed to receive. A panic or a wrong reflexivity result there can
mean serving the wrong response — so it's where fuzzing pays off.

## Run locally

Install once: `cargo install cargo-fuzz` (nightly toolchain required).

```sh
cd fuzz
cargo +nightly fuzz run matcher_compute   # body / header bytes
cargo +nightly fuzz run matcher_compare   # two requests, tier + score invariants
```

Stop with Ctrl-C. Crashing inputs land in `fuzz/artifacts/<target>/`.

## In CI

The `fuzz` job in `.github/workflows/ci.yml` runs each target for 60 seconds
on the nightly schedule. Longer runs (hours / days) are the user's job;
60 seconds is enough to catch regressions on every PR.

## Properties checked

| Target            | Property                                                                  |
|-------------------|---------------------------------------------------------------------------|
| `matcher_compute` | Never panics. Always returns non-empty hex digests for all four tiers.    |
| `matcher_compare` | Never panics. `score ∈ [0,1]`. `compare(a, a)` always matches as `Exact`. |

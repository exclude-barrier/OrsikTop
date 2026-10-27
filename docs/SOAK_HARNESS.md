# Multi-server soak harness (REL-01)

`src/app/tests/soak.rs` drives the *real* LLM worker
(`spawn_llm_worker`) against local loopback mock llama.cpp servers. No
inference server is contacted; every check runs against mocks defined in the
module. It complements the shorter, hand-written worker tests in
`src/app/tests/soak_scenarios.rs`
(`soak_endurance_all_failure_modes_without_mixing_and_recovers`, `:480`;
`soak_set_servers_churn_keeps_generations_and_polling_bounded`, `:299`).

## What it exercises

Rotating over 1, 4 and 16 servers, each server is walked through: repeated
reconnects (connection closed without an answer), server restarts (listener
dropped and re-bound on the same port, counters reset), changing counter
baselines, missing telemetry, delayed responses (inside and beyond the client
request timeout), corrupt JSON, bursts of concurrent server-set changes, and a
shutdown while requests are in flight.

Asserted on every run: no wrong server/slot attribution (each accepted sample
carries only its own server's model, context size, slot count and counter
band), no permanent stall (every server delivers an accepted sample within
`STALL_BOUND`), no unintended requests (per-server counts stay under the poll
floor's bound and stop after the worker is joined), and an orderly shutdown
within `SHUTDOWN_BOUND` even while requests hang and the sample channel is
never drained.

## Smoke (part of `cargo test`)

Two short, deterministic tests run in the normal gate and finish in a few
seconds:

```sh
cargo test soak_smoke
```

`soak_smoke_4_servers_all_scenarios` walks four servers through all eight
scenarios with two set-change bursts; `soak_smoke_16_servers_shutdown_with_undrained_channel`
uses the server cap (equal to the sample-channel capacity) and a receiver that
is never drained, then stops while half the servers hold a request open.

## Long run (opt-in)

The durations are configurable through `ORSIK_SOAK_SECS` (seconds per server
count, default 60, floored at 5). The long matrix is `#[ignore]`d and is never
part of a PR gate:

```sh
ORSIK_SOAK_SECS=300 cargo test --locked -- --ignored --test-threads=1 \
    --nocapture soak_long_
```

`--test-threads=1` is required for the resource assertions: they read
`/proc/self/status` (`Threads:`, `VmRSS:`), which are process-global and would
be meaningless under a parallel run. When the binary is not single-threaded
they are printed (in the `ORSKTOP_SOAK|…` report line) but not asserted. Run it
on an otherwise idle machine; it binds loopback ports only.

## Release-binary soak (SOAK-01)

The release harness runs the optimized binary against four loopback mock
servers for an arbitrary duration. It is an external measurement, not a test
with assertions, and it never contacts a real inference server:

```sh
python3 scripts/perf_release_bench.py --scenarios soak_mock_4 \
    --soak-duration 14400 --out /tmp/orsiktop-bench/soak4h
```

It samples `/proc/<pid>` every 50 ms and records CPU, RSS/VmHWM, thread
count/high-water, thread arrivals (sampling lower bound), frame cadence, poll
requests/cycle and shutdown time. The JSON result is the long-run evidence for
RSS/thread stability; a flat trend supports keeping the current polling
architecture, a growing trend is a concrete leak candidate.

### 4-hour release soak (2026-10-18)

Run on `cf-desktop` with four loopback mocks, `--soak-duration 14400`
(4 h), release binary `c013880` (SHA-256 `d872c5a2…`):

```sh
python3 scripts/perf_release_bench.py --scenarios soak_mock_4 \
    --soak-duration 14400 --out /tmp/orsiktop-bench/soak4h
```

| Metrik | Wert |
| --- | --- |
| Duration / exit | `14400 s` / `0` |
| RSS first → last | `42 334` → `39 052` kB |
| RSS mean / median / HWM | `38 685` / `34 020` / `51 920` kB |
| Threads mean / p95 / max | `8,02` / `8` / `24` |
| Thread starts (50-ms lower bound) | `6 070` total, `0,42`/s |
| Poll requests | `8,13`/s (`117 108` total) |
| Poll cycle median / p95 | `1 000,07` / `1 000,78` ms |
| Fan-out spread median | `0,65` ms |
| Shutdown | `32,9` ms |

No RSS or thread accumulation over 4 h; RSS ends below its first-window value
and threads stay at the 8-thread steady state. Poll cadence is stable. Host
load rose from `0,30` to `9,63` during the run due to foreign work, so the
reported `47,36 %` of one CPU (fast worker `46,32 %`) is **not** a clean
performance baseline; the resource/stability result is still valid. Raw data:
`target/perf-bench-soak4h.json` (not versioned).

## Current-SHA long run (2026-10-17, ee2d778)

Run on `cf-laptop` with the default 60 s per server count. The host was
load-affected (foreign `llama-server`/QEMU/agent work), so latency values are
not a performance baseline; thread, RSS, and shutdown assertions still ran
single-threaded and passed.

```sh
ORSIK_SOAK_SECS=60 cargo test --locked -- --ignored --test-threads=1 \
    --nocapture soak_long_
```

`3 passed` in 189,37 s.

| Server | Phases | Samples | Accepted | Stale | Requests | Max gap ms | Shutdown ms | Threads peak | RSS first→last kB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 40 | 115 | 112 | 3 | 242 | 1 291 | 1 190 | 12 | 16 232 → 16 332 |
| 4 | 40 | 377 | 369 | 8 | 763 | 1 422 | 1 195 | 25 | 16 612 → 16 588 |
| 16 | 40 | 900 | 879 | 21 | 1 781 | 1 386 | 1 203 | 78 | 18 848 → 17 660 |

No leak: RSS does not grow and the thread count returns to 2 after shutdown.
The 16-server peak (`78`) matches the earlier CFA-94 soak (`79`).

## Raw output

Each run prints one line:

```text
ORSKTOP_SOAK|SoakReport { servers, elapsed_ms, phases, set_commands, samples,
accepted, stale_rejected, removal_checks, restarts, requests_total, max_gap_ms,
shutdown_ms, threads_before, threads_peak, threads_after, rss_samples,
rss_first_median_kb, rss_last_median_kb }
```

`accepted` counts samples that belong to the current generation (a healthy or
reconnecting server both count — "accepted" means "attributed to the current
set", not "healthy"); `stale_rejected` counts samples from a generation the app
already replaced. `rss_first_median_kb`/`rss_last_median_kb` are the medians of
the first and last 50 samples after a 30-sample warm-up; the assertion allows a
growth of at most `max(8 MiB, 10 %)`.

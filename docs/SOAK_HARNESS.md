# Multi-server soak harness (REL-01)

`src/app/tests/soak.rs` drives the *real* LLM worker
(`spawn_llm_worker`) against local loopback mock llama.cpp servers. No
inference server is contacted; every check runs against mocks defined in the
module. It complements the shorter, hand-written worker tests in `src/app.rs`
(`soak_endurance_all_failure_modes_without_mixing_and_recovers`,
`soak_set_servers_churn_keeps_generations_and_polling_bounded`, …).

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

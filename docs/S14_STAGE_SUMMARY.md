# Stage Summary — S14 (llama.cpp client performance)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/llama.rs` | `LlamaMonitor::new` now sets a **connect timeout** (`750 ms`) in addition to the existing total request timeout (`1200 ms`, now named `LLM_REQUEST_TIMEOUT_MS`). `sample()` fetches `/props` (when due), `/metrics` and `/slots` **concurrently** via `std::thread::scope`; the three results are merged preserving the exact previous error strings and precedence. New private types: `MetricsOutcome`, `JsonOutcome`; new free helpers `fetch_raw` / `fetch_json`; `refresh_props` split into the outcome consumer `apply_props_result`; `apply_slots` replaced by the outcome consumer `apply_slots_outcome`. New field `props_dirty` so a failed `/props` refresh is retried on the next sample. 2 offline tests added. |
| `docs/S14_STAGE_SUMMARY.md` | This summary. |

## 2. Architecture changes

- **Concurrent endpoint fetch.** The old `sample()` issued up to three **sequential** HTTP
  calls per poll (optional `/props` → `/metrics` → `/slots`), so a slow or dead server
  cost up to 3× the request budget before any failure was visible. Now all due endpoints
  are fetched in parallel on scoped threads sharing the single reqwest blocking `Client`.
  Verified against reqwest 0.12.28: the blocking client's core handler `tokio::spawn`s
  each request (`blocking/client.rs`), so concurrent `send()` calls on the shared
  `Client` genuinely overlap; the poll cycle is bounded by the **slowest single request**
  instead of the sum. The scoped threads are joined before `sample()` returns, so no
  thread outlives the call and the `LlamaMonitor` stays single-threaded-owned by the LLM
  worker.
- **Separate connect vs. total timeout.** `connect_timeout(750 ms)` makes a dead or
  unrouted endpoint fail fast during the connect phase; `timeout(1200 ms)` still bounds
  the whole request. For a local server the connect phase is milliseconds, so the connect
  timeout only matters when the server is gone or the port is filtered — exactly the case
  where 1200 ms × 3 serial calls previously dominated the 250 ms poll floor.
- **Error precedence preserved byte-for-byte.** `/metrics` failure is still fatal for the
  sample (`stats.error` + not connected): 501 → `"/metrics disabled; start llama.cpp
  with --metrics"`, other non-success → `"/metrics returned HTTP {status}"`, transport
  error → `"cannot reach llama.cpp: {err}"`, body read failure → `"metrics response
  error: {err}"`. `/slots` failure still only sets `stats.slots_error` (graceful). `/props`
  failure is still silent (cached props keep being shown). `stabilize_llm_sample` in
  `app.rs` (offline grace, hard-failure detection via substring match) is untouched, and
  its inputs are the same strings as before — including **not** appending server response
  bodies to the error, so a misconfigured local server cannot inject text that changes the
  hard-failure classification.
- **Stale-data handling and retry.** Stale-data handling remains the existing offline
  grace in `app.rs` (`stabilize_llm_sample` holds the last good sample for
  `offline_grace` ms, then clears). What changed: a failed `/props` fetch now sets
  `props_dirty`, so the next sample retries it immediately instead of waiting out the 30 s
  refresh window (previously a single failed refresh delayed model/context data for a full
  window). `props_dirty` starts `true` on a new monitor, preserving the original
  "refresh on first sample" behavior.
- **Backoff policy for a local server.** The LLM worker already enforces a 250 ms poll
  floor and replaces stale samples with the latest; with concurrent + bounded requests the
  worst-case per-cycle cost is one 1200 ms request (not three), so no per-request
  backoff/retry machinery is added — retrying the failed `/props` on the next 250 ms tick
  is the backoff, and it is bounded by the same floor.

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **118 passed, 0 failed, 1 ignored** (was 116;
+2 new offline tests: `props_failure_retries_next_sample` covers the retry/dirty
contract; `slots_outcome_maps_to_error_or_counters` covers the graceful-degradation
error mapping and counter population). `cargo test -- --ignored discovery_real` still
passes on the real machine (Intel xe).

## 4. Known limitations

- **HTTP concurrency itself is not unit-testable** without a live server: the
  `thread::scope` spawn/join wiring and the reqwest shared-client behavior are exercised
  only by the running binary (verified by inspection of reqwest 0.12.28 source and the
  offline tests of the merge/mapping logic). A fixture-driven HTTP test would require a
  test HTTP server dependency, which the no-new-dependencies constraint rules out.
- **One shared blocking runtime.** All three requests funnel through reqwest's single
  internal tokio thread. This is fine for three small local GETs and keeps the code
  simple; it would not scale to many endpoints, which OrsikTop does not have.
- **`/slots` body is discarded on non-2xx** (as before); only the status is surfaced in
  `slots_error`. `/metrics` 501 handling is unchanged.
- The TUI cannot be smoke-tested in this headless shell (no TTY); the ENXIO
  terminal-init error is pre-existing and unrelated.

## 5. Newly discovered issues

- **reqwest blocking `Client` is auto-`Send`+`Sync`** (no `unsafe impl` in 0.12.28):
  `ClientHandle { timeout, inner: Arc<InnerClientHandle> }`, so `&Client` can be shared
  across `thread::scope` threads without extra wrapping.
- **`StatusCode`'s `Display` includes the reason phrase** (`404 Not Found`), so error
  strings built with `format!("HTTP {status}")` contain more than the numeric code —
  asserted exactly in the new tests to lock the format.
- **`reqwest::error::Kind` is `pub(crate)`**, so the transport vs. body-read error
  distinction must be made at the fetch site (phase-split `send()` / `text()` /
  `json()`), which is where `fetch_raw`/`fetch_json` live.

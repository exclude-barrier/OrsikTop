# Runtime risk analysis (REL-02 / CFA-95)

Independent, **read-only** review of the OrsikTop LLM worker's runtime paths:
bounded channels, dropped samples, generation IDs, reconnect transitions,
worker shutdown, panic handling and the thread count under 16 servers.

- **Ticket:** CFA-95 (EPIC 4 / REL-02).
- **Analysed at:** commit `ac424d5` (the CFA-94 soak harness commit on
  `test/cfa-94-95-soak-and-runtime-risks`); this ticket's change then adds the
  single F1 test below.
- **Scope:** analysis and behaviour-pinning tests only. No `app.rs` runtime
  logic was changed — the harness commit adds a `#[cfg(test)] mod soak;` line
  and test code only.
- **Method:** static reading of `src/app.rs`, `src/llama.rs` and `src/domain.rs`
  plus the existing worker tests; line numbers are from the tree above. The one
  dynamic measurement quoted (F5, F6) is the raw output of the CFA-94 long soak
  recorded in `docs/SOAK_HARNESS.md`/the CFA-94 report — it is not a claim about
  a production host.

Every finding below was **re-verified against the current source**: the earlier
CFA-95 comment was written at `e9600d6`, so both the file:line evidence and the
"is it still true?" question were re-checked here. Line numbers below are the
current ones (they shifted by two after the soak module was added under
`mod tests`).

Severities: **high** = wrong/no data with no user signal and no recovery;
**medium** = data gap or unbounded resource under realistic conditions;
**low** = bounded, by-design, or cosmetic.

---

## F1 — Worker panics are swallowed: no hook, no restart, no indication (medium–high)

**Evidence**
- `src/app.rs:544-563` — `join_bounded` waits for a worker to end and calls
  `let _ = handle.join();` (`:547`). The `Result` (the panic payload) is
  discarded, and a worker that misses the 5 s deadline (`:550`) is left
  detached.
- No `std::panic::catch_unwind` and no `panic::set_hook` exist anywhere under
  `src/` (verified by search).
- The UI loop drains both channels with `while let Ok(next) = …` — fast at
  `src/app.rs:221`, LLM at `:225`. `Err(Disconnected)` is never handled, and the
  workers are never checked with `is_finished()`. So when a worker thread dies,
  the receiver simply stops yielding and the loop keeps rendering the last
  values.

**Effect:** if the fast or LLM worker panics, it dies silently. The dashboard
keeps showing stale data indefinitely, with no error surfaced and no restart. A
dead worker is indistinguishable from a healthy one that has nothing to update.

**Recommended follow-up:** install a `panic::set_hook` (or wrap the worker body
in `catch_unwind`) that marks the worker as failed and surfaces it; have the UI
loop check `JoinHandle::is_finished()` and either restart the worker or show a
persistent "monitor worker stopped" state. Do **not** treat "no samples" as
health.

## F2 — LLM samples are silently dropped when the channel is full (low–medium)

**Evidence**
- `src/app.rs:159` — `sync_channel::<LlmSample>(MAX_MONITORED_SERVERS)`; the
  capacity is `MAX_MONITORED_SERVERS = 16` (`src/domain.rs:25`).
- `src/app.rs:1400-1401` — `match tx.try_send(sample) { Ok(()) |
  Err(TrySendError::Full(_)) => {} … }`: a full channel is treated as success.
- `LlmSample` carries only `server`, `generation` and `stats`
  (`src/app.rs:609-614`) — no sequence number — so a receiver cannot detect a
  gap.

**Effect:** one poll cycle of 16 servers fills the channel exactly. If the app
does not drain for a full cycle (e.g. blocked in a repaint or a slow `/proc`
scan), the following cycle's samples are dropped entirely, silently and without
a counter. `generation` does not help: a dropped sample leaves no trace.

**Recommended follow-up:** add a dropped-sample counter (e.g. an `AtomicU64`
bumped on `Full`) and, if it should never happen, surface it in `diag`/the
footer; optionally give `LlmSample` a per-server sequence number so the app can
detect gaps. The CFA-94 test
`soak_smoke_16_servers_shutdown_with_undrained_channel` already pins the drop
behaviour.

## F3 — Fast snapshots are dropped when the channel is full (low, by design)

**Evidence**
- `src/app.rs:156` — `sync_channel::<FastSnapshot>(2)`.
- `src/app.rs:1033-1036` — `try_send`, `Full` → drop; `Disconnected` → the fast
  worker exits.

**Effect:** intentional "latest wins" for a UI/GPU snapshot; only a problem in
combination with F2/F4. There is no drop counter here either, but for a
snapshot that is replaced every refresh this is acceptable.

**Recommended follow-up:** none required. If a counter is added for F2 it can
cover this channel too for consistency.

## F4 — The poll rate under many servers is bounded by the slowest server, not the count (low)

**Evidence**
- `src/app.rs:1372-1409` — one scoped thread per server per cycle (up to 16),
  each publishing its sample immediately; the cycle then waits for **all** of
  them in `for sample in done_rx.iter()` (`:1399`) before
  `sleep_until_next_cycle` (`src/app.rs:1414`).
- HTTP bounds: `LLM_CONNECT_TIMEOUT_MS = 750`, `LLM_REQUEST_TIMEOUT_MS = 1200`
  (`src/llama.rs:42-44`).

**Effect:** a hanging server does not delay *delivery* of the others (they send
as soon as they finish), but it does delay the *next cycle of all servers* by up
to ~1.2 s. The comment at `src/app.rs:1367-1371` describes delivery only; it
does not claim the cycle rate is independent of the slowest server.

**Recommended follow-up:** if a hung server must not slow the others, move to a
fixed per-server poll pool with independent cadences. Low priority: the worst
case is one request timeout per cycle, and the UI already keeps the last values.

## F5 — Thread count stays bounded under 16 servers, but with high thread churn (low)

**Evidence**
- Steady-state threads: 1 main (`run`), 1 fast worker (`spawn_fast_worker`,
  `src/app.rs:854`), 1 LLM worker (`spawn_llm_worker`, `:1329`), plus up to 16
  short-lived scoped threads per cycle (`:1376`), plus one `join_bounded` joiner
  thread per worker at shutdown (`:546`). `MIN_LLM_POLL_MS = 250`
  (`src/domain.rs:21`) allows up to ~64 thread creations/s.
- Measured (CFA-94 long soak, 16 servers, 60 s, single-threaded): peak
  `threads_peak: 79`, settling back to `threads_after: 2` after shutdown
  (`threads_before: 2`).

**Effect:** no leak — the scoped threads end with each cycle — but a measurable
creation/teardown cost and a peak that is bounded by the server cap plus the
fabric runtime, not by design intent alone. Not a correctness risk.

**Recommended follow-up:** a small fixed poll pool (16 workers, work stealing or
per-server assignment) would remove the churn. Optional; only if profiling shows
it matters.

## F6 — Shutdown is orderly and bounded; a worker that misses the deadline is not hard-killed (low)

**Evidence**
- `stop` is an `AtomicBool` (`src/app.rs:155`), set at `:538`. Workers check it
  in the loop head and in `sleep_until_next_cycle` (25 ms granularity,
  `src/app.rs:1469-1481`).
- `join_bounded` uses a 5 s deadline per worker (`src/app.rs:544-563`).

**Effect:** shutdown is clean and time-bounded for healthy workers. If the 5 s
deadline is missed, `run()` still returns `Ok(())`; the worker stays detached
and is reclaimed by the OS at process exit. A final sample can be lost, and the
UI never shows "a worker did not stop". Measured shutdown in the CFA-94 long
soak: `shutdown_ms` ≈ 1.2 s (all three runs), well inside the bound.

**Recommended follow-up:** none required for the deadline itself; optionally
record that a worker was leaked during shutdown (a flag/`diag` note), so a
repeatedly hanging worker is visible rather than silent.

## F7 — Generation IDs: stale rejection is correct; each set change costs one cycle (low)

**Evidence**
- `accepts_llm_sample` (`src/app.rs:617-619`) accepts only samples whose
  `generation` equals the current one.
- App generation `llm_generation` (`src/app.rs:166`) advances only on
  `SetServers` (`src/app.rs:762-766`); the worker keeps its own `generation`
  (`:1342`) and adopts the new value at the cycle head (`:1350-1360`).
- Rejection across generations is covered by
  `stale_llm_sample_is_rejected_across_generations` (`src/app.rs:1878`).

**Effect:** after a set change, the in-flight cycle's samples are rejected — by
design, to prevent cross-server mixing. The cost is at most one poll interval of
latency. `wrapping_add` on the generation (`src/app.rs:762`, `:815`) is
practically irrelevant.

**Recommended follow-up:** none.

## F8 — Control channels are unbounded (low)

**Evidence**
- `src/app.rs:163` — unbounded `mpsc::channel::<LlmCommand>()`.
- `src/app.rs:165` — unbounded `mpsc::channel::<(u64, Option<LocalServerIdentity>)>()`.
- Both are drained at the cycle head (`src/app.rs:1350`).

**Effect:** the messages are idempotent set/config updates issued rarely; with
≤ 16 servers and infrequent set changes there is no realistic growth. A
pathological producer could grow the queue, but the UI is the only producer and
it is single-threaded.

**Recommended follow-up:** none required; a `sync_channel` with a small capacity
would make the bound explicit if that ever changes.

---

## Coverage now in the tree

| Finding | Test (current line) |
| --- | --- |
| F1 (panic invisible to the join) | `runtime_risk_worker_panic_is_not_distinguishable_from_a_clean_exit` (`src/app.rs:4803`) — **added by this ticket** |
| F2 (full channel drops) | `soak_smoke_16_servers_shutdown_with_undrained_channel` (`src/app/tests/soak.rs`) |
| F4 (slow server, independent delivery) | `a_slow_server_does_not_delay_a_healthy_servers_delivery` (`src/app.rs:3046`), `an_unreachable_server_does_not_block_a_healthy_one` (`:2923`) |
| F5 (thread ceiling, churn) | `soak_long_16_servers` (`src/app/tests/soak.rs`, `#[ignore]`), `soak_many_set_servers_rounds_bound_resources_single_threaded` (`src/app.rs:4717`, `#[ignore]`) |
| F6 (bounded shutdown) | `soak_clean_shutdown_is_bounded_while_a_server_hangs` (`src/app.rs:4275`), `soak_reconnects_after_request_timeout` (`:4319`) |
| F7 (generation/stale) | `stale_llm_sample_is_rejected_across_generations` (`src/app.rs:1878`), `soak_set_servers_churn_keeps_generations_and_polling_bounded` (`:4427`) |
| Reconnect/telemetry/corrupt JSON | `soak_reconnects_after_request_timeout` (`:4319`), `soak_counter_reset_is_reported_as_unavailable_not_a_fantasy_rate` (`:4360`), `soak_endurance_all_failure_modes_without_mixing_and_recovers` (`:4611`) |
| Max servers | `soak_polls_the_maximum_monitored_server_count_independently` (`src/app.rs:4215`) |

## Gaps left deliberately open (no runtime fix in this ticket)

- **F1 in the real worker:** only the join helper is pinned; forcing the live
  worker to panic needs an injectable failure point (a runtime change).
- **F5 ceiling on a real host:** the thread peak is measured under the soak's
  loopback mocks, not under NVML/driver threads; a production measurement is a
  cf-desktop load run, outside REL-02.
- **F6 hard-kill:** there is no way to force a worker to overrun the 5 s
  deadline without injecting a wedged call.

## Limits

Static analysis plus behaviour-pinning tests; the only dynamic evidence is the
CFA-94 long-soak run recorded with it. No production host, no NVML, no real
inference server was exercised. All line numbers refer to the analysed tree
(commit `ac424d5` plus the F1 test added by this change).

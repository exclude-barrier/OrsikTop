# Runtime risk analysis (REL-02 / CFA-95)

Independent, **read-only** review of the OrsikTop LLM worker's runtime paths:
bounded channels, dropped samples, generation IDs, reconnect transitions,
worker shutdown, panic handling and the thread count under 16 servers.

- **Ticket:** CFA-95 (EPIC 4 / REL-02).
- **Analysed at:** commit `ac424d5` (the CFA-94 soak harness commit on
  `test/cfa-94-95-soak-and-runtime-risks`); this ticket's change then adds the
  single F1 test below.
- **Update 2026-10-17:** F1 was fixed in the worker-health change; F2/F3 drop
  counters and the footer warning followed in the snapshot-drop change. The
  clean release run on `cf-desktop` and the long soak close the F5
  driver-thread gap. The analysis below is kept as the original baseline;
  F1–F3 and F5 document the new behaviour.
- **Scope:** the original review changed no `app.rs` runtime logic. The
  worker-health follow-up implements F1; the snapshot-drop follow-up implements
  the F2/F3 counters; the clean release run and long soak add F5 evidence. The
  rest of this document remains the historical read-only analysis.
- **Method:** static reading of `src/app.rs`, `src/llama.rs` and `src/domain.rs`
  plus the existing worker tests; line numbers are from the tree above. Dynamic
  evidence includes the CFA-94 soak, the `ee2d778` long soak, and the clean
  release-binary run on `cf-desktop` with an active RTX 4090. No real inference
  server was contacted.

Every finding below was **re-verified against the current source**: the earlier
CFA-95 comment was written at `e9600d6`, so both the file:line evidence and the
"is it still true?" question were re-checked here. Line numbers below are the
current ones (they shifted by two after the soak module was added under
`mod tests`).

Severities: **high** = wrong/no data with no user signal and no recovery;
**medium** = data gap or unbounded resource under realistic conditions;
**low** = bounded, by-design, or cosmetic.

---

## F1 — Worker panics are surfaced on disconnect and at shutdown (resolved)

**Evidence**
- `drain_try_recv` distinguishes `TryRecvError::Empty` from `Disconnected`.
  `app::run` returns an error naming the stopped fast or LLM worker instead of
  continuing with stale data.
- `join_bounded(handle, name, timeout)` preserves clean exit, panic, shutdown
  timeout and watcher-disconnect outcomes; `app::run` joins both workers and
  returns the first error.
- Tests: `drain_try_recv_distinguishes_empty_from_disconnected`,
  `join_bounded_reports_clean_exit`, `join_bounded_reports_panicked_worker`,
  `join_bounded_reports_timeout`.

**Effect:** a worker that panics or exits unexpectedly terminates the dashboard
with a concrete error instead of leaving stale telemetry on screen. A wedged
worker still times out after 5 s; shutdown does not wait indefinitely.

**Remaining limit:** there is no automatic restart or persistent in-TUI health
banner. `run` returns the error to the caller. That is the next reliability step
if automatic recovery is desired.

## F2 — LLM samples dropped on a full channel are counted (low–medium, resolved)

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
scan), the following cycle's samples are dropped entirely. `generation` does
not help: a dropped sample leaves no gap marker.

**Resolution:** both workers now increment an `AtomicU64` on
`TrySendError::Full` through `try_send_snapshot`; the UI publishes the counters
to `UiState` every loop iteration and the footer shows `DROPS fN lN` only when
one is nonzero. A disconnect is not counted. This makes drops visible without
waiting for the next delivered sample.

**Remaining limit:** no per-server sequence number, so the exact missing sample
is not identified; the counter is aggregate. The CFA-94 test
`soak_smoke_16_servers_shutdown_with_undrained_channel` still pins the drop
behaviour.

## F3 — Fast snapshots are dropped when the channel is full (low, by design, counted)

**Evidence**
- `src/app.rs:156` — `sync_channel::<FastSnapshot>(2)`.
- `src/app.rs:1033-1036` — `try_send`, `Full` → drop; `Disconnected` → the fast
  worker exits.

**Effect:** intentional "latest wins" for a UI/GPU snapshot; only a problem in
combination with F2/F4.

**Resolution:** the same `try_send_snapshot` counter covers this channel; the
footer reports the fast and LLM counters together. The snapshot is still
replaced by the next refresh, so no history is fabricated.

## F4 — Polls are independent per server (resolved)

**Evidence**
- `spawn_llm_worker` keeps one shared monitor and one `in_flight` flag per
  server (`LlmPollSlot`). Each cycle starts a poll only for a server whose
  previous request finished; a slow server is simply skipped and the next cycle
  starts on schedule for the others.
- Samples are forwarded from a persistent channel as they arrive; the cycle no
  longer joins a scoped thread per server.
- Shutdown waits up to 2 s for in-flight poll flags to clear before the worker
  returns.

**Effect:** a hanging or slow server no longer delays healthy servers' next
poll. At most one request per server is in flight, so thread growth stays
bounded by the 16-server cap. The worst case is a slow server refreshing less
often; its last value remains visible.

**Remaining limit:** a truly wedged request can outlive the worker until process
exit; the HTTP request timeout bounds normal cases to ~1.2 s.

## F5 — Thread count stays bounded under 16 servers (low, measured; keep architecture)

**Evidence**
- Steady-state threads: 1 main (`run`), 1 fast worker (`spawn_fast_worker`,
  `src/app.rs:854`), 1 LLM worker (`spawn_llm_worker`, `:1329`), plus up to 16
  short-lived scoped threads per cycle (`:1376`), plus one `join_bounded` joiner
  thread per worker at shutdown (`:546`). `MIN_LLM_POLL_MS = 250`
  (`src/domain.rs:21`) allows up to ~64 thread creations/s.
- Measured (CFA-94 long soak, 16 servers, 60 s, single-threaded): peak
  `threads_peak: 79`, settling back to `threads_after: 2` after shutdown
  (`threads_before: 2`).
- Measured (release binary `c013880`, `cf-desktop`, RTX 4090 driver active,
  16 loopback mocks, 30 s, sampled from outside via `/proc/<pid>/status`):
  mean `20,25`, p95 `20`, observed max `67`; 4 servers mean/max `8,02`/`20`,
  1 server `5,00`/`7`, idle max `5`. This includes NVML/driver threads; the
  host was idle (load `0,23`–`0,44`). See `docs/PERFORMANCE_BASELINE.md`.
- Long soak on `ee2d778` (60 s per server count): 16 servers peak `78`,
  settling to `2` after shutdown; RSS median `18 848` → `17 660` kB. See
  `docs/SOAK_HARNESS.md`.
- Release run on `cf-desktop` at 16 servers / 250 ms (two 60-s runs): peak
  `68`/`75`, mean `20,66`, CPU `11,05–11,08 %` of one CPU, poll cycle median
  `250,1` ms, shutdown `32`/`114` ms. The thread-start rate is only an external
  lower bound (`13,0–13,6`/s at 50-ms sampling), not a complete count.
  **Decision:** keep thread-per-poll; no pool without a measured long-run
  trend.
- 4-h release soak (four servers, `14400 s`): threads mean `8,02`, p95 `8`,
  max `24`; RSS first/last `42 334`/`39 052` kB, HWM `51 920` kB. No thread or
  RSS accumulation; shutdown `32,9` ms. See `docs/SOAK_HARNESS.md`.

**Effect:** no leak — the scoped threads end with each cycle — but a measurable
creation/teardown cost and a peak that is bounded by the server cap plus the
fabric runtime, not by design intent alone. The release run confirms the mean
stays at the cap-driven level and the driver-thread gap is closed; the
transient peak is higher than the steady state but below the long-soak peak.
Not a correctness risk.

**Recommended follow-up:** none after PERF-02. A fixed poll pool would remove
the churn, but 16 servers at 250 ms measured only 11.1 % of one CPU and a
bounded peak, so the pool is not justified without a long-run trend.

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
soak: `shutdown_ms` ≈ 1.2 s (all three runs); 4-h release soak: `32,9` ms.
Both are well inside the bound.

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
| F1 (worker panic/disconnect) | `drain_try_recv_distinguishes_empty_from_disconnected`, `join_bounded_reports_clean_exit`, `join_bounded_reports_panicked_worker`, `join_bounded_reports_timeout` |
| F2 (full channel drops) | `soak_smoke_16_servers_shutdown_with_undrained_channel` (`src/app/tests/soak.rs`), `try_send_snapshot_counts_full_channel_and_stops_on_disconnect`, `footer_renders_drop_warning_only_after_a_dropped_snapshot` |
| F4 (slow server, independent delivery) | `a_slow_server_does_not_delay_a_healthy_servers_delivery` (`src/app.rs:3046`), `an_unreachable_server_does_not_block_a_healthy_one` (`:2923`) |
| F5 (thread ceiling, churn) | `soak_long_16_servers` (`src/app/tests/soak.rs`, `#[ignore]`), `soak_many_set_servers_rounds_bound_resources_single_threaded` (`src/app.rs:4717`, `#[ignore]`) |
| F6 (bounded shutdown) | `soak_clean_shutdown_is_bounded_while_a_server_hangs` (`src/app.rs:4275`), `soak_reconnects_after_request_timeout` (`:4319`) |
| F7 (generation/stale) | `stale_llm_sample_is_rejected_across_generations` (`src/app.rs:1878`), `soak_set_servers_churn_keeps_generations_and_polling_bounded` (`:4427`) |
| Reconnect/telemetry/corrupt JSON | `soak_reconnects_after_request_timeout` (`:4319`), `soak_counter_reset_is_reported_as_unavailable_not_a_fantasy_rate` (`:4360`), `soak_endurance_all_failure_modes_without_mixing_and_recovers` (`:4611`) |
| Max servers | `soak_polls_the_maximum_monitored_server_count_independently` (`src/app.rs:4215`) |

## Gaps left deliberately open (no runtime fix in this ticket)

- **F1 recovery:** failures are visible, but there is no automatic worker
  restart or persistent in-TUI banner; `app::run` returns the error.
- **F5 real inference:** the release measurement includes NVML/driver threads,
  but still uses loopback mocks rather than a real llama.cpp workload.
- **F6 hard-kill:** there is no way to force a worker to overrun the 5 s
  deadline without injecting a wedged call.

## Limits

Static analysis plus behaviour-pinning tests. Dynamic evidence now includes the
CFA-94 soak, the long soak on `ee2d778`, and the clean release-binary run on
`cf-desktop` with an active RTX 4090 (NVML), but no real inference workload.
All line numbers refer to the analysed tree (commit `ac424d5` plus the F1 test
added by this change).

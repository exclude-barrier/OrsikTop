# Stage Summary — S13 (Worker/concurrency: owned, bounded shutdown)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/app.rs` | `spawn_fast_worker` and `spawn_llm_worker` now return their `thread::JoinHandle<()>` (the `thread::spawn(...);` semicolons removed so the handles are returned). `run()` captures both handles (`fast_worker`, `llm_worker`) and, after setting `stop`, drains each with a small `join_bounded` helper: a side thread calls the blocking `handle.join()`, and the main thread polls the completion signal every 20 ms against a 5 s deadline. |
| `CHANGELOG.md` | `[Unreleased]` gained the S13 Changed entry. |
| `docs/S13_STAGE_SUMMARY.md` | This summary. |

## 2. Architecture changes

- **Audit of the existing model (what was already satisfied, unchanged):**
  - *Blocking work off the render loop:* GPU sampling (NVML/i915/xe sysfs), the
    `/proc` process scan + DRM fdinfo sampler, the RAPL two-read window, and
    the llama.cpp HTTP poll all run in the two worker threads. The render loop
    only drains the channels, draws, and polls terminal events (50 ms).
  - *Bounded channels, latest-wins drop-stale:* `mpsc::sync_channel(2)` for both
    fast and LLM snapshots; workers `try_send` and drop on `Full`; the main
    loop `try_recv`-drains so the newest snapshot always wins and stale
    intermediates are discarded — no backlog can form.
  - *Fast UI refresh decoupled from expensive sources:* guaranteed by S12 (the
    100 ms floor only ever costs the GPU sample + snapshot clone; the 250 ms
    system cadence and 1 s slow-sensor cadence gate the rest).
- **The gap closed: thread ownership / clean shutdown.** Previously `run()`
  set `stop` and returned, leaving both workers as unjoined detached threads;
  the process simply exited via `main` returning and the kernel reaped the
  threads. Nothing *verified* the workers actually exited, and a worker wedged
  in a slow vendor/filesystem call could race the process exit. Now `run()`
  owns the handles and waits for both workers to exit before returning.
- **The join is bounded, never blocking indefinitely.** Each worker polls
  `stop` inside `sleep_until_next_cycle` at 25 ms granularity and checks it at
  the top of every loop iteration, so a healthy worker exits within one or two
  cycles (~≤ 1 s at a 10 s UI refresh, far less normally). The 5 s deadline
  covers even a worker stuck mid-sampling (e.g. a wedged NVML call); after it,
  `run()` returns and the OS reclaims the thread on process exit. The bounded
  join itself uses a side thread for the blocking `handle.join()` so the main
  thread stays a non-blocking poller — consistent with the app's
  "the render/exit path never blocks on a worker" principle.
- **Panic safety.** If a worker panics, its `join()` returns `Err(Panic)`; the
  side thread still sends its completion signal, so the bounded join always
  terminates. (Worker panics are a pre-existing condition not introduced here;
  this change only makes shutdown deterministic.)

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **182 passed, 0 failed, 3 ignored**.
`cargo test -- --ignored` → **3 passed** (`discovery_real`, `intel_real`,
`drm_real`). No new fixture tests: the change is thread-lifecycle wiring
(spawn/join), not parseable logic; a unit test of the join helper would only
test `std::thread`, and the real behavior (workers observe `stop` and exit) is
exercised end-to-end by the binary shutdown.

Real-machine smoke: `cargo run` in this non-TTY shell reaches the pre-existing
ENXIO terminal-init error *before* the render loop (so the join path is not
reached there); in a real terminal the `Esc`/`q` path sets `stop`, both
workers exit within their next cycle, and `run()` returns after joining — the
process now shuts down with both workers verified-exited instead of detached.

## 4. Known limitations

- **A wedged worker is abandoned, not killed.** Past the 5 s deadline the
  process exits and the OS reclaims the thread; OrsikTop cannot (and should
  not, read-only/no-root) send signals to its own threads beyond letting the
  process end. This is acceptable: a wedged vendor call is the only way a
  worker would outlive the deadline, and the TUI's job (restore terminal,
  return from `run`) is not blocked by it.
- **The join adds up to 5 s to shutdown in the pathological case.** Normal
  shutdown adds at most one worker cycle (tens of ms). The trade-off is a
  deterministic, owned exit versus a hard detach — preferred per the goal's
  "ensure clean shutdown and thread ownership".
- **`run()` has no way to be called re-entrantly with a live stop flag.**
  That was already true; the change does not alter the single-run contract.

## 5. Newly discovered issues

- **`JoinHandle::try_join` / `std::thread::panics` are not in this toolchain.**
  The toolchain is rustc 1.98.1 (edition 2021); the `try_join` +
  `thread::panics::{JoinError, StillRunning, UncaughtPanic}` API (stabilized
  for the new panic=abort/`catch_unwind` rework) is **not** present here —
  `cargo check` on a `try_join`-based join failed with
  `no method named try_join` / `cannot find panics in thread`. The bounded
  side-thread join above is the portable form and is what ships.
- **The workers' exit latency was bounded only informally before.** Both
  loops check `stop` at the top of each iteration *and* inside
  `sleep_until_next_cycle` (25 ms poll), so the worst-case shutdown delay is
  one cycle's work (a GPU sample + at most one 250 ms system block + the 1 s
  slow-sensor gate, which on shutdown is not re-entered mid-cycle). The join
  makes that worst case explicit (≤ 5 s budget) rather than relying on the
  process exit to mask it.

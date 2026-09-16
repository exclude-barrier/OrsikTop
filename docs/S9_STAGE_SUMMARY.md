# Stage Summary — S9 (Process identity + /proc performance)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/domain.rs` | New `ProcessIdentity { pid, start_time }` (Copy, Eq, Hash — the robust process key). `ProcessStats` gains `start_time: u64` and an `identity()` accessor. |
| `src/app.rs` | New `ProcessCache` keyed by `ProcessIdentity`. `collect_process_stats` now takes the cache: stable metadata (program, executable path, thread count) is read **once per process instance**; only CPU/memory are resampled per process refresh. `retain` evicts exited processes. 3 regression tests. |
| `src/ui.rs` | `ProcessRowTarget::Process(ProcessIdentity)`; selection, pin and double-click state are keyed by identity instead of bare PID. Display, sorting and search still use the visible `pid`. All process-related tests updated. |

## 2. Architecture changes

- **Robust process identity.** PID alone is ambiguous because the kernel
  recycles PIDs. Identity is now `(pid, start_time)` — `start_time` comes from
  `/proc/<pid>/stat` (field 22) via `sysinfo`'s `Process::start_time()`. A
  reused PID always carries a different start time, so the old process and its
  replacement are distinct identities. This is the same identity S16 will use
  to map llama.cpp processes to GPUs.
- **Stable metadata cached, dynamic counters resampled.** The expensive part
  of the old hot path was `read_process_thread_count(pid)` — a
  `/proc/<pid>/status` read + parse **per process per process refresh**.
  Thread count (and the program/executable strings) are now cached per
  identity; a long-lived process costs one `/proc` status read over its whole
  lifetime in OrsikTop, not one per second. The cache is bounded by the live
  process set: `retain` drops exited identities every process refresh, so it
  cannot grow unbounded across process churn.
- **UI state follows the identity.** Selection, pinning and double-click
  detection are keyed by identity, so a PID being reused between refreshes can
  no longer silently redirect a selected/pinned row to a different program.
  The row's visible PID column, `min_pid` group sorting, and PID search are
  unchanged (they are presentation, not identity).
- **Behavior preserved.** The process table, grouping, sorting, search,
  pinning, navigation and refresh cadence are all unchanged; only the keying
  and the metadata read frequency differ.

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features
-- -D warnings`, `cargo test --all` → all green: **116 passed, 0 failed, 1
ignored** (was 113; +3 new `ProcessCache` tests: reuse of stable metadata for
the same identity, reused-PID-with-new-start-time treated as a new process,
`retain` evicting exited processes).

## 4. Known limitations

- **`exe` resolution still relies on `sysinfo`.** The executable path is read
  through `sysinfo`'s `with_exe(UpdateKind::OnlyIfNotSet)` as before; the
  cache stores the *result*, so even the string copy is avoided on subsequent
  refreshes, but the underlying `/proc/<pid>/exe` read behavior is sysinfo's.
- **`start_time` granularity is one second.** Two processes cannot start in
  the same wall-clock second *and* share a PID (a PID is single-use), so
  identity remains unique; the field is only used for equality, not ordering.
- **Thread count staleness.** A process that spawns/drops threads keeps its
  cached thread count for the rest of its life in OrsikTop. This matches the
  stage intent (thread count is quasi-static); a live thread count is an
  S12-style fast metric if ever needed.
- The TUI cannot be smoke-tested in this headless shell (no TTY); the ENXIO
  terminal-init error is pre-existing and unrelated to this change.

## 5. Newly discovered issues

- **`sysinfo 0.39` already caches `exe` internally** (`exe: Option<PathBuf>`
  with `OnlyIfNotSet`), so the only truly repeated per-PID `/proc` access in
  the old code was the thread-count read. That confirmed the cache should key
  thread count (and the metadata strings) by identity rather than trying to
  make `sysinfo` skip `exe` work it already skips.
- **`ProcessStats` cannot derive `Copy`/`Eq`/`Hash`** (it holds `String` and
  `f64`), so `ProcessIdentity` is a separate small `Copy` struct derived from
  it via `identity()`, rather than adding those traits to the row struct.

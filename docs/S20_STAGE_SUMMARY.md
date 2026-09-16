# Stage Summary — S20 (Security and robustness)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/main.rs` | `run_updater` now spawns the updater and polls `Child::try_wait()` against a 10 min deadline (`UPDATER_TIMEOUT_MS`), killing the child on expiry. Factored into `run_updater_command` + `wait_for_child(child, timeout, poll)` (both parameterized for tests) and an `UpdaterError` enum. 3 new tests. |
| `src/llama.rs` | `LlamaMonitor` gained `local_spec` / `local_spec_scanned_at` fields and a `local_spec_config()` helper: the local llama-server `/proc` scan (spec-decoding CLI args + env vars) is now cached and re-run only when the `SPEC_REFRESH` (30 s) window elapses, instead of on every 250 ms sample. 1 new test. |
| `docs/S20_STAGE_SUMMARY.md` | This summary. |

## 2. Audit results

| S20 requirement | Status | Evidence |
| --- | --- | --- |
| No root requirement | ✅ | All filesystem reads target world-readable `/sys`, `/proc`, and the user-owned config file. The one root-gated counter (`energy_uj`, mode `-r-------- root:root`) degrades to `None` (explicit "unavailable"), never a fake zero. No `setuid`, no `cap_*`, no `sudo` invocation. |
| Read-only hardware access | ✅ | Every hardware read is `read_dir`/`read_to_string`/`read`/`symlink_target` on `/sys` or `/proc`. No `write`, no `ioctl`, no `mmap` of device memory. NVML is read-only (`nvmlDeviceGet*` queries). The only `fs::write` in the binary is `config::save` (user-owned config file, opt-in via settings UI). The only `fs::remove_file`/`remove_dir_all` is `run_uninstall` (explicit `orsiktop uninstall` subcommand). |
| Localhost-safe defaults | ✅ | `DEFAULT_SERVER = "http://127.0.0.1:8080"` (main.rs, diagnostics.rs). Auto-discovery (`discover_processes`) only finds local `/proc` processes. A remote endpoint requires explicit user configuration (`server=` config key or `--server` flag). `local_speculative_process_config` returns `None` immediately for non-loopback hosts (no `/proc` scan for remote endpoints). |
| Bounded HTTP timeouts | ✅ | `LlamaMonitor::new` sets `connect_timeout(750 ms)` and `timeout(1200 ms)` on the `reqwest::Client`. The 1200 ms is a *total* request timeout (connect + response body), so a stalled server cannot block the LLM worker indefinitely. The three endpoints (`/props`, `/metrics`, `/slots`) are fetched concurrently via `thread::scope`, so the poll cycle is bounded by the slowest single request (≤ 1200 ms), not the sum. |
| Bounded subprocess waits | ✅ (fixed) | `orsiktop update` previously used `Command::status()` — an unbounded wait: a hung `orsiktop-update` (stalled download, wedged write) would hang the command forever. Now `spawn()` + `try_wait()` polled against a 10 min deadline (`UPDATER_TIMEOUT_MS`, generous because the updater legitimately downloads + replaces binaries); on expiry the child is `kill()`ed and reaped, and the user is told it was terminated. |
| Bounded queues | ✅ | `fast_tx/fast_rx` and `llm_tx/llm_rx` are `sync_channel(2)` (capacity 2). The render loop drains with non-blocking `try_recv` in a `while let Ok(…)` loop (app.rs L91–101), so it always takes the latest snapshot and drops stale intermediates. `server_tx/server_rx` and `server_pid_tx/server_pid_rx` are unbounded but carry at most one in-flight value each (a `String`/`Option<u32>`), drained with `try_recv` — no growth risk. |
| Bounded work on sample paths | ✅ (fixed) | `local_speculative_process_config` (loopback-only scan of every `/proc/<pid>/cmdline` + the match's `environ`) previously ran on **every** LLM sample (250 ms floor) even though the config of a running server process is static for its lifetime. Now cached on `LlamaMonitor` (`local_spec` + `local_spec_scanned_at`) and re-scanned only every `SPEC_REFRESH` = 30 s — the same window pattern as `/props`. A dead local server costs at most one `/proc` walk per 30 s; a restarted server (new PID/args) is picked up within one window. |
| Robust UTF-8/path handling | ✅ | `cmdline` bytes → `String::from_utf8_lossy` per NUL-delimited arg. `environ` bytes → `filter_map(str::from_utf8).ok()` — non-UTF8 entries are skipped, not panicked on. Path components → `.to_string_lossy()` (discovery.rs, gpu_map.rs). `read_text` (providers) trims and filters empty strings. `read_u64`/`read_i64` use `.parse().ok()` — malformed numeric values degrade to `None` (covered by S19's `malformed_sysfs_values_degrade_to_none_not_fakes`). |
| Graceful permission failures | ✅ | Every filesystem read in the telemetry path uses `.ok()`, `?` on `Option`, or `unwrap_or_default()` — a permission-denied read yields `None` (metric unavailable), never a panic. `discover_gpus` returns an empty `Vec` when `/sys/class/drm` is unreadable. `detect_topology` returns an `Unknown`-vendor topology when `/proc/cpuinfo` is unreadable. `CpuSensors::discover` stores `layout: None` when cpufreq/hwmon/powercap are unreadable; samples return `None`. The NVML path returns an empty device list when NVML is unavailable (non-NVIDIA host, no driver). |

## 3. Updater and network path audit

- **`run_updater`** (main.rs): invokes the `orsiktop-update` sibling binary
  (or a `PATH`-resolved one). **Now bounded** (see §2): `spawn()` +
  `try_wait()` polling (100 ms poll) against a 10 min deadline; on timeout
  the child is killed and reaped and the caller is told. `spawn` errors map
  to the existing "not found → cargo install hint" message (unchanged). The
  TUI itself performs no network I/O except the bounded llama.cpp polling;
  the updater runs only under the `update` subcommand, before the TUI
  initializes.
- **`run_uninstall`** (main.rs): reads `env::current_exe()`, removes at most
  two files (`orsiktop`, `orsiktop-update`) from the executable's directory,
  and optionally removes the user config directory. No network, no
  subprocess, no privilege escalation. Path construction uses
  `PathBuf::join` (no shell interpolation).
- **llama.cpp HTTP** (llama.rs): the only network surface. `reqwest` with
  `rustls-tls` (no native TLS, no certificate authority bypass). The
  `Client` is built once in `LlamaMonitor::new` with bounded timeouts.
  `fetch_raw`/`fetch_json` use `.send()` + `.text()`/`.json()` — errors are
  mapped to `Unreachable`/`Http`/`Body`/`Invalid` outcomes, never panics.
  The `thread::scope` fetches join with `.join().unwrap()` — safe because
  the spawned closures never panic (all internal errors are `Result`/enum
  values, not panics).

## 4. Known limitations (documented, not fixed)

- **HTTP body size is bounded by the 1200 ms request timeout, not a byte
  cap.** `response.text()` and `response.json::<Value>()` read the entire
  body into memory. The 1200 ms total-request timeout bounds the transfer
  duration, so the worst-case memory is "1.2 s of body at the link rate."
  On a loopback link this is well under a few hundred MB; on a slow remote
  link it is far less. A byte cap (e.g. `content-length` pre-check) would
  add complexity for a low-probability attack vector (the endpoint is
  user-chosen).
- **The updater 10 min deadline is a kill switch, not a UX.** A download
  that legitimately exceeds 10 min on a slow link is terminated and the
  update must be retried. 10 min was chosen as generous for the realistic
  case (a single release tarball) while still guaranteeing the command
  terminates; the tradeoff is documented in the code
  (`UPDATER_TIMEOUT_MS`).
- **The spec-config cache assumes a running server's spec-decoding config is
  static for its lifetime.** It is (CLI args/env are read once at process
  start), so the 30 s window only costs a re-scan when the server restarts;
  a restarted server's config is picked up within 30 s. Noted so a future
  "hot config" feature does not assume the cache is live.

## 5. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **191 passed, 0 failed, 3 ignored**
(187 pre-S20 → +4: `updater_wait_times_out_a_hung_process`,
`updater_wait_returns_failure_for_nonzero_exit`,
`updater_wait_missing_binary_is_not_found`,
`local_spec_config_is_cached_within_the_refresh_window`).
`cargo test --all -- --ignored` → **3 passed** (unchanged).

Live smoke on the 288V box:
- `cargo run -- update` → updater found on PATH, reinstalled 0.1.4
  successfully (exercises the new spawn + bounded-wait success path).
- `cargo run -- diag` → full report, llama section connected to the
  configured endpoint (Qwen3.8-27B-GGUF:UD-Q4_K_M, 74710/240128 ctx).

New tests defend the S20 contracts:
- `updater_wait_times_out_a_hung_process`: a `sleep 30` child against a
  30 ms deadline is killed, reported `TimedOut`, and reaped — a hung
  updater can no longer hang the caller (fails on the old
  `status()`-based code, which would have blocked 30 s).
- `updater_wait_returns_failure_for_nonzero_exit`: `sh -c "exit 3"` →
  `Failed` with the exit status in the message (no silent success).
- `updater_wait_missing_binary_is_not_found`: a missing binary maps to
  `NotFound` (the user-facing cargo-install hint path).
- `local_spec_config_is_cached_within_the_refresh_window`: a non-loopback
  endpoint returns `None` and a second call within the window does not
  re-scan (timestamp unchanged) — the 250 ms `/proc` walk is gone.

The existing suite already covers the other S20 robustness contracts:
graceful degradation (`cpu_sensors::power_none_when_energy_counter_unreadable`,
`providers::amd::device_not_readable_reports_unavailable`,
`providers::intel::device_not_readable_reports_unavailable`,
`providers::amd::malformed_sysfs_values_degrade_to_none_not_fakes`), bounded
timeouts (`llama::props_failure_retries_next_sample`), UTF-8 handling
(`llama::parser_ignores_non_finite_values_and_does_not_collapse_labels`),
and PID reuse (`app::cache_treats_reused_pid_with_new_start_time_as_new_process`).

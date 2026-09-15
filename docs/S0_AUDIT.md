# S0 — Repository & Architecture Audit (HEAD `d135a60`)

## Module structure

| File | Lines | Role |
|---|---|---|
| `src/main.rs` | 375 | CLI (clap), updater/uninstall subcommands, llama.cpp server auto-discovery (`/proc` scan, lowest-PID win), `TerminalGuard` |
| `src/app.rs` | 786 | Event loop, two workers (fast: GPU+CPU+procs; LLM: HTTP), snapshots, settings apply, /proc readers (stat, loadavg, cpuinfo MHz, hwmon temp) |
| `src/ui.rs` | 4939 | All rendering + `UiState` (selection, history, settings popup) **and** the domain structs `ProcessStats`, `SystemStats` |
| `src/gpu.rs` | 415 | `GpuStats` + NVML-only `GpuMonitor` (single ordinal `gpu_index`) |
| `src/cpu.rs` | 510 | `CpuTopology` detection: cpuinfo identity, core groups, `cpu_capacity`, SMT fallback |
| `src/llama.rs` | 1086 | `LlmStats` + `LlamaMonitor` (reqwest blocking), serial `/props` → `/metrics` → `/slots`, counter deltas, speculative-decode heuristics, local process speculative config |
| `src/config.rs` | 222 | `AppConfig` (server, **gpu_index: u32**, refresh_ms, process_refresh_ms, offline_grace_ms, auto_discovery), JSON-ish text file |

## Telemetry ownership

- `GpuStats` (gpu.rs) — NVML fields, `available: bool`, `error: String`; unavailable metrics = 0.0 or `None`.
- `LlmStats` (llama.rs) — `connected`/`reconnecting` flags, string error.
- `SystemStats` + `ProcessStats` (ui.rs!) — CPU/mem/load/processes; built in app.rs workers.
- `CpuTopology` (cpu.rs) — vendor/model/core kinds; built once, cached in the fast worker.

## UI/domain coupling

`ui.rs` owns `ProcessStats`, `SystemStats`, refresh-interval constants
(`MIN_REFRESH_MS`, `MAX_REFRESH_MS`, `MIN_LLM_POLL_MS`) consumed by workers, and
`UiState` mixes navigation state with sample history. `app.rs` imports from
`ui.rs`. This is the primary inversion to fix in S1 (move domain structs and
constants to `domain.rs`). Rendering functions themselves are pure over
`&Stats` — good; no acquisition logic lives in `ui.rs`.

## Worker/thread architecture

- Fast worker (`spawn_fast_worker`): single thread, loop clamped to
  `MIN_REFRESH_MS=100ms`. Per cycle: process refresh (default 1s, sysinfo
  `refresh_processes_specifics` + per-pid `/proc/<pid>/status` thread count),
  250ms system block (sysinfo cpu/mem, `/proc/stat` iowait, `/proc/loadavg`,
  `/proc/cpuinfo` average MHz, full hwmon temp rescan), GPU sample (NVML).
  Sends `FastSnapshot` over `sync_channel(2)`; `Full` drops stale. ✔ drop-stale.
- LLM worker: reqwest blocking client (1200ms timeout), polls every
  `max(refresh, 250ms)`. Serial GETs: `/props` (cached 30s) → `/metrics` →
  `/slots`. Offline grace via `stabilize_llm_sample` (default 2500ms).
  `sync_channel(2)`, drop-stale.
- Main thread: `terminal.draw` per loop + `event::poll(50ms)`; `try_recv`
  drains both channels; `server_tx` unbounded for settings changes.
- Threads are fire-and-forget (`thread::spawn`, no JoinHandle); shutdown via
  `stop` flag only. Acceptable but noted (S13).

## Sampling frequencies

UI refresh default 1000ms (100–10000ms). Process refresh default 1000ms
(100–60000ms). System block fixed 250ms. GPU + everything else sampled every
worker cycle. LLM poll floored at 250ms. No adaptive classes: topology is
cached, but hwmon temp is rescanned and cpuinfo re-parsed every 250ms, and
sysinfo process refresh parses all /proc per process interval.

## Current NVIDIA implementation

`GpuMonitor::new(index)` calls `Nvml::init()`; `sample()` uses
`device_by_index(ordinal)` and ~15 NVML queries per cycle (utilization,
memory, temp, power, limit, pstate, throttle, clocks, enc/dec, fan, PCIe
throughput+link). No UUID/BDF, no MIG, no process info, no sysfs fallback.
Ordinal selection = `gpu_index` config/CLI. PCIe capacity math in gpu.rs
(`pcie_utilization_pct`) is pure and tested.

## Current CPU implementation

`detect_cpu_topology` (cpu.rs): cpuinfo vendor/model; `core_id` groups for
physical cores; core kinds via `cpu_core`/`cpu_atom` files, else
`cpu_capacity` classes, else Intel SMT-sibling fallback; else all Unknown.
Already capability-based, no marketing-name tables — good base for S10
(LP-E distinction missing; `CpuCoreKind` has only P/E/Unknown).
Frequency: `/proc/cpuinfo` average. Temp: hwmon name-substring scan every
250ms. No RAPL/powercap usage. No cpufreq.

## Current llama.cpp implementation

`LlamaMonitor`: single reqwest client, 1200ms overall timeout (also bounds
connect). Serial `/props`, `/metrics`, `/slots` per poll; each can take up to
the timeout, so a hung local server can consume up to ~3.6s per 250ms cycle
(the worker then just lags; UI keeps last snapshot — no backlog because
drop-stale, but samples stall). Props cached 30s. Counter-based live TPS from
`/metrics`; slot-based live TPS from `/slots`. Speculative-decode detection
from props + metrics + local process cmdline/env (reads target port's process
cmdline). Offline grace: hold last good sample up to `offline_grace_ms` with
`reconnecting=true`, except hard failures (501/metrics disabled).

## Configuration model

`AppConfig`: `server: Option<String>`, `gpu_index: u32` (fragile ordinal),
`refresh_ms`, `process_refresh_ms`, `offline_grace_ms`, `auto_discovery: bool`.
Plain-text `key=value` file in config dir. `sanitized()` clamps.

## Tests & CI

94 unit tests: config parsing, cpu topology (fixture-free, pure fns), llama
JSON/prometheus parsing (fixtures in `tests/fixtures/` used by unit tests),
gpu PCIe math, ui interaction (selection/sort/settings), main discovery
helpers. CI: fmt, check, clippy `-D warnings`, test, cargo-audit (deny
warnings), `cargo run -- --help` smoke. No hardware needed. ✔

## Platform assumptions

- Linux-only in practice (hardcoded `/proc`, `/sys/class/hwmon`, NVML).
- One monitored GPU (ordinal).
- NVML optional at runtime (graceful `error` string) but the only GPU path.
- reqwest blocking + rustls; no proxy assumptions.
- Updater: sibling `orsiktop-update` binary or `cargo install`; uninstall
  removes `~/.cargo/bin` artifacts (guarded).

## Expensive hot paths

1. sysinfo `refresh_processes_specifics(All)` every process interval — full
   `/proc` re-scan + per-process stat parse; plus per-pid `/proc/<pid>/status`
   re-read for threads.
2. `/proc/cpuinfo` re-read+re-parse every 250ms (frequency).
3. Full `/sys/class/hwmon` re-walk every 250ms (temp).
4. NVML: ~15 driver round-trips per cycle (fine, but re-init on index change).
5. llama.cpp: serial HTTP, up to 3×1200ms per poll when server hangs.

## Missing abstractions (per plan)

No `DeviceId`/BDF/UUID identity, no capability enum (zero-means-missing
pervasive in `GpuStats`), no filesystem abstraction (all `fs::` direct; parsing
fns are pure and testable though), no provider layer, no multi-GPU, no
AMD/Intel/fdinfo paths, no llama server→GPU mapping, discovery = lowest PID.

## Behavior that must remain compatible

- Single-GPU NVIDIA UX: same panel, same fields, same refresh/poll semantics.
- Config file format for existing keys (S4 migrates `gpu_index` safely).
- CLI: `--server`, `-i/--interval-ms`, `--gpu-index`, `update`,
  `uninstall --purge`, `ORSIKTOP_*` env vars.
- LLM offline grace + `/metrics`-disabled messaging + speculative detection.
- UI keys/mouse interactions, settings popup fields, help popup.
- 94-test suite green; CI gates unchanged.
- Read-only, no root, localhost defaults.

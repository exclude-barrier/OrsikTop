# Changelog

All notable changes to OrsikTop will be documented here.

## [0.2.6] - 2026-10-01

### Added

- Multi-slot llama.cpp servers (`--parallel > 1`) now show which slot
  supplies the displayed CTX value: the CTX row gains a compact `S<n>` tag
  naming the selected slot by its own `/slots` `id` (e.g. `CTX … 68.4%
  S1 78,800 / 115,200 tok`). The tag appears only with two
  or more slots and a known slot identity — single-slot servers keep the
  compact row, a slot without an `id` field prints no number, and on very
  narrow terminals the tag is dropped before any context value is.

- Multi-slot llama.cpp servers (`--parallel > 1`) now also show a compact
  per-slot context overview directly below the CTX row, reusing the same
  `/slots` poll (no extra request): `SLOTS S0 58.7k* S1 34.2k`. Each entry
  names the slot by its own `/slots` id (real IDs, never array positions,
  ordered by id), busy slots are highlighted and idle ones dimmed, and the
  slot supplying the main CTX row is marked with a trailing `*`. When slot
  capacities differ the entry switches to `used/capacity` form
  (`S0 58.7/115.2k* S3 34.2/70.1k`) instead of hiding the difference; absent
  values print `—` instead of a fake zero, and non-fitting entries are
  truncated with a visible `+n` remainder. The main CTX row stays the
  authoritative selected-slot display — the overview never sums slot
  contexts — and on narrow terminals the whole row is dropped before any
  core metric is. Single-slot servers are unaffected.

- Intel and AMD devices are identified by name more reliably. When
  `amdgpu` exposes no `product_name` (common on APUs), a well-known
  device-ID map (Granite Ridge, Strix Halo, Rembrandt, Navi 33, RX
  6600/6800/6900/7900/7700–7800 series) supplies a real name — a real
  `product_name` always wins, and unknown IDs keep the `cardN` fallback.
  The Intel map grows from 1 to 11 entries (Core Ultra 200V, Meteor
  Lake-P, Tiger Lake, Alder Lake, Arc A770/A750, Battlemage B580/B570).
  Intel names are live-verified on an Arc (Core Ultra 200V) iGPU; the AMD
  map is fixture-tested only — no AMD hardware is available.

- Processes are attributed to a GPU through all of its DRM render nodes.
  A card now records every render node it exposes (`render_nodes`), not
  just the first, so a client on a compute-only node (e.g. `renderD129`
  on Intel iGPU/xe, which exposes a display+render node plus a
  compute-only node) is attributed to the right card. The by-path PCI BDF
  parser also accepts 8-digit PCI domains and canonicalizes them through
  `normalize_pci_bdf` (4-digit default domain; ≥ `0x10000` stay 8-digit).
  Multi-node attribution is fixture-tested; the available xe box exposes a
  single render node, so it is not live-verified.

### Fixed

- GPU power (`PWR`) now renders unavailable (`—`) instead of a fabricated
  `0%` when the driver exposes no power telemetry (e.g. the Intel xe
  iGPU). Previously the PWR meter was drawn unconditionally from
  `power_w / power_limit_w` defaulting to `0.0`, so a box with no power
  sensor showed an empty bar and `0%` — indistinguishable from a real 0 W
  reading, while the GPU/VRAM rows correctly showed `—`. The PWR row now
  follows the same `None => —` rule.

- The shared AMD/Intel hwmon lookup is now driver-aware
  (`find_hwmon_for_bdf()`). A hwmon whose `device` symlink contains the
  target BDF is a candidate; a candidate whose `name` matches a known
  driver then wins over one without, and a block exposing `temp1_input`
  wins over one that does not (ties keep directory order, so selection is
  deterministic). This changes which block is picked on systems where a
  power-domain hwmon (e.g. `acpi_power`) sits before the real GPU block.
  Fixture-tested; not yet live-verified on AMD/Intel hardware where both
  block kinds are present.

- RAM telemetry now renders unavailable (`—`) instead of a misleading
  `0.0%` / `0.0 GiB` when the total memory reading is missing or zero, in
  both the system panel meter and the RAM history sparkline — matching the
  existing VRAM and (now) POWER semantics (missing telemetry is never a
  fabricated zero).

- Process thread counts are no longer frozen at first sight. `ProcessCache`
  now caches only static metadata (program, executable path); the thread
  count is re-read from `/proc/<pid>/status` on every process refresh (not
  per UI frame) and stored as `Option`: an unreadable process renders `—`
  instead of a fabricated `1`, and the next refresh retries. A group's
  thread total stays `—` until every member is readable. Regression tests
  drive the cache across scripted refreshes (changed count, failed read,
  recovery, reused PID); an isolated run against the previous revision
  confirms it fabricated `1` and never refreshed.

- The monitor now treats endpoint, local server identity and auto/manual
  origin as one resolved state and compares the three fields independently,
  instead of updating identity/origin only when the URL changed. The local
  server identity is the PID **plus** its `/proc/<pid>/stat` start time, and
  the GPU mapping is only computed while the PID still carries that start
  time: a PID reused by another process (or an unreadable one) invalidates
  the mapping immediately, before the next resync. A llama.cpp restart on the
  same port updates the server→GPU mapping without resetting the LLM session
  or its history; an endpoint change resets it; an auto/manual flip relabels
  the display. In auto-discovery mode the target is re-resolved on a bounded
  5 s cadence (single `/proc` scan, never per UI frame); manual and remote
  endpoints stay static and gain no local attribution. Worst-case staleness
  after a restart or exit is therefore 5 s, while a reused PID is rejected
  at once. State transitions and the reused-PID rejection are covered by
  deterministic unit tests; an actual live llama.cpp restart was not
  exercised.

- `orsiktop diag` no longer prints URL credentials or other confidential URL
  parts. A central `safe_endpoint()` keeps only scheme/host/port (with
  `/…`, `?…`, `#…` markers for dropped path/query/fragment) and never echoes
  an unparseable input; HTTP-probe error text is passed through
  `redact_urls()`, so the error path cannot leak either. The sanitizer is
  display-only — the endpoint used for the connection is unchanged. Fixture
  tests cover userinfo, query, fragment, percent-encoding, IPv6 and invalid
  input, plus an integration test on the rendered report; an isolated run
  against the previous revision confirms it printed the configured
  credentials verbatim.

- Diagnostics error redaction no longer leaves URL tails behind. Free-text
  URL scanning now consumes up to whitespace only, so characters that are
  legal inside a URL (`,` `)` and the `]` of an IPv6 host) cannot truncate the
  token and leak a query secret; a token that still does not parse is
  replaced, never echoed. The transport error path additionally prefers
  structured redaction: the reqwest error is rendered with `without_url()`
  (category + status kept, request URL removed) and the result is redacted
  again as a safety net. New tests cover the three reproduced IPv4/IPv6
  comma/parenthesis cases and a real loopback reqwest error; the same tests
  fail against the previous scanner (reproducing the exact leak).

- Server-dependent async results are tagged with the session generation they
  were produced for and applied only to the matching target, so a /metrics
  reply for server A that lands after a switch to B (or a buffered reply from
  before) can no longer be shown under B or enter B's history; A→B→A is
  covered because generations are monotonic. The fast snapshot's GPU mapping
  is gated the same way, while its system/CPU data still applies. A new
  process on the same endpoint (a restart, not just a relabel) is signalled to
  the LLM worker separately and drops the server-dependent caches (counter
  baselines, cached /props) without starting a new UI session. Deterministic
  tests exercise the held-A → B → release-A flow, the A→B→A case and the
  stale-mapping rejection; they fail against the previous untagged flow.

- The settings dialog preserves the endpoint it was opened with. Only host and
  port are edited; scheme, path, userinfo, query and fragment are kept, and
  an unchanged host/port returns the original string byte for byte, so
  opening the dialog and saving (or editing only refresh/GPU) can no longer
  downgrade `https` to `http` or silently drop a path. URLs without an
  explicit port show the scheme default (443/80) and keep it implicit. Tests
  go through `open_settings` → `settings_config` for HTTPS with and without a
  port, IPv4, hostnames, IPv6 and a path; they fail against the previous
  always-`http://host:port` rebuild.

- A successful `/props` refresh now replaces the cached speculative state
  instead of accumulating it: an explicit disable (`speculative: false`,
  `n_max: 0`, empty `types`) clears a previously active MTP state and a
  changed `n_max` (3 → 8) is adopted, while a response that says nothing about
  speculation (or a failed fetch) keeps the cache. `default_generation_settings`
  takes precedence over root fields per field. The local server process config
  is only a fallback until `/props` has described speculation. Regression
  tests cover the A→B disable, 3→8, no-info, failed-fetch and nested/root
  priority cases; they fail against the previous `|=` accumulation.

- Per-slot throughput no longer treats the array position as a slot identity.
  A delta is only computed when every slot carries a unique `id` that also
  exists in the previous sample and its task did not switch; missing,
  duplicate or newly appeared ids, a first sample or a task change make the
  slot-delta path abstain and fall back to the aggregated `/metrics`
  counters, so reordering id-less slots no longer fabricates 900/90 tok/s.
  Context/slot display is unchanged. Tests cover reordered id-less slots,
  stable ids under reordering, duplicate ids and a new slot; they fail
  against the previous index-as-id pairing.

## [0.2.5] - 2026-09-18

### Added

- The llama.cpp→GPU mapping is now visible in the TUI. When the shown GPU
  is one the inference server is attributed to, the GPU panel title gains
  an `LLM` marker, and the LLM panel's state line gains a `GPU
  <identity>` entry naming the mapped device by PCI BDF or vendor UUID.
  A GPU without a stable identity, or an unresolved mapping, never shows
  the marker — no fake attribution; on a multi-GPU mapping the LLM panel
  names the first mapped device. Verified live on cf-desktop (RTX 4090,
  S24): the marker and the identity line agree with `orsiktop diag`.

- NVIDIA MIG awareness (S17). MIG-capable systems are now understood end to
  end: at construction the NVIDIA provider builds each physical device's MIG
  topology (`mig_mode` enabled → probe every MIG device slot); a MIG child is
  a first-class identity keyed by its driver MIG UUID
  (`MIG-GPU-<parent>-<gi>-<ci>`, with a synthetic `{parent}#mig-{slot}`
  fallback when a driver exposes no MIG UUID), selectable via
  `--gpu <MIG-uuid>`, and sampled through its own MIG device handle — child
  panels are titled `… MIG <slot>`, and metrics the driver does not expose
  per child (temperature, power, fan, clocks, PCIe) render `—` rather than
  fake zeros. The llama.cpp→GPU mapping attributes a process's NVML
  placements to the specific MIG child they run on
  (`gpu_instance_id`/`compute_instance_id`), one placement yielding exactly
  one identity, and the TUI's `LLM` chip matches a MIG child with its
  physical parent panel and vice versa. Non-MIG systems are unaffected: a
  device whose `mig_mode` reports unsupported (e.g. an RTX 4090) builds an
  empty topology and takes the original sampling path. Fixture-tested; not
  live-verified — no MIG hardware (A100/H100-class) is available, and the
  `Device::uuid()`-on-a-MIG-child assumption is documented in
  `docs/HARDWARE_SUPPORT.md`. The MIG topology is captured at construction
  only; an admin re-partition requires an OrsikTop restart.

### Fixed

- On llama.cpp servers before b10700, which do not expose
  speculative-decoding token counters, the LLM panel's SPEC row showed
  `0 draft / 0 accepted` — indistinguishable from a spec run that
  accepted nothing. An enabled spec run whose counters all read zero now
  shows the explicit hint `no data (server < b10700)` instead of the
  misleading zeros.

- On a llama.cpp server with `--parallel` (multiple slots), the LLM panel's
  context pair (`CTX used / size`) could mix values from two different slots:
  `used` was the maximum across all slots while `size` was the maximum
  `n_ctx`, and once slot capacities differ those maxima come from different
  slots. The pair is now always read from a single display slot — the
  most-used busy slot, or the most-used slot overall when no slot is busy —
  because an idle slot keeps its last task's context and its `used` value is
  stale. Verified live against a `--parallel 2` server: idle frames show the
  most-used slot's retained pair and busy frames show the busy slot's own
  pair, each cross-checked against the server's `/slots` output at the same
  moment.

- On NVIDIA machines the GPU's PCI BDF arrived in two different forms: NVML
  reports an 8-digit domain prefix (`00000000:01:00.0`) while the sysfs/DRM
  discovery path reports the 4-digit form (`0000:01:00.0`). The two never
  matched, so the llama.cpp→GPU mapping key did not unify with the
  discovered device — the LLM panel's `GPU …` identity line rendered the
  8-digit NVML form, the mapping's name/vendor fell back to unresolved, and
  the lspci-form selection `--gpu 0000:01:00.0` did not resolve the device.
  BDFs are now normalized to the canonical 4-digit form at every ingress
  point (device identity, GPU selector, NVML device list, NVML compute-app
  evidence), so one physical GPU carries one identity end to end. Verified on
  an RTX 4090: discovery, the mapping key, and the identity line all agree on
  `0000:01:00.0`, and `--gpu 0000:01:00.0` selects the GPU.

### Changed

- The NVIDIA backend no longer re-reads the device name, the enforced power
  limit, and the PCIe link speed/width on every sample. The device name is
  fetched once — it is immutable for the device's lifetime — and the three
  slow properties are refreshed at most once per second, keeping the last
  good values when a refresh fails. This removes four NVML driver round-trips
  from the default 10 Hz sampling path; measured on an RTX 4090 the process
  syscall rate drops by ~30/s and the steady-state telemetry-thread CPU by
  ~0.2 % of one core. Displayed values are unchanged in practice: power
  limits and PCIe link parameters only change on user action or link retrain,
  so a 1 s refresh is indistinguishable from live for them.

## [0.2.4] - 2026-09-16

### Fixed

- The SYSTEM panel's per-core minibar now renders on small hybrid CPUs. The
  full-view height gate was hard-coded to 15 rows, calibrated against an
  8-row hybrid box (e.g. an i9-12900K, 8P+8E). Hybrid boxes with fewer than
  7 max(P,E) physical core rows — e.g. a 4P+4E Core Ultra (such as the 288V)
  — got an inner panel height of 12 and silently collapsed to the compact
  4-line view (CPU and RAM meters only), hiding the P-CORES/E-CORES minibar
  entirely. The decision now lives in a single `system_full_view` check with
  a height floor matching the minimum full-view content, and the line budget
  of the full view fits the panel height the layout allocates exactly.

## [0.2.3] - 2026-09-16

### Fixed

- A bare-number GPU selection (`gpu=0`, `--gpu 0`, `--gpu-index 0`) no longer
  reports `no GPU matches the selection` on machines **without** an NVIDIA
  GPU. Since 0.2.1 the number meant the n-th *NVIDIA* device (the legacy
  NVML ordinal); on an NVIDIA-less box (e.g. an Intel APU laptop) that matched
  nothing and a legacy `gpu=0` config broke. The NVIDIA-ordinal
  interpretation is now used only when an NVIDIA GPU is present; otherwise it
  falls back to the n-th device overall (the pre-0.2.1 behavior). Boxes with
  an NVIDIA GPU are unaffected: `gpu=0` still selects the first NVIDIA device
  even when an iGPU is BDF-sorted first.

## [0.2.2] - 2026-09-16

### Changed

- Intel GPU/APU panels now show a short, real device name for a known
  device ID instead of the bare `card0` sysfs name (e.g. `Intel Arc (Core
  Ultra 200V iGPU)`). The kernel exposes no product name for these devices, so
  the map is intentionally small; unknown device IDs keep the previous `cardN`
  name — nothing regresses and no name is invented.

### Note

- On an Intel APU (e.g. Core Ultra iGPU) the xe driver exposes only the
  graphics clock and throttle reason; utilization, VRAM, temperature, power,
  fan, and PCIe have no value source and are shown as `—` (unavailable), never
  as a fake zero. This is driver behavior, not a detection failure — the GPU
  is detected and its available metrics are shown.

## [0.2.1] - 2026-09-16

### Fixed

- `gpu=0` (and `--gpu` / `--gpu-index` with a bare number) no longer selects
  the first *discovered* device overall — it again selects the n-th **NVIDIA**
  GPU (the legacy NVML ordinal the value always meant). On iGPU + dGPU
  machines the iGPU is usually BDF-sorted first, so a migrated `gpu_index=0`
  config silently pointed at the iGPU and the GPU panel showed nothing. The
  fix keeps `Auto`, `--gpu <BDF>` and `--gpu GPU-…` selection unchanged and is
  consistent with how the NVIDIA provider already resolves numeric indices.

## [0.2.0] - 2026-09-16

### Added

- AMD GPU/APU telemetry backend (`src/providers/amd.rs`) reading the amdgpu
  sysfs and its hwmon: utilization, memory bandwidth, VRAM total/used, package
  temperature, power draw, GPU/memory clocks, and fan.
- Intel GPU/APU telemetry backend (`src/providers/intel.rs`) reading the i915 and
  xe kernel sysfs plus the Intel hwmon: graphics clock (MHz) and throttle reason
  from the GT sysfs, package temperature and the PL1 power limit from the hwmon.
  `new_gpu_provider` now dispatches NVIDIA → NVML, AMD → amdgpu sysfs, Intel →
  i915/xe sysfs, choosing the backend from the *discovered device's* vendor.
- Process-level GPU telemetry via DRM fdinfo (`src/drm.rs`): for every process
  holding a DRM render node, per-engine utilization (i915/amdgpu busy-nanoseconds
  over wall time; xe busy/total cycles) and resident GPU memory, one entry per
  PCI BDF. The wide process table gains a `GPU` resident-memory column (`—` when
  the process has no DRM fd); compact mode is unchanged.
- Intel low-power E-core (LP-E) classification: the kernel's `cpu_lowpower`
  perf-PMU mask (mutually disjoint from `cpu_core`/`cpu_atom`) now yields a
  distinct `LowPower` core kind, counted in the new `low_power_cores`
  topology field. The CPU heatmap marks LP cores with an `L` suffix and the
  panel title appends `+{n}L` when LP cores are present; P/E-only boxes are
  unchanged.
- CPU sensor telemetry (`src/cpu_sensors.rs`): the SYSTEM panel now reports CPU
  frequency, temperature, and package power. Frequency is the cpufreq
  `scaling_cur_freq` weighted average across policies (falling back to
  `/proc/cpuinfo` `cpu MHz`); temperature is the CPU hwmon package/Tctl reading
  (per-core fallback); power is the RAPL `intel-rapl` package zone read twice a
  short window apart. Sensor discovery runs once and is cached; samples only
  re-read the dynamic counters. An unavailable metric (no cpufreq, no CPU
  hwmon, or a root-only `energy_uj` counter) reports an explicit `None` and
  renders as `—`, never a fake zero. `SystemStats` gains `cpu_power_w`.
- `orsiktop diag` subcommand: a human-readable, no-secrets diagnostics dump for
  bug reports. It prints CPU topology (vendor/model/logical/physical, P/E/LP-E
  classes, hybrid state, per-core kinds), DRM GPU discovery (card, BDF,
  vendor/device IDs, PCI class, driver, render node, outputs), the one-shot
  `Auto` provider capability matrix (which normalized metrics the chosen backend
  can expose vs. report `None`), CPU sensor discovery, and llama.cpp
  endpoint/server discovery plus the computed server→GPU mapping. Missing data is
  an explicit `—`/`not computed`/`unknown` state, never a fake zero; process
  evidence is PID-only (command lines are never printed), and the only network
  contact is one bounded read-only probe of the resolved llama endpoint.

### Changed

- GPU utilization, memory-bandwidth utilization, and VRAM used/total in
  `GpuStats` are now explicit `Option`s. A vendor whose driver does not expose a
  given metric (e.g. Intel has no GPU-busy counter, no VRAM figures, no
  instantaneous power, or no fan-speed maximum in sysfs) reports `None` — an
  honest "unavailable" — instead of a fake `0.0`. The UI renders these as `—`.
- Adaptive sampling: CPU temperature and RAPL power now update on a separate
  1 s cadence and are reused on the 250 ms system cycles, instead of
  resampling (and, for power, blocking ~50 ms for the RAPL two-read window)
  every 250 ms. CPU frequency, usage, load and memory keep the fast cadence;
  GPU, process-table, and llama.cpp polling cadences are unchanged. A fast UI
  refresh no longer forces the slow sensor sources to run at that rate.
- Owned worker shutdown: the two telemetry workers (system/GPU and
  llama.cpp) are now joined after `stop` is set, with a bounded (5 s) wait,
  so the process exits with both workers verified-stopped instead of detached.
  The render loop and exit path never block on a worker longer than that
  bound.
- `orsiktop update` no longer blocks forever if the `orsiktop-update` helper
  hangs: the updater now runs under a bounded 10 minute deadline (polling
  `try_wait`), and a hung process is terminated and reported instead of
  wedging the command.
- The local llama-server spec-decoding probe (CLI args + the two
  spec-decoding env vars, loopback endpoints only) is cached per
  `LlamaMonitor` and re-scanned at most every 30 s, instead of re-walking
  `/proc` on every 250 ms LLM sample.

## [0.1.4] - 2026-09-15

### Changed

- GPU telemetry now runs behind a vendor-neutral `GpuProvider` interface
  (`src/providers/`). The NVIDIA NVML backend is the first implementation;
  application code and the UI no longer reference NVML or any vendor API
  directly, which is the seam for AMD and Intel backends.
- NVML device enumeration (PCI BDF / UUID per device) is cached when the
  provider is created instead of re-enumerated on every sample.
- Each GPU sample now carries the stable device identity (PCI BDF + vendor
  UUID) used for selection; rendering it in the dashboard is a follow-up.

## [0.1.3] - 2026-09-15

### Added

- Vendor-neutral GPU discovery from Linux DRM/sysfs: PCI BDF, vendor/device
  and class IDs, bound driver, render node and connected outputs, independent of
  enumeration order.
- Stable GPU selection by PCI BDF or vendor UUID through the new `--gpu`
  option (env `ORSIKTOP_GPU`) and the in-app settings; `--gpu-index` is kept as
  a legacy alias.

### Changed

- GPU selection no longer depends on a fragile ordinal NVML index. The config
  stores a stable selector (`gpu=`) and migrates legacy `gpu_index=` values
  automatically (a `gpu=` key always wins).
- Telemetry is split into normalized domain models behind a small filesystem
  abstraction so hardware parsing is fixture-testable without physical GPUs.

- `orsiktop uninstall` removes a standalone installation
  (`~/.local/bin/orsiktop` and `~/.local/bin/orsiktop-update`) and prints
  every file it removed. `orsiktop uninstall --purge` additionally removes
  the config directory. Cargo installations are detected and answered with
  `cargo uninstall orsiktop` instead of being touched.

- README restructured around a quick start (install, start llama.cpp with
  metrics, run orsiktop), with a dedicated uninstalling section and
  condensed installation/PATH text.
- SECURITY.md release integrity section now references the actual release
  artifact names (`orsiktop-x86_64-unknown-linux-gnu.tar.xz`) and documents
  GitHub artifact attestations with a verified `gh attestation verify`
  command, replacing the outdated "not cryptographically signed" statement.

## [0.1.2] - 2026-09-14

### Fixed

- Standalone installer no longer executes `orsiktop` / `orsiktop update` during
  installation. Backticks in the `dist` install success message were embedded in
  a double-quoted shell line and expanded as command substitutions, which made
  the installer appear to hang after "installing to ~/.local/bin".

### Security

- Pinned `rustls` 0.23.45 in Cargo.lock, fixing RUSTSEC-2026-0285 (medium:
  TLS 1.3 handshake messages accepted across encryption level boundaries).
  No manifest or API changes.

## [0.1.1] - 2026-09-14

### Added

- Standalone Linux x86_64 installation through a `dist`-generated shell installer.
- Installation into `~/.local/bin` with PATH setup when required.
- Standalone `orsiktop-update` updater and the user-facing `orsiktop update` command.
- SHA-256 release checksums and GitHub artifact attestations for release artifacts.
- RustSec dependency auditing in CI, with warnings such as soundness advisories treated as failures.
- Dependabot configuration for Cargo and GitHub Actions.
- Repository security policy.

### Changed

- Release packaging and GitHub Release publishing now use `dist` 0.33.0.
- `ratatui` updated from 0.29.0 to 0.30.2, removing the vulnerable transitive `lru` version.
- `crossterm` updated from 0.28.1 to 0.29.0.
- `sysinfo` updated from 0.33.1 to 0.39.6.
- Cargo installation remains available as a developer and fallback installation path.
- CLI package description now consistently uses “Fast terminal monitoring for local LLM Orks”.

## [0.1.0] - 2026-09-13

### Added

- Fast Rust/ratatui terminal UI for monitoring local LLM inference, NVIDIA GPU telemetry, Linux system load and processes.
- llama.cpp `/metrics` support using current Prometheus metric names.
- llama.cpp `/slots` support for live slot and context telemetry.
- Cached `/props` metadata lookup, including configured slot-count fallback.
- Local llama.cpp auto discovery from running `llama-server` / `llama serve` processes.
- Configurable local or remote llama.cpp endpoint with persistent in-app settings.
- Live prompt-processing / prefill and decode / generation throughput.
- Average prompt/generation throughput from monotonic counters with legacy gauge fallbacks.
- Prompt, prompt-cache and generated token counters.
- Active/deferred request and slot monitoring.
- Sampling-window speculative decoding / MTP state and acceptance telemetry.
- 60-second prefill and decode history graphs.
- LLM connection state, uptime, smoothed polling latency and transient reconnect grace handling.
- Direct NVIDIA NVML telemetry without per-refresh `nvidia-smi` subprocesses.
- GPU utilization, VRAM, clocks, temperature, power, P-state, fan, encoder/decoder and PCIe throughput.
- Runtime GPU selection via settings and `--gpu-index` / `ORSIKTOP_GPU_INDEX`.
- 60-second GPU and VRAM history.
- Linux CPU, per-core, RAM, swap, load-average and I/O-wait monitoring.
- Intel P-core / E-core grouping when exposed by Linux.
- 60-second CPU and RAM history.
- Grouped process view for processes with the same program name.
- Expand/collapse process groups with right-click.
- Process sorting, keyboard navigation, mouse scrolling and process pinning.
- `/` process search by program, command or PID.
- Configurable 100–10,000 ms main refresh interval.
- Separate process refresh interval and offline grace period.
- Clickable `[ - ]` and `[ + ]` refresh controls plus keyboard shortcuts.
- Persistent settings for endpoint, GPU index, refresh intervals, offline grace and auto discovery.
- Help and settings overlays with mouse/keyboard controls.
- Separate fast GPU/system and llama.cpp telemetry workers so HTTP latency cannot delay GPU sampling.
- Unit tests and llama.cpp fixtures for metrics and slot telemetry.
- GitHub Actions CI for format, build, Clippy, tests and CLI smoke testing.
- Automated Linux x86_64 release workflow with checksum generation and tag/version validation.

### Changed

- `/slots` failures are represented as unavailable instead of misleading `0/0` slot telemetry.
- Prometheus labels are preserved instead of implicitly collapsing all same-name series.
- Unsupported NVML values render as unavailable instead of false zeroes.
- PCIe throughput follows NVML's documented KB/s source unit and is displayed as MB/s.
- Fan telemetry no longer clamps valid NVML readings above 100%.
- CPU/RAM refresh uses only the required sysinfo data instead of `refresh_all()`.
- Footer/help keybindings now use `Esc` to quit and `q` to open settings.
- Terminal state restoration is guarded so raw mode, mouse capture and cursor state are restored on errors/unwind.
- README now documents the current feature set, controls, settings, installation flow and includes a repository-hosted screenshot.

### Scope

The first release intentionally targets Linux + NVIDIA + llama.cpp only.

# Stage Summary — S18 (`orsiktop diag` diagnostics command)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/diagnostics.rs` (new) | The whole S18 surface. `run()` loads the config, renders the report to stdout, and runs one bounded live llama probe. `render<S: Sys>(sys, logical, auto_discovery, server)` builds the report as a `String` — **pure with respect to the filesystem** (every read goes through `Sys`), so a `FixtureSys` yields a deterministic, network-free report. Sections: `cpu_section`, `gpu_section`, `provider_section`, `cpu_sensors_section`, `llama_section`. `describe_mapping(&GpuMapping)` renders the four mapping states explicitly; `evidence_name`/`kind_short`/`vendor_name`/`opt`/`join_or_none` are small display helpers. `DEFAULT_SERVER` (`http://127.0.0.1:8080`) duplicates the one in `main.rs` (different module, intentional). |
| `src/main.rs` | `mod diagnostics;` registered (alphabetical, before the `#[allow(dead_code)] mod discovery;`). `Commands` gains the `Diag` variant with doc `/// Print a diagnostics summary for bug reports (no secrets).`. The dispatch match gains `Commands::Diag => diagnostics::run(),`. No new imports. |
| `CHANGELOG.md` | `[Unreleased]` `### Added` gained the `diag` subcommand entry. |
| `docs/S18_STAGE_SUMMARY.md` | This summary. |

## 2. Architecture changes

- **One new subcommand, zero new dependencies, read-only.** `diag` is a leaf
  command that reuses the existing discovery, provider, sensor, and mapping
  machinery — it never opens a TTY, never mutates state, and never starts the
  workers. It is the "what does OrsikTop *see* at startup" snapshot for bug
  reports.
- **`render` is pure over the filesystem; the live probe lives in `run`.**
  `render` takes a `Sys` plus the already-loaded `auto_discovery`/`server`
  values and performs no network I/O, so the fixture tests are deterministic.
  `run` is the only place that reads the real config (`config::load()`) and
  performs the one bounded, read-only HTTP probe of the resolved llama
  endpoint (`probe_llama`), which prints after the report so a slow/unreachable
  endpoint never delays the static sections.
- **The provider capability matrix is a one-shot sample.** `provider_section`
  calls `new_gpu_provider(GpuSelector::Auto, &gpus).sample()` exactly once and
  prints which normalized metrics the chosen backend can actually expose
  (`available`) versus which are `None` (`unavailable`). This is the S18
  "selected provider + available metrics/capabilities" requirement: it shows
  *why* a metric is `—` (the backend has no source for it) rather than
  pretending it is zero.
- **The server→GPU mapping is computed, not guessed.** `llama_section` derives
  the local PID behind the selected endpoint (`discovery_llm::selected_endpoint_pid`),
  then runs the exact same S16 pipeline the TUI uses —
  `process_render_gpus` + `nvml_compute_gpus` + `map_server_gpus` — and prints
  the result via `describe_mapping`. No local process → explicit
  `not computed`; a process with no usable GPU evidence → explicit
  `unknown (insufficient evidence)`.
- **No-secrets policy is enforced by construction.** Process evidence is
  PID-only (`/proc/<pid>/cmdline` is parsed *inside* `discover_processes` to
  find the server, but only the PID and endpoint are ever printed — command
  lines are dropped). No environment variables, no credentials, no API keys.
  The only external contact is the one bounded GET to the resolved endpoint.
  Endpoint URLs are printed (they are user-chosen, not secret).

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **186 passed, 0 failed, 3 ignored**
(S18 adds 3 `FixtureSys`/pure tests in `diagnostics::tests`).

- `render_on_empty_sys_is_well_formed` — empty `FixtureSys`: all four section
  headers present, `no display-class DRM devices found`, the default endpoint is
  selected (`http://127.0.0.1:8080 (default)`), and the mapping is explicitly
  `not computed`. This is the regression guard for the "missing metric →
  explicit state, never a fake zero" rule across every section.
- `render_maps_local_server_process_to_render_gpu` — a fixture that has an
  Intel GPU (via the same DRM-sysfs layout `discover_gpus` reads), a
  `llama-server` process whose `/proc/<pid>/cmdline` matches, and an open fd on
  the by-path render node. Asserts the candidate line, the
  `auto-discovered` endpoint, and `gpu map: single 0000:01:00.0 (evidence: DRM
  render-node fd)` — i.e. the full discovery→endpoint→attribution chain,
  end to end, through `Sys` alone.
- `mapping_states_are_described_explicitly` — all four `GpuMapping` variants
  render to their exact human strings (`None`/`Single`/`Multi`/`Unknown`).
- `cpu_section_renders_fixture_topology` — cpuinfo + `cpu_core`/`cpu_atom` masks
  + per-cpu `core_cpus_list` singletons: asserts vendor `Intel`,
  `classes: P=2 E=2 LP=0`, `hybrid: yes`, and `core kinds: P,P,E,E`.

Real-machine smoke on **this** box (Intel Core Ultra 9 288V, xe APU):
`cargo run -- diag` prints the full report — CPU `P=4 E=4 LP=0`,
`P,P,P,P,E,E,E,E`; the xe APU `0000:00:02.0` discovered with driver `xe` and
four outputs; the Intel provider reports `available: clocks` and an explicit
`unavailable` list for the metrics the i915/xe sysfs does not expose
(utilization, memory, temperature, power, fan, pcie, enc/dec); CPU sensors show
a live frequency + temperature with power `— (unavailable)` (root-only
`energy_uj`); and the llama section found a **configured** endpoint
(`http://100.99.68.81:8081`) and the bounded probe **connected**, printing real
model / context / slots / rate. `cargo run -- --help` lists `diag`;
`cargo run -- diag --help` shows the subcommand.

## 4. Known limitations

- **The provider section samples the `Auto` device only.** With multiple GPUs it
  shows the capability matrix for the single backend `Auto` resolves to (first
  NVIDIA, else first device). Per-device matrices are an S23/UX concern.
- **`provider_section` and `run` re-discover GPUs against `RealSys`.**
  `render` is `Sys`-pure for the topology/sensor/llama sections, but the
  provider one-shot sample reads the *real* backend (NVML/sysfs), which cannot
  go through `Sys` (it is a library handle, not a file read). In a fixture the
  provider therefore samples whatever the host actually has; the static layout
  of that section is what the tests assert.
- **The llama probe is one-shot and bounded** (750 ms connect + 1200 ms total,
  the `llama.rs` constants). It reports the first sample's state (connected /
  unreachable / client-init-failed); rate figures are `0.0` on the first sample
  because `LlamaMonitor` computes TPS from a prior sample.
- **`logical` CPU count is passed in, not read from the host, in `render`.**
  `run` supplies `available_parallelism()`; a fixture passes its own count so
  the topology section is deterministic and machine-independent.

## 5. Newly discovered issues

- **`opt()` unwraps `Option`, so the topology classes render as `P=2 E=2 LP=0`
  (and `—` for `None`), not `Some(2)`/`Some(0)`.** The first draft asserted the
  `Some(…)` form (inherited from the domain's raw `Option<usize>` fields) and
  failed; the clean `2`/`—` rendering is the intended human form, so the
  *expectation* was corrected to the actual output (per the S26 "fix the
  expectation only when the actual output is the intended behavior" rule).
- **`gpu_section`/`provider_section` originally called `discover_gpus(&RealSys)`
  instead of the `Sys` argument**, so the empty-fixture test leaked the host's
  real GPU (the "no display-class DRM devices found" assert failed on a box that
  has one). Threading `sys` through made `render` fully `Sys`-pure and the test
  deterministic. This is the same class of bug S11 hit when `is_dir_entry`
  filtered `is_dir` on symlinked `/sys/class/*` entries.
- **The CPU topology section is machine-dependent unless `logical` is a
  parameter.** `detect_topology` sizes its `core_kinds`/`core_cpus_list` walk to
  the `logical` argument; using `available_parallelism()` inside `render` made a
  4-core fixture render on an 8-core host as `P,P,E,E,?,?,?,?`. Passing
  `logical` in (as the S10 `detect_topology` fixtures already do) removes the
  dependency.
- **The host's real config pointed at a live, reachable llama server**
  (a Tailscale IP), so the live smoke exercised the *connected* branch — the
  most valuable one — rather than the expected `unreachable`. Both branches are
  plain `println!` and the report is byte-stable regardless.

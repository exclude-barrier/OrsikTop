# Stage Summary — S21 (CI)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `.github/workflows/ci.yml` | CLI smoke step now also runs `cargo run --locked -- diag`, so the `diag` subcommand (S18) is exercised on every push/PR — including on a headless runner (no GPU, no llama server), where it must still exit 0. |
| `docs/S21_STAGE_SUMMARY.md` | This summary. |

## 2. What S21 required, and what was found

S21 mandates: keep `fmt` / `check` / `clippy` / `test` / `audit` / CLI smoke;
add fixture-based cross-vendor telemetry tests; never require physical
NVIDIA/AMD/Intel hardware in CI.

Audit result:

- **Existing checks — kept, unchanged.** `.github/workflows/ci.yml` already
  runs all six mandated steps (fmt, check, clippy `-D warnings`, test,
  `actions-rust-lang/audit` with `denyWarnings: true`, CLI smoke). No change.
- **Fixture-based cross-vendor telemetry tests — already present, no new
  tests added.** S19 built the fixture suite, and every cross-vendor item
  already runs in CI through `cargo test --all-targets --locked` on a plain
  Linux runner (no hardware, no `#[ignore]`): AMD dGPU
  (`providers::amd::samples_amd_dgpu_full_sensor_set`), Intel i915 + xe
  (`providers::intel::*`), iGPU+dGPU / multi-GPU discovery
  (`discovery::discovers_intel_xe_igpu_and_amd_dgpu_sorted_by_bdf`,
  `gpu::resolves_selector_against_discovered_gpus`), hybrid/homogeneous CPU
  (`cpu::fixture_tests::*`), DRM fdinfo variants (`drm::tests::*`),
  malformed sysfs (`providers::amd::malformed_sysfs_values_degrade_to_none_not_fakes`),
  llama endpoint fixtures (`tests/fixtures/*.{prom,json}`). Adding duplicates
  is forbidden padding; the full S19→CI coverage map is in
  `docs/S19_STAGE_SUMMARY.md` §3.
- **`#[ignore]` real-system smokes deliberately stay out of CI.**
  `discovery::discovery_real_system_smoke` **asserts at least one GPU exists**,
  so it fails on a headless `ubuntu-latest` runner; `intel_real_system_smoke`
  and `drm_real_system_smoke` are hardware-tolerant but add no CI value
  beyond what the fixture suite covers. They remain developer opt-in
  (`cargo test --all -- --ignored`).

The one real gap: the CLI smoke covered only `--help`. The `diag` subcommand
(S18) — which exercises the vendor-neutral discovery path, provider
dispatch, CPU-sensor discovery, and one bounded network probe against the
*real* filesystem — was never run in CI. A regression that panics or hangs
`orsiktop diag` on a headless machine (no DRM devices, no llama server)
would have shipped silently. Fixed by extending the smoke step (§3).

## 3. The change

```yaml
- name: CLI smoke test
  run: |
    cargo run --locked -- --help
    # `diag` reads the real /sys, /proc and runs one bounded llama probe;
    # on a headless runner (no GPU, no server) it must still exit 0.
    cargo run --locked -- diag
```

Why `diag` is safe on a bare runner (verified, not assumed):

- No DRM devices → `discover_gpus` returns an empty list → `gpu_section`
  prints `no display-class DRM devices found`, and `new_gpu_provider` returns
  `UnavailableGpuProvider` (`available: false`, `no sample: no devices to
  probe`) — no NVML/AMD/Intel backend is ever constructed.
- No llama server → `probe_llama` uses the bounded client (750 ms connect /
  1200 ms total) and prints `status: unreachable (…)` — a bounded wait, no
  hang.
- CPU/sensor sections read only world-readable `/sys`/`/proc`; missing
  sources render `—`.
- Exit code is 0 in all of the above; failures of the *probe* are printed,
  not errors.

Verified live: `XDG_CONFIG_HOME=/tmp/empty cargo run -- diag` → full report,
`status: unreachable (cannot reach llama.cpp: …)`, `exit=0`.

## 4. Known limitations

- CI runs `cargo check --all-targets --locked` and
  `cargo clippy --all-targets --locked` **without** `--all-features`; the
  crate has no `[features]`, so today this is a no-op difference. The local
  S26 gate uses `--all-features` as belt-and-braces. If features are ever
  added, CI should be updated to match.
- `cargo audit` runs with `denyWarnings: true` and no lockfile re-resolve:
  it audits `Cargo.lock` as committed, which is the correct behavior (the
  release build is `--locked`).
- The three `#[ignore]` real-system smokes are not run in CI by design
  (§2). Their coverage intent (real-hardware sanity) is out of CI scope;
  they remain the developer's opt-in check on real hardware.

## 5. Newly discovered issues

- None. The workflow was already in the mandated shape; the only drift was
  the CLI smoke not covering the S18 `diag` subcommand, which §3 closes.

## 6. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **191 passed, 0 failed, 3 ignored**
(unchanged from S20; S21 adds no Rust tests — the change is a workflow
extension). `cargo test --all -- --ignored` → **3 passed** (unchanged).
`.github/workflows/ci.yml` parses as valid YAML.

Live smoke: `cargo run -- --help` (lists `diag`) and
`XDG_CONFIG_HOME=/tmp/empty cargo run -- diag` (exit 0, unreachable-server
path) both pass locally, mirroring the CI runner conditions.

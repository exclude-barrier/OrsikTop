# Stage Summary — S5 (NVIDIA backend on the common provider interface)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/providers/mod.rs` (new) | `GpuProvider` trait (the common GPU telemetry interface) + normalized `GpuStats` model (moved from `gpu.rs`, now carries `DeviceId`). |
| `src/providers/nvidia.rs` (new) | `NvidiaGpuProvider`: the NVIDIA NVML backend. Holds `Option<Nvml>` (verified `Send`+`Sync`), a cached device enumeration `(index, uuid, bdf)`, and all NVML-specific helpers (`bytes_to_mib`, `kb_per_second_to_mb`, `normalize_pstate`, `format_throttle_reasons`, `sanitize`, `clamp_percent`, `nonnegative`) + their unit tests. |
| `src/gpu.rs` | Slimmed to a vendor-neutral facade: `new_gpu_provider(selector) -> Box<dyn GpuProvider>` (the single backend-selection point) + the shared, vendor-agnostic PCIe link math (`pcie_mbps_per_lane`, `pcie_utilization_pct`) + those tests. NVML code removed. |
| `src/domain.rs` | Re-export `GpuStats` from `crate::providers` (was `crate::gpu`). `DeviceId` now derives `Default` (both fields `Option`) and its staged `#[allow(dead_code)]` is dropped — it is now consumed by the provider. |
| `src/main.rs` | Declares `mod providers;`. |
| `src/app.rs` | Fast worker uses `gpu::new_gpu_provider(selector)` instead of `GpuMonitor`; the selector-change path rebuilds the provider. |

## 2. Architecture changes

- **Provider layer introduced.** The application and UI now consume only
  `GpuProvider` + `GpuStats`; nothing outside `src/providers/nvidia.rs`
  references `Nvml`, `nvml_wrapper`, or a `GpuMonitor` type. This is the seam
  S6 (AMD) and S7 (Intel) plug into without touching the worker or UI.
- **`GpuStats` is now the vendor-neutral normalized model** (lives in the
  provider contract). Unavailable metrics remain `None` (explicit, not zero);
  utilization/memory keep their 0.0 defaults (an idle GPU is genuinely 0%).
- **NVIDIA behind the interface, behavior preserved.** `NvidiaGpuProvider`
  keeps every existing NVML query (utilization, memory, temp, power, limit,
  pstate, throttle, clocks, enc/dec, fan, PCIe throughput+link) and all three
  error paths (NVML init failure, no-match selection, per-device access error).
- **Enumeration cached at construction.** The `(index, uuid, bdf)` list is
  built once when the provider is created (i.e. once per selector change),
  not re-enumerated on every `sample()` — the S4 "re-enumerate per sample"
  limitation is removed. Sampling still calls `device_by_index(index)` per
  cycle (cheap, avoids the `Device<'static>` lifetime, preserves error paths).
- **Stable identity consumed.** The provider populates `GpuStats.device`
  (`DeviceId`) from the resolved BDF/UUID. The field is set but not yet
  *rendered* — rendering stable identity is S23's UX scope, so it carries a
  staged `#[allow(dead_code)]` until then.
- **Backend selection is a single factory.** `gpu::new_gpu_provider` is the
  one place that picks the concrete provider. Today it is unconditionally
  NVIDIA; S6/S7 will make it consult vendor-neutral discovery.

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets
--all-features -- -D warnings`, `cargo test --all` → all green: **113 passed,
0 failed, 1 ignored** (count unchanged from S4 — the move is a relocation, not
a behavior change; the relocated NVML helper tests now live under
`providers::nvidia::tests`). Real-system discovery smoke
(`--ignored discovery_real`) still passes on the Intel xe machine. Binary
smoke: `--version` → `orsiktop 0.1.3`, `--help` shows the `--gpu`/`--gpu-index`
options.

## 4. Known limitations

- **Per-field `Metric<T>` is not yet in the sampling path.** `GpuStats` still
  uses `Option<f64>` + `available: bool`. Converting the UI to render the
  richer `Metric<T>` states (Unsupported / PermissionDenied / Stale / Error)
  is deliberately deferred to S23 (heterogeneous-hardware UX), per the stage
  rule to preserve current NVIDIA display behavior. `Metric<T>` remains in
  `domain.rs` with its staged `#[allow(dead_code)]`.
- **Single backend.** `new_gpu_provider` always returns NVIDIA. There is no
  device discovery fallback yet; a machine with only AMD/Intel GPUs will report
  the NVML init error (graceful, not a crash) until S6/S7 land.
- **`GpuStats.device` is populated but not displayed** (S23).
- **`discovery.rs` is still not wired into the sampling path** (S3's
  `#[allow(dead_code)]` on `mod discovery` remains). The provider uses NVML's
  own enumeration for identity; DRM-based discovery feeds the factory decision
  in S6/S7.

## 5. Newly discovered issues

- **`Nvml` is `Send` + `Sync`** (nvml-wrapper 0.13, `assert_impl_all!` at
  `lib.rs:219`), which is what makes a `GpuProvider` that owns `Option<Nvml>`
  `Send` and therefore safe to own on the worker thread. This was verified
  before committing to the trait bound `GpuProvider: Send`.
- **`DeviceId` had to derive `Default`** because `GpuStats: Default` (used by
  `..Default::default()` in the error paths) now embeds a `DeviceId`. Both
  fields are `Option`, so `None`/`None` (unknown identity) is a sound default.
- No regressions: the relocation preserved all 113 tests and the real-machine
  smoke, confirming the refactor is behavior-preserving.

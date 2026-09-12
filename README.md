# OrsikTop

**btop for local LLM Orks**

[![CI](https://github.com/exclude-barrier/OrsikTop/actions/workflows/ci.yml/badge.svg)](https://github.com/exclude-barrier/OrsikTop/actions/workflows/ci.yml)

OrsikTop is a fast Rust terminal UI for monitoring local LLM inference. It combines current llama.cpp inference data, NVIDIA GPU telemetry and Linux system telemetry in one dense btop-style view.

> Orsik = fantasy ork. Local models need tokens. Orks need more power.

## Status

Early alpha. The first target stays intentionally narrow:

- Linux
- NVIDIA GPUs
- llama.cpp / `llama-server` / `llama serve`
- one local server
- low-overhead terminal monitoring

This is deliberate: YAGNI first, adapters later.

## What it monitors

### llama.cpp

- model and configured context
- live prompt-processing and generation throughput from counter deltas
- average prompt-processing and generation throughput from monotonic token/time counters
- processed, prompt-cache and generated token counters
- active and deferred requests
- active / total server slots
- live context usage from `/slots`
- context high-watermark fallback when `/slots` is unavailable
- speculative decoding / MTP acceptance over the current sampling window

OrsikTop uses current llama.cpp Prometheus metric names. `/props` is cached instead of being requested every refresh. `/slots` is treated as optional telemetry: when it is unavailable, OrsikTop does not report a misleading `0/0`; it falls back to the configured slot count from `/props` when available.

The average throughput display prefers `prompt_tokens_total / prompt_seconds_total` and `tokens_predicted_total / tokens_predicted_seconds_total`. The older throughput gauges are retained only as compatibility fallbacks because some current llama.cpp builds can report zero from those gauges while inference is active.

### NVIDIA GPU

GPU telemetry comes directly from NVML. OrsikTop does **not** spawn `nvidia-smi` on every refresh.

- GPU utilization and 60-second pixel history
- VRAM used / total and memory utilization
- graphics and memory clocks
- temperature
- power draw and enforced power limit
- P-state
- fan speed
- encoder / decoder utilization
- PCIe RX / TX throughput in MB/s

Unsupported NVML fields are shown as unavailable (`—`) instead of being reported as false zeroes.

### System

- CPU utilization, frequency, package temperature, load and I/O wait
- automatic Intel P-core / E-core topology grouping when Linux exposes it
- RAM and swap usage
- process list with sorting, scrolling and pinned processes
- 60-second GPU/VRAM/CPU/RAM history

## Install

From a local checkout:

```bash
git clone https://github.com/exclude-barrier/OrsikTop.git
cd OrsikTop
cargo install --path . --locked --force
```

Or directly from GitHub:

```bash
cargo install --git https://github.com/exclude-barrier/OrsikTop --locked
```

Cargo installs the binary as `orsiktop` (normally into `~/.cargo/bin`). With that directory in your `PATH`, start it exactly like btop:

```bash
orsiktop
```

## Quick start

Start llama.cpp with metrics enabled:

```bash
llama-server -m /path/to/model.gguf --metrics
```

If you use the newer CLI form:

```bash
llama serve -hf user/model:Q4_K_M --metrics
```

Then simply run:

```bash
orsiktop
```

When `--server` / `ORSIKTOP_SERVER` is not set, OrsikTop scans local `/proc` entries for a running `llama-server` or `llama serve` process and derives its `--host` and `--port` automatically. A wildcard bind such as `0.0.0.0` is reached through loopback. If no local llama.cpp process is found, OrsikTop still starts and falls back to the standard `http://127.0.0.1:8080` endpoint, so GPU/system/process telemetry remains available while the LLM panel reports the server as offline.

Manual server override remains available:

```bash
orsiktop --server http://127.0.0.1:8081
```

For a 500 ms telemetry interval:

```bash
orsiktop --interval-ms 500
```

To monitor another NVIDIA GPU:

```bash
orsiktop --gpu-index 1
```

## Controls

| Action | Control |
| --- | --- |
| Quit | `q` or `Esc` |
| Faster refresh | click `[ - ]`, `-` or `[` |
| Slower refresh | click `[ + ]`, `+` or `]` |
| Process navigation | `↑` / `↓`, `j` / `k`, `PgUp` / `PgDn`, `Home` / `End`, mouse wheel |
| Pin process | click process row |
| Unpin process | click away from process row |
| Sort processes | click `PID`, `PROGRAM`, `CPU`, `MEM` or `THR` header |

Refresh can be changed live from **100 ms to 10,000 ms** in 100 ms steps. GPU telemetry follows that interval; CPU/RAM are refreshed on a lower-overhead cadence and llama.cpp HTTP polling runs independently so a slow `/metrics` or `/slots` response does not stall GPU updates.

## Configuration

| Option | Environment variable | Default |
| --- | --- | --- |
| `--server` | `ORSIKTOP_SERVER` | auto-discover local llama.cpp; fallback `http://127.0.0.1:8080` |
| `--interval-ms`, `-i` | `ORSIKTOP_INTERVAL_MS` | `1000` |
| `--gpu-index` | `ORSIKTOP_GPU_INDEX` | `0` |

Explicit `--server` and `ORSIKTOP_SERVER` values take precedence over auto-discovery.

## Build

```bash
git clone https://github.com/exclude-barrier/OrsikTop.git
cd OrsikTop
cargo build --release --locked
./target/release/orsiktop
```

OrsikTop loads NVIDIA NVML dynamically through `nvml-wrapper`. A normal NVIDIA Linux driver installation provides NVML; the application itself does not need to link against a bundled NVIDIA library.

## Architecture

The code deliberately stays small:

```text
src/
├── main.rs   terminal setup + CLI + local llama.cpp discovery
├── app.rs    event loop + fast/LLM telemetry workers
├── llama.rs  llama.cpp /metrics, /slots and /props
├── gpu.rs    NVIDIA NVML telemetry
└── ui.rs     ratatui rendering + mouse hitboxes + 60 s history
```

GPU/system telemetry and llama.cpp HTTP polling run in separate background workers. A slow HTTP response therefore cannot freeze mouse/keyboard input or delay fast GPU sampling.

## Design goals

OrsikTop should stay fast, readable and boring to operate: one binary, no daemon, no database, no browser and no account.

## License

Apache License 2.0

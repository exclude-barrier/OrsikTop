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

- CPU utilization
- RAM used / total
- 60-second CPU/RAM history

## Quick start

Start llama.cpp with metrics enabled:

```bash
llama-server -m /path/to/model.gguf --metrics
```

If you use the newer CLI form:

```bash
llama serve -hf user/model:Q4_K_M --metrics
```

Then run OrsikTop:

```bash
cargo run --release -- --server http://127.0.0.1:8080
```

For a 500 ms telemetry interval:

```bash
cargo run --release -- --server http://127.0.0.1:8080 --interval-ms 500
```

To monitor another NVIDIA GPU:

```bash
cargo run --release -- --gpu-index 1
```

## Controls

| Action | Control |
| --- | --- |
| Quit | `q` or `Esc` |
| Faster refresh | click `[ - ]`, `-` or `[` |
| Slower refresh | click `[ + ]`, `+` or `]` |

Refresh can be changed live from **100 ms to 10,000 ms** in 100 ms steps. GPU telemetry follows that interval; CPU/RAM are refreshed on a lower-overhead cadence and llama.cpp HTTP polling runs independently so a slow `/metrics` or `/slots` response does not stall GPU updates.

## Configuration

| Option | Environment variable | Default |
| --- | --- | --- |
| `--server` | `ORSIKTOP_SERVER` | `http://127.0.0.1:8080` |
| `--interval-ms`, `-i` | `ORSIKTOP_INTERVAL_MS` | `1000` |
| `--gpu-index` | `ORSIKTOP_GPU_INDEX` | `0` |

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
├── main.rs   terminal setup + CLI
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

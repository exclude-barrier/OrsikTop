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
- live prompt throughput
- live generation throughput
- llama.cpp average prompt / generation throughput
- prompt and generated token counters
- active and deferred requests
- active / total server slots
- live context usage from `/slots`
- context high-watermark fallback
- speculative decoding / MTP acceptance

OrsikTop uses the current llama.cpp Prometheus metric names and queries `/slots` for live per-slot context information. `/props` is cached instead of being requested every refresh.

### NVIDIA GPU

GPU telemetry comes directly from NVML. OrsikTop does **not** spawn `nvidia-smi` on every refresh.

- GPU utilization and pixel history
- VRAM used / total and memory utilization
- graphics and memory clocks
- temperature
- power draw and enforced power limit
- P-state
- fan speed
- encoder / decoder utilization
- PCIe RX / TX throughput

### System

- CPU utilization
- RAM used / total

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

## Controls

| Action | Control |
| --- | --- |
| Quit | `q` or `Esc` |
| Faster refresh | click `[ - ]`, `-` or `[` |
| Slower refresh | click `[ + ]`, `+` or `]` |

Refresh can be changed live from **100 ms to 10,000 ms** in 100 ms steps.

## Configuration

| Option | Environment variable | Default |
| --- | --- | --- |
| `--server` | `ORSIKTOP_SERVER` | `http://127.0.0.1:8080` |
| `--interval-ms`, `-i` | `ORSIKTOP_INTERVAL_MS` | `1000` |

## Build

```bash
git clone https://github.com/exclude-barrier/OrsikTop.git
cd OrsikTop
cargo build --release
./target/release/orsiktop
```

OrsikTop loads NVIDIA NVML dynamically through `nvml-wrapper`. A normal NVIDIA Linux driver installation provides NVML; the application itself does not need to link against a bundled NVIDIA library.

## Architecture

The code deliberately stays small:

```text
src/
├── main.rs   terminal setup + CLI
├── app.rs    event loop + background telemetry worker
├── llama.rs  llama.cpp /metrics, /slots and /props
├── gpu.rs    NVIDIA NVML telemetry
└── ui.rs     ratatui rendering + mouse hitboxes
```

Network and GPU polling run outside the UI event loop so a slow HTTP response does not freeze mouse or keyboard input.

## Design goals

OrsikTop should stay fast, readable and boring to operate: one binary, no daemon, no database, no browser and no account.

## License

MIT

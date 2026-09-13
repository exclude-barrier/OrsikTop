# OrsikTop

**btop for local LLM Orks**

[![CI](https://github.com/exclude-barrier/OrsikTop/actions/workflows/ci.yml/badge.svg)](https://github.com/exclude-barrier/OrsikTop/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/exclude-barrier/OrsikTop)](./LICENSE)
[![Rust](https://img.shields.io/badge/Rust-stable-orange?logo=rust)](https://www.rust-lang.org/)
[![Linux](https://img.shields.io/badge/Linux-supported-FCC624?logo=linux&logoColor=black)](https://www.kernel.org/)
[![llama.cpp](https://img.shields.io/badge/llama.cpp-supported-5C6BC0)](https://github.com/ggml-org/llama.cpp)

OrsikTop is a fast Rust TUI for monitoring **local LLM inference, NVIDIA GPU telemetry, Linux system load and processes** in one dense btop-style view.

> Orsik = fantasy ork. Local models need tokens. Orks need more power.
<img width="926" height="785" alt="image" src="https://github.com/user-attachments/assets/4fd5278f-adf4-44c6-9325-6ee84e971ba2" />

## Current scope

OrsikTop intentionally targets a narrow stack first:

- Linux
- NVIDIA GPUs via NVML
- llama.cpp (`llama-server` or `llama serve`)
- local or manually configured llama.cpp endpoint
- low-overhead terminal monitoring

No daemon, browser, database or account is required.

## Highlights

### LLM inference

OrsikTop reads llama.cpp telemetry directly and shows:

- model and configured context size
- current context usage
- live prompt-processing / prefill throughput
- live decode / generation throughput
- server-wide average throughput
- prompt, cache and generated token counters
- active and queued/deferred requests
- active / total slots
- speculative decoding / MTP state and acceptance data when available
- cumulative prompt and generation time
- 60-second prefill and decode history
- connection state, uptime and smoothed polling latency
- transient reconnect handling so a short polling hiccup does not immediately flash the server as offline

`/metrics` must be enabled in llama.cpp. `/props` is cached and `/slots` is treated as optional telemetry; when `/slots` is unavailable, OrsikTop falls back gracefully instead of displaying misleading values.

### NVIDIA GPU

GPU telemetry comes directly from NVML. OrsikTop does **not** spawn `nvidia-smi` on every refresh.

- GPU utilization
- VRAM used / total and memory-controller utilization
- graphics and memory clocks
- temperature
- power draw and enforced power limit
- P-state
- fan speed
- encoder / decoder utilization
- PCIe RX / TX throughput
- 60-second GPU and VRAM history

Unsupported NVML fields are shown as unavailable (`—`) instead of false zeroes.

### Linux system

- CPU utilization and frequency
- package temperature
- load averages and I/O wait
- Intel P-core / E-core grouping when exposed by Linux
- per-core activity
- RAM and swap usage
- 60-second CPU and RAM history

### Processes

- process list with CPU, memory and thread counts
- sorting by PID, program, CPU, memory or threads
- processes with the same program name grouped together
- right-click groups to expand / collapse them
- keyboard navigation with selection auto-scroll
- double-click to pin a process
- mouse-wheel scrolling
- `/` process search by program, command or PID

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

Cargo normally installs the binary into `~/.cargo/bin`:

```bash
orsiktop
```

## Quick start

Start llama.cpp with metrics enabled:

```bash
llama-server -m /path/to/model.gguf --metrics
```

or with the newer CLI form:

```bash
llama serve -hf user/model:Q4_K_M --metrics
```

Then run:

```bash
orsiktop
```

### Auto discovery

With **Auto discovery = ON**, OrsikTop scans local `/proc` entries for a running `llama-server` or `llama serve` process and derives its `--host` and `--port` automatically.

A wildcard bind such as `0.0.0.0` or `::` is reached through loopback. If no local llama.cpp process is found, OrsikTop falls back to the saved endpoint and then to:

```text
http://127.0.0.1:8080
```

This is only local process discovery; OrsikTop does **not** scan the LAN for llama.cpp servers.

For a remote or fixed endpoint, disable Auto discovery in Settings and enter the desired host and port.

## Controls

| Action | Control |
| --- | --- |
| Quit | `Esc` |
| Help | `h` |
| Settings | `q` |
| Process search | `/` |
| Faster refresh | click `[ - ]`, `-` or `[` |
| Slower refresh | click `[ + ]`, `+` or `]` |
| Process navigation | `↑` / `↓`, `j` / `k`, `PgUp` / `PgDn`, `Home` / `End` |
| Scroll processes | mouse wheel |
| Pin process | double-click process row |
| Expand / collapse process group | right-click group |
| Sort processes | click `PID`, `PROGRAM`, `CPU`, `MEM` or `THR` |

The main telemetry refresh can be changed live from **100 ms to 10,000 ms**.

## Settings

Press `q` to open the in-app settings menu:

```text
LLM Host/IP      127.0.0.1
LLM Port         8081
GPU              0
Refresh          100 ms
Process refresh  1000 ms
Offline grace    2500 ms
Auto discovery   ON
```

Settings can be changed without restarting OrsikTop.

- `Tab`, `↑`, `↓` — move between fields
- type / `Backspace` — edit values
- `Space`, `←`, `→` — toggle Auto discovery
- `Enter` — validate, save and apply
- `Esc` — discard changes and close

Configuration is stored in:

```text
$XDG_CONFIG_HOME/orsiktop/config
```

or, when `XDG_CONFIG_HOME` is not set:

```text
~/.config/orsiktop/config
```

Saved settings include the LLM endpoint, GPU index, main refresh interval, process refresh interval, offline grace period and auto-discovery state.

## CLI overrides

CLI arguments and environment variables remain available for one-off overrides:

| Option | Environment variable | Purpose |
| --- | --- | --- |
| `--server` | `ORSIKTOP_SERVER` | llama.cpp endpoint; disables auto discovery for that run |
| `--interval-ms`, `-i` | `ORSIKTOP_INTERVAL_MS` | main telemetry refresh interval |
| `--gpu-index` | `ORSIKTOP_GPU_INDEX` | NVIDIA GPU index |

Examples:

```bash
orsiktop --server http://192.168.1.20:8081
orsiktop --interval-ms 500
orsiktop --gpu-index 1
```

## Build

```bash
git clone https://github.com/exclude-barrier/OrsikTop.git
cd OrsikTop
cargo build --release --locked
./target/release/orsiktop
```

OrsikTop loads NVIDIA NVML dynamically through `nvml-wrapper`. A normal NVIDIA Linux driver installation provides NVML; OrsikTop does not bundle NVIDIA libraries.

## Architecture

```text
src/
├── main.rs    terminal setup, CLI and llama.cpp discovery
├── config.rs  persistent runtime settings
├── app.rs     event loop and telemetry workers
├── llama.rs   llama.cpp /metrics, /slots and /props
├── gpu.rs     NVIDIA NVML telemetry
├── cpu.rs     CPU topology / Linux CPU helpers
└── ui.rs      ratatui rendering, controls and histories
```

GPU/system sampling and llama.cpp HTTP polling run independently. A slow `/metrics` or `/slots` response therefore does not block keyboard/mouse input or fast GPU updates.

## Design goals

OrsikTop should stay:

- fast
- readable
- useful for local LLM inference
- easy to run
- deliberately small

Features are added when they improve monitoring rather than simply making the TUI busier.

## Status

OrsikTop is still early software. The current implementation is focused on llama.cpp + NVIDIA + Linux before adding broader backend or platform support.

## License

Apache License 2.0

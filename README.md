# OrsikTop

**btop for local LLM Orks**

OrsikTop is a fast terminal UI for monitoring local LLM inference. It combines llama.cpp inference metrics with NVIDIA GPU and Linux system telemetry in one btop-style view.

> Orsik = fantasy ork. Local models need tokens. Orks need more power.

## Status

Early alpha. The first target is intentionally narrow:

- Linux
- NVIDIA GPUs
- llama.cpp / `llama-server`
- one local server
- low-overhead terminal monitoring

This is deliberate: YAGNI first, adapters later.

## Current metrics

- model name and configured context
- prompt throughput
- generation throughput
- prompt / generated token counters
- KV-cache usage and KV tokens
- active / deferred requests
- GPU utilization
- VRAM usage
- GPU temperature and power draw
- CPU and RAM usage

OrsikTop calculates live token throughput from llama.cpp counters instead of relying only on transient throughput gauges.

## Quick start

Start llama.cpp with metrics enabled:

```bash
./llama-server -m /path/to/model.gguf --metrics
```

Then run OrsikTop:

```bash
cargo run --release -- --server http://127.0.0.1:8080
```

Press `q` or `Esc` to quit.

The default server is `http://127.0.0.1:8080`. You can also set `ORSIKTOP_SERVER`.

## Build

```bash
git clone https://github.com/exclude-barrier/OrsikTop.git
cd OrsikTop
cargo build --release
./target/release/orsiktop
```

## Design goals

OrsikTop should stay fast, readable and boring to operate: one binary, no daemon, no database, no browser and no account.

## License

MIT

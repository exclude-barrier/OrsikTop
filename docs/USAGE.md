# Usage guide

[Back to README](../README.md)

OrsikTop observes inference servers and the local Linux machine. It never sends inference requests or changes server configuration. CPU, RAM, GPU and process panels always describe the host running OrsikTop, even when the selected LLM endpoint is remote.

## Controls

| Action | Control |
| --- | --- |
| Quit the dashboard | `Esc` |
| Open / close help | `h` |
| Open settings | `q` |
| Open server selector | `s` |
| Process search by program, command or PID | `/` |
| Faster refresh | click `[ - ]`, `-` or `[` |
| Slower refresh | click `[ + ]`, `+` or `]` |
| Process navigation | `↑` / `↓`, `j` / `k`, `PgUp` / `PgDn`, `Home` / `End` |
| Scroll processes | mouse wheel |
| Pin a process | double-click its row |
| Expand / collapse a same-program group | right-click its row |
| Sort processes | click `PID`, `PROGRAM`, `CPU`, `MEM` or `THR` |

`Esc` closes an open dialog before it quits the dashboard. The help popup includes a metric legend. The full dashboard needs 72×22 terminal cells; between that size and the 54×16 minimum, a reduced layout shows the essentials. Below the minimum, resize the terminal to continue.

The main refresh interval is adjustable from 100 to 10,000 ms. LLM polling has a 250 ms floor; a 100 ms UI setting does not mean 100 ms HTTP polling. Process refresh and offline grace have separate settings.

## Server discovery

With **Auto discovery = ON**, OrsikTop reads local `/proc` command lines for `llama-server` and `llama serve`, deriving endpoints from their host and port arguments. Wildcard bindings such as `0.0.0.0` or `::` are reached through loopback. It does not scan the LAN and does not discover Strata processes.

All discovered local llama.cpp endpoints and configured endpoints join the server set. The monitoring cap prioritizes the primary configured endpoint, then additional configured endpoints, then discovered servers. The preferred startup selection is a discovered server, ordered by port and endpoint rather than PID, otherwise the saved primary endpoint or `http://127.0.0.1:8080`. If that preferred target is not in the monitored set, the first monitored endpoint is selected. The default loopback endpoint is added only when the server set would otherwise be empty.

For a fixed or remote endpoint, disable Auto discovery in Settings and enter its host and port, or use a one-run override:

```bash
orsiktop --server http://HOST:PORT
```

`--server` disables auto discovery for that run. Additional configured `servers=` entries are still monitored; it is not a single-server-only mode. Configure Strata through the primary endpoint or the additional server list. Backend recognition happens from the `/metrics` response body.

## Multiple servers

Press `s` to open the selector. Each row shows that server's connection state, activity phase, age of the last successful sample, backend type, slot counts, context and throughput. `↑` / `↓` scroll, `Enter` selects and `Esc` closes. The header shows `[n/N]` when multiple servers are monitored.

Each server keeps its own counters, slot identities, connection state and in-memory throughput history. Unselected servers continue accumulating history; selecting another server does not clear it. A verified restart of the same endpoint starts a new history boundary. Removing a server discards its runtime history.

An unreachable server does not hide healthy servers. Its state is reported independently; recovered counter streams restart from a clean baseline rather than producing a false throughput spike.

At most **16 servers** are polled. Extra configured endpoints stay in the saved configuration and appear under **NOT MONITORED — limit reached**. The header shows `· N over limit`. These endpoints are not probed, so their availability is not claimed.

Endpoint identity is normalized: a trailing slash, host case or default port does not create another server. `localhost` and `127.0.0.1` remain distinct. Credentials do not create a separate endpoint identity.

For a local server, process identity plus NVML compute-process or DRM render-node evidence can establish its GPU association. The association follows the selected server. A remote or unverifiable server shows `GPU —`; this means unknown, not CPU-only or 0% utilization. Local device utilization is not presented as a remote server's GPU usage.

## Settings

Press `q` to open Settings. Host/IP and Port edit the primary endpoint; **SERVER LIST** manages additional endpoints.

- `Tab`, `↑`, `↓`: move between fields.
- Type / `Backspace`: edit values.
- `Space`, `←`, `→`: toggle Auto discovery.
- `Enter`: validate, save and apply the dialog.
- `Esc`: discard the dialog's changes.

### Additional server list

Focus **SERVER LIST** with `Tab`, or `↑` from the first field.

| Action | Control |
| --- | --- |
| Highlight an endpoint | `↑` / `↓` |
| Add an endpoint | `a` or `Insert` |
| Edit highlighted endpoint | `e` |
| Remove highlighted endpoint | `d` or `Delete` |
| Keep the entry being typed | `Enter` |
| Discard only the entry being typed | `Esc` |
| Save the whole dialog outside entry editing | `Enter` |
| Discard the whole dialog outside entry editing | `Esc` |

At either end of the list, arrow navigation moves to the neighbouring field. Entries must be absolute URLs with a host, for example `http://192.168.1.20:8080`. Duplicates of the primary or another additional endpoint are rejected. Saving applies additions and removals without restarting OrsikTop.

Endpoint list rows and validation messages show scheme, host and port. Paths and queries are hidden with `…`; userinfo and query values are masked while typing. Stored endpoints retain their full values. These display protections are not encryption: saved configuration can contain credentials, so keep it private.

The primary Host/IP and Port fields preserve the endpoint's scheme, path, credentials and query. Editing unrelated settings does not turn an HTTPS endpoint into HTTP or drop its path.

### Configuration file

Settings are stored at `$XDG_CONFIG_HOME/orsiktop/config`, or `~/.config/orsiktop/config` when `XDG_CONFIG_HOME` is unset. The format is line-oriented `key=value`:

```text
server=http://127.0.0.1:8080
servers=http://127.0.0.1:8081,http://192.168.1.20:8080
gpu=auto
refresh_ms=1000
process_refresh_ms=1000
offline_grace_ms=2500
auto_discovery=true
```

`server` is the primary configured endpoint; `servers` is a comma-separated list of additional endpoints. Remove the example additional endpoints if you do not intend to monitor them. GPU selectors use the same syntax as `--gpu` below.

| Setting | Default | Bounds / behavior |
| --- | --- | --- |
| `server` | unset | Primary configured endpoint; discovery and empty-set fallback rules are described above |
| `servers` | empty | Additional endpoints; monitored together with primary and discovered servers |
| `gpu` | `auto` | Prefer the first NVIDIA GPU in PCI BDF order, otherwise the first discovered GPU |
| `refresh_ms` | 1000 ms | 100–10,000 ms |
| `process_refresh_ms` | 1000 ms | 100–60,000 ms |
| `offline_grace_ms` | 2500 ms | 0–60,000 ms; 0 disables holding the last good LLM sample |
| `auto_discovery` | `true` | Local llama.cpp discovery |

Unknown keys are ignored. Repeated scalar keys use the last parsed value; repeated `servers=` lines accumulate endpoints, which are then de-duplicated. Legacy `gpu_index=` is accepted when `gpu=` is absent. A legacy `server=` configuration without an explicit `auto_discovery` key selects manual mode. Bounds are applied when settings are loaded or changed.

## CLI reference

```bash
orsiktop --help
orsiktop --version
```

Options below override saved settings for the current run. CLI values take precedence over their matching environment variable; saving the Settings dialog persists its current values.

| Option | Environment variable | Purpose |
| --- | --- | --- |
| `--server URL` | `ORSIKTOP_SERVER` | Primary llama.cpp or Strata endpoint; disables local auto discovery for that run |
| `-i, --interval-ms MS` | `ORSIKTOP_INTERVAL_MS` | Main refresh interval in milliseconds |
| `--gpu SELECTOR` | `ORSIKTOP_GPU` | GPU by PCI BDF, NVIDIA UUID, NVIDIA MIG UUID or numeric index |
| `--gpu-index N` | `ORSIKTOP_GPU_INDEX` | Legacy index; `--gpu` / `ORSIKTOP_GPU` takes precedence |

A numeric GPU selector is the n-th NVIDIA device on a machine with NVIDIA GPUs, or the n-th device overall on NVIDIA-less machines. Prefer PCI BDF identity for mixed-vendor setups. An unavailable explicit GPU selection does not silently fall back to another device.

```bash
orsiktop --server http://192.168.1.20:8081
orsiktop --interval-ms 500
orsiktop --gpu 0000:41:00.0
```

| Command | Effect |
| --- | --- |
| `orsiktop update` | Update a standalone installation through its companion updater |
| `orsiktop diag` | Print a read-only diagnostics summary for bug reports |
| `orsiktop diag --servers` | Probe each monitored endpoint once and list over-limit entries without probing them |
| `orsiktop uninstall` | Remove standalone binaries, keeping configuration |
| `orsiktop uninstall --purge` | Also delete the configuration directory |

Put global overrides before the subcommand, for example `orsiktop --server http://HOST:PORT diag`. Diagnostics honor the same endpoint and GPU overrides as the TUI. For install lifecycle details, see [Installation](INSTALLATION.md).

## Reading telemetry

LLM fields and sensor gauges distinguish unavailable (`—`) from a reported zero. Backend format, schema, permissions and drivers determine which measurements are available. **Current history-view limitation:** GPU/VRAM history headers can still show `0%` above an empty graph when those measurements are unavailable. Use the GPU panel to check availability; an empty history is not evidence of idle hardware.

### LLM panel

| Field | Meaning |
| --- | --- |
| `LIVE` PP / TG | Instantaneous or short-window prefill / decode throughput, when available |
| `AVG (LIFE)` | Server lifetime averages, separate from current request speed |
| `REQUEST` | In-flight request prompt / generated-token progress; not current context occupancy |
| `TOTAL` | Cumulative prompt and generated-token counters |
| `CTX` | Reported context used / capacity from one selected slot; not exact physical KV-cache usage |
| `CACHE` | Cumulative cached prompt-token count; no cache-hit percentage is inferred |
| `SLOTS` | Busy / total slots |
| `REQ` | Active / deferred requests, when exposed |
| `STATE` | Evidence-backed activity: `IDLE`, `PREFILL`, `GENERATING`, `PROCESSING` or `QUEUED` |
| `SAMPLE` | Smoothed successful sample interval, or age / stale indication when no longer fresh |
| `SPEC TOK` | Draft / accepted token counters and acceptance data, when exposed |

A connected server with no activity evidence shows `UNKNOWN`, not a guessed `IDLE`. Offline or never-answered servers have no activity phase (`—`). During offline grace, a last-good sample is marked **STALE (held sample)** and selector ages read `stale … ago`; held rates are not highlighted as live.

For llama.cpp, enable `/metrics` with `--metrics`. `/props` is cached; `/slots` is optional. Live slot rates use verified slot IDs paired across the same task, not array position. Missing or ambiguous slot identity, changed tasks or changed slot membership cause the per-slot path to abstain and use aggregate `/metrics` counters when available. Rates aggregate across active slots, while the CTX used/capacity pair comes from a single slot: the most-used busy slot, or most-used slot overall when idle. Occupancy comes only from direct `n_prompt_tokens`, never inferred from request progress or the high-water mark.

For Strata, LIVE TG uses the server's short-window `live.tok_s` only while generating. LIVE PP stays unavailable because Strata does not expose an instantaneous prefill rate. Lifetime averages use cumulative totals, not per-request means. Build 0.1.40 lacks the slot occupancy field, so CTX used stays unavailable; newer schemas can report direct occupancy. REQUEST still uses the server's live request fields. See [Backend versions](BACKEND_VERSIONS.md) for schema mappings and verification limits, and [Telemetry verification](TELEMETRY_VERIFICATION.md) for detailed field evidence.

### GPU, system and processes

The selected GPU can expose utilization, VRAM, memory-controller utilization, clocks, temperature, power/limit, P-state, throttle reasons, fan, encoder/decoder activity and PCIe RX/TX. This is not a promise that every vendor exposes every field. NVIDIA uses NVML without running `nvidia-smi` each refresh; AMD and Intel use their kernel sysfs/hwmon interfaces. Per-process GPU data uses DRM fdinfo where available. See [Hardware support](HARDWARE_SUPPORT.md) for exact coverage.

System telemetry includes CPU utilization/frequency, per-core activity, hybrid P/E grouping where exposed, package temperature, power when readable, load, I/O wait, RAM and swap. Histories cover 60 seconds for CPU/RAM, GPU/VRAM and LLM prefill/decode. Missing sensors and insufficient permissions leave readings unavailable rather than substituting another measurement.

## Troubleshooting

### Command not found

See [PATH setup](INSTALLATION.md#path-setup), including Bash, Zsh and Fish instructions. Cargo installs normally use `~/.cargo/bin`, not `~/.local/bin`.

### Offline or incomplete LLM telemetry

Check the selected endpoint and ensure llama.cpp has `--metrics` enabled. Probe its metrics read-only, adjusting the address and port:

```bash
curl http://127.0.0.1:8080/metrics
```

For remote servers, use a reachable address instead of the server's wildcard bind address. A missing `/slots` endpoint does not prevent basic monitoring, but fields that depend on it can remain unavailable. Strata must be configured explicitly; its missing instantaneous prefill rate is an API limitation, not a connection failure.

### Missing GPU readings

Check that the vendor driver is working:

```bash
nvidia-smi                              # NVIDIA
cat /sys/class/drm/card*/device/vendor   # 0x10de NVIDIA, 0x1002 AMD, 0x8086 Intel
```

OrsikTop does not bundle vendor drivers or NVML. Some sensors are inaccessible without extra permissions, and some do not exist on particular GPUs. Consult [Hardware support](HARDWARE_SUPPORT.md) before treating `—` as a bug.

### Diagnostics for bug reports

```bash
orsiktop diag
orsiktop diag --servers
```

Reports redact URL credentials, paths and query values, but still identify hardware, processes and endpoint hosts/addresses. Review them before sharing. Over-limit endpoints are listed as not monitored and never probed; unavailable telemetry is not an availability claim.

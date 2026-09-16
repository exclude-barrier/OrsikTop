# Stage Summary — S15 (llama.cpp server discovery)

## 1. Files changed/created

| File | Change |
| --- | --- |
| `src/discovery_llm.rs` (new) | llama.cpp server discovery as its own module. Keeps **every** candidate endpoint (no more lowest-PID drop), selects one endpoint deterministically **by port, never PID**, and exposes the full candidate list for pinning/diagnostics (S18). All `/proc` reads route through the `Sys` abstraction so the scan is fixture-testable. 11 offline tests (3 moved from `main.rs`, 8 new). |
| `src/main.rs` | `mod discovery_llm;` added. `resolve_server` and `server_is_auto_discovered` rewritten to build a candidate list via `discovery_llm::collect_candidates(&RealSys, …)` + `discovery_llm::select_endpoint(…)`. The old inline `discover_local_llama_server` / `parse_cmdline` / `is_llama_server_process` / `endpoint_from_args` / `connect_host` / `cli_value` helpers and their 3 tests are removed (moved to the new module). Public contract preserved: `server_is_auto_discovered` still `pub(crate)`, same semantics. |
| `docs/S15_STAGE_SUMMARY.md` | This summary. |

## 2. Architecture changes

- **Multiple candidates, not one.** The old `discover_local_llama_server()` collected
  every `(pid, endpoint)` then did `sort_by_key(pid)` and kept the **first** — i.e. the
  lowest PID — silently discarding every other running server. `discover_processes()`
  now returns `Vec<LlamaServerCandidate>`, one entry per distinct endpoint, each tagged
  with its `ServerSource` (`Process { pid }` or `Configured`) and, for process
  candidates, the originating command line. The full list is what S18's diagnostics
  consumes and what the user can pin via `server=`.
- **Selection is by port, never PID.** `discover_processes()` sorts by
  `(port, endpoint-string)` and de-duplicates identical endpoints, so the pick is stable
  regardless of process spawn order. This is a deliberate behavior change from
  lowest-PID: a `llama-server` on port 8081 started *after* one on 9090 now wins, because
  8081 < 9090 — the lower port is the conventional "primary" server and the choice is
  reproducible across restarts (PIDs are not).
- **Candidate assembly & precedence preserved.** `collect_candidates(sys, auto, configured)`
  returns discovered servers (only when auto-discovery is on) followed by the configured
  endpoint (added once, only if it isn't already a discovered endpoint). `select_endpoint`
  then prefers the best *discovered* server, else the *configured* one, else the built-in
  default. This keeps the historical rule — a discovered server beats the configured
  `server` key when auto-discovery is on; `--server`/env sets `auto_discovery = false` so a
  manual endpoint is used verbatim; nothing running → default. The configured endpoint is
  still recorded even when a discovered one wins, so diagnostics can show "you configured
  X but Y was used."
- **Fixture-testable `/proc` scan.** The scan no longer calls `std::fs` directly; it reads
  `/proc/<pid>` directory entries and `/proc/<pid>/cmdline` through `Sys::read_dir` /
  `Sys::read_to_string`. `RealSys` is production; `FixtureSys` drives the tests, so no
  test depends on a live llama server or on the host's process table.
- **Endpoint parsing is byte-identical to before.** `parse_cmdline` (NUL-split),
  `is_llama_server_process`, `endpoint_from_args` (`--host`/`--port`, both `--flag val`
  and `--flag=val`), and `connect_host` (wildcard `0.0.0.0`/`::` → `127.0.0.1`, bare
  IPv6 bracketed) are unchanged — the 3 tests that guarded them moved verbatim into the
  new module and still pass.

## 3. Test results

`cargo fmt --check`, `cargo check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --all` → all green: **126 passed, 0 failed, 1 ignored** (was 118;
+11 in `discovery_llm.rs`, −3 removed from `main.rs`, net +8). New coverage includes:
`discovers_all_servers_and_ignores_non_llama` (multi-server + non-llama filtering),
`selection_is_by_port_not_pid` (the core S15 assertion — a fixture where lowest PID
disagrees with lowest port, asserting the lower **port** wins), `dedupes_identical_endpoints`,
`missing_proc_yields_no_candidates`, `configured_and_default_selection`,
`configured_duplicate_of_discovered_is_not_pushed_twice`,
`auto_discovery_off_excludes_processes`, and
`discovered_beats_configured_when_auto_is_on`. `cargo test -- --ignored
discovery_real` still passes on the real machine (Intel xe). Binary smoke: `main()` runs
the new `resolve_server`/`server_is_auto_discovered` → `discover_processes(&RealSys)` path
before terminal init and reaches only the pre-existing ENXIO, confirming the real `/proc`
scan does not panic when no llama server is running.

## 4. Known limitations

- **Listening-socket cross-reference (not implemented).** The S15 plan lists "listening
  sockets where practical" as a *secondary* source. It is intentionally omitted for now:
  attributing a `/proc/net/tcp{,6}` LISTEN inode to a PID requires scanning
  `/proc/<pid>/fd/*` readlinks (a large, slow, permission-sensitive walk) to add value
  the process command-line scan already covers for local servers, and it would complicate
  the `Sys` trait (socket parsing + inode→pid mapping) without a concrete consumer yet.
  The command-line scan is the authoritative local source; a remote/configured endpoint
  that has no matching local process is still honored via `ServerSource::Configured`. If a
  real case surfaces (e.g. a server that binds a port not visible in its CLI), socket
  discovery can be added to `discover_processes` without changing the `Candidate`
  contract.
- **Selection is deterministic but not "smart."** With two genuinely distinct local
  servers on different ports, OrsikTop always polls the lower port and shows the other as
  a candidate. There is no live reachability probe at discovery time (that belongs to the
  LLM worker's connection state, S14's territory). The user can switch servers by setting
  `server=` to the other candidate.
- **No interactive "pick a server" UI yet.** Multiple candidates are represented and
  recorded (for S18), but the settings panel still shows a single resolved endpoint; the
  "let the user choose among candidates" UX is deferred to S18/S23.
- The TUI cannot be smoke-tested in this headless shell (no TTY); the ENXIO
  terminal-init error is pre-existing and unrelated.

## 5. Newly discovered issues

- **Clippy `octal-escapes` on `\0`-followed-by-digit in test literals.** Writing a
  `cmdline` fixture as `"/usr/bin/llama-server\0--port\09090\0"` trips
  `clippy::octal-escapes` (Rust 1.98) because `\0` directly before a digit *looks* like an
  octal escape, even though `\0` is always NUL. Fixed by a tiny `cmdline(&[&str])` test
  helper that `join("\0")`s the args — cleaner and immune to the lint.
- **The `Sys` trait is the right seam for `/proc` too.** `system.rs` was built for DRM
  sysfs, but `read_dir` + `read_to_string` cover the llama `/proc` scan without new
  methods, so discovery now shares the same fixture machinery as GPU discovery. No
  `Sys` change was needed.
- **`server_is_auto_discovered` is called on the UI thread** (`app.rs:152` after a
  settings edit). The rewrite keeps it a cheap `/proc` scan (no HTTP), so re-running it
  after each settings change stays within budget; the heavier work (actual polling) still
  lives in the LLM worker.

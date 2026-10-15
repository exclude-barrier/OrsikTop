#!/usr/bin/env python3
"""Release-binary benchmark runner for OrsikTop (CFA-92 / PERF-01).

Measures the *real* release binary (`target/release/orsiktop`) from the
outside. The application runs in its own pseudo-terminal; every application
figure (CPU, RSS, threads, frames, input latency, startup) is read from
`/proc/<app pid>` or from the application's own terminal output. The runner
and the loopback mock servers run in *separate* processes and are never
counted as application cost.

No real inference server is contacted: every monitored endpoint is a
loopback mock started by this script (or a closed loopback port for the
"no server" scenario), and auto-discovery is disabled through an isolated
`XDG_CONFIG_HOME`, so the user's saved config is neither read nor written.

Usage (from the repository root):

    cargo build --release --locked
    python3 scripts/perf_release_bench.py --out target/perf-bench

    # quick smoke run
    python3 scripts/perf_release_bench.py --duration 5 --scenarios idle_no_mock

    # long stable run (10 minutes)
    python3 scripts/perf_release_bench.py --scenarios soak_mock_4 --soak-duration 600

Raw results are printed as `ORSKTOP_RELEASE_BENCH|<scenario>|<key>=<value>`
lines and written as JSON to `--out`. Every timing is reported, never
asserted: this is a per-machine baseline, not a threshold gate.
Only the Python standard library is used. Linux only (`/proc`, `pty`).
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import platform
import select
import signal
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CLK_TCK = os.sysconf("SC_CLK_TCK")
PAGE_KB = os.sysconf("SC_PAGE_SIZE") // 1024
# Terminal geometry: the same 160x48 the in-process render benchmark uses.
COLS, ROWS = 160, 48
# crossterm `Hide` (`ESC [ ? 25 l`). ratatui executes it after *every*
# `Terminal::draw` (no cursor is placed in the dashboard view), and `main.rs`
# emits it once before the first draw. Counting it therefore counts frames.
FRAME_MARKER = b"\x1b[?25l"
MOCK_MODEL = "benchmodel-q4"


# ---------------------------------------------------------------------------
# Loopback mock servers (run in a separate process: `--mock-serve N`)
# ---------------------------------------------------------------------------


def mock_serve(count: int, log_path: str) -> None:
    """Serve `count` llama.cpp-like loopback mocks until stdin closes.

    HTTP/1.1 keep-alive, like llama-server. Each request is logged with the
    system-wide CLOCK_MONOTONIC so the runner can align it with app events.
    """
    log: list[tuple[float, float, int, str]] = []
    log_lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):  # silence stderr
            pass

        def do_GET(self):  # noqa: N802 (http.server API)
            start = time.monotonic()
            idx = self.server.server_index  # type: ignore[attr-defined]
            if "/props" in self.path:
                body = json.dumps(
                    {
                        "model_path": f"/models/{MOCK_MODEL}-{idx}.gguf",
                        "total_slots": 1,
                        "default_generation_settings": {"n_ctx": 8192},
                    }
                )
                ctype = "application/json"
            elif "/slots" in self.path:
                body = json.dumps([{"id": 0, "n_ctx": 8192, "is_processing": False}])
                ctype = "application/json"
            else:
                tick = int(start)
                body = (
                    f"llamacpp:prompt_tokens_total {1000 + tick}\n"
                    f"llamacpp:tokens_predicted_total {500 + tick}\n"
                )
                ctype = "text/plain"
            data = body.encode()
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            with log_lock:
                log.append((start, time.monotonic(), idx, self.path))

    servers = []
    for idx in range(count):
        srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        srv.daemon_threads = True
        srv.server_index = idx  # type: ignore[attr-defined]
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        servers.append(srv)
    print(json.dumps([s.server_address[1] for s in servers]), flush=True)
    sys.stdin.read()  # parent closes stdin to stop us
    for srv in servers:
        srv.shutdown()
    with log_lock, open(log_path, "w") as fh:
        json.dump(log, fh)


class MockProcess:
    def __init__(self, count: int, workdir: Path):
        self.count = count
        self.log_path = workdir / "mock-requests.json"
        self.proc = None
        self.ports: list[int] = []

    def __enter__(self):
        if self.count == 0:
            return self
        self.proc = subprocess.Popen(
            [sys.executable, "-I", str(Path(__file__).resolve()), "--mock-serve",
             str(self.count), "--mock-log", str(self.log_path)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
        )
        self.ports = json.loads(self.proc.stdout.readline())
        return self

    def requests(self) -> list:
        if self.proc is None:
            return []
        self.proc.stdin.close()
        self.proc.wait(timeout=30)
        self.proc = None
        return json.loads(self.log_path.read_text())

    def __exit__(self, *_exc):
        if self.proc is not None:
            self.proc.kill()
            self.proc.wait()


def closed_loopback_port() -> int:
    """A loopback port nothing listens on (bound, then released)."""
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


# ---------------------------------------------------------------------------
# /proc helpers (application process only)
# ---------------------------------------------------------------------------


def proc_cpu_ticks(pid: int) -> int:
    stat = Path(f"/proc/{pid}/stat").read_text()
    fields = stat.rsplit(")", 1)[1].split()
    return int(fields[11]) + int(fields[12])  # utime + stime


def proc_status(pid: int) -> dict:
    out = {}
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        key, _, value = line.partition(":")
        if key in ("VmRSS", "VmHWM", "Threads", "voluntary_ctxt_switches",
                   "nonvoluntary_ctxt_switches"):
            out[key] = int(value.split()[0])
    return out


def thread_cpu_ticks(pid: int) -> dict[int, int]:
    """utime+stime per *live* thread (tid -> ticks)."""
    out = {}
    for tid in os.listdir(f"/proc/{pid}/task"):
        try:
            stat = Path(f"/proc/{pid}/task/{tid}/stat").read_text()
        except OSError:
            continue  # thread exited between listdir and read
        fields = stat.rsplit(")", 1)[1].split()
        out[int(tid)] = int(fields[11]) + int(fields[12])
    return out


def host_process_count() -> int:
    return sum(1 for d in os.listdir("/proc") if d.isdigit())


def read_first(path: str) -> str | None:
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def host_info() -> dict:
    cpuinfo = Path("/proc/cpuinfo").read_text()
    model = next((l.split(":", 1)[1].strip() for l in cpuinfo.splitlines()
                  if l.startswith("model name")), "unknown")
    mem = next(l.split()[1] for l in Path("/proc/meminfo").read_text().splitlines()
               if l.startswith("MemTotal"))

    def cmd(*args):
        try:
            return subprocess.run(args, capture_output=True, text=True, cwd=REPO,
                                  timeout=30).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            return None

    ac = [read_first(str(p / "online")) for p in Path("/sys/class/power_supply").glob("A*")]
    return {
        "hostname": platform.node(),
        "cpu_model": model,
        "logical_cpus": os.cpu_count(),
        "mem_total_kb": int(mem),
        "kernel": platform.release(),
        "rustc": cmd("rustc", "-V"),
        "git_head": cmd("git", "rev-parse", "--short", "HEAD"),
        "git_dirty": bool(cmd("git", "status", "--porcelain", "--untracked-files=no")),
        "governor": read_first("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        "epp": read_first(
            "/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference"),
        "ac_online": ac or None,
        "python": platform.python_version(),
    }


# ---------------------------------------------------------------------------
# Application under a pseudo-terminal
# ---------------------------------------------------------------------------


class App:
    """The release binary on its own pty; collects frame timestamps."""

    def __init__(self, binary: Path, config_home: Path):
        self.binary = binary
        self.config_home = config_home
        self.frames: list[tuple[float, int]] = []  # (t, bytes since previous frame)
        self.first_output: float | None = None
        self.model_seen: float | None = None
        self.frame_cond = threading.Condition()
        self._stop = False

    def start(self) -> None:
        master, slave = os.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        env = {k: v for k, v in os.environ.items()
               if not k.startswith("ORSIKTOP_") and "proxy" not in k.lower()}
        env.update(XDG_CONFIG_HOME=str(self.config_home), TERM="xterm-256color",
                   NO_PROXY="*")
        self.master = master
        self.t_spawn = time.monotonic()
        self.proc = subprocess.Popen([str(self.binary)], stdin=slave, stdout=slave,
                                     stderr=slave, env=env, start_new_session=True,
                                     close_fds=True)
        os.close(slave)
        self.pid = self.proc.pid
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self) -> None:
        tail = b""
        pending = 0
        needle = MOCK_MODEL.encode()
        while not self._stop:
            ready, _, _ = select.select([self.master], [], [], 0.2)
            if not ready:
                continue
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                break
            if not chunk:
                break
            now = time.monotonic()
            if self.first_output is None:
                self.first_output = now
            data = tail + chunk
            if self.model_seen is None and needle in data:
                self.model_seen = now
            pending += len(chunk)
            hits = data.count(FRAME_MARKER) - tail.count(FRAME_MARKER)
            if hits:
                with self.frame_cond:
                    for _ in range(hits):
                        self.frames.append((now, pending))
                        pending = 0
                    self.frame_cond.notify_all()
            tail = data[-(len(FRAME_MARKER) - 1):]

    def wait_frames(self, count: int, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        with self.frame_cond:
            while len(self.frames) < count:
                left = deadline - time.monotonic()
                if left <= 0:
                    return False
                self.frame_cond.wait(left)
        return True

    def send(self, data: bytes) -> float:
        t = time.monotonic()
        os.write(self.master, data)
        return t

    def stop(self) -> dict:
        t = self.send(b"\x1b")  # Esc quits the dashboard view
        try:
            code = self.proc.wait(timeout=10)
            exit_s = time.monotonic() - t
        except subprocess.TimeoutExpired:
            self.proc.kill()
            code, exit_s = self.proc.wait(), None
        self._stop = True
        self.reader.join(timeout=2)
        os.close(self.master)
        return {"exit_code": code, "shutdown_ms": None if exit_s is None else exit_s * 1000}


def write_config(config_home: Path, endpoints: list[str], refresh_ms: int) -> None:
    cfg = config_home / "orsiktop"
    cfg.mkdir(parents=True, exist_ok=True)
    lines = [f"server={endpoints[0]}"]
    if len(endpoints) > 1:
        lines.append("servers=" + ",".join(endpoints[1:]))
    lines += [f"refresh_ms={refresh_ms}", "process_refresh_ms=1000", "auto_discovery=false"]
    (cfg / "config").write_text("\n".join(lines) + "\n")


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------

SCENARIOS = {
    # name: (mock servers, refresh_ms, extra dummy processes)
    "idle_no_mock": (0, 1000, 0),
    "mock_1": (1, 1000, 0),
    "mock_4": (4, 1000, 0),
    "mock_16": (16, 1000, 0),
    "mock_1_refresh_100": (1, 100, 0),
    "mock_1_refresh_250": (1, 250, 0),
    "mock_1_refresh_5000": (1, 5000, 0),
    "large_proc_list_mock_1": (1, 1000, 2000),
    "soak_mock_4": (4, 1000, 0),
}
DEFAULT_SCENARIOS = [s for s in SCENARIOS if not s.startswith("soak")]


def spawn_dummies(count: int) -> list[subprocess.Popen]:
    """Idle `sleep` processes that only inflate the host process table."""
    procs = []
    for _ in range(count):
        procs.append(subprocess.Popen(["sleep", "3600"], stdin=subprocess.DEVNULL,
                                      stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL))
    return procs


def pct(values: list[float], q: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, round((len(ordered) - 1) * q))]


def summarize(values: list[float]) -> dict:
    if not values:
        return {"n": 0}
    return {"n": len(values), "mean": statistics.fmean(values),
            "median": statistics.median(values), "p95": pct(values, 0.95),
            "min": min(values), "max": max(values)}


def polling_stats(requests: list, t0: float, t1: float, servers: int) -> dict:
    """Mock-side view of the LLM poll: cycle period and fan-out spread."""
    window = [r for r in requests if t0 <= r[0] <= t1]
    metrics = sorted((r for r in window if "/metrics" in r[3]), key=lambda r: r[0])
    out = {"requests_in_window": len(window),
           "requests_per_s": len(window) / (t1 - t0),
           "props_requests": sum(1 for r in window if "/props" in r[3]),
           "mock_service_ms": summarize([(r[1] - r[0]) * 1000 for r in window])}
    periods = []
    for idx in range(servers):
        times = [r[0] for r in metrics if r[2] == idx]
        periods += [(b - a) * 1000 for a, b in zip(times, times[1:])]
    out["cycle_period_ms"] = summarize(periods)
    # A cycle = one /metrics per server; group by gaps larger than 50 ms.
    cycles, current = [], []
    for r in metrics:
        if current and r[0] - current[-1][0] > 0.05:
            cycles.append(current)
            current = []
        current.append(r)
    if current:
        cycles.append(current)
    full = [c for c in cycles if len({r[2] for r in c}) == servers]
    out["fanout_spread_ms"] = summarize([(c[-1][0] - c[0][0]) * 1000 for c in full])
    return out


def run_scenario(name: str, binary: Path, duration: float, warmup: float,
                 input_reps: int) -> dict:
    servers, refresh_ms, dummies = SCENARIOS[name]
    result: dict = {"scenario": name, "mock_servers": servers, "refresh_ms": refresh_ms,
                    "dummy_processes": dummies, "duration_s": duration}
    with tempfile.TemporaryDirectory(prefix="orsiktop-bench-") as tmp:
        tmp = Path(tmp)
        dummy_procs = spawn_dummies(dummies)
        try:
            with MockProcess(servers, tmp) as mocks:
                endpoints = [f"http://127.0.0.1:{p}" for p in mocks.ports] or [
                    f"http://127.0.0.1:{closed_loopback_port()}"]
                write_config(tmp, endpoints, refresh_ms)
                result["host_processes"] = host_process_count()
                result["loadavg_start"] = read_first("/proc/loadavg")
                app = App(binary, tmp)
                app.start()
                # --- startup ---------------------------------------------
                app.wait_frames(2, 10)  # init Hide + first drawn frame
                result["startup_first_output_ms"] = _ms(app.first_output, app.t_spawn)
                result["startup_first_frame_ms"] = (
                    _ms(app.frames[1][0], app.t_spawn) if len(app.frames) > 1 else None)
                time.sleep(warmup)
                result["startup_model_on_screen_ms"] = _ms(app.model_seen, app.t_spawn)
                # --- steady-state window ---------------------------------
                samples = []
                f0 = len(app.frames)
                t0 = time.monotonic()
                c0 = proc_cpu_ticks(app.pid)
                tc0 = thread_cpu_ticks(app.pid)
                s0 = proc_status(app.pid)
                while time.monotonic() - t0 < duration:
                    st = proc_status(app.pid)
                    samples.append((time.monotonic() - t0, st["VmRSS"], st["Threads"]))
                    time.sleep(0.05)
                t1 = time.monotonic()
                c1 = proc_cpu_ticks(app.pid)
                tc1 = thread_cpu_ticks(app.pid)
                s1 = proc_status(app.pid)
                window_frames = app.frames[f0:]
                elapsed = t1 - t0
                result["cpu_pct_one_cpu"] = (c1 - c0) / CLK_TCK / elapsed * 100
                result["cpu_ticks"] = c1 - c0
                # Per-thread split. Long-lived threads keep their tid; the
                # main (UI/render) thread is tid == pid. Threads are numbered
                # by creation order, so in `app::run` the first two workers
                # are the fast (system/process) worker and the LLM worker.
                # Short-lived poll threads exit inside the window; their CPU
                # is the remainder ("exited_or_new").
                live = sorted(t for t in tc1 if t in tc0)
                split = {}
                for rank, tid in enumerate(live):
                    label = "main_ui" if tid == app.pid else f"thread_{rank}"
                    split[label] = (tc1[tid] - tc0[tid]) / CLK_TCK / elapsed * 100
                split["exited_or_new"] = result["cpu_pct_one_cpu"] - sum(split.values())
                result["cpu_pct_by_thread"] = split
                result["frames_per_s"] = len(window_frames) / elapsed
                result["bytes_per_frame"] = summarize([b for _, b in window_frames])
                gaps = [(b[0] - a[0]) * 1000 for a, b in zip(window_frames, window_frames[1:])]
                result["frame_gap_ms"] = summarize(gaps)
                rss = [s[1] for s in samples]
                thr = [s[2] for s in samples]
                result["rss_kb"] = summarize(rss)
                result["rss_kb_first_s"] = statistics.fmean(
                    [s[1] for s in samples if s[0] < 10] or rss)
                result["rss_kb_last_s"] = statistics.fmean(
                    [s[1] for s in samples if s[0] > elapsed - 10] or rss)
                result["vm_hwm_kb"] = s1["VmHWM"]
                result["threads"] = summarize(thr)
                result["ctx_switches_per_s"] = (
                    (s1["voluntary_ctxt_switches"] - s0["voluntary_ctxt_switches"])
                    + (s1["nonvoluntary_ctxt_switches"] - s0["nonvoluntary_ctxt_switches"])
                ) / elapsed
                result["sample_count"] = len(samples)
                # --- input latency ---------------------------------------
                latencies = []
                for rep in range(input_reps):
                    have = len(app.frames)
                    if not app.wait_frames(have + 1, 2):
                        continue
                    time.sleep(0.03)  # stay clear of the frame just drawn
                    have = len(app.frames)
                    sent = app.send(b"j" if rep % 2 == 0 else b"k")
                    if app.wait_frames(have + 1, 2):
                        latencies.append((app.frames[have][0] - sent) * 1000)
                result["input_to_frame_ms"] = summarize(latencies)
                result["loadavg_end"] = read_first("/proc/loadavg")
                result.update(app.stop())
                requests = mocks.requests()
                if servers:
                    result["polling"] = polling_stats(requests, t0, t1, servers)
        finally:
            for proc in dummy_procs:
                proc.kill()
            for proc in dummy_procs:
                proc.wait()
    return result


def _ms(t: float | None, base: float) -> float | None:
    return None if t is None else (t - base) * 1000


def flatten(prefix: str, value, out: list) -> None:
    if isinstance(value, dict):
        for key, inner in value.items():
            flatten(f"{prefix}.{key}" if prefix else key, inner, out)
    else:
        if isinstance(value, float):
            value = round(value, 3)
        out.append((prefix, value))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/orsiktop")
    parser.add_argument("--scenarios", nargs="+", default=DEFAULT_SCENARIOS,
                        choices=list(SCENARIOS))
    parser.add_argument("--duration", type=float, default=30.0,
                        help="steady-state window per scenario in seconds")
    parser.add_argument("--soak-duration", type=float, default=600.0)
    parser.add_argument("--warmup", type=float, default=5.0)
    parser.add_argument("--input-reps", type=int, default=40)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--out", type=Path, default=REPO / "target/perf-bench")
    parser.add_argument("--mock-serve", type=int, help=argparse.SUPPRESS)
    parser.add_argument("--mock-log", help=argparse.SUPPRESS)
    args = parser.parse_args()

    if args.mock_serve is not None:
        mock_serve(args.mock_serve, args.mock_log)
        return 0
    if not args.binary.is_file():
        print(f"missing {args.binary}; run `cargo build --release --locked` first",
              file=sys.stderr)
        return 2

    host = host_info()
    host["binary"] = str(args.binary)
    host["binary_mtime"] = time.strftime("%Y-%m-%dT%H:%M:%S",
                                         time.localtime(args.binary.stat().st_mtime))
    for key, value in host.items():
        print(f"ORSKTOP_RELEASE_BENCH|host|{key}={value}", flush=True)
    results = []
    for rep in range(args.repeat):
        for name in args.scenarios:
            duration = args.soak_duration if name.startswith("soak") else args.duration
            res = run_scenario(name, args.binary, duration, args.warmup, args.input_reps)
            res["repeat"] = rep
            results.append(res)
            flat: list = []
            flatten("", res, flat)
            for key, value in flat:
                print(f"ORSKTOP_RELEASE_BENCH|{name}#{rep}|{key}={value}", flush=True)
    args.out.mkdir(parents=True, exist_ok=True)
    stamp = time.strftime("%Y%m%dT%H%M%S")
    path = args.out / f"release-bench-{stamp}.json"
    path.write_text(json.dumps({"host": host, "results": results}, indent=2))
    print(f"ORSKTOP_RELEASE_BENCH|raw_json={path}", flush=True)
    return 0


if __name__ == "__main__":
    signal.signal(signal.SIGINT, signal.default_int_handler)
    sys.exit(main())

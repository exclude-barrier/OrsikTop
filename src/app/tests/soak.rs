//! CFA-94 (EPIC 4 / REL-01): reproducible multi-server soak harness.
//!
//! Drives the *real* `spawn_llm_worker` against 1, 4 or 16 loopback mock
//! servers and rotates every server through the failure scenarios the ticket
//! names: repeated reconnects (connection closed without an answer), server
//! restarts (listener dropped and re-bound on the same port), changing
//! counter baselines, missing telemetry, delayed responses (below and above
//! the client's request timeout), corrupt JSON, bursts of concurrent
//! server-set changes, and a shutdown while requests are in flight. No real
//! inference server is contacted.
//!
//! Checked on every run:
//! * **attribution** — every sample accepted under the app's own rule
//!   (`accepts_llm_sample`) belongs to a server of the current set and carries
//!   only that server's model, context size, slot count and counter band;
//! * **no permanent stall** — every server of the current set delivers an
//!   accepted sample within `STALL_BOUND`, and all recover to connected with
//!   their own model once healthy again;
//! * **no unintended requests** — per-mock request counts stay below the
//!   bound the poll floor allows, a server removed from the set receives no
//!   further requests once the switch has settled, and nothing is requested
//!   after the worker has been joined;
//! * **orderly shutdown** — `stop` during in-flight (hung) requests joins the
//!   worker within `SHUTDOWN_BOUND`, without a panic, even if the sample
//!   channel is never drained;
//! * **resources** (opt-in long runs only, single-threaded) — no uncontrolled
//!   RSS growth and no lingering threads after shutdown.
//!
//! Two short smoke variants run in the normal `cargo test` gate. The long
//! matrix is `#[ignore]`d; run it on demand with
//!
//! ```text
//! ORSIK_SOAK_SECS=300 cargo test --locked -- --ignored --test-threads=1 \
//!     --nocapture soak_long_
//! ```
//!
//! `ORSIK_SOAK_SECS` is the duration of *each* of the 1/4/16-server runs
//! (default 60 s). `/proc/self` values are process-global, so the RSS and
//! thread assertions are only enforced when the test binary runs
//! single-threaded; otherwise they are reported but not asserted.

use super::{
    accepts_llm_sample, assert_no_cross_mixing, read_request_path, self_status_field,
    start_soak_worker, test_spec, LlmCommand, LlmSample, LocalServerIdentity, ServerSpec,
    MAX_MONITORED_SERVERS, MIN_LLM_POLL_MS,
};

use std::collections::HashSet;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// How a mock answers the requests it accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Behavior {
    Healthy,
    /// Answer correctly after `SHORT_DELAY` (a slow server, inside the timeout).
    Delay,
    /// Read the request, then close without answering (forces a reconnect).
    Close,
    /// Read the request and hold the connection open until the behaviour
    /// changes or the mock stops, so the client's own request timeout fires.
    Hang,
    /// 200 with malformed JSON on `/props` and `/slots`, garbage on `/metrics`.
    CorruptJson,
    /// `/props` without any field, `/metrics` without a known metric, `/slots`
    /// 404: the server is up but reports no telemetry.
    MissingTelemetry,
}

const SHORT_DELAY: Duration = Duration::from_millis(150);
/// Last-resort cap for `Behavior::Hang`, only relevant for a mis-wired test.
const HANG_CAP: Duration = Duration::from_secs(20);
/// Maximum time any server of the current set may go without an accepted
/// sample. A cycle is bounded by the client request timeout (1.2 s,
/// `LLM_REQUEST_TIMEOUT_MS` in `src/llama.rs`) plus the 250 ms poll floor,
/// so this leaves ample headroom for a loaded CI machine while still catching
/// a wedged worker.
const STALL_BOUND: Duration = Duration::from_secs(6);
/// Join bound after `stop`: request timeout + one poll floor + margin.
const SHUTDOWN_BOUND: Duration = Duration::from_secs(4);
/// How long after a set switch is observed (first sample of the new
/// generation) a removed server's request counter is snapshotted.
const REMOVAL_SETTLE: Duration = Duration::from_millis(400);
/// Fallback for the snapshot when no sample of the new generation arrives
/// (e.g. the only server was removed): one full cycle including a request
/// timeout, so the removed server's last in-flight request has drained.
const REMOVAL_SETTLE_FALLBACK: Duration = Duration::from_millis(1_500);
/// Upper bound on HTTP requests one server may receive per poll cycle
/// (`/props`, `/metrics`, `/slots`).
const REQUESTS_PER_CYCLE: u64 = 3;
/// Rate values above this are a fabricated jump, not a measured rate.
const MAX_PLAUSIBLE_RATE: f64 = 1e8;

/// Counter band of server `index`: its cumulative counters always stay inside
/// `[band, band + 1e9)`, so a counter value from another server is caught as
/// cross-server mixing even when model and slots happen to match.
fn band(index: usize) -> u64 {
    (index as u64 + 1) * 1_000_000_000
}

struct MockShared {
    behavior: Mutex<Behavior>,
    stop: AtomicBool,
    counter_base: AtomicU64,
    props_requests: AtomicU64,
    metrics_requests: AtomicU64,
    slots_requests: AtomicU64,
    other_requests: AtomicU64,
    in_flight: AtomicUsize,
    handlers: AtomicUsize,
    model: String,
    n_ctx: u64,
    slots: u64,
}

impl MockShared {
    fn behavior(&self) -> Behavior {
        *self.behavior.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A loopback llama.cpp stand-in with per-request behaviour, request counters
/// and a real restart (the listener is dropped and the same port re-bound).
/// Every connection is served on its own thread, so a hung request never
/// blocks the others.
struct SoakMock {
    addr: SocketAddr,
    shared: Arc<MockShared>,
    accept: Option<(Arc<AtomicBool>, thread::JoinHandle<()>)>,
}

impl SoakMock {
    fn start(index: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind soak mock");
        let addr = listener.local_addr().expect("soak mock addr");
        let shared = Arc::new(MockShared {
            behavior: Mutex::new(Behavior::Healthy),
            stop: AtomicBool::new(false),
            counter_base: AtomicU64::new(band(index)),
            props_requests: AtomicU64::new(0),
            metrics_requests: AtomicU64::new(0),
            slots_requests: AtomicU64::new(0),
            other_requests: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
            handlers: AtomicUsize::new(0),
            model: format!("soak-model-{index}"),
            n_ctx: 1_000 + index as u64,
            slots: index as u64 + 1,
        });
        let accept = Some(spawn_accept(listener, Arc::clone(&shared)));
        Self {
            addr,
            shared,
            accept,
        }
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn set_behavior(&self, behavior: Behavior) {
        *self
            .shared
            .behavior
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = behavior;
    }

    fn set_counter_base(&self, base: u64) {
        self.shared.counter_base.store(base, Ordering::SeqCst);
    }

    fn requests(&self) -> u64 {
        self.shared.props_requests.load(Ordering::SeqCst)
            + self.shared.metrics_requests.load(Ordering::SeqCst)
            + self.shared.slots_requests.load(Ordering::SeqCst)
            + self.shared.other_requests.load(Ordering::SeqCst)
    }

    fn unexpected_requests(&self) -> u64 {
        self.shared.other_requests.load(Ordering::SeqCst)
    }

    fn in_flight(&self) -> usize {
        self.shared.in_flight.load(Ordering::SeqCst)
    }

    fn is_up(&self) -> bool {
        self.accept.is_some()
    }

    /// Stop accepting and close the listening socket: new connections are
    /// refused until `come_up`.
    fn go_down(&mut self) {
        if let Some((halt, handle)) = self.accept.take() {
            halt.store(true, Ordering::SeqCst);
            let _ = handle.join();
        }
    }

    /// Re-bind the same port. Failing to re-bind is a harness error and is
    /// reported, never silently ignored.
    fn come_up(&mut self) {
        if self.accept.is_some() {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let listener = loop {
            match TcpListener::bind(self.addr) {
                Ok(listener) => break listener,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Err(err) => panic!("soak mock could not re-bind {}: {err}", self.addr),
            }
        };
        self.accept = Some(spawn_accept(listener, Arc::clone(&self.shared)));
    }
}

impl Drop for SoakMock {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.go_down();
        // Connection handlers observe `stop` within one 20 ms tick; wait so a
        // dropped mock leaves no thread behind (bounded, never blocks Drop).
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.shared.handlers.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn spawn_accept(
    listener: TcpListener,
    shared: Arc<MockShared>,
) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
    // Non-blocking accept so `go_down`/`Drop` never wait on a blocked accept.
    listener
        .set_nonblocking(true)
        .expect("non-blocking soak listener");
    let halt = Arc::new(AtomicBool::new(false));
    let halt_flag = Arc::clone(&halt);
    let handle = thread::spawn(move || {
        while !halt_flag.load(Ordering::SeqCst) && !shared.stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    shared.handlers.fetch_add(1, Ordering::SeqCst);
                    let shared = Arc::clone(&shared);
                    thread::spawn(move || {
                        serve_connection(stream, &shared);
                        shared.handlers.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
        // `listener` drops here: the port is closed until `come_up`.
    });
    (halt, handle)
}

fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn serve_connection(mut stream: TcpStream, shared: &MockShared) {
    let _ = stream.set_nonblocking(false);
    let Some(path) = read_request_path(&mut stream) else {
        return;
    };
    let metrics_index = if path.contains("/metrics") {
        shared.metrics_requests.fetch_add(1, Ordering::SeqCst) + 1
    } else {
        let counter = if path.contains("/props") {
            &shared.props_requests
        } else if path.contains("/slots") {
            &shared.slots_requests
        } else {
            &shared.other_requests
        };
        counter.fetch_add(1, Ordering::SeqCst);
        0
    };
    shared.in_flight.fetch_add(1, Ordering::SeqCst);
    match shared.behavior() {
        Behavior::Close => drop(stream),
        Behavior::Hang => {
            let cap = Instant::now() + HANG_CAP;
            while !shared.stop.load(Ordering::SeqCst)
                && Instant::now() < cap
                && shared.behavior() == Behavior::Hang
            {
                thread::sleep(Duration::from_millis(20));
            }
            drop(stream);
        }
        Behavior::Delay => {
            thread::sleep(SHORT_DELAY);
            serve_healthy_soak(&mut stream, &path, shared, metrics_index);
        }
        Behavior::CorruptJson => {
            if path.contains("/metrics") {
                write_response(&mut stream, "200 OK", "text/plain", "\u{1}garbage{{{");
            } else {
                write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    "{\"model_path\":",
                );
            }
        }
        Behavior::MissingTelemetry => {
            if path.contains("/props") {
                write_response(&mut stream, "200 OK", "application/json", "{}");
            } else if path.contains("/metrics") {
                write_response(
                    &mut stream,
                    "200 OK",
                    "text/plain",
                    "# HELP unrelated_metric nothing llama.cpp knows\nunrelated_metric 1\n",
                );
            } else {
                write_response(&mut stream, "404 Not Found", "text/plain", "not found");
            }
        }
        Behavior::Healthy => serve_healthy_soak(&mut stream, &path, shared, metrics_index),
    }
    shared.in_flight.fetch_sub(1, Ordering::SeqCst);
}

fn serve_healthy_soak(stream: &mut TcpStream, path: &str, shared: &MockShared, index: u64) {
    let (model, n_ctx, slots) = (&shared.model, shared.n_ctx, shared.slots);
    if path.contains("/props") {
        let body = format!(
            "{{\"model_path\":\"{model}\",\"total_slots\":{slots},\"default_generation_settings\":{{\"n_ctx\":{n_ctx}}}}}"
        );
        write_response(stream, "200 OK", "application/json", &body);
    } else if path.contains("/slots") {
        let entries: Vec<String> = (0..slots)
            .map(|id| format!("{{\"id\":{id},\"n_ctx\":{n_ctx},\"is_processing\":false}}"))
            .collect();
        let body = format!("[{}]", entries.join(","));
        write_response(stream, "200 OK", "application/json", &body);
    } else {
        let base = shared.counter_base.load(Ordering::SeqCst);
        let prompt = base + index * 100;
        let predicted = base + index * 50;
        let body = format!(
            "llamacpp:prompt_tokens_total {prompt}\nllamacpp:tokens_predicted_total {predicted}\n"
        );
        write_response(stream, "200 OK", "text/plain", &body);
    }
}

/// Join `worker` off-thread and return its result, or `None` when it did not
/// finish within `bound`. Unlike `join_within`, a hung worker is *leaked*
/// (the joiner is never joined), so a hang turns into a clear assertion
/// instead of a test that blocks until the CI timeout. The panic payload is
/// returned, so a panicked worker is never mistaken for a clean exit.
fn join_or_leak(worker: thread::JoinHandle<()>, bound: Duration) -> Option<thread::Result<()>> {
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = done_tx.send(worker.join());
    });
    done_rx.recv_timeout(bound).ok()
}

/// Scenario applied to one server for one phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Healthy,
    Reconnect,
    Restart,
    BaselineShift,
    MissingTelemetry,
    Delayed,
    OverTimeout,
    CorruptJson,
}

const SCENARIOS: [Scenario; 8] = [
    Scenario::Healthy,
    Scenario::Reconnect,
    Scenario::Restart,
    Scenario::BaselineShift,
    Scenario::MissingTelemetry,
    Scenario::Delayed,
    Scenario::OverTimeout,
    Scenario::CorruptJson,
];

struct SoakParams {
    servers: usize,
    duration: Duration,
    phase: Duration,
    measure_resources: bool,
}

// Every field is reported through `Debug` in `emit`.
#[allow(dead_code)]
#[derive(Debug, Default)]
struct SoakReport {
    servers: usize,
    elapsed_ms: u128,
    phases: u64,
    set_commands: u64,
    samples: u64,
    accepted: u64,
    stale_rejected: u64,
    removal_checks: u64,
    restarts: u64,
    requests_total: u64,
    max_gap_ms: u128,
    shutdown_ms: u128,
    threads_before: u64,
    threads_peak: u64,
    threads_after: u64,
    rss_samples: usize,
    rss_first_median_kb: u64,
    rss_last_median_kb: u64,
}

impl SoakReport {
    fn emit(&self) {
        println!("ORSKTOP_SOAK|{self:?}");
    }
}

/// Watch on a server removed from the set: once the switch has settled, its
/// request counter is snapshotted and must not move until it is re-added.
struct RemovalWatch {
    generation: u64,
    removed_at: Instant,
    first_seen: Option<Instant>,
    snapshot: Option<u64>,
}

/// Per-server activity bookkeeping for the request upper bound.
#[derive(Default)]
struct Activity {
    since: Option<Instant>,
    total: Duration,
    activations: u64,
}

impl Activity {
    fn activate(&mut self, now: Instant) {
        self.since = Some(now);
        self.activations += 1;
    }

    fn deactivate(&mut self, now: Instant) {
        if let Some(since) = self.since.take() {
            self.total += now.saturating_duration_since(since);
        }
    }

    /// Maximum requests the worker may legitimately have sent: at most one
    /// cycle per poll floor while active, plus one in-flight cycle at every
    /// activation edge and at the end.
    fn request_bound(&self, now: Instant) -> u64 {
        let active = self.total
            + self
                .since
                .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        let cycles = active.as_millis() as u64 / MIN_LLM_POLL_MS + 2 * self.activations + 2;
        cycles * REQUESTS_PER_CYCLE
    }
}

fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

/// True when this test binary runs one test at a time, so `/proc/self` values
/// belong to this test alone.
fn single_threaded_run() -> bool {
    if std::env::var("RUST_TEST_THREADS").is_ok_and(|value| value.trim() == "1") {
        return true;
    }
    let args: Vec<String> = std::env::args().collect();
    args.iter().any(|arg| arg == "--test-threads=1")
        || args
            .windows(2)
            .any(|pair| pair[0] == "--test-threads" && pair[1] == "1")
}

struct Driver<'a> {
    mocks: &'a mut [SoakMock],
    keys: Vec<String>,
    active: Vec<bool>,
    identity_round: Vec<u64>,
    activity: Vec<Activity>,
    removals: Vec<Option<RemovalWatch>>,
    last_sample: Vec<Instant>,
    restart_at: Vec<Option<Instant>>,
    generation: u64,
    last_stream_generation: u64,
    cmds: mpsc::Sender<LlmCommand>,
    report: SoakReport,
}

impl Driver<'_> {
    fn spec(&self, index: usize) -> ServerSpec {
        let round = self.identity_round[index];
        ServerSpec {
            identity: (round > 0).then(|| LocalServerIdentity {
                pid: 40_000 + index as u32,
                start_time: round,
            }),
            ..test_spec(&self.keys[index], &self.mocks[index].base())
        }
    }

    fn current_set(&self) -> Vec<ServerSpec> {
        (0..self.mocks.len())
            .filter(|&index| self.active[index])
            .map(|index| self.spec(index))
            .collect()
    }

    fn send_set(&mut self, servers: Vec<ServerSpec>) {
        self.generation += 1;
        self.report.set_commands += 1;
        self.cmds
            .send(LlmCommand::SetServers {
                generation: self.generation,
                servers,
            })
            .expect("the LLM worker must still receive commands");
        // Samples of the replaced generation are rejected by the app from now
        // on; restart every stall clock at the switch.
        self.last_sample.fill(Instant::now());
    }

    fn apply_scenario(&mut self, index: usize, scenario: Scenario, phase: u64, now: Instant) {
        let mock = &mut self.mocks[index];
        mock.come_up();
        self.restart_at[index] = None;
        let behavior = match scenario {
            Scenario::Healthy | Scenario::Restart | Scenario::BaselineShift => Behavior::Healthy,
            Scenario::Reconnect => Behavior::Close,
            Scenario::MissingTelemetry => Behavior::MissingTelemetry,
            Scenario::Delayed => Behavior::Delay,
            Scenario::OverTimeout => Behavior::Hang,
            Scenario::CorruptJson => Behavior::CorruptJson,
        };
        mock.set_behavior(behavior);
        match scenario {
            Scenario::Restart => {
                // A restarted server comes back with its counters reset to the
                // bottom of its band (a backwards jump for the monitor).
                mock.go_down();
                mock.set_counter_base(band(index));
                self.restart_at[index] = Some(now);
                self.report.restarts += 1;
            }
            Scenario::BaselineShift => {
                // A different baseline, sometimes lower, sometimes higher.
                mock.set_counter_base(band(index) + (phase % 5) * 1_000_000);
            }
            _ => {}
        }
    }

    /// A burst of two back-to-back set changes (the worker drains both at its
    /// next cycle head and must end up on the second): the first rebuilds the
    /// first active server's monitor under a new process identity, the second
    /// toggles `target` in or out of the set.
    fn set_change_burst(&mut self, target: usize, now: Instant) {
        if let Some(first) = (0..self.mocks.len()).find(|&index| self.active[index]) {
            self.identity_round[first] += 1;
            let intermediate = self.current_set();
            self.send_set(intermediate);
        }
        if self.active[target] {
            self.active[target] = false;
            self.activity[target].deactivate(now);
            self.removals[target] = Some(RemovalWatch {
                generation: self.generation + 1,
                removed_at: now,
                first_seen: None,
                snapshot: None,
            });
        } else {
            self.readd(target, now);
        }
        let set = self.current_set();
        self.send_set(set);
    }

    fn readd(&mut self, target: usize, now: Instant) {
        if let Some(watch) = self.removals[target].take() {
            if let Some(snapshot) = watch.snapshot {
                let requests = self.mocks[target].requests();
                assert_eq!(
                    requests,
                    snapshot,
                    "server {target} received {} request(s) while removed from the set",
                    requests - snapshot
                );
                self.report.removal_checks += 1;
            }
        }
        self.active[target] = true;
        self.activity[target].activate(now);
    }

    fn check_sample(&mut self, sample: LlmSample, now: Instant) {
        self.report.samples += 1;
        assert!(
            sample.generation >= self.last_stream_generation,
            "generation went backwards in the sample stream ({} after {})",
            sample.generation,
            self.last_stream_generation
        );
        self.last_stream_generation = sample.generation;
        assert!(
            sample.generation <= self.generation,
            "sample tagged with a generation ({}) the app never issued (latest {})",
            sample.generation,
            self.generation
        );
        let index = self
            .keys
            .iter()
            .position(|key| *key == sample.server)
            .unwrap_or_else(|| panic!("unknown server key published: {}", sample.server));
        for watch in self.removals.iter_mut().flatten() {
            if watch.first_seen.is_none() && sample.generation >= watch.generation {
                watch.first_seen = Some(now);
            }
        }
        if !accepts_llm_sample(self.generation, &sample) {
            self.report.stale_rejected += 1;
            return;
        }
        assert!(
            self.active[index],
            "an accepted sample was attributed to server {index}, which is not in the current set"
        );
        let shared = &self.mocks[index].shared;
        assert_no_cross_mixing(&sample, &self.keys[index], &shared.model, shared.n_ctx);
        let stats = &sample.stats;
        if stats.slots_available {
            assert_eq!(
                stats.slot_count, shared.slots,
                "server {index} reported a foreign slot count (cross-server slot mixing)"
            );
        }
        if let Some(prompt) = stats.prompt_total {
            let low = band(index) as f64;
            assert!(
                (low..low + 1e9).contains(&prompt),
                "server {index} reported a counter ({prompt}) outside its own band"
            );
        }
        for (label, value) in [
            ("prompt_tps", stats.prompt_tps),
            ("generation_tps", stats.generation_tps),
        ] {
            if let Some(value) = value {
                assert!(
                    value.is_finite() && (0.0..MAX_PLAUSIBLE_RATE).contains(&value),
                    "server {index}: a baseline change produced an implausible {label}={value}"
                );
            }
        }
        self.last_sample[index] = now;
        self.report.accepted += 1;
    }

    fn periodic(&mut self, now: Instant) {
        for (index, slot) in self.removals.iter_mut().enumerate() {
            if let Some(watch) = slot {
                if watch.snapshot.is_none() {
                    let settled = match watch.first_seen {
                        Some(seen) => now.saturating_duration_since(seen) >= REMOVAL_SETTLE,
                        None => {
                            now.saturating_duration_since(watch.removed_at)
                                >= REMOVAL_SETTLE_FALLBACK
                        }
                    };
                    if settled {
                        watch.snapshot = Some(self.mocks[index].requests());
                    }
                }
            }
        }
        for index in 0..self.mocks.len() {
            if !self.active[index] {
                continue;
            }
            let gap = now.saturating_duration_since(self.last_sample[index]);
            self.report.max_gap_ms = self.report.max_gap_ms.max(gap.as_millis());
            assert!(
                gap < STALL_BOUND,
                "server {index} delivered no accepted sample for {gap:?} (stall)"
            );
        }
    }
}

fn run_soak(params: &SoakParams) -> SoakReport {
    let n = params.servers;
    assert!((1..=MAX_MONITORED_SERVERS).contains(&n));
    let threads_before = self_status_field("Threads:").unwrap_or(0);

    let mut mocks: Vec<SoakMock> = (0..n).map(SoakMock::start).collect();
    let keys: Vec<String> = mocks
        .iter()
        .map(|mock| crate::domain::server_key(&mock.base()))
        .collect();
    let unique: HashSet<&String> = keys.iter().collect();
    assert_eq!(unique.len(), n, "every mock needs its own server key");

    let specs: Vec<ServerSpec> = (0..n)
        .map(|i| test_spec(&keys[i], &mocks[i].base()))
        .collect();
    let (stop, samples, cmds, worker) = start_soak_worker(specs, 400);

    let start = Instant::now();
    let mut driver = Driver {
        mocks: &mut mocks,
        keys,
        active: vec![true; n],
        identity_round: vec![0; n],
        activity: (0..n).map(|_| Activity::default()).collect(),
        removals: (0..n).map(|_| None).collect(),
        last_sample: vec![start; n],
        restart_at: vec![None; n],
        generation: 0,
        last_stream_generation: 0,
        cmds,
        report: SoakReport {
            servers: n,
            threads_before,
            ..SoakReport::default()
        },
    };
    for activity in &mut driver.activity {
        activity.activate(start);
    }

    let mut rss: Vec<u64> = Vec::new();
    let mut next_probe = start;
    let mut phase: u64 = 0;
    while start.elapsed() < params.duration {
        let now = Instant::now();
        for index in 0..n {
            let scenario = SCENARIOS[(phase as usize + index) % SCENARIOS.len()];
            driver.apply_scenario(index, scenario, phase, now);
        }
        if phase % 3 == 2 {
            let target = (phase as usize / 3) % n;
            driver.set_change_burst(target, now);
        }
        let phase_end = now + params.phase;
        let come_up_at = now + params.phase / 2;
        loop {
            let now = Instant::now();
            if now >= phase_end {
                break;
            }
            if now >= come_up_at {
                for index in 0..n {
                    if driver.restart_at[index].take().is_some() {
                        driver.mocks[index].come_up();
                    }
                }
            }
            if let Ok(sample) = samples.recv_timeout(Duration::from_millis(20)) {
                driver.check_sample(sample, Instant::now());
            }
            let now = Instant::now();
            driver.periodic(now);
            if now >= next_probe {
                next_probe = now + Duration::from_millis(200);
                if let Some(threads) = self_status_field("Threads:") {
                    driver.report.threads_peak = driver.report.threads_peak.max(threads);
                }
                if params.measure_resources {
                    if let Some(kb) = self_status_field("VmRSS:") {
                        rss.push(kb);
                    }
                }
            }
        }
        phase += 1;
    }
    driver.report.phases = phase;

    // Recovery: everything healthy, every removed server back in the set; each
    // server must then report connected with its own model under the current
    // generation (no permanent stall, no lost server).
    let now = Instant::now();
    for index in 0..n {
        driver.apply_scenario(index, Scenario::Healthy, phase, now);
    }
    if driver.active.iter().any(|active| !active) {
        // Let pending removal snapshots settle so the re-add check runs.
        let settle_end = Instant::now() + Duration::from_millis(600);
        while Instant::now() < settle_end {
            if let Ok(sample) = samples.recv_timeout(Duration::from_millis(20)) {
                driver.check_sample(sample, Instant::now());
            }
            driver.periodic(Instant::now());
        }
        let now = Instant::now();
        for index in 0..n {
            if !driver.active[index] {
                driver.readd(index, now);
            }
        }
        let set = driver.current_set();
        driver.send_set(set);
    }
    let mut recovered = vec![false; n];
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && recovered.iter().any(|ok| !ok) {
        if let Ok(sample) = samples.recv_timeout(Duration::from_millis(20)) {
            let index = driver.keys.iter().position(|key| *key == sample.server);
            let ok = sample.generation == driver.generation
                && sample.stats.connected
                && !sample.stats.reconnecting;
            let model_ok =
                index.is_some_and(|i| sample.stats.model == driver.mocks[i].shared.model);
            driver.check_sample(sample, Instant::now());
            if let (Some(index), true, true) = (index, ok, model_ok) {
                recovered[index] = true;
            }
        }
        driver.periodic(Instant::now());
    }
    let missing: Vec<usize> = (0..n).filter(|&i| !recovered[i]).collect();
    assert!(
        missing.is_empty(),
        "servers {missing:?} never recovered to connected with their own model"
    );

    // Request accounting: never more than the poll floor allows, never a
    // request outside the llama.cpp telemetry endpoints.
    let now = Instant::now();
    for (index, mock) in driver.mocks.iter().enumerate() {
        let requests = mock.requests();
        let bound = driver.activity[index].request_bound(now);
        assert!(requests >= 1, "server {index} was never polled");
        assert!(
            requests <= bound,
            "server {index} received {requests} requests, more than the poll floor allows ({bound})"
        );
        assert_eq!(
            mock.unexpected_requests(),
            0,
            "server {index} received requests outside /props, /metrics and /slots"
        );
    }

    // Shutdown during in-flight requests: hang every other server, wait until
    // a request is genuinely held open, then stop.
    for index in (0..n).step_by(2) {
        driver.mocks[index].set_behavior(Behavior::Hang);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && driver.mocks.iter().all(|mock| mock.in_flight() == 0) {
        while samples.try_recv().is_ok() {}
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        driver.mocks.iter().any(|mock| mock.in_flight() > 0),
        "no request was in flight when shutdown began"
    );
    let stop_at = Instant::now();
    stop.store(true, Ordering::SeqCst);
    match join_or_leak(worker, SHUTDOWN_BOUND) {
        Some(Ok(())) => {}
        Some(Err(_)) => panic!("the LLM worker panicked during the soak"),
        None => panic!("the LLM worker did not stop within {SHUTDOWN_BOUND:?} (hung shutdown)"),
    }
    driver.report.shutdown_ms = stop_at.elapsed().as_millis();
    let after_join: Vec<u64> = driver.mocks.iter().map(SoakMock::requests).collect();
    thread::sleep(Duration::from_millis(500));
    let later: Vec<u64> = driver.mocks.iter().map(SoakMock::requests).collect();
    assert_eq!(
        after_join, later,
        "requests were sent after the worker was joined"
    );

    driver.report.requests_total = later.iter().sum();
    driver.report.elapsed_ms = start.elapsed().as_millis();
    let mut report = std::mem::take(&mut driver.report);
    drop(driver);
    drop(samples);
    drop(mocks);

    // Resources (single-threaded long runs only).
    let threads_settle = Instant::now() + Duration::from_secs(2);
    let mut threads_after = self_status_field("Threads:").unwrap_or(0);
    while threads_after > threads_before && Instant::now() < threads_settle {
        thread::sleep(Duration::from_millis(50));
        threads_after = self_status_field("Threads:").unwrap_or(0);
    }
    report.threads_after = threads_after;
    report.rss_samples = rss.len();
    const WARMUP: usize = 30;
    const WINDOW: usize = 50;
    if rss.len() >= WARMUP + 2 * WINDOW {
        let measured = &rss[WARMUP..];
        report.rss_first_median_kb = median(&mut measured[..WINDOW].to_vec());
        report.rss_last_median_kb = median(&mut measured[measured.len() - WINDOW..].to_vec());
    }
    report.emit();

    if params.measure_resources {
        assert!(
            threads_after <= threads_before,
            "threads lingered after shutdown ({threads_before} before, {threads_after} after)"
        );
        if report.rss_first_median_kb > 0 {
            let first = report.rss_first_median_kb;
            let growth = report.rss_last_median_kb.saturating_sub(first);
            let allowed = (8 * 1024).max(first / 10);
            assert!(
                growth <= allowed,
                "RSS grew by {growth} KiB (first-window median {first} KiB, allowed {allowed} KiB)"
            );
        }
    }
    report
}

/// Serialises the long runs so their `/proc/self` measurements do not overlap.
static SOAK_LOCK: Mutex<()> = Mutex::new(());

fn long_soak(servers: usize) {
    let _guard = SOAK_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let secs = std::env::var("ORSIK_SOAK_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(60)
        .max(5);
    let measure_resources = cfg!(target_os = "linux") && single_threaded_run();
    if !measure_resources {
        println!(
            "ORSKTOP_SOAK|note=resource bounds reported, not asserted (run with --test-threads=1)"
        );
    }
    let report = run_soak(&SoakParams {
        servers,
        duration: Duration::from_secs(secs),
        phase: Duration::from_millis(1_500),
        measure_resources,
    });
    assert!(report.accepted > 0);
    assert!(
        report.removal_checks > 0,
        "the long run must verify at least one removed server stays unpolled"
    );
}

#[test]
#[ignore = "long soak (ORSIK_SOAK_SECS, default 60 s); run with --ignored --test-threads=1"]
fn soak_long_1_server() {
    long_soak(1);
}

#[test]
#[ignore = "long soak (ORSIK_SOAK_SECS, default 60 s); run with --ignored --test-threads=1"]
fn soak_long_4_servers() {
    long_soak(4);
}

#[test]
#[ignore = "long soak (ORSIK_SOAK_SECS, default 60 s); run with --ignored --test-threads=1"]
fn soak_long_16_servers() {
    long_soak(MAX_MONITORED_SERVERS);
}

/// Short, CI-sized pass over every scenario with four servers: each server
/// sees all eight scenarios once, two set-change bursts remove and re-add
/// servers, and the run ends with a shutdown during hung requests.
#[test]
fn soak_smoke_4_servers_all_scenarios() {
    let report = run_soak(&SoakParams {
        servers: 4,
        duration: Duration::from_millis(2_100),
        phase: Duration::from_millis(250),
        measure_resources: false,
    });
    assert!(report.phases >= SCENARIOS.len() as u64);
    assert!(report.accepted > 0);
}

/// Sixteen servers (the cap, equal to the sample channel's capacity) whose
/// receiver is never drained: the worker must keep cycling (`try_send` never
/// blocks on the full channel) and still stop within the bound while half
/// of the servers hold a request open.
#[test]
fn soak_smoke_16_servers_shutdown_with_undrained_channel() {
    let mocks: Vec<SoakMock> = (0..MAX_MONITORED_SERVERS).map(SoakMock::start).collect();
    let specs: Vec<ServerSpec> = mocks
        .iter()
        .map(|mock| test_spec(&crate::domain::server_key(&mock.base()), &mock.base()))
        .collect();
    let (stop, samples, _cmds, worker) = start_soak_worker(specs, 400);

    // Two full cycles fill the channel (16 samples) and keep going.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline
        && mocks
            .iter()
            .any(|mock| mock.shared.slots_requests.load(Ordering::SeqCst) < 3)
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        mocks
            .iter()
            .all(|mock| mock.shared.slots_requests.load(Ordering::SeqCst) >= 3),
        "the worker stalled on the full sample channel"
    );

    for mock in mocks.iter().step_by(2) {
        mock.set_behavior(Behavior::Hang);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && mocks.iter().all(|mock| mock.in_flight() == 0) {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(mocks.iter().any(|mock| mock.in_flight() > 0));

    stop.store(true, Ordering::SeqCst);
    match join_or_leak(worker, SHUTDOWN_BOUND) {
        Some(Ok(())) => {}
        Some(Err(_)) => panic!("the LLM worker panicked"),
        None => panic!("the LLM worker did not stop within {SHUTDOWN_BOUND:?}"),
    }
    let after_join: Vec<u64> = mocks.iter().map(SoakMock::requests).collect();
    thread::sleep(Duration::from_millis(300));
    let later: Vec<u64> = mocks.iter().map(SoakMock::requests).collect();
    assert_eq!(
        after_join, later,
        "requests were sent after the worker was joined"
    );

    // Exactly the channel capacity is buffered; everything else was dropped.
    let buffered = samples.try_iter().count();
    assert_eq!(buffered, MAX_MONITORED_SERVERS);
}

/// The mock itself: restart re-binds the same port, counters count, and a
/// dropped mock leaves no connection handler behind.
#[test]
fn soak_mock_restart_rebinds_the_same_port_and_counts_requests() {
    let mut mock = SoakMock::start(3);
    let addr = mock.addr;
    let fetch = |path: &str| -> Option<String> {
        let mut stream = TcpStream::connect(addr).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        write!(stream, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").ok()?;
        let mut text = String::new();
        stream.read_to_string(&mut text).ok()?;
        Some(text)
    };
    let props = fetch("/props").expect("props answered");
    assert!(props.contains("soak-model-3"), "{props}");
    let slots = fetch("/slots").expect("slots answered");
    assert_eq!(slots.matches("\"id\"").count(), 4, "{slots}");
    assert!(fetch("/metrics").expect("metrics").contains("4000000100"));

    mock.go_down();
    assert!(!mock.is_up());
    assert!(TcpStream::connect(addr).is_err(), "a down mock must refuse");
    mock.come_up();
    assert_eq!(mock.addr, addr);
    assert!(
        fetch("/slots").is_some(),
        "the restarted mock answers on the same port"
    );
    assert_eq!(mock.requests(), 4);
    assert_eq!(mock.unexpected_requests(), 0);
    let _ = fetch("/elsewhere");
    assert_eq!(mock.unexpected_requests(), 1);

    let shared = Arc::clone(&mock.shared);
    drop(mock);
    assert_eq!(shared.handlers.load(Ordering::SeqCst), 0);
}

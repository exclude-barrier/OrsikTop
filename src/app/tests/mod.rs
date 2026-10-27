//! Split of the former inline `mod tests` of `app.rs`, grouped by unit under test.
//!
//! The shared test infrastructure (mock HTTP backends, soak helpers,
//! identity constructors) lives here; theme files import it via `super`.
//! The HTTP primitives themselves (request-line reading, response writing) are
//! shared crate-wide through `crate::testutil`, so the harnesses differ only in
//! what they assert, not in how they speak HTTP.
use super::{
    accepts_llm_sample, apply_fast_snapshot, build_server_summaries, change_refresh,
    changed_identity_keys, collect_process_stats, drain_try_recv, io_wait_percent, join_bounded,
    map_server_identity, new_server_monitor, next_cycle_target, parse_cpu_times,
    per_cpu_usage_by_id, publish_selected_mapping, read_process_thread_count,
    rebuild_server_monitors, reconcile_server, refresh_process_metadata, selected_identity_of,
    spawn_llm_worker, stabilize_llm_sample, try_send_snapshot, ui, update_resolved_server,
    DashboardSnapshot, DrmSamplerState, FastSnapshot, GpuMapping, LlmCommand, LlmSample, LlmStats,
    LocalServerIdentity, ProcessCache, ProcessStats, RealSys, RedrawScheduler, ServerChange,
    ServerSpec, SystemStats, UiState, IDLE_REDRAW_INTERVAL, MAX_MONITORED_SERVERS,
    MAX_POLL_INTERVAL, MAX_REFRESH_MS, MIN_LLM_POLL_MS, MIN_REFRESH_MS,
};
pub(crate) use crate::testutil::{read_request_path, write_response};

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod process_cache;
mod server_reconcile;
mod soak;
mod soak_scenarios;
mod workers;

fn ident(pid: u32, start_time: u64) -> LocalServerIdentity {
    LocalServerIdentity { pid, start_time }
}

fn target(
    endpoint: &str,
    identity: Option<LocalServerIdentity>,
    auto: bool,
) -> crate::ResolvedServer {
    crate::ResolvedServer {
        endpoint: endpoint.to_string(),
        identity,
        auto,
    }
}

fn test_spec(key: &str, endpoint: &str) -> ServerSpec {
    ServerSpec {
        key: key.to_string(),
        endpoint: endpoint.to_string(),
        identity: None,
    }
}

// --- Real-worker integration test for delayed results -------------------
//
// Unlike the pure `accepts_llm_sample` tests above, these drive the actual
// `spawn_llm_worker` against isolated local mock HTTP servers: the worker
// performs real `/props` + `/metrics` + `/slots` requests through reqwest,
// the command channel is the production one, and the results pass through
// the production acceptance helper. Synchronization is explicit (channels
// and a condition variable); the timeouts only bound failure, they are
// never the success condition.

fn mock_body(
    path: &str,
    model: &str,
    n_ctx: u64,
    prompt_tokens: u64,
    predicted: u64,
) -> (String, &'static str) {
    if path.contains("/props") {
        (
                format!(
                    "{{\"model_path\":\"{model}\",\"total_slots\":1,\"default_generation_settings\":{{\"n_ctx\":{n_ctx}}}}}"
                ),
                "application/json",
            )
    } else if path.contains("/slots") {
        (
            format!("[{{\"id\":0,\"n_ctx\":{n_ctx},\"is_processing\":false}}]"),
            "application/json",
        )
    } else {
        (
                format!(
                    "llamacpp:prompt_tokens_total {prompt_tokens}\nllamacpp:tokens_predicted_total {predicted}\n"
                ),
                "text/plain",
            )
    }
}

/// A plain mock server that answers every request immediately.
fn spawn_plain_server(
    model: &'static str,
    n_ctx: u64,
    prompt_tokens: u64,
    predicted: u64,
) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    if let Some(path) = read_request_path(&mut stream) {
                        let (body, content_type) =
                            mock_body(&path, model, n_ctx, prompt_tokens, predicted);
                        write_response(&mut stream, 200, content_type, &body);
                    }
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
    (addr, stop, handle)
}

/// A mock server that signals when the first non-`/props` request arrives
/// and then holds that response until `release` is flipped.
fn spawn_gated_server(
    release: Arc<(Mutex<bool>, Condvar)>,
    started: mpsc::Sender<()>,
) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let mut signaled = false;
        while !stop_flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let Some(path) = read_request_path(&mut stream) else {
                        continue;
                    };
                    if path.contains("/props") {
                        let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                        write_response(&mut stream, 200, content_type, &body);
                        continue;
                    }
                    if !signaled {
                        let _ = started.send(());
                        signaled = true;
                    }
                    // Hold the response until the test releases it.
                    let (lock, condvar) = &*release;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = condvar.wait(released).unwrap();
                    }
                    let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                    write_response(&mut stream, 200, content_type, &body);
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
    (addr, stop, handle)
}

/// A mock server whose `/metrics` reports 501 (metrics disabled) while
/// `/props` and `/slots` answer normally.
fn spawn_metrics_off_server() -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let Some(path) = read_request_path(&mut stream) else {
                        continue;
                    };
                    if path.contains("/metrics") {
                        let body = "metrics disabled";
                        let response = format!(
                                "HTTP/1.1 501 Not Implemented\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    } else {
                        let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                        write_response(&mut stream, 200, content_type, &body);
                    }
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
    (addr, stop, handle)
}

/// Write a non-2xx HTTP response with a short body.
/// A mock whose `/metrics` returns 501 on request `fail_on` and 200 with
/// increasing counters otherwise; `/slots` is always 501 so the metric
/// path (not the slot path) supplies the live rate.
fn spawn_flaky_metrics_server(
    fail_on: usize,
) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let metrics_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handle = thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let Some(path) = read_request_path(&mut stream) else {
                        continue;
                    };
                    if path.contains("/metrics") {
                        let index = metrics_requests.fetch_add(1, Ordering::Relaxed) + 1;
                        if index == fail_on {
                            write_response(&mut stream, 501, "text/plain", "disabled");
                        } else {
                            let prompt = 1000 * index as u64;
                            let predicted = 500 * index as u64;
                            let body = format!(
                                    "llamacpp:prompt_tokens_total {prompt}\nllamacpp:tokens_predicted_total {predicted}\n"
                                );
                            write_response(&mut stream, 200, "text/plain", &body);
                        }
                    } else if path.contains("/slots") {
                        write_response(&mut stream, 501, "text/plain", "no slots");
                    } else {
                        let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                        write_response(&mut stream, 200, content_type, &body);
                    }
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
    (addr, stop, handle)
}

/// A mock whose `/metrics` returns 200 with an empty body while `/slots`
/// answers normally.
fn spawn_empty_metrics_server() -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let Some(path) = read_request_path(&mut stream) else {
                        continue;
                    };
                    if path.contains("/metrics") {
                        write_response(&mut stream, 200, "text/plain", "");
                    } else {
                        let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                        write_response(&mut stream, 200, content_type, &body);
                    }
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
    (addr, stop, handle)
}

/// A mock whose `/metrics` returns 200 with the given body; `/slots` is 501.
fn spawn_body_metrics_server(
    body: &'static str,
) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("addr");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let Some(path) = read_request_path(&mut stream) else {
                        continue;
                    };
                    if path.contains("/metrics") {
                        write_response(&mut stream, 200, "text/plain", body);
                    } else if path.contains("/props") {
                        let (body, content_type) = mock_body(&path, "model-A", 4096, 1000, 500);
                        write_response(&mut stream, 200, content_type, &body);
                    } else {
                        write_response(&mut stream, 501, "text/plain", "no slots");
                    }
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
    (addr, stop, handle)
}

fn serve_healthy(
    stream: &mut TcpStream,
    path: &str,
    model: &str,
    n_ctx: u64,
    counter_base: u64,
    index: u64,
) {
    if path.contains("/props") {
        let body = format!(
                "{{\"model_path\":\"{model}\",\"total_slots\":1,\"default_generation_settings\":{{\"n_ctx\":{n_ctx}}}}}"
            );
        write_response(stream, 200, "application/json", &body);
    } else if path.contains("/slots") {
        let body = format!("[{{\"id\":0,\"n_ctx\":{n_ctx},\"is_processing\":false}}]");
        write_response(stream, 200, "application/json", &body);
    } else {
        let prompt = counter_base + index * 100;
        let predicted = counter_base + index * 50;
        let body = format!(
            "llamacpp:prompt_tokens_total {prompt}\nllamacpp:tokens_predicted_total {predicted}\n"
        );
        write_response(stream, 200, "text/plain", &body);
    }
}

fn serve_bad_json(stream: &mut TcpStream, path: &str) {
    if path.contains("/metrics") {
        // Not a recognised metrics sample -> metrics unavailable.
        write_response(stream, 200, "text/plain", "this is not a metrics body");
    } else {
        // Malformed JSON for /props and /slots.
        write_response(stream, 200, "application/json", "{ this is not json");
    }
}

fn programmable_mock(model: String, n_ctx: u64, counter_base: u64) -> ProgrammableMock {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind programmable mock");
    let addr = listener.local_addr().expect("addr");
    let mode = Arc::new(Mutex::new(MockMode::Healthy));
    let requests = Arc::new(AtomicU64::new(0));
    let hang_active = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let mode_flag = Arc::clone(&mode);
    let requests_flag = Arc::clone(&requests);
    let hang_flag = Arc::clone(&hang_active);
    let handle = thread::spawn(move || {
        while !stop_flag.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => break,
            };
            // The stop wake-up connects without a request; break before the
            // read timeout would stall the join.
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }
            let Some(path) = read_request_path(&mut stream) else {
                continue;
            };
            let current = *mode_flag.lock().expect("mock mode lock");
            match current {
                MockMode::Disconnect => drop(stream),
                MockMode::Hang => {
                    // Hold the connection open for as long as the mode stays
                    // `Hang` and `stop` is unset, so only the *client's*
                    // request timeout can end the request. Waking on `stop`
                    // and re-checking the mode keep teardown and recovery
                    // bounded; `HANG_SAFETY_LIMIT` is a last-resort cap for a
                    // mis-wired test, never the normal exit. No response is
                    // ever written.
                    hang_flag.fetch_add(1, Ordering::SeqCst);
                    let safety = Instant::now() + HANG_SAFETY_LIMIT;
                    while !stop_flag.load(Ordering::SeqCst)
                        && Instant::now() < safety
                        && *mode_flag.lock().expect("mock mode lock") == MockMode::Hang
                    {
                        thread::sleep(Duration::from_millis(25));
                    }
                    hang_flag.fetch_sub(1, Ordering::SeqCst);
                    drop(stream);
                }
                mode => {
                    let index = requests_flag.fetch_add(1, Ordering::SeqCst) + 1;
                    match mode {
                        MockMode::Slow => {
                            thread::sleep(SLOW_RESPONSE);
                            serve_healthy(&mut stream, &path, &model, n_ctx, counter_base, index);
                        }
                        MockMode::BadJson => serve_bad_json(&mut stream, &path),
                        MockMode::Empty => {
                            write_response(&mut stream, 200, "application/json", "");
                        }
                        MockMode::MetricsOff => {
                            if path.contains("/metrics") {
                                write_response(&mut stream, 501, "text/plain", "disabled");
                            } else {
                                serve_healthy(
                                    &mut stream,
                                    &path,
                                    &model,
                                    n_ctx,
                                    counter_base,
                                    index,
                                );
                            }
                        }
                        MockMode::Healthy => {
                            serve_healthy(&mut stream, &path, &model, n_ctx, counter_base, index);
                        }
                        // Handled above.
                        MockMode::Disconnect | MockMode::Hang => unreachable!(),
                    }
                }
            }
        }
    });
    ProgrammableMock {
        addr,
        mode,
        requests,
        hang_active,
        stop,
        handle: Some(handle),
    }
}

fn bench_emit(key: &str, value: impl std::fmt::Display) {
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let _ = writeln!(out, "ORSKTOP_BENCH|{key}={value}");
    let _ = out.flush();
}

// -----------------------------------------------------------------
// EPIC 4.1 (CFA-37): mock-based soak / failure-mode harness
// -----------------------------------------------------------------
//
// A bounded endurance harness that drives the *real* `spawn_llm_worker`
// against programmable loopback mocks. Success is signalled by observed
// samples, never by a wall-clock threshold; the deadlines only bound
// failure. Each mock carries a distinct model, so a sample that reports a
// foreign model is caught as cross-server mixing. No real inference server
// is contacted.

/// Failure modes a `ProgrammableMock` can be switched into at runtime. The
/// mode is stored as the enum (behind a `Mutex`), never as its raw
/// discriminant, so reordering this enum cannot silently mis-decode a mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MockMode {
    Healthy,
    /// Accept the request, then close without answering (connection drop).
    Disconnect,
    /// Answer correctly, but only after `SLOW_RESPONSE` — a slow server
    /// still inside the client's request timeout.
    Slow,
    /// Accept and read the request, then hold the connection open until the
    /// mode changes or `stop` is set, so the client aborts with a
    /// **timeout** error rather than a connection drop (the real "reconnect
    /// after timeout" path). `LLM_REQUEST_TIMEOUT_MS` (1200 ms,
    /// `src/llama.rs`) is the only thing that ends the request in practice;
    /// `HANG_SAFETY_LIMIT` is a last-resort cap.
    Hang,
    /// 200 with a malformed JSON body (or unrecognised metrics text).
    BadJson,
    /// 200 with an empty body.
    Empty,
    /// 501 on `/metrics` (metrics disabled), healthy elsewhere.
    MetricsOff,
}

/// Delayed-but-successful response, kept well below the client's request
/// timeout: a slow server, not a hung one.
const SLOW_RESPONSE: Duration = Duration::from_millis(150);

/// Safety cap on how long `MockMode::Hang` holds a connection. The hold is
/// released as soon as the mode changes or `stop` is set, so in practice
/// only the *client's* request timeout (`LLM_REQUEST_TIMEOUT_MS` = 1200 ms
/// in `src/llama.rs`) ends a request — a fixed self-close would let a client
/// with a longer timeout pass by seeing a connection drop instead of a real
/// timeout. This cap only keeps a mis-wired test from hanging forever.
const HANG_SAFETY_LIMIT: Duration = Duration::from_secs(30);

/// A loopback mock whose behaviour can be changed while it runs.
struct ProgrammableMock {
    addr: SocketAddr,
    mode: Arc<Mutex<MockMode>>,
    /// Monotonic request counter driving the fake `/metrics` counters.
    /// `restart_counters()` resets it to zero to model a server restart
    /// whose cumulative counters begin again at zero.
    requests: Arc<AtomicU64>,
    /// Number of requests currently held open by `MockMode::Hang`. Lets a
    /// test wait until a request is genuinely in flight before it stops the
    /// worker, instead of racing the next poll cycle.
    hang_active: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ProgrammableMock {
    fn base(&self) -> String {
        let addr = self.addr;
        format!("http://{addr}")
    }

    fn set_mode(&self, mode: MockMode) {
        *self.mode.lock().expect("mock mode lock") = mode;
    }

    /// Number of requests the mock is currently holding open in
    /// `MockMode::Hang` (0 once the mode changes or `stop` is set).
    fn hang_active(&self) -> u64 {
        self.hang_active.load(Ordering::SeqCst)
    }

    /// Reset the fake cumulative counters to zero **without** changing the
    /// endpoint, modelling a server whose counters restart while the model
    /// stays the same. A monitor that keeps polling this endpoint must then
    /// see a reset (rate unavailable), never a negative or absurd rate.
    fn restart_counters(&self) {
        self.requests.store(0, Ordering::SeqCst);
    }
}

impl Drop for ProgrammableMock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake a blocked `accept`; the loop then observes `stop` and exits.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A sample for `key` must never carry another server's model or context
/// size. `model` is empty or the explicit unknown label until `/props`
/// succeeds, so only a *known* model is checked; a known model implies
/// `/props` answered, so its `context_size` is checked alongside it (`n_ctx`
/// is a monotonic identity of the mock, unlike the model name alone).
fn assert_no_cross_mixing(sample: &LlmSample, key: &str, expected_model: &str, expected_ctx: u64) {
    if sample.server != key {
        return;
    }
    let model = sample.stats.model.as_str();
    if model.is_empty() || model == crate::llama::UNKNOWN_MODEL_LABEL {
        return;
    }
    assert_eq!(
        model, expected_model,
        "server {key} reported the foreign model {model:?} (cross-server mixing)"
    );
    assert_eq!(
        sample.stats.context_size, expected_ctx,
        "server {key} reported the foreign context size {} (cross-server mixing)",
        sample.stats.context_size
    );
}

/// Start the real worker against `specs` with the given offline grace and a
/// 250 ms poll floor; returns the handles a soak test drives.
#[allow(clippy::type_complexity)]
fn start_soak_worker(
    specs: Vec<ServerSpec>,
    offline_ms: u64,
) -> (
    Arc<AtomicBool>,
    mpsc::Receiver<LlmSample>,
    mpsc::Sender<LlmCommand>,
    thread::JoinHandle<()>,
) {
    let refresh = Arc::new(AtomicU64::new(MIN_LLM_POLL_MS));
    let offline = Arc::new(AtomicU64::new(offline_ms));
    let stop = Arc::new(AtomicBool::new(false));
    let (sample_tx, sample_rx) = mpsc::sync_channel::<LlmSample>(MAX_MONITORED_SERVERS);
    let (cmd_tx, cmd_rx) = mpsc::channel::<LlmCommand>();
    let worker = spawn_llm_worker(
        specs,
        refresh,
        offline,
        Arc::clone(&stop),
        sample_tx,
        Arc::new(AtomicU64::new(0)),
        cmd_rx,
    );
    (stop, sample_rx, cmd_tx, worker)
}

/// Join `worker` off-thread so a hang cannot wedge the test, returning
/// whether it finished within `bound`.
fn join_within(worker: thread::JoinHandle<()>, bound: Duration) -> bool {
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let joiner = thread::spawn(move || {
        let _ = worker.join();
        let _ = done_tx.send(());
    });
    let joined = done_rx.recv_timeout(bound).is_ok();
    let _ = joiner.join();
    joined
}

/// Drain samples until `key` reports connected with its own `model`, or
/// panic at the deadline (a timeout bounds failure, never success).
fn wait_for_connected(
    samples: &mpsc::Receiver<LlmSample>,
    key: &str,
    model: &str,
    deadline: Duration,
) {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if let Ok(sample) = samples.recv_timeout(Duration::from_millis(200)) {
            if sample.server == key && sample.stats.connected && sample.stats.model == model {
                return;
            }
        }
    }
    panic!("server {key} never reported connected with model {model}");
}

/// First numeric field of a `/proc/self/status` line (e.g. `VmRSS:`,
/// `Threads:`).
fn self_status_field(name: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix(name)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
    })
}

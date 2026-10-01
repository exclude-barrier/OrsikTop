use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use reqwest::{blocking::Client, StatusCode};
use serde_json::Value;

const PROPS_REFRESH: Duration = Duration::from_secs(30);
const SPEC_ACCEPTANCE_HOLD: Duration = Duration::from_secs(3);
/// How often the local llama-server process is re-scanned for its
/// spec-decoding config (CLI args + two env vars). The scan reads every
/// `/proc/<pid>/cmdline`, so it is amortized over a window instead of
/// running on every 250 ms sample. The config of a running server process
/// is static for its lifetime, so a 30 s window only costs a re-scan when
/// the server restarts (a new PID) and picks up the new config promptly.
const SPEC_REFRESH: Duration = Duration::from_secs(30);
/// Connect timeout for a single llama.cpp request. Kept well below the total
/// request timeout so a dead or unrouted endpoint fails fast instead of
/// stalling the poll cycle; the connect phase of a local server takes
/// milliseconds.
const LLM_CONNECT_TIMEOUT_MS: u64 = 750;
/// Total (connect + response) timeout for a single llama.cpp request.
const LLM_REQUEST_TIMEOUT_MS: u64 = 1200;

#[derive(Clone, Debug, Default)]
pub struct LlmStats {
    pub connected: bool,
    pub reconnecting: bool,
    pub model: String,
    pub context_size: u64,
    pub context_used: u64,
    pub context_high_watermark: u64,
    /// ID of the slot the displayed (context_used, context_size) pair comes
    /// from, per the slot's own `id` field in /slots. `None` when /slots is
    /// unavailable, empty, or the selected slot reports no `id` — never a
    /// guess. Only meaningful when `slots_available`.
    pub context_slot_id: Option<u64>,
    pub slots_available: bool,
    pub slot_count: u64,
    pub props_slot_count: u64,
    pub busy_slots: u64,
    /// Context occupancy of every slot that reports its own `/slots` `id`,
    /// from the very same /slots response that supplies the selected CTX
    /// pair. Ordered by slot ID (never array position); slots without an
    /// `id` field are omitted rather than guessed. `context_used` /
    /// `context_size` stay `None` when the slot reports no usable value —
    /// never a fake zero. Empty when /slots is unavailable.
    pub slot_overview: Vec<LlmSlotInfo>,
    pub slots_error: String,
    pub prompt_total: f64,
    pub prompt_cached_total: Option<f64>,
    pub generated_total: f64,
    pub request_prompt_tokens: u64,
    pub request_generated_tokens: u64,
    pub prompt_seconds_total: f64,
    pub generation_seconds_total: f64,
    pub prompt_tps: f64,
    pub generation_tps: f64,
    pub prompt_avg_tps: f64,
    pub generation_avg_tps: f64,
    pub active_requests: f64,
    pub deferred_requests: f64,
    pub spec_drafts_total: f64,
    pub spec_draft_tokens: f64,
    pub spec_accepted_tokens: f64,
    pub spec_enabled: bool,
    pub spec_is_mtp: bool,
    pub spec_n_max: Option<u64>,
    pub spec_acceptance_pct: Option<f64>,
    pub error: String,
}

/// One visible slot in the per-slot context overview: the slot's own
/// `/slots` `id`, its busy state, and its context usage / capacity. Missing
/// values stay `None` instead of becoming fabricated zeros.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LlmSlotInfo {
    pub id: u64,
    pub busy: bool,
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
}

#[derive(Default)]
struct PreviousMetricCounters {
    at: Option<Instant>,
    prompt_total: f64,
    generated_total: f64,
    draft_total: f64,
    accepted_total: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SlotCounter {
    /// The slot's own `id` from /slots, or `None` when the response did not
    /// provide one. Deliberately not the array position: order is not identity.
    slot_id: Option<u64>,
    task_id: Option<i64>,
    prompt_processed: u64,
    decoded: u64,
}

#[derive(Default)]
struct PreviousSlotCounters {
    at: Option<Instant>,
    slots: Vec<SlotCounter>,
}

#[derive(Default)]
struct CachedProps {
    model: String,
    context_size: u64,
    total_slots: u64,
    spec_enabled: bool,
    /// True when the server's response explicitly carried the `speculative`
    /// boolean. Then the server's enabled/disabled decision is authoritative.
    spec_enabled_known: bool,
    spec_is_mtp: bool,
    /// True when a server response explicitly named a speculative type other
    /// than `none`/empty (`draft-mtp`, `ngram`, …). A named type is
    /// authoritative; a type list that names nothing (`none`, empty or
    /// absent) leaves the field to the local process CLI.
    spec_named_type: bool,
    spec_n_max: Option<u64>,
    /// True when the server's response explicitly carried `speculative.n_max`
    /// (including an explicit `0`, which disables it).
    spec_n_max_known: bool,
    last_refresh: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MetricSample {
    pub name: String,
    pub labels: Option<String>,
    pub value: f64,
}

pub struct LlamaMonitor {
    client: Client,
    base: String,
    previous_metrics: PreviousMetricCounters,
    previous_slots: PreviousSlotCounters,
    props: CachedProps,
    last_spec_acceptance_pct: Option<f64>,
    last_spec_acceptance_at: Option<Instant>,
    /// Set when a /props fetch failed so the next sample retries it. A
    /// failure must not extend the 30 s refresh window; otherwise the first
    /// failed refresh would delay the model/context data for a full window.
    props_dirty: bool,
    /// Cached result of the local llama-server process scan (spec-decoding
    /// config from CLI args + env vars). `Some(config)` is a successful scan;
    /// `None` is a successful scan that found no local server. The scan is
    /// amortized over `SPEC_REFRESH` (tracked by `local_spec_scanned_at`)
    /// instead of running on every 250 ms sample.
    local_spec: Option<LocalSpeculativeConfig>,
    local_spec_scanned_at: Option<Instant>,
}

impl LlamaMonitor {
    pub fn new(server: &str) -> Result<Self, reqwest::Error> {
        let client = Client::builder()
            .connect_timeout(Duration::from_millis(LLM_CONNECT_TIMEOUT_MS))
            .timeout(Duration::from_millis(LLM_REQUEST_TIMEOUT_MS))
            .build()?;

        Ok(Self {
            client,
            base: server.trim_end_matches('/').to_string(),
            previous_metrics: PreviousMetricCounters::default(),
            previous_slots: PreviousSlotCounters::default(),
            props: CachedProps::default(),
            last_spec_acceptance_pct: None,
            last_spec_acceptance_at: None,
            props_dirty: true,
            local_spec: None,
            local_spec_scanned_at: None,
        })
    }

    pub fn sample(&mut self) -> LlmStats {
        let mut stats = LlmStats::default();

        // /props, /metrics and /slots are independent endpoints, so fetch them
        // concurrently: the poll cycle is bounded by the slowest single request
        // instead of the sum of the three. /props is amortized to the 30 s
        // refresh window; when it is due it runs alongside the other two.
        let fetch_props = self.props_needs_refresh();
        let (props_result, metrics_result, slots_result) = thread::scope(|scope| {
            let client = &self.client;
            let base = &self.base;
            let props_handle = if fetch_props {
                Some(scope.spawn(move || fetch_json(client, format!("{base}/props"))))
            } else {
                None
            };
            let metrics_handle = scope.spawn(move || fetch_raw(client, format!("{base}/metrics")));
            let slots_handle = scope.spawn(move || fetch_json(client, format!("{base}/slots")));

            (
                props_handle.map(|handle| handle.join().unwrap()),
                metrics_handle.join().unwrap(),
                slots_handle.join().unwrap(),
            )
        });

        if let Some(props_result) = props_result {
            self.apply_props_result(props_result);
        }
        stats.model = self.props.model.clone();
        stats.context_size = self.props.context_size;
        stats.props_slot_count = self.props.total_slots;
        stats.spec_enabled = self.props.spec_enabled;
        stats.spec_is_mtp = self.props.spec_is_mtp;
        stats.spec_n_max = self.props.spec_n_max;

        // Fill in the fields the server's own responses did not decide from
        // the local server process CLI (see `apply_local_spec_fallback`).
        if let Some(local_spec) = self.local_spec_config() {
            apply_local_spec_fallback(&self.props, &mut stats, local_spec);
        }

        let metrics_text = match metrics_result {
            MetricsOutcome::Ok(text) => text,
            MetricsOutcome::Http(status) => {
                stats.error = if status == StatusCode::NOT_IMPLEMENTED {
                    "/metrics disabled; start llama.cpp with --metrics".to_string()
                } else {
                    format!("/metrics returned HTTP {status}")
                };
                return stats;
            }
            MetricsOutcome::Body(err) => {
                stats.error = format!("metrics response error: {err}");
                return stats;
            }
            MetricsOutcome::Unreachable(err) => {
                stats.error = format!("cannot reach llama.cpp: {err}");
                return stats;
            }
        };

        stats.connected = true;
        let metrics = parse_prometheus(&metrics_text);

        // Current llama.cpp names first, older names retained as compatibility fallbacks.
        stats.prompt_total = pick_metric(
            &metrics,
            &[
                "llamacpp:prompt_tokens_total",
                "llamacpp:tokens_evaluated_total",
                "llamacpp_prompt_tokens_total",
                "prompt_tokens_total",
            ],
        );
        stats.prompt_cached_total =
            pick_metric_opt(&metrics, &["llamacpp:prompt_tokens_cached_total"]);
        stats.generated_total = pick_metric(
            &metrics,
            &[
                "llamacpp:tokens_predicted_total",
                "llamacpp_predicted_tokens_total",
                "predicted_tokens_total",
            ],
        );
        stats.prompt_seconds_total = pick_metric(&metrics, &["llamacpp:prompt_seconds_total"]);
        stats.generation_seconds_total =
            pick_metric(&metrics, &["llamacpp:tokens_predicted_seconds_total"]);

        // Prefer monotonic token/time counters. The legacy throughput gauges have had
        // upstream regressions where they report 0 while inference is active.
        stats.prompt_avg_tps = safe_ratio(stats.prompt_total, stats.prompt_seconds_total)
            .unwrap_or_else(|| {
                pick_metric(
                    &metrics,
                    &[
                        "llamacpp:prompt_tokens_seconds",
                        "llamacpp_prompt_tokens_seconds",
                    ],
                )
            });
        stats.generation_avg_tps =
            safe_ratio(stats.generated_total, stats.generation_seconds_total).unwrap_or_else(
                || {
                    pick_metric(
                        &metrics,
                        &[
                            "llamacpp:predicted_tokens_seconds",
                            "llamacpp_predicted_tokens_seconds",
                        ],
                    )
                },
            );
        stats.active_requests = pick_metric(
            &metrics,
            &[
                "llamacpp:requests_processing",
                "llamacpp_requests_processing",
                "requests_processing",
            ],
        );
        stats.deferred_requests = pick_metric(
            &metrics,
            &[
                "llamacpp:requests_deferred",
                "llamacpp_requests_deferred",
                "requests_deferred",
            ],
        );
        stats.context_high_watermark = pick_metric(
            &metrics,
            &["llamacpp:n_tokens_max", "llamacpp_n_tokens_max"],
        ) as u64;
        stats.spec_drafts_total = pick_metric(&metrics, &["llamacpp:spec_decode_num_drafts_total"]);
        stats.spec_draft_tokens =
            pick_metric(&metrics, &["llamacpp:spec_decode_num_draft_tokens_total"]);
        stats.spec_accepted_tokens = pick_metric(
            &metrics,
            &["llamacpp:spec_decode_num_accepted_tokens_total"],
        );
        if stats.spec_drafts_total > 0.0
            || stats.spec_draft_tokens > 0.0
            || stats.spec_accepted_tokens > 0.0
        {
            stats.spec_enabled = true;
        }

        let metric_live = self.update_metric_counters(&mut stats);
        match self.apply_slots_outcome(&mut stats, slots_result) {
            Some(slots) => {
                // A formally successful /slots array is not enough: if its
                // entries cannot be paired by a verified identity, the
                // aggregated /metrics counters are used instead of fabricating
                // per-slot deltas.
                let slot_live = self.update_live_slot_throughput(&mut stats, slots);
                (stats.prompt_tps, stats.generation_tps) =
                    choose_live_throughput(slot_live, metric_live);
            }
            None => {
                (stats.prompt_tps, stats.generation_tps) =
                    choose_live_throughput(None, metric_live);
                self.previous_slots = PreviousSlotCounters::default();
            }
        }

        if stats.model.is_empty() {
            stats.model = "llama.cpp model".to_string();
        }

        stats
    }

    fn props_needs_refresh(&self) -> bool {
        self.props_dirty
            || self
                .props
                .last_refresh
                .map(|at| at.elapsed() >= PROPS_REFRESH)
                .unwrap_or(true)
    }

    /// Returns the cached local-server spec config, re-scanning `/proc` only
    /// when the `SPEC_REFRESH` window has elapsed (or never scanned). For
    /// non-loopback endpoints the scan is skipped entirely (returns `None`),
    /// matching `local_speculative_process_config`. The result — including
    /// "no local server found" — is cached for the window, so a dead local
    /// server does not cost a `/proc` walk on every 250 ms sample.
    fn local_spec_config(&mut self) -> Option<LocalSpeculativeConfig> {
        let due = self
            .local_spec_scanned_at
            .map(|at| at.elapsed() >= SPEC_REFRESH)
            .unwrap_or(true);
        if due {
            self.local_spec = local_speculative_process_config(&self.base);
            self.local_spec_scanned_at = Some(Instant::now());
        }
        self.local_spec
    }

    fn apply_props_result(&mut self, outcome: JsonOutcome) {
        let props = match outcome {
            JsonOutcome::Ok(props) => props,
            // A failed /props refresh must not extend the 30 s window: the
            // next sample retries while the cached props keep being shown.
            JsonOutcome::Http(..) | JsonOutcome::Unreachable(..) | JsonOutcome::Invalid(..) => {
                self.props_dirty = true;
                return;
            }
        };

        self.props_dirty = false;
        self.props.last_refresh = Some(Instant::now());

        self.props.context_size = json_u64_path(&props, &["default_generation_settings", "n_ctx"])
            .or_else(|| json_u64_path(&props, &["n_ctx"]))
            .unwrap_or(self.props.context_size);
        self.props.total_slots = props
            .get("total_slots")
            .and_then(Value::as_u64)
            .unwrap_or(self.props.total_slots);

        // Merge the nested (`default_generation_settings`) and root variants,
        // nested field taking precedence. A successful response that mentions
        // the speculative config is then authoritative: the cache is replaced,
        // not accumulated, so an explicit disable (`false`/`0`/empty) clears
        // the previous state and a changed `n_max` (3 → 8) is adopted. A
        // response with no speculative information at all leaves the cache
        // untouched, so older servers do not erase what was already known.
        let nested = props
            .get("default_generation_settings")
            .map(speculative_config);
        let root = speculative_config(&props);
        // The nested variant wins per field; root fills what it omits. The
        // type is taken from whichever source actually carried a `types`
        // string, so a root `speculative.types` is not silently mixed with a
        // nested variant that describes the type differently.
        let type_source = nested.filter(|spec| spec.types_known);
        let merged = SpeculativeConfig {
            present: nested.is_some_and(|spec| spec.present) || root.present,
            enabled: nested.and_then(|spec| spec.enabled).or(root.enabled),
            is_mtp: type_source.map_or(root.is_mtp, |spec| spec.is_mtp),
            n_max: nested.and_then(|spec| spec.n_max).or(root.n_max),
            types_known: type_source.is_some() || root.types_known,
            named_type: type_source.map_or(root.named_type, |spec| spec.named_type),
        };
        if merged.present {
            let n_max = merged.n_max.filter(|value| *value > 0);
            // A named MTP type implies speculative decoding is enabled even
            // when the server did not send the `speculative` boolean.
            self.props.spec_enabled = merged.enabled.unwrap_or(n_max.is_some() || merged.is_mtp);
            self.props.spec_enabled_known = merged.enabled.is_some();
            self.props.spec_is_mtp = merged.is_mtp;
            self.props.spec_named_type = merged.named_type;
            self.props.spec_n_max_known = merged.n_max.is_some();
            self.props.spec_n_max = n_max;
        }

        let model = json_string(&props, &["model_name", "model_alias", "model_path"])
            .map(|value| model_display_name(&value));
        if let Some(model) = model.filter(|name| !name.is_empty()) {
            self.props.model = model;
        }
    }

    fn update_metric_counters(&mut self, stats: &mut LlmStats) -> (f64, f64) {
        let now = Instant::now();
        let mut live = (0.0, 0.0);

        if let Some(previous_at) = self.previous_metrics.at {
            let seconds = now.saturating_duration_since(previous_at).as_secs_f64();
            if seconds > 0.0 {
                live.0 =
                    counter_delta(stats.prompt_total, self.previous_metrics.prompt_total) / seconds;
                live.1 =
                    counter_delta(stats.generated_total, self.previous_metrics.generated_total)
                        / seconds;
            }

            let draft_delta =
                counter_delta(stats.spec_draft_tokens, self.previous_metrics.draft_total);
            if draft_delta > 0.0 {
                let accepted_delta = counter_delta(
                    stats.spec_accepted_tokens,
                    self.previous_metrics.accepted_total,
                );
                let acceptance = (accepted_delta / draft_delta * 100.0).clamp(0.0, 100.0);
                self.last_spec_acceptance_pct = Some(acceptance);
                self.last_spec_acceptance_at = Some(now);
                stats.spec_acceptance_pct = Some(acceptance);
            } else if self
                .last_spec_acceptance_at
                .is_some_and(|at| now.saturating_duration_since(at) <= SPEC_ACCEPTANCE_HOLD)
            {
                stats.spec_acceptance_pct = self.last_spec_acceptance_pct;
            }
        }

        self.previous_metrics.at = Some(now);
        self.previous_metrics.prompt_total = stats.prompt_total;
        self.previous_metrics.generated_total = stats.generated_total;
        self.previous_metrics.draft_total = stats.spec_draft_tokens;
        self.previous_metrics.accepted_total = stats.spec_accepted_tokens;
        live
    }

    /// Update the slot-derived live throughput. Returns the values on success,
    /// or `None` when the slots could not be paired safely; the caller then
    /// uses the aggregated `/metrics` counters.
    fn update_live_slot_throughput(
        &mut self,
        stats: &mut LlmStats,
        slots: Vec<SlotCounter>,
    ) -> Option<(f64, f64)> {
        let now = Instant::now();
        let mut live = None;

        if let Some(previous_at) = self.previous_slots.at {
            let seconds = now.saturating_duration_since(previous_at).as_secs_f64();
            if let Some((prompt_tps, generation_tps)) =
                slot_delta_tps(&self.previous_slots.slots, &slots, seconds)
            {
                stats.prompt_tps = prompt_tps;
                stats.generation_tps = generation_tps;
                live = Some((prompt_tps, generation_tps));
            }
        }

        self.previous_slots.at = Some(now);
        self.previous_slots.slots = slots;
        live
    }

    fn apply_slots_outcome(
        &self,
        stats: &mut LlmStats,
        outcome: JsonOutcome,
    ) -> Option<Vec<SlotCounter>> {
        let value = match outcome {
            JsonOutcome::Ok(value) => value,
            JsonOutcome::Unreachable(err) => {
                stats.slots_error = format!("cannot reach /slots: {err}");
                return None;
            }
            JsonOutcome::Http(status) => {
                stats.slots_error = format!("/slots returned HTTP {status}");
                return None;
            }
            JsonOutcome::Invalid(err) => {
                stats.slots_error = format!("invalid /slots response: {err}");
                return None;
            }
        };

        match apply_slots_json(stats, &value) {
            Ok(slots) => Some(slots),
            Err(err) => {
                stats.slots_error = err;
                None
            }
        }
    }
}

/// The outcome of a /metrics fetch: the raw body on success, the non-success
/// status, or the transport/body error that made the endpoint unreachable or
/// unreadable.
enum MetricsOutcome {
    Ok(String),
    Http(StatusCode),
    Body(String),
    Unreachable(String),
}

/// The outcome of a JSON endpoint fetch (/props, /slots): the parsed value on
/// success, the non-success status, a body-read/parse error, or the transport
/// error that made the endpoint unreachable.
enum JsonOutcome {
    Ok(Value),
    Http(StatusCode),
    Invalid(String),
    Unreachable(String),
}

/// Fetches a raw text endpoint (currently only /metrics).
fn fetch_raw(client: &Client, url: String) -> MetricsOutcome {
    let response = match client.get(url).send() {
        Ok(response) => response,
        // Structured redaction: strip the request URL (which may carry
        // credentials/query secrets) before it becomes a telemetry string.
        Err(err) => return MetricsOutcome::Unreachable(crate::redact::describe_reqwest_error(err)),
    };
    let status = response.status();
    if !status.is_success() {
        return MetricsOutcome::Http(status);
    }
    match response.text() {
        Ok(body) => MetricsOutcome::Ok(body),
        Err(err) => MetricsOutcome::Body(crate::redact::describe_reqwest_error(err)),
    }
}

/// Fetches a JSON endpoint (/props, /slots).
fn fetch_json(client: &Client, url: String) -> JsonOutcome {
    let response = match client.get(url).send() {
        Ok(response) => response,
        Err(err) => return JsonOutcome::Unreachable(crate::redact::describe_reqwest_error(err)),
    };
    let status = response.status();
    if !status.is_success() {
        return JsonOutcome::Http(status);
    }
    match response.json::<Value>() {
        Ok(value) => JsonOutcome::Ok(value),
        Err(err) => JsonOutcome::Invalid(crate::redact::describe_reqwest_error(err)),
    }
}

fn apply_slots_json(stats: &mut LlmStats, value: &Value) -> Result<Vec<SlotCounter>, String> {
    let slots = value
        .as_array()
        .ok_or_else(|| "/slots response is not an array".to_string())?;

    stats.slots_available = true;
    stats.slots_error.clear();
    stats.slot_count = slots.len() as u64;

    // The context pair (used, size) and the slot ID it belongs to are always
    // read from a single slot: the most-used busy slot, or the most-used slot
    // overall when no slot is busy. Taking max(used) and max(n_ctx)
    // independently would mix values from different slots once slot
    // capacities differ.
    let mut best_busy: Option<(u64, u64, Option<u64>)> = None;
    let mut best_any: Option<(u64, u64, Option<u64>)> = None;
    let mut busy = 0u64;
    let mut request_prompt_tokens = 0u64;
    let mut request_generated_tokens = 0u64;
    let mut counters = Vec::with_capacity(slots.len());
    let mut overview = Vec::with_capacity(slots.len());

    for slot in slots.iter() {
        let n_ctx = slot.get("n_ctx").and_then(Value::as_u64).unwrap_or(0);
        // The slot's own identity from /slots. Absent in older responses —
        // kept as None rather than guessed.
        let slot_id = slot.get("id").and_then(Value::as_u64);

        let is_processing = slot
            .get("is_processing")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_processing {
            busy += 1;
        }

        let slot_spec = speculative_config(slot);
        if slot_spec.enabled.unwrap_or(false) || slot_spec.n_max.is_some_and(|value| value > 0) {
            stats.spec_enabled = true;
        }
        stats.spec_is_mtp |= slot_spec.is_mtp;
        if let Some(n_max) = slot_spec.n_max.filter(|value| *value > 0) {
            stats.spec_n_max = Some(stats.spec_n_max.map_or(n_max, |current| current.max(n_max)));
        }

        let prompt_tokens = slot.get("n_prompt_tokens").and_then(Value::as_u64);
        let prompt_processed = slot
            .get("n_prompt_tokens_processed")
            .and_then(Value::as_u64)
            .or(prompt_tokens);
        let decoded = slot_decoded_tokens_opt(slot);

        if is_processing {
            request_prompt_tokens =
                request_prompt_tokens.saturating_add(prompt_processed.unwrap_or(0));
            request_generated_tokens =
                request_generated_tokens.saturating_add(decoded.unwrap_or(0));
        }

        // Current llama.cpp exposes n_prompt_tokens as the slot's current prompt/context
        // token count. Older responses may only expose processed prompt + decoded
        // tokens. A present-but-zero n_prompt_tokens still falls back to the
        // retained occupancy (validated CTX selection semantics). When the slot
        // reports none of them, the occupancy stays unknown (None) so the
        // per-slot overview does not print a fake zero.
        let used_opt = match prompt_tokens {
            Some(value) if value > 0 => Some(value),
            _ => match (prompt_processed, decoded) {
                (Some(processed), decoded) => Some(processed.saturating_add(decoded.unwrap_or(0))),
                (None, Some(decoded)) => Some(decoded),
                (None, None) => None,
            },
        };
        let used = used_opt.unwrap_or(0);
        if is_processing && best_busy.is_none_or(|best| used > best.0) {
            best_busy = Some((used, n_ctx, slot_id));
        }
        if best_any.is_none_or(|best| used > best.0) {
            best_any = Some((used, n_ctx, slot_id));
        }

        counters.push(SlotCounter {
            // Never substitute the array position: a slot without a reported
            // id has no trustworthy identity (see `slot_delta_tps`).
            slot_id,
            task_id: slot.get("id_task").and_then(Value::as_i64),
            prompt_processed: prompt_processed.unwrap_or(0),
            decoded: decoded.unwrap_or(0),
        });
        if let Some(id) = slot_id {
            overview.push(LlmSlotInfo {
                id,
                busy: is_processing,
                context_used: used_opt,
                context_size: (n_ctx > 0).then_some(n_ctx),
            });
        }
    }

    overview.sort_by_key(|slot| slot.id);

    stats.busy_slots = busy;
    stats.request_prompt_tokens = request_prompt_tokens;
    stats.request_generated_tokens = request_generated_tokens;
    let (context_used, context_size, context_slot_id) =
        best_busy
            .or(best_any)
            .unwrap_or((0, stats.context_size, None));
    stats.context_used = context_used;
    stats.context_slot_id = context_slot_id;
    stats.slot_overview = overview;
    // A slot without n_ctx reports 0; keep the /props-seeded (or previous)
    // size instead of shrinking the pair to 0.
    stats.context_size = if context_size > 0 {
        context_size
    } else {
        stats.context_size
    };
    Ok(counters)
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct SpeculativeConfig {
    /// True when the payload actually mentions the speculative config, so a
    /// successful response can be treated as authoritative. An explicit
    /// `null` counts as present (it clears the cached state); a payload with
    /// no speculative keys at all does not.
    present: bool,
    enabled: Option<bool>,
    is_mtp: bool,
    n_max: Option<u64>,
    /// True when the payload carried a `speculative.types` string.
    types_known: bool,
    /// True when that string names a real type (`draft-mtp`, `ngram`, …),
    /// as opposed to being absent, empty or just `none`.
    named_type: bool,
}

fn speculative_config(value: &Value) -> SpeculativeConfig {
    let params = value.get("params");
    let enabled_value = value.get("speculative");
    let n_max_value = params
        .and_then(|params| params.get("speculative.n_max"))
        .or_else(|| value.get("speculative.n_max"));
    let types_value = params
        .and_then(|params| params.get("speculative.types"))
        .or_else(|| value.get("speculative.types"));
    let enabled = enabled_value.and_then(Value::as_bool);
    let n_max = n_max_value.and_then(Value::as_u64);
    let types = types_value.and_then(Value::as_str).unwrap_or_default();
    let named_type = types.split(',').any(|kind| {
        let kind = kind.trim();
        !kind.is_empty() && kind != "none"
    });
    SpeculativeConfig {
        present: enabled_value.is_some() || n_max_value.is_some() || types_value.is_some(),
        enabled,
        is_mtp: types.split(',').any(|kind| kind.trim() == "draft-mtp"),
        n_max,
        types_known: types_value.is_some(),
        named_type,
    }
}

/// Fill the speculative fields the server did not decide from the local
/// server process CLI.
///
/// Precedence (unambiguous):
/// * a server `speculative` boolean is authoritative for enabled/disabled;
/// * a server type list decides the type when it names a real type
///   (`draft-mtp` → MTP; any other named type → not MTP);
/// * a server `speculative.n_max` (including an explicit `0`) is authoritative;
/// * anything the server did not state is filled from the CLI, so a server
///   whose only type report is `speculative.types: "none"` still shows the MTP
///   the process was actually launched with;
/// * a server that explicitly says `speculative: false` is never overridden
///   to MTP by the CLI.
fn apply_local_spec_fallback(
    props: &CachedProps,
    stats: &mut LlmStats,
    local: LocalSpeculativeConfig,
) {
    // The server disabled speculation either explicitly (`speculative: false`)
    // or by reporting `speculative.n_max: 0` (known, but filtered to `None`).
    let n_max_zero = props.spec_n_max_known && props.spec_n_max.is_none();
    let server_disabled = (props.spec_enabled_known && !props.spec_enabled) || n_max_zero;
    if !props.spec_enabled_known && !server_disabled {
        stats.spec_enabled |= local.enabled;
    }
    if !props.spec_named_type && !server_disabled {
        stats.spec_is_mtp |= local.is_mtp;
    }
    if !props.spec_n_max_known && !server_disabled && stats.spec_n_max.is_none() {
        stats.spec_n_max = local.n_max;
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct LocalSpeculativeConfig {
    enabled: bool,
    is_mtp: bool,
    n_max: Option<u64>,
}

fn local_speculative_process_config(base: &str) -> Option<LocalSpeculativeConfig> {
    let url = reqwest::Url::parse(base).ok()?;
    let host = url.host_str()?;
    if !matches!(host, "127.0.0.1" | "localhost" | "::1") {
        return None;
    }
    let target_port = url.port_or_known_default()?;

    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let pid = entry.file_name();
        if !pid.to_string_lossy().chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        let cmdline = match fs::read(entry.path().join("cmdline")) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let args: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        if args.is_empty() || !is_llama_server_process(&args) {
            continue;
        }
        if !command_targets_port(&args, target_port) {
            continue;
        }
        let env = fs::read(entry.path().join("environ")).ok();
        return Some(speculative_from_process(&args, env.as_deref()));
    }
    None
}

fn is_llama_server_process(args: &[String]) -> bool {
    let Some(executable) = args.first() else {
        return false;
    };
    let executable = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(executable);

    executable.contains("llama-server")
        || (executable == "llama" && args.get(1).is_some_and(|arg| arg == "serve"))
}

fn command_targets_port(args: &[String], target_port: u16) -> bool {
    let mut saw_port = false;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--port" {
            saw_port = true;
            if args.get(index + 1).and_then(|v| v.parse::<u16>().ok()) == Some(target_port) {
                return true;
            }
            index += 2;
            continue;
        }
        if let Some(value) = args[index].strip_prefix("--port=") {
            saw_port = true;
            if value.parse::<u16>().ok() == Some(target_port) {
                return true;
            }
        }
        index += 1;
    }
    !saw_port && target_port == 8080
}

fn speculative_from_process(args: &[String], env_bytes: Option<&[u8]>) -> LocalSpeculativeConfig {
    let spec_type = cli_value(args, "--spec-type").unwrap_or_default();
    let is_mtp = spec_type.split(',').any(|kind| kind.trim() == "draft-mtp");
    let n_max = cli_value(args, "--spec-draft-n-max")
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| {
            env_value(env_bytes, "LLAMA_ARG_SPEC_DRAFT_N_MAX").and_then(|value| value.parse().ok())
        });
    let env_type = env_value(env_bytes, "LLAMA_ARG_SPEC_TYPE").unwrap_or_default();
    let env_mtp = env_type.split(',').any(|kind| kind.trim() == "draft-mtp");
    LocalSpeculativeConfig {
        enabled: is_mtp || env_mtp || n_max.is_some(),
        is_mtp: is_mtp || env_mtp,
        n_max,
    }
}

fn cli_value(args: &[String], flag: &str) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        if arg == flag {
            return args.get(index + 1).cloned();
        }
        if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
            return Some(value.to_string());
        }
    }
    None
}

fn env_value(env_bytes: Option<&[u8]>, key: &str) -> Option<String> {
    env_bytes?
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix(&format!("{key}=")).map(str::to_string))
}

/// Per-slot delta throughput, or `None` when the slots cannot be paired
/// safely.
///
/// Array position is never used as identity. Every current slot must carry a
/// unique `id` that also exists exactly once in the previous sample, and the
/// task must not have switched. If any of that fails — missing ids, duplicate
/// ids, a new/disappeared slot, a task change, or the first sample — the whole
/// slot-delta path abstains and the caller falls back to the aggregated
/// `/metrics` counters, so reordered or changed slots cannot fabricate
/// activity.
fn slot_delta_tps(
    previous: &[SlotCounter],
    current: &[SlotCounter],
    seconds: f64,
) -> Option<(f64, f64)> {
    if !seconds.is_finite() || seconds <= 0.0 || current.is_empty() {
        return None;
    }

    let mut prompt_delta = 0u64;
    let mut decoded_delta = 0u64;

    for current_slot in current {
        let slot_id = current_slot.slot_id?;
        if current
            .iter()
            .filter(|slot| slot.slot_id == Some(slot_id))
            .count()
            != 1
        {
            return None;
        }
        if previous
            .iter()
            .filter(|slot| slot.slot_id == Some(slot_id))
            .count()
            != 1
        {
            return None;
        }
        let previous_slot = previous.iter().find(|slot| slot.slot_id == Some(slot_id))?;
        if !task_ids_match(previous_slot.task_id, current_slot.task_id) {
            return None;
        }

        prompt_delta = prompt_delta.saturating_add(
            current_slot
                .prompt_processed
                .saturating_sub(previous_slot.prompt_processed),
        );
        decoded_delta = decoded_delta
            .saturating_add(current_slot.decoded.saturating_sub(previous_slot.decoded));
    }

    Some((
        prompt_delta as f64 / seconds,
        decoded_delta as f64 / seconds,
    ))
}

/// Prefer trustworthy per-slot deltas; otherwise fall back to the aggregated
/// `/metrics` counters. Never a fabricated per-slot value.
fn choose_live_throughput(slot_live: Option<(f64, f64)>, metric_live: (f64, f64)) -> (f64, f64) {
    slot_live.unwrap_or(metric_live)
}

fn task_ids_match(previous: Option<i64>, current: Option<i64>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => previous == current,
        // Both absent: no task id is available (older responses), so slot-id
        // pairing is the best available identity. A one-sided id is a
        // possible task switch and must not be paired.
        (None, None) => true,
        _ => false,
    }
}

fn slot_decoded_tokens_opt(slot: &Value) -> Option<u64> {
    match slot.get("next_token") {
        Some(Value::Array(items)) => items
            .first()
            .and_then(|item| item.get("n_decoded"))
            .and_then(Value::as_u64),
        Some(Value::Object(map)) => map.get("n_decoded").and_then(Value::as_u64),
        _ => None,
    }
}

pub(crate) fn parse_prometheus(input: &str) -> Vec<MetricSample> {
    let mut metrics = Vec::new();

    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut parts = line.split_whitespace();
        let Some(name_with_labels) = parts.next() else {
            continue;
        };
        let Some(raw_value) = parts.next() else {
            continue;
        };
        let Ok(value) = raw_value.parse::<f64>() else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }

        let (name, labels) = match name_with_labels.split_once('{') {
            Some((name, raw_labels)) => {
                let labels = raw_labels.trim_end_matches('}');
                let labels = (!labels.is_empty()).then(|| labels.to_string());
                (name, labels)
            }
            None => (name_with_labels, None),
        };

        metrics.push(MetricSample {
            name: name.to_string(),
            labels,
            value,
        });
    }

    metrics
}

fn pick_metric_opt(metrics: &[MetricSample], names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        metrics
            .iter()
            .find(|sample| sample.name == **name && sample.labels.is_none())
            .map(|sample| sample.value)
    })
}

fn pick_metric(metrics: &[MetricSample], names: &[&str]) -> f64 {
    pick_metric_opt(metrics, names).unwrap_or(0.0)
}

fn safe_ratio(value: f64, seconds: f64) -> Option<f64> {
    (value.is_finite() && seconds.is_finite() && seconds > 0.0).then(|| value / seconds)
}

fn counter_delta(current: f64, previous: f64) -> f64 {
    if current.is_finite() && previous.is_finite() && current >= previous {
        current - previous
    } else {
        0.0
    }
}

fn json_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn json_u64_path(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_u64()
}

fn model_display_name(value: &str) -> String {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .trim_end_matches(".gguf")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn props_failure_retries_next_sample() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        // A failed /props fetch must not extend the 30 s refresh window, so
        // the next sample retries while the cached props keep being shown.
        assert!(monitor.props_needs_refresh());
        monitor.apply_props_result(JsonOutcome::Unreachable("connect error".into()));
        assert!(monitor.props_dirty);
        assert!(monitor.props_needs_refresh());

        // A successful fetch clears the dirty flag and populates the cache.
        let props = json!({
            "model_name": "/models/Qwen3-4B-Q4_K_M.gguf",
            "total_slots": 2,
            "default_generation_settings": { "n_ctx": 4096 }
        });
        monitor.apply_props_result(JsonOutcome::Ok(props));
        assert!(!monitor.props_dirty);
        assert_eq!(monitor.props.model, "Qwen3-4B-Q4_K_M");
        assert_eq!(monitor.props.context_size, 4096);
        assert_eq!(monitor.props.total_slots, 2);
    }

    #[test]
    fn speculative_type_list_distinguishes_none_from_a_named_type() {
        let none = speculative_config(&json!({"speculative.types": "none"}));
        assert!(none.types_known);
        assert!(!none.named_type, "\"none\" names no type");
        assert!(!none.is_mtp);

        let mtp = speculative_config(&json!({"speculative.types": "draft-mtp"}));
        assert!(mtp.named_type);
        assert!(mtp.is_mtp);

        let other = speculative_config(&json!({"speculative.types": "ngram"}));
        assert!(other.named_type);
        assert!(!other.is_mtp);

        let absent = speculative_config(&json!({"n_ctx": 4096}));
        assert!(!absent.types_known);
        assert!(!absent.named_type);
    }

    #[test]
    fn local_mtp_is_shown_when_the_server_only_reports_types_none() {
        // Live shape on cf-desktop: /props carries only
        // `speculative.types: "none"`, the CLI is launched with draft-mtp/8.
        // The server did not name a type, so the CLI fills it in.
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "default_generation_settings": {"params": {"speculative.types": "none"}}
        })));
        let mut stats = LlmStats {
            spec_enabled: monitor.props.spec_enabled,
            spec_is_mtp: monitor.props.spec_is_mtp,
            spec_n_max: monitor.props.spec_n_max,
            ..Default::default()
        };
        apply_local_spec_fallback(
            &monitor.props,
            &mut stats,
            LocalSpeculativeConfig {
                enabled: true,
                is_mtp: true,
                n_max: Some(8),
            },
        );
        assert!(stats.spec_enabled);
        assert!(stats.spec_is_mtp, "CLI draft-mtp must surface");
        assert_eq!(stats.spec_n_max, Some(8));
    }

    #[test]
    fn named_mtp_type_implies_enabled_from_the_server_alone() {
        // A server that names draft-mtp but omits the `speculative` boolean
        // still means speculative decoding is on.
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative.types": "draft-mtp"
        })));
        assert!(monitor.props.spec_enabled);
        assert!(monitor.props.spec_is_mtp);
    }

    #[test]
    fn nested_type_none_takes_precedence_over_root_mtp() {
        // Documented precedence: the nested default_generation_settings wins
        // per field, so a nested `types: "none"` overrides a root draft-mtp.
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative.types": "draft-mtp",
            "default_generation_settings": {"params": {"speculative.types": "none"}}
        })));
        assert!(!monitor.props.spec_is_mtp);
        assert!(!monitor.props.spec_named_type);
    }

    #[test]
    fn standalone_n_max_zero_suppresses_local_mtp() {
        // An explicit server `speculative.n_max: 0` (no `speculative` boolean)
        // disables speculation: the CLI MTP must not override it.
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "params": {"speculative.n_max": 0}
        })));
        let mut stats = LlmStats {
            spec_enabled: monitor.props.spec_enabled,
            spec_is_mtp: monitor.props.spec_is_mtp,
            spec_n_max: monitor.props.spec_n_max,
            ..Default::default()
        };
        apply_local_spec_fallback(
            &monitor.props,
            &mut stats,
            LocalSpeculativeConfig {
                enabled: true,
                is_mtp: true,
                n_max: Some(8),
            },
        );
        assert!(!stats.spec_enabled);
        assert!(!stats.spec_is_mtp);
        assert_eq!(stats.spec_n_max, None);
    }

    #[test]
    fn local_mtp_does_not_override_an_explicit_server_disable() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": false,
            "speculative.types": ""
        })));
        let mut stats = LlmStats {
            spec_enabled: monitor.props.spec_enabled,
            spec_is_mtp: monitor.props.spec_is_mtp,
            spec_n_max: monitor.props.spec_n_max,
            ..Default::default()
        };
        apply_local_spec_fallback(
            &monitor.props,
            &mut stats,
            LocalSpeculativeConfig {
                enabled: true,
                is_mtp: true,
                n_max: Some(8),
            },
        );
        assert!(!stats.spec_enabled);
        assert!(!stats.spec_is_mtp);
        assert_eq!(stats.spec_n_max, None);
    }

    #[test]
    fn local_mtp_does_not_override_a_server_named_non_mtp_type() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8081").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "speculative.types": "ngram"
        })));
        let mut stats = LlmStats {
            spec_enabled: monitor.props.spec_enabled,
            spec_is_mtp: monitor.props.spec_is_mtp,
            spec_n_max: monitor.props.spec_n_max,
            ..Default::default()
        };
        apply_local_spec_fallback(
            &monitor.props,
            &mut stats,
            LocalSpeculativeConfig {
                enabled: true,
                is_mtp: true,
                n_max: Some(8),
            },
        );
        assert!(stats.spec_enabled);
        assert!(!stats.spec_is_mtp, "server named a non-MTP type");
    }

    #[test]
    fn successful_props_response_replaces_spec_state_instead_of_accumulating() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "model_name": "model-A",
            "speculative": true,
            "speculative.n_max": 3,
            "speculative.types": "draft-mtp"
        })));
        assert!(monitor.props.spec_enabled);
        assert!(monitor.props.spec_is_mtp);
        assert_eq!(monitor.props.spec_n_max, Some(3));

        // Explicit disable on the same endpoint (new model / restarted server):
        // the previous active state must be removed, not OR-ed into.
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "model_name": "model-B",
            "speculative": false,
            "speculative.n_max": 0,
            "speculative.types": ""
        })));
        assert!(!monitor.props.spec_enabled, "stale enabled flag");
        assert!(!monitor.props.spec_is_mtp, "stale mtp flag");
        assert_eq!(monitor.props.spec_n_max, None, "stale n_max");
        assert_eq!(monitor.props.model, "model-B");
    }

    #[test]
    fn successful_props_response_adopts_a_changed_n_max() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "params": {"speculative.n_max": 3}
        })));
        assert_eq!(monitor.props.spec_n_max, Some(3));
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "params": {"speculative.n_max": 8}
        })));
        assert_eq!(monitor.props.spec_n_max, Some(8));
    }

    #[test]
    fn props_without_spec_info_keeps_the_cached_state() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "speculative.n_max": 4,
            "speculative.types": "draft-mtp"
        })));
        // An older response that says nothing about spec must not erase it...
        monitor.apply_props_result(JsonOutcome::Ok(json!({"model_name": "model-C"})));
        assert!(monitor.props.spec_enabled);
        assert!(monitor.props.spec_is_mtp);
        assert_eq!(monitor.props.spec_n_max, Some(4));

        // ...and neither must a failed refresh.
        monitor.apply_props_result(JsonOutcome::Unreachable("connect error".into()));
        assert!(monitor.props.spec_enabled);
        assert_eq!(monitor.props.spec_n_max, Some(4));
    }

    #[test]
    fn default_generation_settings_takes_precedence_over_root() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "speculative.n_max": 3,
            "default_generation_settings": {
                "speculative": false,
                "speculative.n_max": 0
            }
        })));
        assert!(!monitor.props.spec_enabled, "nested disable must win");
        assert_eq!(monitor.props.spec_n_max, None);

        // Root fields fill what the nested variant does not mention.
        monitor.apply_props_result(JsonOutcome::Ok(json!({
            "speculative": true,
            "speculative.n_max": 6,
            "default_generation_settings": {"n_ctx": 4096}
        })));
        assert!(monitor.props.spec_enabled);
        assert_eq!(monitor.props.spec_n_max, Some(6));
    }

    #[test]
    fn local_spec_config_is_cached_within_the_refresh_window() {
        // A non-loopback endpoint never triggers the /proc scan: the result
        // is None and the window starts on the first call. A second call
        // within the window returns the cached None without rescanning
        // (the timestamp is unchanged, proving no new scan ran).
        let mut monitor = LlamaMonitor::new("http://10.0.0.7:9090").unwrap();
        assert!(monitor.local_spec_config().is_none());
        let first = monitor.local_spec_scanned_at.expect("scan timestamp set");
        assert!(monitor.local_spec_config().is_none());
        assert_eq!(
            monitor.local_spec_scanned_at,
            Some(first),
            "cached value, no rescan"
        );
    }

    #[test]
    fn slots_outcome_maps_to_error_or_counters() {
        let monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        // A failed /slots fetch degrades gracefully: it sets slots_error and
        // does not touch the previous-slot counters or mark the slot set
        // unavailable.
        let mut stats = LlmStats::default();
        let slots =
            monitor.apply_slots_outcome(&mut stats, JsonOutcome::Http(StatusCode::NOT_FOUND));
        assert!(slots.is_none());
        assert!(!stats.slots_available);
        assert_eq!(stats.slots_error, "/slots returned HTTP 404 Not Found");

        let mut stats = LlmStats::default();
        let slots = monitor.apply_slots_outcome(
            &mut stats,
            JsonOutcome::Unreachable("connection refused".into()),
        );
        assert!(slots.is_none());
        assert!(!stats.slots_available);
        assert_eq!(stats.slots_error, "cannot reach /slots: connection refused");

        // A valid /slots payload populates the counters and clears any error.
        let value = json!([{ "id": 0, "task_id": 1, "state": "busy" }]);
        let mut stats = LlmStats::default();
        let slots = monitor.apply_slots_outcome(&mut stats, JsonOutcome::Ok(value));
        assert!(slots.is_some());
        assert!(stats.slots_available);
        assert_eq!(stats.slot_count, 1);
        assert!(stats.slots_error.is_empty());
    }

    #[test]
    fn parses_current_llama_metrics_fixture() {
        let metrics = parse_prometheus(include_str!("../tests/fixtures/metrics_current.prom"));

        assert_eq!(
            pick_metric(&metrics, &["llamacpp:prompt_tokens_total"]),
            12000.0
        );
        assert_eq!(
            pick_metric(&metrics, &["llamacpp:prompt_tokens_cached_total"]),
            26000.0
        );
        assert_eq!(
            pick_metric(&metrics, &["llamacpp:prompt_seconds_total"]),
            12.0
        );
        assert_eq!(
            pick_metric(&metrics, &["llamacpp:tokens_predicted_seconds_total"]),
            20.0
        );
        assert_eq!(
            safe_ratio(
                pick_metric(&metrics, &["llamacpp:prompt_tokens_total"]),
                pick_metric(&metrics, &["llamacpp:prompt_seconds_total"]),
            ),
            Some(1000.0)
        );
    }

    #[test]
    fn speculative_fixture_preserves_position_labels() {
        let metrics = parse_prometheus(include_str!("../tests/fixtures/metrics_speculative.prom"));
        assert_eq!(
            pick_metric(&metrics, &["llamacpp:spec_decode_num_draft_tokens_total"]),
            200.0
        );
        assert_eq!(
            pick_metric(
                &metrics,
                &["llamacpp:spec_decode_num_accepted_tokens_total"]
            ),
            140.0
        );
        assert_eq!(
            metrics
                .iter()
                .filter(|sample| {
                    sample.name == "llamacpp:spec_decode_num_accepted_tokens_per_pos_total"
                        && sample.labels.is_some()
                })
                .count(),
            4
        );
    }

    #[test]
    fn parser_ignores_non_finite_values_and_does_not_collapse_labels() {
        let metrics = parse_prometheus(
            "llamacpp:test{slot=\"0\"} 20\nllamacpp:test{slot=\"1\"} 30\nnot_finite NaN\n",
        );
        assert_eq!(
            metrics
                .iter()
                .filter(|metric| metric.name == "llamacpp:test")
                .count(),
            2
        );
        assert_eq!(pick_metric(&metrics, &["llamacpp:test"]), 0.0);
        assert_eq!(pick_metric_opt(&metrics, &["llamacpp:test"]), None);
        assert_eq!(pick_metric_opt(&metrics, &["missing"]), None);
        assert!(!metrics.iter().any(|metric| metric.name == "not_finite"));
    }

    #[test]
    fn derives_average_throughput_from_time_counters() {
        assert_eq!(safe_ratio(1200.0, 2.0), Some(600.0));
        assert_eq!(safe_ratio(1200.0, 0.0), None);
    }

    #[test]
    fn counter_delta_handles_counter_reset() {
        assert_eq!(counter_delta(150.0, 100.0), 50.0);
        assert_eq!(counter_delta(10.0, 100.0), 0.0);
    }

    #[test]
    fn derives_live_context_from_slots_fixture() {
        let mut stats = LlmStats {
            context_size: 4096,
            ..Default::default()
        };
        let slots: Value =
            serde_json::from_str(include_str!("../tests/fixtures/slots_active.json")).unwrap();

        let counters = apply_slots_json(&mut stats, &slots).unwrap();

        assert!(stats.slots_available);
        assert_eq!(stats.slot_count, 2);
        assert_eq!(stats.busy_slots, 1);
        assert_eq!(stats.context_size, 196608);
        assert_eq!(stats.context_used, 38779);
        assert_eq!(stats.context_slot_id, Some(0));
        assert_eq!(counters[0].prompt_processed, 12000);
        assert_eq!(counters[0].decoded, 779);
        assert_eq!(stats.request_prompt_tokens, 12000);
        assert_eq!(stats.request_generated_tokens, 779);
    }

    #[test]
    fn derives_live_throughput_from_matching_slot_task() {
        let previous = vec![SlotCounter {
            slot_id: Some(0),
            task_id: Some(42),
            prompt_processed: 1000,
            decoded: 100,
        }];
        let current = vec![SlotCounter {
            slot_id: Some(0),
            task_id: Some(42),
            prompt_processed: 1250,
            decoded: 130,
        }];

        let (prompt_tps, generation_tps) = slot_delta_tps(&previous, &current, 0.5).unwrap();
        assert_eq!(prompt_tps, 500.0);
        assert_eq!(generation_tps, 60.0);
    }

    #[test]
    fn new_slot_task_does_not_create_false_live_spike() {
        let previous = vec![SlotCounter {
            slot_id: Some(0),
            task_id: Some(42),
            prompt_processed: 1000,
            decoded: 500,
        }];
        let current = vec![SlotCounter {
            slot_id: Some(0),
            task_id: Some(43),
            prompt_processed: 100,
            decoded: 5,
        }];

        // A task switch makes the pair unverifiable: abstain (the caller falls
        // back to the aggregated counters) rather than emit a fake spike.
        assert_eq!(slot_delta_tps(&previous, &current, 0.5), None);
    }

    #[test]
    fn idle_slots_fixture_reports_no_busy_slots() {
        let mut stats = LlmStats::default();
        let slots: Value =
            serde_json::from_str(include_str!("../tests/fixtures/slots_idle.json")).unwrap();
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.slot_count, 1);
        assert_eq!(stats.busy_slots, 0);
        assert_eq!(stats.request_prompt_tokens, 0);
        assert_eq!(stats.request_generated_tokens, 0);
        assert_eq!(stats.context_used, 0);
        assert_eq!(stats.context_slot_id, Some(0));
    }

    #[test]
    fn props_fixture_exposes_context_slots_and_model() {
        let props: Value =
            serde_json::from_str(include_str!("../tests/fixtures/props.json")).unwrap();
        assert_eq!(
            json_u64_path(&props, &["default_generation_settings", "n_ctx"]),
            Some(196608)
        );
        assert_eq!(props.get("total_slots").and_then(Value::as_u64), Some(2));
        assert_eq!(
            json_string(&props, &["model_path"])
                .map(|value| model_display_name(&value))
                .as_deref(),
            Some("Qwen3.8-27B-UD-Q4_K_M")
        );
    }

    #[test]
    fn slot_fallback_uses_processed_plus_decoded() {
        let mut stats = LlmStats::default();
        let slots = json!([{
            "n_ctx": 8192,
            "n_prompt_tokens_processed": 4000,
            "next_token": [{"n_decoded": 250}]
        }]);

        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.context_used, 4250);
        // The slot reports no `id`; the display identity stays unknown.
        assert_eq!(stats.context_slot_id, None);
    }

    #[test]
    fn context_pair_stays_with_most_used_busy_slot_when_capacities_differ() {
        // The pair must come from one slot: the busy slot's own used AND the
        // busy slot's own n_ctx — never max(used) from one slot mixed with
        // max(n_ctx) from another.
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "id_task": 1, "n_ctx": 100000, "is_processing": true, "n_prompt_tokens": 5000 },
            { "id": 1, "id_task": 2, "n_ctx": 200000, "is_processing": false, "n_prompt_tokens": 9000 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.busy_slots, 1);
        assert_eq!(stats.context_used, 5000);
        assert_eq!(stats.context_size, 100000);
        assert_eq!(stats.context_slot_id, Some(0));
    }

    #[test]
    fn context_pair_falls_back_to_most_used_slot_when_all_idle() {
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "n_ctx": 100000, "is_processing": false, "n_prompt_tokens": 5000 },
            { "id": 1, "n_ctx": 200000, "is_processing": false, "n_prompt_tokens": 9000 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.busy_slots, 0);
        assert_eq!(stats.context_used, 9000);
        assert_eq!(stats.context_size, 200000);
        assert_eq!(stats.context_slot_id, Some(1));
    }

    #[test]
    fn both_busy_slots_select_the_most_used_pair() {
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "n_ctx": 100000, "is_processing": true, "n_prompt_tokens": 2000 },
            { "id": 1, "n_ctx": 150000, "is_processing": true, "n_prompt_tokens": 4000 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.busy_slots, 2);
        assert_eq!(stats.context_used, 4000);
        assert_eq!(stats.context_size, 150000);
        assert_eq!(stats.request_prompt_tokens, 6000);
        assert_eq!(stats.context_slot_id, Some(1));
    }

    #[test]
    fn idle_slot_near_capacity_reports_its_own_pair() {
        // Real observed case: n_ctx_slot = 70144, n_tokens = 70143, and a
        // request of 70210 tokens was rejected as exceeding the slot context.
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "n_ctx": 70144, "is_processing": false, "n_prompt_tokens": 70143 },
            { "id": 1, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 100 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.context_used, 70143);
        assert_eq!(stats.context_size, 70144);
        assert_eq!(stats.context_slot_id, Some(0));
    }

    #[test]
    fn slot_reuse_tracks_new_task_slot_over_stale_retained_context() {
        // Slot reuse: a busy slot runs a new small-prompt task while a second
        // slot still holds its previous task's retained context. The pair
        // follows the active slot, not the stale occupancy.
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "id_task": 99, "n_ctx": 115200, "is_processing": true, "n_prompt_tokens": 1022 },
            { "id": 1, "id_task": 42, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 37943 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.context_used, 1022);
        assert_eq!(stats.context_size, 115200);
        assert_eq!(stats.context_slot_id, Some(0));
    }

    #[test]
    fn context_slot_id_is_the_slot_api_id_not_the_array_position() {
        // Non-contiguous /slots ids: the reported identity must be the
        // slot's own `id` field, never its position in the array.
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 3, "n_ctx": 100000, "is_processing": false, "n_prompt_tokens": 9000 },
            { "id": 7, "n_ctx": 150000, "is_processing": true, "n_prompt_tokens": 2000 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.context_used, 2000);
        assert_eq!(stats.context_size, 150000);
        assert_eq!(stats.context_slot_id, Some(7));
    }

    #[test]
    fn missing_slot_id_leaves_context_slot_id_unknown() {
        // A /slots response without per-slot `id` fields (older llama.cpp)
        // must not fabricate an identity for the displayed pair.
        let mut stats = LlmStats::default();
        let slots = json!([
            { "n_ctx": 100000, "is_processing": true, "n_prompt_tokens": 5000 },
            { "n_ctx": 200000, "is_processing": false, "n_prompt_tokens": 9000 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.context_used, 5000);
        assert_eq!(stats.context_size, 100000);
        assert_eq!(stats.context_slot_id, None);
    }

    #[test]
    fn single_slot_reports_its_id_for_the_pair() {
        let mut stats = LlmStats::default();
        let slots =
            json!([{ "id": 0, "n_ctx": 115200, "is_processing": true, "n_prompt_tokens": 28851 }]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.slot_count, 1);
        assert_eq!(stats.context_used, 28851);
        assert_eq!(stats.context_size, 115200);
        assert_eq!(stats.context_slot_id, Some(0));
    }

    #[test]
    fn empty_slots_array_keeps_props_context_size() {
        let mut stats = LlmStats {
            context_size: 4096,
            ..Default::default()
        };
        apply_slots_json(&mut stats, &json!([])).unwrap();
        assert_eq!(stats.slot_count, 0);
        assert_eq!(stats.context_used, 0);
        assert_eq!(stats.context_size, 4096);
        assert_eq!(stats.context_slot_id, None);
    }

    #[test]
    fn slot_missing_n_ctx_keeps_props_context_size() {
        let mut stats = LlmStats {
            context_size: 4096,
            ..Default::default()
        };
        let slots = json!([{ "id": 0, "n_prompt_tokens": 512, "is_processing": true }]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.context_used, 512);
        assert_eq!(stats.context_size, 4096);
        assert_eq!(stats.context_slot_id, Some(0));
    }

    #[test]
    fn slot_overview_reports_each_slot_with_own_id_state_and_usage() {
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "n_ctx": 115200, "is_processing": true, "n_prompt_tokens": 58745 },
            { "id": 1, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 34200 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(
            stats.slot_overview,
            vec![
                LlmSlotInfo {
                    id: 0,
                    busy: true,
                    context_used: Some(58745),
                    context_size: Some(115200),
                },
                LlmSlotInfo {
                    id: 1,
                    busy: false,
                    context_used: Some(34200),
                    context_size: Some(115200),
                }
            ]
        );
        // The main CTX pair must not be the sum of the slots.
        assert_eq!(stats.context_used, 58745);
        assert_eq!(stats.context_size, 115200);
        assert_eq!(stats.context_slot_id, Some(0));
    }

    #[test]
    fn slot_overview_marks_busy_state_of_all_slots() {
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "n_ctx": 100000, "is_processing": true, "n_prompt_tokens": 2000 },
            { "id": 1, "n_ctx": 150000, "is_processing": true, "n_prompt_tokens": 4000 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert!(stats.slot_overview.iter().all(|slot| slot.busy));

        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "n_ctx": 100000, "is_processing": false, "n_prompt_tokens": 2000 },
            { "id": 1, "n_ctx": 150000, "is_processing": false, "n_prompt_tokens": 4000 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert!(stats.slot_overview.iter().all(|slot| !slot.busy));
    }

    #[test]
    fn slot_overview_orders_by_slot_id_not_array_position() {
        // Scrambled array order + non-contiguous IDs: the overview must be
        // deterministic and follow the slot's own `id`, never its position.
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 7, "n_ctx": 200000, "is_processing": true, "n_prompt_tokens": 14000 },
            { "id": 2, "n_ctx": 150000, "is_processing": false, "n_prompt_tokens": 28000 },
            { "id": 1, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 31000 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        let ids: Vec<u64> = stats.slot_overview.iter().map(|slot| slot.id).collect();
        assert_eq!(ids, vec![1, 2, 7]);
        assert_eq!(
            stats.slot_overview[0].context_used,
            Some(31000),
            "values must travel with their slot id"
        );
    }

    #[test]
    fn slot_overview_keeps_differing_capacities_per_slot() {
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "n_ctx": 115200, "is_processing": true, "n_prompt_tokens": 58745 },
            { "id": 3, "n_ctx": 70144, "is_processing": false, "n_prompt_tokens": 34200 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.slot_overview[0].context_size, Some(115200));
        assert_eq!(stats.slot_overview[1].context_size, Some(70144));
        // The main pair still comes from a single slot.
        assert_eq!(stats.context_used, 58745);
        assert_eq!(stats.context_size, 115200);
        assert_eq!(stats.context_slot_id, Some(0));
    }

    #[test]
    fn slot_overview_keeps_missing_values_unknown_and_drops_unidentified_slots() {
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 0 },
            { "id": 1, "is_processing": false },
            { "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 5000 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        // The slot without an `id` must not be guessed from its position.
        assert_eq!(
            stats
                .slot_overview
                .iter()
                .map(|slot| slot.id)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            stats.slot_overview[0].context_used,
            Some(0),
            "a real zero is a zero"
        );
        let unknown = &stats.slot_overview[1];
        assert_eq!(
            unknown.context_used, None,
            "absent data must not become a fake zero"
        );
        assert_eq!(unknown.context_size, None);
    }

    #[test]
    fn present_zero_prompt_tokens_falls_back_to_retained_occupancy() {
        // Validated CTX selection semantics: n_prompt_tokens present-but-zero
        // falls back to the retained processed occupancy. The overview must
        // inherit the exact same value the main CTX row shows.
        let mut stats = LlmStats::default();
        let slots = json!([
            { "id": 0, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 0, "n_prompt_tokens_processed": 37943 },
            { "id": 1, "n_ctx": 115200, "is_processing": false, "n_prompt_tokens": 100 }
        ]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert_eq!(stats.context_used, 37943);
        assert_eq!(stats.context_slot_id, Some(0));
        assert_eq!(stats.slot_overview[0].context_used, Some(37943));
        assert_eq!(stats.slot_overview[1].context_used, Some(100));
    }

    #[test]
    fn slot_overview_is_empty_when_slots_endpoint_is_unavailable() {
        let stats = LlmStats::default();
        // No apply_slots_json call at all: /slots failed -> default is empty.
        assert!(!stats.slots_available);
        assert!(stats.slot_overview.is_empty());
    }

    #[test]
    fn slot_delta_tps_sums_active_slots_into_server_aggregate() {
        let previous = vec![
            SlotCounter {
                slot_id: Some(0),
                task_id: Some(1),
                prompt_processed: 1000,
                decoded: 100,
            },
            SlotCounter {
                slot_id: Some(1),
                task_id: Some(2),
                prompt_processed: 2000,
                decoded: 200,
            },
        ];
        let current = vec![
            SlotCounter {
                slot_id: Some(0),
                task_id: Some(1),
                prompt_processed: 1100,
                decoded: 150,
            },
            SlotCounter {
                slot_id: Some(1),
                task_id: Some(2),
                prompt_processed: 2200,
                decoded: 300,
            },
        ];

        let (prompt_tps, generation_tps) = slot_delta_tps(&previous, &current, 2.0).unwrap();
        assert_eq!(prompt_tps, 150.0);
        assert_eq!(generation_tps, 75.0);
    }

    #[test]
    fn slot_delta_tps_ignores_disappeared_slot_and_restarted_task() {
        let previous = vec![
            SlotCounter {
                slot_id: Some(0),
                task_id: Some(1),
                prompt_processed: 1000,
                decoded: 500,
            },
            SlotCounter {
                slot_id: Some(1),
                task_id: Some(2),
                prompt_processed: 2000,
                decoded: 300,
            },
        ];
        // Slot 0 disappeared; slot 1's task finished and a new task started
        // on the same slot — neither may leak a stale delta.
        let current = vec![SlotCounter {
            slot_id: Some(1),
            task_id: Some(3),
            prompt_processed: 50,
            decoded: 5,
        }];

        assert_eq!(slot_delta_tps(&previous, &current, 1.0), None);
    }

    #[test]
    fn slot_delta_tps_clamps_counter_decrease_to_zero() {
        let previous = vec![SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: 1000,
            decoded: 900,
        }];
        let current = vec![SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: 900,
            decoded: 100,
        }];

        assert_eq!(slot_delta_tps(&previous, &current, 1.0), Some((0.0, 0.0)));
    }

    #[test]
    fn reordered_slots_without_ids_abstain_and_fall_back_to_metrics() {
        // Two slots with no `id`: array order is not identity, so a pure
        // reordering (identical counters) must not be read as activity.
        let previous = apply_slots_json(
            &mut LlmStats::default(),
            &json!([
                {"is_processing": true, "n_prompt_tokens_processed": 100, "next_token": {"n_decoded": 10}},
                {"is_processing": true, "n_prompt_tokens_processed": 1000, "next_token": {"n_decoded": 100}}
            ]),
        )
        .unwrap();
        let current = apply_slots_json(
            &mut LlmStats::default(),
            &json!([
                {"is_processing": true, "n_prompt_tokens_processed": 1000, "next_token": {"n_decoded": 100}},
                {"is_processing": true, "n_prompt_tokens_processed": 100, "next_token": {"n_decoded": 10}}
            ]),
        )
        .unwrap();
        assert!(previous.iter().all(|slot| slot.slot_id.is_none()));

        let slot_live = slot_delta_tps(&previous, &current, 1.0);
        assert_eq!(slot_live, None, "no identity → no per-slot delta");
        // The decision must fall back to the aggregated metrics, never to the
        // fabricated 900/90 the old index-as-id behavior produced.
        assert_eq!(choose_live_throughput(slot_live, (0.0, 0.0)), (0.0, 0.0));
    }

    #[test]
    fn stable_slot_ids_keep_correct_deltas_when_reordered() {
        let previous = vec![
            SlotCounter {
                slot_id: Some(0),
                task_id: Some(1),
                prompt_processed: 1000,
                decoded: 100,
            },
            SlotCounter {
                slot_id: Some(1),
                task_id: Some(2),
                prompt_processed: 2000,
                decoded: 200,
            },
        ];
        // Same slots, reordered, each advanced by its own amount.
        let current = vec![
            SlotCounter {
                slot_id: Some(1),
                task_id: Some(2),
                prompt_processed: 2200,
                decoded: 300,
            },
            SlotCounter {
                slot_id: Some(0),
                task_id: Some(1),
                prompt_processed: 1100,
                decoded: 150,
            },
        ];
        assert_eq!(
            slot_delta_tps(&previous, &current, 2.0),
            Some((150.0, 75.0))
        );
    }

    #[test]
    fn duplicate_slot_ids_abstain() {
        let previous = vec![SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: 1000,
            decoded: 100,
        }];
        let current = vec![
            SlotCounter {
                slot_id: Some(0),
                task_id: Some(1),
                prompt_processed: 1100,
                decoded: 150,
            },
            SlotCounter {
                slot_id: Some(0),
                task_id: Some(1),
                prompt_processed: 1200,
                decoded: 160,
            },
        ];
        assert_eq!(slot_delta_tps(&previous, &current, 1.0), None);
    }

    #[test]
    fn new_slot_among_known_ones_abstains() {
        // A new slot (id present, no previous match) makes the totals
        // untrustworthy, so the whole slot-delta path abstains.
        let previous = vec![SlotCounter {
            slot_id: Some(0),
            task_id: Some(1),
            prompt_processed: 1000,
            decoded: 100,
        }];
        let current = vec![
            SlotCounter {
                slot_id: Some(0),
                task_id: Some(1),
                prompt_processed: 1100,
                decoded: 150,
            },
            SlotCounter {
                slot_id: Some(1),
                task_id: Some(2),
                prompt_processed: 5,
                decoded: 1,
            },
        ];
        assert_eq!(slot_delta_tps(&previous, &current, 1.0), None);
    }

    #[test]
    fn spec_acceptance_computes_from_deltas_and_holds_after_quiet() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        let mut stats = LlmStats {
            spec_draft_tokens: 200.0,
            spec_accepted_tokens: 140.0,
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);

        // Second sample: 100 new draft tokens, 60 accepted -> 60%.
        let mut stats = LlmStats {
            spec_draft_tokens: 300.0,
            spec_accepted_tokens: 200.0,
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, Some(60.0));

        // Third sample with no new draft tokens: the last value is held for
        // SPEC_ACCEPTANCE_HOLD instead of dropping to None.
        let mut stats = LlmStats {
            spec_draft_tokens: 300.0,
            spec_accepted_tokens: 200.0,
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, Some(60.0));
    }

    #[test]
    fn spec_acceptance_expires_after_the_hold_window() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        let mut stats = LlmStats {
            spec_draft_tokens: 100.0,
            spec_accepted_tokens: 50.0,
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        let mut stats = LlmStats {
            spec_draft_tokens: 200.0,
            spec_accepted_tokens: 100.0,
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, Some(50.0));

        let mut stats = LlmStats {
            spec_draft_tokens: 200.0,
            spec_accepted_tokens: 100.0,
            ..Default::default()
        };
        let expired = Instant::now() - (SPEC_ACCEPTANCE_HOLD + Duration::from_secs(1));
        monitor.last_spec_acceptance_at = Some(expired);
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, None);
    }

    #[test]
    fn spec_acceptance_clamps_to_hundred_percent() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        let mut stats = LlmStats {
            spec_draft_tokens: 100.0,
            spec_accepted_tokens: 100.0,
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);

        // Accepted delta larger than the draft delta must clamp, not exceed.
        let mut stats = LlmStats {
            spec_draft_tokens: 200.0,
            spec_accepted_tokens: 250.0,
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, Some(100.0));
    }

    #[test]
    fn spec_counter_reset_does_not_create_fake_acceptance() {
        let mut monitor = LlamaMonitor::new("http://127.0.0.1:8080").unwrap();

        let mut stats = LlmStats {
            spec_draft_tokens: 1000.0,
            spec_accepted_tokens: 900.0,
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);

        // Server restart: counters reset below the previous values. No draft
        // progress and no fresh hold -> no acceptance value, no NaN.
        let mut stats = LlmStats {
            spec_draft_tokens: 10.0,
            spec_accepted_tokens: 5.0,
            ..Default::default()
        };
        monitor.update_metric_counters(&mut stats);
        assert_eq!(stats.spec_acceptance_pct, None);
    }

    #[test]
    fn rejects_non_array_slots_payload() {
        let mut stats = LlmStats::default();
        let error = apply_slots_json(&mut stats, &json!({"error": "disabled"})).unwrap_err();
        assert!(!stats.slots_available);
        assert!(error.contains("not an array"));
    }

    #[test]
    fn model_path_is_shortened_for_display() {
        assert_eq!(
            model_display_name("/models/Qwen3.8-27B-UD-Q4_K_M.gguf"),
            "Qwen3.8-27B-UD-Q4_K_M"
        );
    }
}

#[cfg(test)]
mod speculative_status_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_speculative_status_and_n_max_from_slot_shape() {
        let slot = json!({
            "speculative": true,
            "params": {"speculative.n_max": 3}
        });
        assert_eq!(
            speculative_config(&slot),
            SpeculativeConfig {
                present: true,
                enabled: Some(true),
                is_mtp: false,
                n_max: Some(3),
                types_known: false,
                named_type: false,
            }
        );
    }

    #[test]
    fn slot_telemetry_marks_speculative_enabled() {
        let mut stats = LlmStats::default();
        let slots = json!([{
            "id": 0,
            "id_task": 11,
            "n_ctx": 4096,
            "speculative": true,
            "is_processing": true,
            "params": {"speculative.n_max": 3},
            "n_prompt_tokens": 128,
            "n_prompt_tokens_processed": 128,
            "next_token": {"n_decoded": 9}
        }]);
        apply_slots_json(&mut stats, &slots).unwrap();
        assert!(stats.spec_enabled);
        assert_eq!(stats.spec_n_max, Some(3));
    }
}

#[cfg(test)]
mod local_speculative_process_tests {
    use super::*;

    #[test]
    fn reads_draft_mtp_type_from_custom_slot_shape() {
        let value = serde_json::json!({
            "speculative": true,
            "params": {"speculative.types": "none,draft-mtp"}
        });
        let spec = speculative_config(&value);
        assert_eq!(spec.enabled, Some(true));
        assert!(spec.is_mtp);
        assert_eq!(spec.n_max, None);
    }

    #[test]
    fn recognizes_llama_serve_subcommand_and_reads_n_max() {
        let args = vec![
            "/home/christopherf/.local/bin/llama".to_string(),
            "serve".to_string(),
            "-hf".to_string(),
            "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M".to_string(),
            "--port".to_string(),
            "8081".to_string(),
            "--spec-type".to_string(),
            "draft-mtp".to_string(),
            "--spec-draft-n-max".to_string(),
            "3".to_string(),
        ];

        assert!(is_llama_server_process(&args));
        assert!(command_targets_port(&args, 8081));
        let spec = speculative_from_process(&args, None);
        assert!(spec.enabled);
        assert!(spec.is_mtp);
        assert_eq!(spec.n_max, Some(3));
    }

    #[test]
    fn reads_n_max_from_cli() {
        let args = vec![
            "llama-server".to_string(),
            "--port".to_string(),
            "8081".to_string(),
            "--spec-type".to_string(),
            "draft-mtp".to_string(),
            "--spec-draft-n-max".to_string(),
            "3".to_string(),
        ];
        let spec = speculative_from_process(&args, None);
        assert!(spec.enabled);
        assert!(spec.is_mtp);
        assert_eq!(spec.n_max, Some(3));
        assert!(command_targets_port(&args, 8081));
    }

    #[test]
    fn reads_n_max_from_environment() {
        let args = vec!["llama-server".to_string(), "--port=8081".to_string()];
        let env = b"LLAMA_ARG_SPEC_TYPE=draft-mtp\0LLAMA_ARG_SPEC_DRAFT_N_MAX=3\0";
        let spec = speculative_from_process(&args, Some(env));
        assert!(spec.enabled);
        assert!(spec.is_mtp);
        assert_eq!(spec.n_max, Some(3));
    }
}

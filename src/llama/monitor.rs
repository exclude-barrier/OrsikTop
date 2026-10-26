//! Extracted from the former monolithic `llama.rs`. Behavior-preserving split.
//! `LlamaMonitor` state machine: concurrent /props, /metrics and /slots fetches,
//! counter baselines and the derived `LlmStats`.
use std::{
    thread,
    time::{Duration, Instant},
};

use reqwest::{blocking::Client, StatusCode};
use serde_json::Value;

use super::{
    apply_local_spec_fallback, apply_slots_json, average_over_seconds, choose_live_throughput,
    classify_backend, counter_delta_opt, counter_rate, endpoint_url, fetch_json, fetch_raw,
    join_or, json_string, json_u64_path, local_speculative_process_config, model_display_name,
    parse_metrics_body, pick_metric_opt, sanitize_error_text, slot_delta_tps, speculative_config,
    CachedProps, JsonOutcome, LlamaMonitor, LlmStats, LocalSpeculativeConfig, MetricsFormat,
    MetricsOutcome, ParsedMetrics, PreviousMetricCounters, PreviousSlotCounters, SlotCounter,
    SpeculativeConfig, LLM_CONNECT_TIMEOUT_MS, LLM_REQUEST_TIMEOUT_MS, PROPS_REFRESH,
    SPEC_ACCEPTANCE_HOLD, SPEC_REFRESH, UNKNOWN_MODEL_LABEL,
};

impl LlamaMonitor {
    pub fn new(server: &str) -> Result<Self, reqwest::Error> {
        // Proxy behavior is deliberately left at reqwest's default: the
        // standard HTTP_PROXY/HTTPS_PROXY/ALL_PROXY/NO_PROXY environment
        // variables are honored, with no automatic loopback bypass. That keeps
        // an explicitly configured remote endpoint usable behind a corporate
        // proxy, at the cost that a user with a proxy env var set (and no
        // matching NO_PROXY) would route even the default 127.0.0.1 endpoint
        // through it. Disabling proxies here would break the intentional remote
        // case, and adding a dedicated setting is not justified by a proven
        // requirement, so this stays a documented future configurable behavior.
        // Timeouts and redirect policy below remain fully effective either way.
        let client = Client::builder()
            .connect_timeout(Duration::from_millis(LLM_CONNECT_TIMEOUT_MS))
            .timeout(Duration::from_millis(LLM_REQUEST_TIMEOUT_MS))
            // Monitor exactly the configured endpoint: never silently follow
            // a redirect to another host/port.
            .redirect(reqwest::redirect::Policy::none())
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
            last_metrics_format: None,
            last_strata_since: None,
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
                Some(scope.spawn(move || fetch_json(client, endpoint_url(base, "props"))))
            } else {
                None
            };
            let metrics_handle =
                scope.spawn(move || fetch_raw(client, endpoint_url(base, "metrics")));
            let slots_handle = scope.spawn(move || fetch_json(client, endpoint_url(base, "slots")));

            (
                props_handle.map(|handle| {
                    join_or(
                        handle,
                        JsonOutcome::Unreachable("props fetch task panicked".to_string()),
                    )
                }),
                join_or(
                    metrics_handle,
                    MetricsOutcome::Unreachable("metrics fetch task panicked".to_string()),
                ),
                join_or(
                    slots_handle,
                    JsonOutcome::Unreachable("slots fetch task panicked".to_string()),
                ),
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

        // /metrics and /slots are independent endpoints: a metrics-disabled or
        // failing server must not hide a healthy /slots response. The metrics
        // outcome decides only whether the metric-derived fields are
        // available; /slots is applied below regardless.
        let metrics_text = match metrics_result {
            MetricsOutcome::Ok(text) => Some(text),
            MetricsOutcome::Http(status) => {
                stats.error = if status == StatusCode::NOT_IMPLEMENTED {
                    "/metrics disabled; start llama.cpp with --metrics".to_string()
                } else {
                    format!("/metrics returned HTTP {status}")
                };
                None
            }
            MetricsOutcome::Body(err) => {
                stats.error = format!("metrics response error: {err}");
                None
            }
            MetricsOutcome::Unreachable(err) => {
                stats.error = format!("cannot reach llama.cpp: {err}");
                None
            }
        };
        // A 2xx body carries metric data only when it parses to samples, or is
        // a recognised Strata JSON telemetry object whose cumulative counters
        // are normalised by the adapter. A body with no usable telemetry must
        // not be presented as a real set of zeroes.
        let parsed = metrics_text
            .as_deref()
            .map(parse_metrics_body)
            .unwrap_or_else(|| ParsedMetrics {
                samples: Vec::new(),
                format: None,
                strata_since: None,
                strata_live: None,
                strata_capacity: None,
                json: false,
            });
        let metrics = parsed.samples;
        let strata_live = parsed.strata_live;
        stats.metrics_available = match parsed.format {
            // A recognised Strata endpoint is available even when a particular
            // sample exposes no counters: the fields stay `—`, not fabricated.
            Some(MetricsFormat::Strata) => true,
            Some(MetricsFormat::Prometheus) => metrics_text.is_some() && !metrics.is_empty(),
            None => false,
        };
        if metrics_text.is_some() && !stats.metrics_available {
            stats.error = if parsed.json {
                "/metrics returned an unrecognized JSON format".to_string()
            } else {
                "/metrics returned no metrics".to_string()
            };
        }
        // Backend is identified from the parsed wire body, never assumed (a
        // reachable `/metrics` alone does not imply a backend). Computed from
        // the already-parsed sample; no extra request.
        stats.backend = classify_backend(parsed.format, &metrics);
        // Capacity fallback: if /props did not supply n_ctx, a recognised Strata
        // engine reports its configured context; use it as the CTX denominator
        // (capacity only — never occupancy). /slots n_ctx still refines it below.
        if stats.context_size == 0 {
            if let Some(capacity) = parsed.strata_capacity {
                stats.context_size = capacity;
            }
        }

        // Slot state is served by its own endpoint and stays valid (and
        // visible) when /metrics is disabled or failing.
        let slots_outcome = self.apply_slots_outcome(&mut stats, slots_result);

        // The server is reachable when either endpoint answered successfully;
        // missing metric values are unavailable, not zero.
        stats.connected = stats.metrics_available || stats.slots_available;

        // Current llama.cpp names first, older names retained as compatibility fallbacks.
        stats.prompt_total = pick_metric_opt(
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
        stats.generated_total = pick_metric_opt(
            &metrics,
            &[
                "llamacpp:tokens_predicted_total",
                "llamacpp_predicted_tokens_total",
                "predicted_tokens_total",
            ],
        );
        stats.prompt_seconds_total = pick_metric_opt(&metrics, &["llamacpp:prompt_seconds_total"]);
        stats.generation_seconds_total =
            pick_metric_opt(&metrics, &["llamacpp:tokens_predicted_seconds_total"]);

        // Prefer monotonic token/time counters. The legacy throughput gauges have
        // had upstream regressions where they report 0 while inference is active,
        // so they are only a fallback when the counters are absent.
        stats.prompt_avg_tps = average_over_seconds(stats.prompt_total, stats.prompt_seconds_total)
            .or_else(|| {
                pick_metric_opt(
                    &metrics,
                    &[
                        "llamacpp:prompt_tokens_seconds",
                        "llamacpp_prompt_tokens_seconds",
                    ],
                )
            });
        stats.generation_avg_tps =
            average_over_seconds(stats.generated_total, stats.generation_seconds_total).or_else(
                || {
                    pick_metric_opt(
                        &metrics,
                        &[
                            "llamacpp:predicted_tokens_seconds",
                            "llamacpp_predicted_tokens_seconds",
                        ],
                    )
                },
            );
        stats.active_requests = pick_metric_opt(
            &metrics,
            &[
                "llamacpp:requests_processing",
                "llamacpp_requests_processing",
                "requests_processing",
            ],
        );
        stats.deferred_requests = pick_metric_opt(
            &metrics,
            &[
                "llamacpp:requests_deferred",
                "llamacpp_requests_deferred",
                "requests_deferred",
            ],
        );
        stats.context_high_watermark = pick_metric_opt(
            &metrics,
            &["llamacpp:n_tokens_max", "llamacpp_n_tokens_max"],
        )
        .map(|value| value as u64);
        stats.spec_drafts_total =
            pick_metric_opt(&metrics, &["llamacpp:spec_decode_num_drafts_total"]);
        stats.spec_draft_tokens =
            pick_metric_opt(&metrics, &["llamacpp:spec_decode_num_draft_tokens_total"]);
        stats.spec_accepted_tokens = pick_metric_opt(
            &metrics,
            &["llamacpp:spec_decode_num_accepted_tokens_total"],
        );
        if stats.spec_drafts_total.is_some_and(|value| value > 0.0)
            || stats.spec_draft_tokens.is_some_and(|value| value > 0.0)
            || stats.spec_accepted_tokens.is_some_and(|value| value > 0.0)
        {
            stats.spec_enabled = true;
        }

        // A wire-format switch (llama.cpp ↔ Strata) or a Strata counter reset
        // (`totals.since` change) makes the stored baseline incomparable, so
        // drop it instead of diffing across implementations or a restart.
        let format_changed =
            self.last_metrics_format.is_some() && self.last_metrics_format != parsed.format;
        let since_changed = parsed.format == Some(MetricsFormat::Strata)
            && parsed.strata_since.is_some()
            && self.last_strata_since.is_some()
            && parsed.strata_since != self.last_strata_since;
        if format_changed || since_changed {
            self.previous_metrics = PreviousMetricCounters::default();
        }
        self.last_metrics_format = parsed.format;
        if parsed.format == Some(MetricsFormat::Strata) {
            self.last_strata_since = parsed.strata_since;
        }

        let metric_live = if stats.metrics_available {
            self.update_metric_counters(&mut stats)
        } else {
            // No /metrics: leave the metric-derived throughput unavailable
            // rather than deriving it from zeroed counters, and drop the
            // counter baseline so the first sample after recovery cannot
            // report a delta averaged over the whole outage.
            self.previous_metrics = PreviousMetricCounters::default();
            (None, None)
        };
        match slots_outcome {
            Some(slots) => {
                // A formally successful /slots array is not enough: if its
                // entries cannot be paired by a verified identity, the
                // aggregated /metrics counters are used instead of fabricating
                // per-slot deltas. A dimension the slot path cannot derive falls
                // back to the aggregate counter for that dimension only.
                let slot_live = self.update_live_slot_throughput(slots);
                (stats.prompt_tps, stats.generation_tps) =
                    choose_live_throughput(slot_live, metric_live);
            }
            None => {
                (stats.prompt_tps, stats.generation_tps) =
                    choose_live_throughput((None, None), metric_live);
                self.previous_slots = PreviousSlotCounters::default();
            }
        }

        // Strata's cumulative `totals` only advance at/near request completion,
        // so counter deltas are not a live rate. Use Strata's authoritative
        // windowed decode rate while it explicitly reports `state=generating`;
        // otherwise report unavailable (`—`) rather than a stale or fabricated
        // speed. Strata exposes no instantaneous prefill field (its
        // `prefill_tok_s_mean` is a per-request mean), so the live prefill rate
        // stays unavailable; the lifetime averages from `totals` are unaffected.
        if parsed.format == Some(MetricsFormat::Strata) {
            let live = strata_live.unwrap_or_default();
            stats.prompt_tps = None;
            stats.generation_tps = live.decode_tps;
            stats.strata_phase = live.phase;
            // Reuse the already-fetched /metrics for active request progress on
            // a version whose /slots carries no progress counters (Strata
            // 0.1.40). Only fills a gap; a newer /slots value is not overridden.
            if stats.request_prompt_tokens.is_none() {
                stats.request_prompt_tokens = live.request_prompt_tokens;
            }
            if stats.request_generated_tokens.is_none() {
                stats.request_generated_tokens = live.request_generated;
            }
        }

        if stats.model.is_empty() {
            stats.model = UNKNOWN_MODEL_LABEL.to_string();
        }

        // Free-text errors can carry server-influenced transport/framing detail
        // (reqwest source chain, malformed-payload diagnostics). Strip terminal
        // control characters and bound the length here, at the single boundary
        // where untrusted text enters `LlmStats`, so no consumer (TUI or
        // `orsiktop diag`) can be injected through the error string. URLs were
        // already redacted at construction.
        sanitize_error_text(&mut stats);

        stats
    }

    pub(crate) fn props_needs_refresh(&self) -> bool {
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
    pub(crate) fn local_spec_config(&mut self) -> Option<LocalSpeculativeConfig> {
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

    pub(crate) fn apply_props_result(&mut self, outcome: JsonOutcome) {
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

    pub(crate) fn update_metric_counters(
        &mut self,
        stats: &mut LlmStats,
    ) -> (Option<f64>, Option<f64>) {
        let now = Instant::now();
        let mut live = (None, None);

        if let Some(previous_at) = self.previous_metrics.at {
            let seconds = now.saturating_duration_since(previous_at).as_secs_f64();
            if seconds > 0.0 {
                live.0 = counter_rate(
                    stats.prompt_total,
                    self.previous_metrics.prompt_total,
                    seconds,
                );
                live.1 = counter_rate(
                    stats.generated_total,
                    self.previous_metrics.generated_total,
                    seconds,
                );
            }

            // Acceptance needs an identified draft delta from present counters.
            // Otherwise (absent counters, reset, or no progress) the last value
            // is held briefly, then dropped — never fabricated as 0.
            let draft_delta = match (stats.spec_draft_tokens, self.previous_metrics.draft_total) {
                (Some(now_drafts), Some(previous_drafts)) => {
                    counter_delta_opt(now_drafts, previous_drafts).filter(|delta| *delta > 0.0)
                }
                _ => None,
            };
            // Acceptance needs a *present* draft delta AND a *present* accepted
            // delta; a missing accepted counter is unknown, not "0 accepted".
            let accepted_delta = match (
                stats.spec_accepted_tokens,
                self.previous_metrics.accepted_total,
            ) {
                (Some(now_accepted), Some(previous_accepted)) => {
                    counter_delta_opt(now_accepted, previous_accepted)
                }
                _ => None,
            };
            if let (Some(draft_delta), Some(accepted_delta)) = (draft_delta, accepted_delta) {
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

        // Only a *present* sample may become the baseline: storing a fabricated
        // `None`/0 here would make the first real sample after a gap look like a
        // spike.
        self.previous_metrics.at = Some(now);
        self.previous_metrics.prompt_total = stats.prompt_total;
        self.previous_metrics.generated_total = stats.generated_total;
        self.previous_metrics.draft_total = stats.spec_draft_tokens;
        self.previous_metrics.accepted_total = stats.spec_accepted_tokens;
        live
    }

    /// Update the slot-counter baseline and return the verified per-dimension
    /// live throughput `(prompt_tps, generation_tps)`. Each element is `None`
    /// when it cannot be derived safely; the caller then uses the aggregated
    /// `/metrics` value for that dimension.
    fn update_live_slot_throughput(
        &mut self,
        slots: Vec<SlotCounter>,
    ) -> (Option<f64>, Option<f64>) {
        let now = Instant::now();
        let live = match self.previous_slots.at {
            Some(previous_at) => {
                let seconds = now.saturating_duration_since(previous_at).as_secs_f64();
                slot_delta_tps(&self.previous_slots.slots, &slots, seconds)
            }
            None => (None, None),
        };

        self.previous_slots.at = Some(now);
        self.previous_slots.slots = slots;
        live
    }

    pub(crate) fn apply_slots_outcome(
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

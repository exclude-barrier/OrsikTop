//! Extracted from the former monolithic `llama.rs`. Behavior-preserving split.
use super::*;

/// Parse a `/slots` array into per-slot counters and the displayed context
/// pair.
///
/// Context **occupancy** (`context_used`) is taken only from a slot's direct
/// `n_prompt_tokens` field, which both llama.cpp and newer Strata report as the
/// slot's context in use (Strata keeps it after a request ends; its front-end
/// meter divides it by `n_ctx`). It is never derived from the progress counters
/// (`n_prompt_tokens_processed` / decoded) — that would present request
/// processing progress as if it were KV occupancy. A present `n_prompt_tokens`
/// is authoritative, **including a real `0`**; an absent one leaves occupancy
/// unknown (`None`), never a fabricated zero. The progress counters are still
/// collected for the REQUEST row and live throughput.
pub(super) fn apply_slots_json(
    stats: &mut LlmStats,
    value: &Value,
) -> Result<Vec<SlotCounter>, String> {
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
    // Carries (n_ctx, slot id) for the size/id when no slot reports a usable
    // occupancy, so the pair still has a real capacity while the used count
    // stays unknown.
    let mut fallback_busy: Option<(u64, Option<u64>)> = None;
    let mut fallback_any: Option<(u64, Option<u64>)> = None;
    let mut busy = 0u64;
    let mut request_prompt_tokens: Option<u64> = None;
    let mut request_generated_tokens: Option<u64> = None;
    // A busy slot that lacks a counter makes that total incomplete, not a
    // smaller number: the aggregate must then be unavailable rather than a
    // silent partial sum.
    let mut request_prompt_complete = true;
    let mut request_generated_complete = true;
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
            // Only a reported count contributes; a busy slot that reports no
            // prompt/decoded count makes that total incomplete (None), not 0.
            match prompt_processed {
                Some(value) => {
                    request_prompt_tokens =
                        Some(request_prompt_tokens.unwrap_or(0).saturating_add(value));
                }
                None => request_prompt_complete = false,
            }
            match decoded {
                Some(value) => {
                    request_generated_tokens =
                        Some(request_generated_tokens.unwrap_or(0).saturating_add(value));
                }
                None => request_generated_complete = false,
            }
        }

        // `n_prompt_tokens` is the slot's direct context figure in both
        // llama.cpp and newer Strata, so it is used verbatim (a real `0`
        // included). It is deliberately NOT derived from the progress counters
        // (`n_prompt_tokens_processed` / decoded): those measure request
        // progress, not occupied KV, and must not masquerade as occupancy. A
        // slot that reports no `n_prompt_tokens` leaves occupancy unknown
        // (`None`), never a fabricated zero.
        let used_opt = prompt_tokens;
        // Only a known occupancy may win the CTX row; an unknown slot can at
        // most supply the capacity/id via `fallback_*`.
        if let Some(used) = used_opt {
            if is_processing && best_busy.is_none_or(|best| used > best.0) {
                best_busy = Some((used, n_ctx, slot_id));
            }
            if best_any.is_none_or(|best| used > best.0) {
                best_any = Some((used, n_ctx, slot_id));
            }
        }
        if is_processing && fallback_busy.is_none_or(|best| n_ctx > best.0) {
            fallback_busy = Some((n_ctx, slot_id));
        }
        if fallback_any.is_none_or(|best| n_ctx > best.0) {
            fallback_any = Some((n_ctx, slot_id));
        }

        counters.push(SlotCounter {
            // Never substitute the array position: a slot without a reported
            // id has no trustworthy identity (see `slot_delta_tps`).
            slot_id,
            task_id: slot.get("id_task").and_then(Value::as_i64),
            // Preserve absence per dimension rather than defaulting to 0.
            prompt_processed,
            decoded,
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
    // A complete sum only when every busy slot reported the counter; otherwise
    // the total is unknown (None), never a silent partial sum.
    stats.request_prompt_tokens = request_prompt_complete
        .then_some(request_prompt_tokens)
        .flatten();
    stats.request_generated_tokens = request_generated_complete
        .then_some(request_generated_tokens)
        .flatten();
    let (context_used, context_size, context_slot_id) = match best_busy.or(best_any) {
        Some((used, n_ctx, slot_id)) => (Some(used), n_ctx, slot_id),
        None => {
            let (n_ctx, slot_id) = fallback_busy.or(fallback_any).unwrap_or((0, None));
            (None, n_ctx, slot_id)
        }
    };
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
/// Verified per-slot prompt/decode deltas as `(prompt_tps, generation_tps)`.
/// Each dimension is `None` when it cannot be derived safely (a missing or
/// reset counter on any paired slot); the two dimensions are independent. Any
/// identity problem (missing/duplicate slot id, disappeared slot, task change)
/// makes both dimensions unavailable, preserving the existing safeguards.
pub(super) fn slot_delta_tps(
    previous: &[SlotCounter],
    current: &[SlotCounter],
    seconds: f64,
) -> (Option<f64>, Option<f64>) {
    let unavailable = (None, None);
    if !seconds.is_finite() || seconds <= 0.0 || current.is_empty() {
        return unavailable;
    }

    let mut prompt_delta = 0u64;
    let mut decoded_delta = 0u64;
    let mut prompt_known = true;
    let mut decoded_known = true;
    // A dimension must be *reported* by at least one paired slot to be a real
    // measurement. Otherwise an all-`None` pair (e.g. a counter-less `/slots`
    // server such as Strata) would leave `known` true and fabricate `0.0`.
    let mut prompt_reported = false;
    let mut decoded_reported = false;

    for current_slot in current {
        let Some(slot_id) = current_slot.slot_id else {
            return unavailable;
        };
        if current
            .iter()
            .filter(|slot| slot.slot_id == Some(slot_id))
            .count()
            != 1
        {
            return unavailable;
        }
        if previous
            .iter()
            .filter(|slot| slot.slot_id == Some(slot_id))
            .count()
            != 1
        {
            return unavailable;
        }
        let Some(previous_slot) = previous.iter().find(|slot| slot.slot_id == Some(slot_id)) else {
            return unavailable;
        };
        if !task_ids_match(previous_slot.task_id, current_slot.task_id) {
            return unavailable;
        }

        // A dimension is usable when both samples reported it and it did not
        // decrease (reset). A counter missing on exactly one side is a real gap
        // while the slot is active, so that dimension becomes unavailable (no
        // catch-up spike); a counter missing on BOTH sides carries no
        // information and contributes nothing (e.g. an idle slot that omits the
        // field), so it must not invalidate the dimension. The two dimensions
        // are independent.
        match (
            current_slot.prompt_processed,
            previous_slot.prompt_processed,
        ) {
            (Some(now), Some(before)) if now >= before => {
                prompt_delta = prompt_delta.saturating_add(now - before);
                prompt_reported = true;
            }
            (None, None) => {}
            _ => prompt_known = false,
        }
        match (current_slot.decoded, previous_slot.decoded) {
            (Some(now), Some(before)) if now >= before => {
                decoded_delta = decoded_delta.saturating_add(now - before);
                decoded_reported = true;
            }
            (None, None) => {}
            _ => decoded_known = false,
        }
    }

    // The loop above only verifies that every *current* slot can be paired.
    // Every *previous* slot must also still be present exactly once — without
    // this, a slot that disappeared would let the survivors' deltas be summed
    // and preferred over the aggregate, silently under-reporting server
    // activity. A previous slot without an `id` cannot be verified either, so
    // it abstains for the same reason.
    for previous_slot in previous {
        let Some(slot_id) = previous_slot.slot_id else {
            return unavailable;
        };
        if current
            .iter()
            .filter(|slot| slot.slot_id == Some(slot_id))
            .count()
            != 1
        {
            return unavailable;
        }
    }

    (
        (prompt_known && prompt_reported).then(|| prompt_delta as f64 / seconds),
        (decoded_known && decoded_reported).then(|| decoded_delta as f64 / seconds),
    )
}

/// Live throughput, preferring the verified per-slot delta and otherwise the
/// aggregate `/metrics` counter delta, per dimension. `None` means unavailable
/// — never a fabricated zero.
pub(super) fn choose_live_throughput(
    slot_live: (Option<f64>, Option<f64>),
    metric_live: (Option<f64>, Option<f64>),
) -> (Option<f64>, Option<f64>) {
    // Per dimension: the verified slot delta wins; otherwise the aggregate
    // metric; otherwise unavailable. Never a fabricated zero.
    (slot_live.0.or(metric_live.0), slot_live.1.or(metric_live.1))
}

pub(super) fn task_ids_match(previous: Option<i64>, current: Option<i64>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => previous == current,
        // Both absent: no task id is available (older responses), so slot-id
        // pairing is the best available identity. A one-sided id is a
        // possible task switch and must not be paired.
        (None, None) => true,
        _ => false,
    }
}

pub(super) fn slot_decoded_tokens_opt(slot: &Value) -> Option<u64> {
    match slot.get("next_token") {
        Some(Value::Array(items)) => items
            .first()
            .and_then(|item| item.get("n_decoded"))
            .and_then(Value::as_u64),
        Some(Value::Object(map)) => map.get("n_decoded").and_then(Value::as_u64),
        _ => None,
    }
}

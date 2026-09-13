from pathlib import Path

# app.rs: reconnecting is the short grace/held-sample state only. Once the grace
# expires the server is genuinely offline (while the worker keeps retrying).
path = Path("src/app.rs")
text = path.read_text()
old = "    stats.reconnecting = !hard_failure;\n    stats\n}\n"
if old not in text:
    raise SystemExit("offline reconnect status anchor not found")
text = text.replace(old, "    stats\n}\n", 1)
path.write_text(text)

# ui.rs: avoid shadowing UiState with the STATE-line spans, keep old percentage
# history formatting after TimedSample becomes f64, and ignore stale samples when
# calculating the dynamic throughput graph scale.
path = Path("src/ui.rs")
text = path.read_text()

old = "    let mut state = vec![\n"
if old not in text:
    raise SystemExit("LLM state-line vector anchor not found")
text = text.replace(old, "    let mut state_line = vec![\n", 1)
text = text.replace("        state.push(", "        state_line.push(")
text = text.replace("        state = vec![\n", "        state_line = vec![\n", 1)
text = text.replace("        Line::from(state),\n", "        Line::from(state_line),\n", 1)

old_latest = "    let latest = history.back().map(|sample| sample.value).unwrap_or(0);\n"
if old_latest not in text:
    raise SystemExit("history latest anchor not found")
text = text.replace(
    old_latest,
    "    let latest = history.back().map(|sample| sample.value).unwrap_or(0.0);\n",
    1,
)

old_percent = '                format!("{:>3}%", latest),\n'
if old_percent not in text:
    raise SystemExit("history percent format anchor not found")
text = text.replace(old_percent, '                format!("{latest:>3.0}%"),\n', 1)

old_max = '''fn history_max(history: &VecDeque<TimedSample>) -> f64 {
    history
        .iter()
        .map(|sample| sample.value)
        .filter(|value| value.is_finite())
        .fold(0.0, f64::max)
}
'''
new_max = '''fn history_max(history: &VecDeque<TimedSample>) -> f64 {
    let now = Instant::now();
    history
        .iter()
        .filter(|sample| now.saturating_duration_since(sample.at) <= HISTORY_WINDOW)
        .map(|sample| sample.value)
        .filter(|value| value.is_finite())
        .fold(0.0, f64::max)
}
'''
if old_max not in text:
    raise SystemExit("history max anchor not found")
text = text.replace(old_max, new_max, 1)

path.write_text(text)

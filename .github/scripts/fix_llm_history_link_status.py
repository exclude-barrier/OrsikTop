from pathlib import Path

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

path.write_text(text)

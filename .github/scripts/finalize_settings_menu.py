from pathlib import Path

path = Path('src/app.rs')
text = path.read_text()
old = "Event::Key(key) if key.kind == KeyEventKind::Repeat && !ui_state.is_help_open() => {"
new = "Event::Key(key)\n                    if key.kind == KeyEventKind::Repeat\n                        && !ui_state.is_help_open()\n                        && !ui_state.is_settings_open() =>\n                {"
if old not in text:
    raise SystemExit('repeat guard anchor not found')
path.write_text(text.replace(old, new, 1))

from pathlib import Path

path = Path("src/ui.rs")
text = path.read_text()
old = '    let help = " [q] quit  [h] help ";\n'
new = '    let help = " [q] quit  [h] help  [esc] settings ";\n'
if old not in text:
    raise SystemExit("footer help string not found")
path.write_text(text.replace(old, new, 1))

from pathlib import Path
import runpy

patch = Path('.github/scripts/fix_llm_panel_stability.py')
text = patch.read_text()
old = "insert_before = '''fn sleep_until_next_cycle(\\n'''"
new = "insert_before = '''fn sleep_until_next_cycle(started:'''"
if old not in text:
    raise SystemExit('expected sleep anchor declaration not found')
patch.write_text(text.replace(old, new, 1))
runpy.run_path(str(patch), run_name='__main__')

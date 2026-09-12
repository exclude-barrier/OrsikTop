from pathlib import Path

app = Path('src/app.rs')
text = app.read_text()
text = text.replace('''                    MouseEventKind::ScrollUp => {
                        if ui_state.process_pane_contains(mouse.column, mouse.row) {
                            ui_state.move_process_selection(-3, snapshot.system.processes.len());
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if ui_state.process_pane_contains(mouse.column, mouse.row) {
                            ui_state.move_process_selection(3, snapshot.system.processes.len());
                        }
                    }
''', '''                    MouseEventKind::ScrollUp
                        if ui_state.process_pane_contains(mouse.column, mouse.row) =>
                    {
                        ui_state.move_process_selection(-3, snapshot.system.processes.len());
                    }
                    MouseEventKind::ScrollDown
                        if ui_state.process_pane_contains(mouse.column, mouse.row) =>
                    {
                        ui_state.move_process_selection(3, snapshot.system.processes.len());
                    }
''')
app.write_text(text)

ui = Path('src/ui.rs')
text = ui.read_text()
text = text.replace("fn sorted_processes<'a>(\n    processes: &'a [ProcessStats],", "fn sorted_processes(\n    processes: &[ProcessStats],")
text = text.replace(") -> Vec<&'a ProcessStats> {", ") -> Vec<&ProcessStats> {")
ui.write_text(text)

mod app;
mod gpu;
mod llama;
mod ui;

use std::io;

use clap::Parser;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

#[derive(Parser, Debug)]
#[command(name = "orsiktop", version, about = "btop for local LLM Orks")]
struct Args {
    /// llama.cpp server base URL
    #[arg(long, env = "ORSIKTOP_SERVER", default_value = "http://127.0.0.1:8080")]
    server: String,

    /// Refresh interval in milliseconds
    #[arg(
        short = 'i',
        long = "interval-ms",
        alias = "interval",
        env = "ORSIKTOP_INTERVAL_MS",
        default_value_t = 1000
    )]
    interval_ms: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    let app_result = app::run(&mut terminal, &args.server, args.interval_ms);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    app_result
}

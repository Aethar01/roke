mod app;
mod cli;
mod config;
mod mcp;
mod model;
mod ops;
mod sync;
mod theme;
mod ui;

use app::App;
use cli::Command;
use config::AppConfig;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use model::TaskStore;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::time::{Duration, Instant};
use sync::SyncManager;

const LIVE_RELOAD_INTERVAL: Duration = Duration::from_millis(500);

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match cli::parse_args(&args) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(2);
        }
    };

    // Commands that do not need config or task file loading
    match parsed.command {
        Command::PrintConfig | Command::Version | Command::Help(_) => {
            if let Err(err) = cli::run_command(parsed, &AppConfig::default()) {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }

    let cfg = match AppConfig::load_or_default(&parsed.global.config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!(
                "Warning: could not load config {}: {err}. Using defaults.",
                parsed.global.config_path.display()
            );
            AppConfig::default()
        }
    };

    if !matches!(parsed.command, Command::Tui) {
        if let Err(err) = cli::run_command(parsed, &cfg) {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let storage_path = parsed.global.storage_path;
    let mut store = match TaskStore::load_or_init(&storage_path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!(
                "Warning: could not load {}: {err}. Starting with empty list.",
                storage_path.display()
            );
            TaskStore::new_empty()
        }
    };
    store.ensure_sync_metadata(&cfg.sync.effective_device_name());

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::with_config(store, Some(storage_path), cfg);
    let mut sync_mgr = SyncManager::new();
    if app.sync_config.auto_sync_on_start && !app.sync_config.peers.is_empty() {
        app.request_sync();
    }

    let result = run_loop(&mut terminal, &mut app, &mut sync_mgr);

    restore_terminal();
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    sync_mgr: &mut SyncManager,
) -> io::Result<()> {
    let mut last_reload_check = Instant::now();

    loop {
        if last_reload_check.elapsed() >= LIVE_RELOAD_INTERVAL {
            app.reload_if_changed();
            last_reload_check = Instant::now();
        }

        if app.sync_requested && !sync_mgr.in_flight {
            app.sync_requested = false;
            if sync_mgr.trigger(app.store.clone(), app.sync_config.clone()) {
                app.syncing = true;
            }
        }

        if let Some(outcome) = sync_mgr.poll() {
            app.syncing = false;
            app.apply_sync_outcome(outcome);
        }

        terminal.draw(|frame| ui::render(frame, app))?;

        if app.should_quit {
            break;
        }

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key);
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse(mouse);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

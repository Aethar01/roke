mod app;
mod config;
mod model;
mod sync;
mod theme;
mod ui;

use app::App;
use config::{default_config_path, AppConfig};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use model::{default_storage_path, TaskStore};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use sync::SyncManager;

struct CliOptions {
    storage_path: PathBuf,
    config_path: PathBuf,
}

fn parse_cli() -> CliOptions {
    let mut storage_path = default_storage_path();
    let mut config_path = default_config_path();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--file" || arg == "-f" {
            if let Some(path) = args.next() {
                storage_path = PathBuf::from(path);
            }
        } else if arg == "--config" || arg == "-c" {
            if let Some(path) = args.next() {
                config_path = PathBuf::from(path);
            }
        } else if arg == "--print-config" {
            println!("{}", AppConfig::default_template_toml());
            std::process::exit(0);
        } else if arg == "--version" || arg == "-V" {
            println!("roke {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        } else if arg == "--help" || arg == "-h" {
            println!("roke — Terminal Todo & Kanban board");
            println!();
            println!("Usage: roke [OPTIONS]");
            println!();
            println!("Options:");
            println!("  -f, --file <PATH>     Path to JSONL task file");
            println!("  -c, --config <PATH>   Path to TOML config file (theme + SSH sync)");
            println!("      --print-config    Print default config.toml to stdout");
            println!("  -V, --version         Print version");
            println!("  -h, --help            Show this help message");
            std::process::exit(0);
        }
    }

    CliOptions {
        storage_path,
        config_path,
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}

fn main() -> io::Result<()> {
    let opts = parse_cli();
    let cfg = match AppConfig::load_or_default(&opts.config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!(
                "Warning: could not load config {}: {err}. Using defaults.",
                opts.config_path.display()
            );
            AppConfig::default()
        }
    };

    let mut store = match TaskStore::load_or_init(&opts.storage_path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!(
                "Warning: could not load {}: {err}. Starting with empty list.",
                opts.storage_path.display()
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

    let mut app = App::with_config(store, Some(opts.storage_path), cfg);
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
    loop {
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

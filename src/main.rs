mod app;
mod cli;
mod config;
mod editor;
mod mcp;
mod model;
mod ops;
mod sync;
mod theme;
mod ui;

use app::App;
use clap::Parser;
use cli::{format_error, Cli};
use config::AppConfig;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use model::TaskStore;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use sync::SyncManager;

const LIVE_RELOAD_INTERVAL: Duration = Duration::from_millis(500);

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn copy_to_clipboard(text: &str, out: &mut impl Write) {
    // 1. Emit OSC 52 escape sequence (works locally and over SSH/tmux)
    let encoded = base64_encode(text.as_bytes());
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();

    // 2. Best-effort native clipboard fallback
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    #[cfg(not(unix))]
    let candidates: &[(&str, &[&str])] = &[];

    for (prog, args) in candidates {
        if let Ok(mut child) = Command::new(prog)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok_and(|s| s.success()) {
                break;
            }
        }
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if cli.print_config {
        println!("{}", AppConfig::default_template_toml());
        return Ok(());
    }

    let config_path = cli.config_path();
    let cfg = match AppConfig::load_or_default(&config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!(
                "Warning: could not load config {}: {err}. Using defaults.",
                config_path.display()
            );
            AppConfig::default()
        }
    };

    if let Some(cmd) = &cli.command {
        if let Err(err) = cli::run_subcommand(&cli, cmd, &cfg) {
            eprintln!("{}", format_error(&err, cli.color));
            std::process::exit(1);
        }
        return Ok(());
    }

    let storage_path = cli.storage_path();
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

fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()
}

fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
    terminal.clear()
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    sync_mgr: &mut SyncManager,
) -> io::Result<()> {
    let mut last_reload_check = Instant::now();
    let mut needs_draw = true;

    loop {
        if let Some(text) = app.pending_yank.take() {
            copy_to_clipboard(&text, terminal.backend_mut());
        }

        if let Some(target) = app.pending_editor.take() {
            if let Some((initial, title)) = app.editor_initial_content(target) {
                suspend_terminal(terminal)?;
                let edit_res =
                    editor::edit_markdown_in_editor(&initial, &title, app.editor_config.as_deref());
                resume_terminal(terminal)?;
                app.apply_editor_result(target, edit_res);
                needs_draw = true;
            }
        }

        if last_reload_check.elapsed() >= LIVE_RELOAD_INTERVAL {
            if app.reload_if_changed() {
                needs_draw = true;
            }
            last_reload_check = Instant::now();
        }

        if app.sync_requested && !sync_mgr.in_flight {
            app.sync_requested = false;
            if sync_mgr.trigger(app.store.clone(), app.sync_config.clone()) {
                app.syncing = true;
                needs_draw = true;
            }
        }

        if let Some(outcome) = sync_mgr.poll() {
            app.syncing = false;
            app.apply_sync_outcome(outcome);
            needs_draw = true;
        }

        if app.tick_status_message() {
            needs_draw = true;
        }

        if needs_draw {
            terminal.draw(|frame| ui::render(frame, app))?;
            needs_draw = false;
        }

        if app.should_quit {
            break;
        }

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key);
                    needs_draw = true;
                }
                Event::Mouse(mouse) => {
                    if !matches!(mouse.kind, MouseEventKind::Moved) {
                        app.handle_mouse(mouse);
                        needs_draw = true;
                    }
                }
                Event::Resize(_, _) => {
                    needs_draw = true;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

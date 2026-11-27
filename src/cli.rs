use crate::config::{
    default_config_path, default_state_dir, find_local_config, normalize_device_tag, AppConfig,
};
use crate::editor;
use crate::mcp::McpServer;
use crate::model::{
    default_storage_path, find_local_board, parse_due_spec, parse_due_time, parse_quick_add,
    parse_reminder, parse_repeat, parse_tags, sort_tasks, today_ymd, DueState, Priority, SortOrder,
    Status, Task, TaskStore, LOCAL_BOARD_FILENAME,
};
use crate::ops::{self, AddTaskInput, SyncReport, TagCount, TaskFilter, UpdateTaskInput};
use crate::sync::{oauth, project_cache_board_path};
use crate::theme::{to_crossterm_color, Theme};
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use crossterm::style::{Attribute, ContentStyle, StyledContent};
use ratatui::style::Color;
use serde::Deserialize;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

fn clap_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .usage(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .literal(AnsiColor::Green.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Yellow.on_default())
        .valid(AnsiColor::Green.on_default())
        .invalid(AnsiColor::Red.on_default() | Effects::BOLD)
        .error(AnsiColor::Red.on_default() | Effects::BOLD)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    pub fn should_color_stdout(self) -> bool {
        match self {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                if std::env::var_os("NO_COLOR").is_some() {
                    return false;
                }
                if let Ok(force) = std::env::var("CLICOLOR_FORCE") {
                    if !force.is_empty() && force != "0" {
                        return true;
                    }
                }
                io::stdout().is_terminal()
            }
        }
    }

    pub fn should_color_stderr(self) -> bool {
        match self {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                if std::env::var_os("NO_COLOR").is_some() {
                    return false;
                }
                if let Ok(force) = std::env::var("CLICOLOR_FORCE") {
                    if !force.is_empty() && force != "0" {
                        return true;
                    }
                }
                io::stderr().is_terminal()
            }
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "roke",
    version,
    about = "A quiet terminal todo list and Kanban board",
    long_about = "roke — A quiet terminal todo list and Kanban board.\n\nRun with no subcommand to launch the interactive TUI, or use subcommands for scripting and AI agents.",
    styles = clap_styles(),
    subcommand_negates_reqs = false,
)]
pub struct Cli {
    /// Path to JSONL task file
    #[arg(short = 'f', long = "file", value_name = "PATH", global = true)]
    pub file: Option<PathBuf>,

    /// Force the global board and config even inside a directory containing .roke.jsonl or .roke.toml
    #[arg(long = "global", global = true, conflicts_with = "all_boards")]
    pub global_board: bool,

    /// Show and sync both the project board and the global board simultaneously
    #[arg(long = "all", global = true, conflicts_with = "global_board")]
    pub all_boards: bool,

    /// Path to TOML config file (overrides both global config.toml and local .roke.toml)
    #[arg(short = 'c', long = "config", value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Output structured JSON (for CLI subcommands)
    #[arg(long = "json", global = true)]
    pub json: bool,

    /// Control colored CLI output
    #[arg(
        long = "color",
        value_enum,
        default_value_t = ColorChoice::Auto,
        value_name = "WHEN",
        global = true
    )]
    pub color: ColorChoice,

    /// Print default config.toml to stdout and exit
    #[arg(long = "print-config")]
    pub print_config: bool,

    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[derive(Debug, Clone)]
pub struct ResolvedProjectContext {
    pub storage_path: PathBuf,
    pub global_storage_path: PathBuf,
    pub global_config_path: PathBuf,
    pub local_config_path: Option<PathBuf>,
    pub project_name: Option<String>,
    pub isolate_sync_peers: bool,
}

impl Cli {
    /// Resolve board storage path and global/local config paths taking `.roke.jsonl` and
    /// `.roke.toml` precedence into account (overridden by `--global`, `--file`, or `--config`).
    pub fn resolve_context(&self) -> ResolvedProjectContext {
        let global_config_path = self.config_path();
        let global_storage_path = default_storage_path();
        if self.global_board {
            return ResolvedProjectContext {
                storage_path: self
                    .file
                    .clone()
                    .unwrap_or_else(|| global_storage_path.clone()),
                global_storage_path,
                global_config_path,
                local_config_path: None,
                project_name: None,
                isolate_sync_peers: false,
            };
        }

        let cwd = std::env::current_dir().ok();
        let local_board = cwd.as_deref().and_then(find_local_board);
        let local_config = if self.config.is_some() {
            None
        } else {
            cwd.as_deref().and_then(find_local_config)
        };

        // Pick the nearest project marker when both exist in different ancestor directories.
        let (active_board, active_local_cfg) = match (local_board, local_config) {
            (Some(b), Some(c)) => {
                let b_depth = b.parent().map(|p| p.components().count()).unwrap_or(0);
                let c_depth = c.parent().map(|p| p.components().count()).unwrap_or(0);
                if b_depth == c_depth {
                    (Some(b), Some(c))
                } else if b_depth > c_depth {
                    (Some(b), None)
                } else {
                    (None, Some(c))
                }
            }
            (b, c) => (b, c),
        };

        let project_dir = active_board
            .as_deref()
            .or(active_local_cfg.as_deref())
            .and_then(Path::parent);
        let project_name = project_dir.and_then(|dir| {
            dir.file_name()
                .and_then(|n| n.to_str())
                .map(normalize_device_tag)
                .filter(|s| !s.is_empty())
        });

        let storage_path = if let Some(file) = &self.file {
            file.clone()
        } else if let Some(board) = &active_board {
            board.clone()
        } else if let Some(cfg_file) = &active_local_cfg {
            let dir = cfg_file.parent().unwrap_or(Path::new("."));
            project_cache_board_path(dir)
        } else {
            global_storage_path.clone()
        };

        let isolate_sync_peers = active_board.is_some() || active_local_cfg.is_some();

        ResolvedProjectContext {
            storage_path,
            global_storage_path,
            global_config_path,
            local_config_path: active_local_cfg,
            project_name,
            isolate_sync_peers,
        }
    }

    #[cfg(test)]
    pub fn storage_path(&self) -> PathBuf {
        self.resolve_context().storage_path
    }

    pub fn config_path(&self) -> PathBuf {
        self.config.clone().unwrap_or_else(default_config_path)
    }

    pub fn load_effective_config(&self) -> Result<(AppConfig, ResolvedProjectContext), String> {
        let ctx = self.resolve_context();
        let cfg = if self.config.is_some() {
            AppConfig::load_or_default(&ctx.global_config_path)?
        } else {
            AppConfig::load_with_local_overlay(
                &ctx.global_config_path,
                ctx.local_config_path.as_deref(),
                ctx.isolate_sync_peers,
            )?
        };
        Ok((cfg, ctx))
    }
}

#[derive(Debug, Subcommand)]
pub enum SubCommand {
    /// List tasks (filter by status, priority, tag, or search)
    #[command(alias = "ls")]
    List(ListArgs),

    /// Show details for a single task by ID or UID
    #[command(alias = "show")]
    Get {
        /// Task numeric ID or stable UID
        selector: String,
    },

    /// Add a new task (or batch import via --stdin)
    #[command(alias = "new")]
    Add(AddArgs),

    /// Edit fields or tags on an existing task
    #[command(alias = "update")]
    Edit(EditArgs),

    /// View or edit a task's Markdown notes in $EDITOR (or via --print / --set / --stdin)
    #[command(alias = "note")]
    Notes(NotesArgs),

    /// Move one or more tasks to todo, in-progress, or done (e.g. `roke move 1 2 in-progress`)
    #[command(alias = "mv")]
    Move {
        /// Task ID(s)/UID(s) followed by target status (todo, in-progress, done)
        #[arg(value_name = "SELECTOR... STATUS", required = true, num_args = 2..)]
        args: Vec<String>,
    },

    /// Mark one or more tasks as done (e.g. `roke done 1 2 3`)
    Done {
        /// One or more task numeric IDs or stable UIDs
        #[arg(value_name = "SELECTOR", required = true, num_args = 1..)]
        selectors: Vec<String>,
    },

    /// Delete one or more tasks and record sync tombstones (e.g. `roke rm 1 2 3`)
    #[command(aliases = ["delete", "del"])]
    Rm {
        /// One or more task numeric IDs or stable UIDs
        #[arg(value_name = "SELECTOR", required = true, num_args = 1..)]
        selectors: Vec<String>,
    },

    /// List all tags and their task counts
    Tags,

    /// Show a compact summary of board counts, due dates, velocity, and top tags
    #[command(alias = "summary")]
    Stats {
        /// Output a single-line status segment for tmux or shell prompts (e.g. `○3 ◐1 !1`)
        #[arg(long = "oneline")]
        oneline: bool,
    },

    /// Sync tasks with peers configured in config.toml (SSH, CalDAV, WebDAV, file, ms-todo, google-tasks)
    Sync,

    /// Authenticate an OAuth sync peer (ms-todo or google-tasks), or list OAuth peer status
    Auth {
        /// Peer name from config.toml (omit to list OAuth peers and login status)
        peer: Option<String>,
    },

    /// Prune old sync tombstones from the JSONL file
    Gc {
        /// Remove tombstones older than this many days (0 removes all)
        #[arg(long = "days", default_value_t = 90)]
        days: u64,
    },

    /// Move completed (done) tasks into archive.jsonl, or browse archived tasks with --list
    Archive {
        /// List tasks currently stored in archive.jsonl instead of archiving
        #[arg(short = 'l', long = "list")]
        list: bool,

        /// Only archive done tasks older than this many days
        #[arg(long = "days")]
        days: Option<u64>,

        /// Preview which tasks would be archived without modifying files
        #[arg(long = "dry-run")]
        dry_run: bool,
    },

    /// Restore one or more archived tasks from archive.jsonl back onto the active board
    #[command(alias = "restore")]
    Unarchive {
        /// One or more archived task IDs or stable UIDs
        #[arg(value_name = "SELECTOR", required = true, num_args = 1..)]
        selectors: Vec<String>,
    },

    /// Create a project-local .roke.jsonl board in the current directory
    Init,

    /// Generate shell completion scripts (bash, zsh, fish, elvish, powershell)
    Completions {
        /// Target shell
        shell: Shell,
    },

    /// Run built-in Model Context Protocol (MCP) server over stdio
    Mcp {
        /// Restrict the MCP server to read-only tools and resources
        #[arg(long = "read-only")]
        read_only: bool,
    },
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Filter by status (todo, in-progress, done)
    #[arg(short = 's', long = "status", value_parser = Status::parse)]
    pub status: Option<Status>,

    /// Filter by priority (low, medium, high)
    #[arg(short = 'p', long = "priority", value_parser = Priority::parse)]
    pub priority: Option<Priority>,

    /// Filter by tag(s); repeatable or comma-separated
    #[arg(short = 't', long = "tag", visible_alias = "tags")]
    pub tags: Vec<String>,

    /// Match any tag (OR) instead of all tags (AND)
    #[arg(long = "any-tag", visible_alias = "or")]
    pub any_tag: bool,

    /// Case-insensitive search across title, description, and tags
    #[arg(short = 'q', long = "search")]
    pub search: Option<String>,

    /// Show only overdue (non-done) tasks
    #[arg(long = "overdue")]
    pub overdue: bool,

    /// Sort tasks (manual, priority, due, updated)
    #[arg(long = "sort", value_parser = SortOrder::parse, default_value = "manual")]
    pub sort: SortOrder,

    /// Show descriptions and UIDs beneath each task
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
}

fn validate_due_arg(raw: &str) -> Result<String, String> {
    let (d, t) = parse_due_spec(raw)?;
    Ok(match t {
        Some(hm) => format!("{d} {hm}"),
        None => d,
    })
}

fn parse_progress_arg(raw: &str) -> Result<u8, String> {
    let cleaned = raw.trim().trim_end_matches('%');
    let pct: u8 = cleaned
        .parse()
        .map_err(|_| format!("invalid progress '{raw}' (expected integer 0..=100)"))?;
    if pct > 100 {
        return Err(format!("invalid progress '{raw}' (must be 0..=100)"));
    }
    Ok(pct)
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Task title (omit when using --stdin)
    #[arg(value_name = "TITLE")]
    pub title: Vec<String>,

    /// Task description or notes
    #[arg(
        short = 'd',
        long = "desc",
        visible_alias = "description",
        default_value = ""
    )]
    pub description: String,

    /// Initial status (todo, in-progress, done)
    #[arg(short = 's', long = "status", value_parser = Status::parse, default_value = "todo")]
    pub status: Status,

    /// Task priority (low, medium, high)
    #[arg(short = 'p', long = "priority", value_parser = Priority::parse, default_value = "medium")]
    pub priority: Priority,

    /// Tag(s); repeatable or comma-separated
    #[arg(short = 't', long = "tag", visible_alias = "tags")]
    pub tags: Vec<String>,

    /// Optional due date (YYYY-MM-DD, today, tomorrow, +3d, fri, or with time e.g. 'tomorrow 14:30')
    #[arg(long = "due", value_parser = validate_due_arg)]
    pub due: Option<String>,

    /// Optional local due time (HH:MM, 9am, 2:30pm)
    #[arg(long = "due-time", value_parser = parse_due_time)]
    pub due_time: Option<String>,

    /// Optional start / hide-until date (YYYY-MM-DD, today, tomorrow, +3d, or with time)
    #[arg(long = "start", value_parser = validate_due_arg)]
    pub start: Option<String>,

    /// Optional local start time (HH:MM, 9am, 2:30pm)
    #[arg(long = "start-time", value_parser = parse_due_time)]
    pub start_time: Option<String>,

    /// Optional recurrence rule (daily, weekdays, weekly, monthly, every 3d/2w/2m)
    #[arg(long = "repeat", value_parser = parse_repeat)]
    pub repeat: Option<String>,

    /// Reminder(s) (repeatable: at-due, at-start, -15m, -1h, -1d, start-15m, YYYY-MM-DD HH:MM)
    #[arg(long = "remind", visible_alias = "reminder", value_parser = parse_reminder)]
    pub reminders: Vec<String>,

    /// Parent task numeric ID or UID (marks this task as a subtask)
    #[arg(long = "parent")]
    pub parent: Option<String>,

    /// Optional location / place name
    #[arg(long = "location")]
    pub location: Option<String>,

    /// Optional associated URL
    #[arg(long = "url")]
    pub url: Option<String>,

    /// Optional completion progress (0..=100)
    #[arg(long = "progress", value_parser = parse_progress_arg)]
    pub progress: Option<u8>,

    /// Override task origin (defaults to local device_name)
    #[arg(long = "origin")]
    pub origin: Option<String>,

    /// Parse quick-add shorthand in title (#tag !high @tomorrow@14:30 ~weekly)
    #[arg(short = 'Q', long = "quick")]
    pub quick: bool,

    /// Read a JSON object, JSON array, or JSONL of tasks from stdin
    #[arg(long = "stdin")]
    pub from_stdin: bool,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    /// Task numeric ID or stable UID
    pub selector: String,

    /// New task title
    #[arg(long = "title")]
    pub title: Option<String>,

    /// New task description
    #[arg(short = 'd', long = "desc", visible_alias = "description")]
    pub description: Option<String>,

    /// New status (todo, in-progress, done)
    #[arg(short = 's', long = "status", value_parser = Status::parse)]
    pub status: Option<Status>,

    /// New priority (low, medium, high)
    #[arg(short = 'p', long = "priority", value_parser = Priority::parse)]
    pub priority: Option<Priority>,

    /// Replace all tags (pass "" to clear)
    #[arg(short = 't', long = "tags")]
    pub tags: Option<String>,

    /// Add tag(s) without removing existing ones
    #[arg(long = "add-tag")]
    pub add_tags: Vec<String>,

    /// Remove tag(s)
    #[arg(long = "rm-tag", visible_alias = "remove-tag")]
    pub remove_tags: Vec<String>,

    /// Set due date (YYYY-MM-DD, today, tomorrow, +3d, fri, or with time e.g. 'tomorrow 14:30')
    #[arg(long = "due", value_parser = validate_due_arg)]
    pub due: Option<String>,

    /// Clear the task's due date (and due time)
    #[arg(long = "clear-due")]
    pub clear_due: bool,

    /// Set local due time (HH:MM, 9am, 2:30pm)
    #[arg(long = "due-time", value_parser = parse_due_time)]
    pub due_time: Option<String>,

    /// Clear only the task's due time (keeps due date)
    #[arg(long = "clear-due-time")]
    pub clear_due_time: bool,

    /// Set start / hide-until date (YYYY-MM-DD, today, tomorrow, +3d, or with time)
    #[arg(long = "start", value_parser = validate_due_arg)]
    pub start: Option<String>,

    /// Clear the task's start date (and start time)
    #[arg(long = "clear-start")]
    pub clear_start: bool,

    /// Set local start time (HH:MM, 9am, 2:30pm)
    #[arg(long = "start-time", value_parser = parse_due_time)]
    pub start_time: Option<String>,

    /// Clear only the task's start time (keeps start date)
    #[arg(long = "clear-start-time")]
    pub clear_start_time: bool,

    /// Set recurrence rule (daily, weekdays, weekly, monthly, every 3d/2w/2m)
    #[arg(long = "repeat", value_parser = parse_repeat)]
    pub repeat: Option<String>,

    /// Clear the task's recurrence rule
    #[arg(long = "clear-repeat")]
    pub clear_repeat: bool,

    /// Replace reminders (repeatable: at-due, -15m, -1h, -1d, start-15m, YYYY-MM-DD HH:MM)
    #[arg(long = "remind", visible_alias = "reminder", value_parser = parse_reminder)]
    pub reminders: Vec<String>,

    /// Clear all reminders
    #[arg(long = "clear-reminders")]
    pub clear_reminders: bool,

    /// Set parent task numeric ID or UID (subtask)
    #[arg(long = "parent")]
    pub parent: Option<String>,

    /// Clear the task's parent (promote to top-level task)
    #[arg(long = "clear-parent")]
    pub clear_parent: bool,

    /// Set location / place name
    #[arg(long = "location")]
    pub location: Option<String>,

    /// Clear the task's location
    #[arg(long = "clear-location")]
    pub clear_location: bool,

    /// Set associated URL
    #[arg(long = "url")]
    pub url: Option<String>,

    /// Clear the task's URL
    #[arg(long = "clear-url")]
    pub clear_url: bool,

    /// Set completion progress (0..=100)
    #[arg(long = "progress", value_parser = parse_progress_arg)]
    pub progress: Option<u8>,

    /// Clear completion progress
    #[arg(long = "clear-progress")]
    pub clear_progress: bool,

    /// Open the task (YAML frontmatter + Markdown notes) in $EDITOR (nvim, etc.)
    #[arg(short = 'e', long = "editor")]
    pub open_editor: bool,
}

#[derive(Debug, Args)]
pub struct NotesArgs {
    /// Task numeric ID or stable UID
    pub selector: String,

    /// Print the raw Markdown notes to stdout without launching an editor
    #[arg(short = 'p', long = "print")]
    pub print: bool,

    /// Replace Markdown notes directly with this text
    #[arg(long = "set", value_name = "MARKDOWN")]
    pub set: Option<String>,

    /// Replace Markdown notes by reading from stdin
    #[arg(long = "stdin")]
    pub from_stdin: bool,

    /// Clear the task's notes
    #[arg(long = "clear")]
    pub clear: bool,
}

#[derive(Debug, Deserialize)]
struct StdinTaskSpec {
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default)]
    due_time: Option<String>,
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    start_time: Option<String>,
    #[serde(default)]
    repeat: Option<String>,
    #[serde(default)]
    reminders: Vec<String>,
    #[serde(default)]
    parent_uid: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    percent_complete: Option<u8>,
    #[serde(default)]
    origin: Option<String>,
}

fn update_input_from_doc_diff(orig: &Task, edited: &editor::TaskDocument) -> UpdateTaskInput {
    let mut input = UpdateTaskInput::default();
    if edited.title != orig.title {
        input.title = Some(edited.title.clone());
    }
    if edited.description != orig.description {
        input.description = Some(edited.description.clone());
    }
    if edited.status != orig.status {
        input.status = Some(edited.status);
    }
    if edited.priority != orig.priority {
        input.priority = Some(edited.priority);
    }
    if edited.tags != orig.tags {
        input.tags = Some(edited.tags.clone());
    }
    if edited.due != orig.due {
        input.due = Some(edited.due.clone());
    }
    if edited.due_time != orig.due_time {
        input.due_time = Some(edited.due_time.clone());
    }
    if edited.start != orig.start {
        input.start = Some(edited.start.clone());
    }
    if edited.start_time != orig.start_time {
        input.start_time = Some(edited.start_time.clone());
    }
    if edited.repeat != orig.repeat {
        input.repeat = Some(edited.repeat.clone());
    }
    if edited.reminders != orig.reminders {
        input.reminders = Some(edited.reminders.clone());
    }
    if edited.parent_uid != orig.parent_uid {
        input.parent_uid = Some(edited.parent_uid.clone());
    }
    if edited.location != orig.location {
        input.location = Some(edited.location.clone());
    }
    if edited.url != orig.url {
        input.url = Some(edited.url.clone());
    }
    if edited.percent_complete != orig.percent_complete {
        input.percent_complete = Some(edited.percent_complete);
    }
    input
}

fn parse_stdin_tasks(raw: &str, defaults: &AddTaskInput) -> Result<Vec<AddTaskInput>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("stdin was empty; expected JSON object, JSON array, or JSONL".into());
    }

    let specs: Vec<StdinTaskSpec> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON array on stdin: {e}"))?
    } else if let Ok(single) = serde_json::from_str::<StdinTaskSpec>(trimmed) {
        vec![single]
    } else {
        let mut items = Vec::new();
        for (idx, line) in trimmed.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let spec: StdinTaskSpec = serde_json::from_str(line)
                .map_err(|e| format!("invalid JSON on stdin line {}: {e}", idx + 1))?;
            items.push(spec);
        }
        items
    };

    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let status = match spec.status {
            Some(s) => Status::parse(&s)?,
            None => defaults.status,
        };
        let priority = match spec.priority {
            Some(p) => Priority::parse(&p)?,
            None => defaults.priority,
        };
        let mut tags = defaults.tags.clone();
        tags.extend(spec.tags);
        let description = if spec.description.trim().is_empty() {
            defaults.description.clone()
        } else {
            spec.description
        };
        let due = spec.due.or_else(|| defaults.due.clone());
        let due_time = spec.due_time.or_else(|| defaults.due_time.clone());
        let start = spec.start.or_else(|| defaults.start.clone());
        let start_time = spec.start_time.or_else(|| defaults.start_time.clone());
        let repeat = spec.repeat.or_else(|| defaults.repeat.clone());
        let mut reminders = defaults.reminders.clone();
        reminders.extend(spec.reminders);
        let parent_uid = spec.parent_uid.or_else(|| defaults.parent_uid.clone());
        let location = spec.location.or_else(|| defaults.location.clone());
        let url = spec.url.or_else(|| defaults.url.clone());
        let percent_complete = spec.percent_complete.or(defaults.percent_complete);
        let origin = spec.origin.or_else(|| defaults.origin.clone());
        out.push(AddTaskInput {
            title: spec.title,
            description,
            status,
            priority,
            tags,
            due,
            due_time,
            start,
            start_time,
            repeat,
            reminders,
            parent_uid,
            location,
            url,
            percent_complete,
            origin,
        });
    }
    Ok(out)
}

pub fn run_subcommand(cli: &Cli, cmd: &SubCommand, cfg: &AppConfig) -> Result<(), String> {
    let ctx = cli.resolve_context();
    let path = ctx.storage_path.clone();
    let dual_all = cli.all_boards && ctx.isolate_sync_peers && cli.file.is_none();
    let global_cfg = if dual_all {
        AppConfig::load_or_default(&ctx.global_config_path).unwrap_or_default()
    } else {
        cfg.clone()
    };
    let json = cli.json;
    let painter = Painter::new(cfg.resolved_theme(), cli.color.should_color_stdout());

    match cmd {
        SubCommand::List(args) => {
            let filter = TaskFilter {
                status: args.status,
                priority: args.priority,
                tags: args.tags.iter().flat_map(|t| parse_tags(t)).collect(),
                tag_match_all: !args.any_tag,
                search: args.search.clone(),
                overdue_only: args.overdue,
                sort: args.sort,
            };
            let (tasks, board_labels): (Vec<Task>, Vec<Option<String>>) = if dual_all {
                let proj_label = ctx
                    .project_name
                    .clone()
                    .unwrap_or_else(|| "project".to_string());
                let proj_tasks = ops::list_tasks(&path, cfg, &filter)?;
                let glob_tasks = ops::list_tasks(&ctx.global_storage_path, &global_cfg, &filter)?;
                let combined: Vec<(Task, String)> = proj_tasks
                    .into_iter()
                    .map(|t| (t, proj_label.clone()))
                    .chain(glob_tasks.into_iter().map(|t| (t, "global".to_string())))
                    .collect();
                let mut only_tasks: Vec<Task> = combined.iter().map(|(t, _)| t.clone()).collect();
                sort_tasks(&mut only_tasks, filter.sort);
                if filter.sort != SortOrder::Manual {
                    let labels_by_uid: std::collections::HashMap<String, String> =
                        combined.into_iter().map(|(t, l)| (t.uid, l)).collect();
                    let labels = only_tasks
                        .iter()
                        .map(|t| labels_by_uid.get(&t.uid).cloned())
                        .collect();
                    (only_tasks, labels)
                } else {
                    let labels = combined.iter().map(|(_, l)| Some(l.clone())).collect();
                    let tasks = combined.into_iter().map(|(t, _)| t).collect();
                    (tasks, labels)
                }
            } else {
                let tasks = ops::list_tasks(&path, cfg, &filter)?;
                let labels = vec![None; tasks.len()];
                (tasks, labels)
            };
            if json {
                print_json(&tasks)?;
            } else if tasks.is_empty() {
                println!("{}", painter.muted("No tasks found."));
            } else {
                let id_width = tasks
                    .iter()
                    .map(|t| t.id.to_string().len() + 1)
                    .max()
                    .unwrap_or(2);
                for (task, board_tag) in tasks.iter().zip(board_labels.iter()) {
                    let mut eff_tags = ops::effective_tags_for(task, &cfg.sync);
                    if let Some(bt) = board_tag {
                        if !eff_tags.iter().any(|t| t == bt) {
                            eff_tags.push(bt.clone());
                        }
                    }
                    println!("{}", painter.format_task_line(task, &eff_tags, id_width));
                    if args.verbose {
                        let indent = " ".repeat(id_width + 5);
                        if !task.description.is_empty() {
                            for line in task.description.lines() {
                                println!("{indent}{}", painter.subtle(line));
                            }
                        }
                        println!("{indent}{}", painter.muted(&format!("uid: {}", task.uid)));
                    }
                }
            }
        }
        SubCommand::Get { selector } => {
            let task = ops::get_task(&path, cfg, selector)?;
            if json {
                print_json(&task)?;
            } else {
                let eff_tags = ops::effective_tags_for(&task, &cfg.sync);
                print!("{}", painter.format_task_detail(&task, &eff_tags));
            }
        }
        SubCommand::Add(args) => {
            let input = AddTaskInput {
                title: args.title.join(" ").trim().to_string(),
                description: args.description.clone(),
                status: args.status,
                priority: args.priority,
                tags: args.tags.iter().flat_map(|t| parse_tags(t)).collect(),
                due: args.due.clone(),
                due_time: args.due_time.clone(),
                start: args.start.clone(),
                start_time: args.start_time.clone(),
                repeat: args.repeat.clone(),
                reminders: args.reminders.clone(),
                parent_uid: args.parent.clone(),
                location: args.location.clone(),
                url: args.url.clone(),
                percent_complete: args.progress,
                origin: args.origin.clone(),
            };
            if args.from_stdin {
                let mut buf = String::new();
                io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| format!("failed to read stdin: {e}"))?;
                let batch = parse_stdin_tasks(&buf, &input)?;
                let created = ops::add_tasks_batch(&path, cfg, batch)?;
                if json {
                    print_json(&created)?;
                } else {
                    for task in &created {
                        let eff_tags = ops::effective_tags_for(task, &cfg.sync);
                        println!(
                            "{} {}",
                            painter.badge("+", painter.theme.add),
                            painter.format_task_line(task, &eff_tags, 0)
                        );
                    }
                }
            } else {
                let mut input = input;
                if input.title.is_empty() {
                    return Err("missing task title. Usage: roke add <TITLE> [OPTIONS]".into());
                }
                if args.quick {
                    let parsed = parse_quick_add(&input.title)?;
                    input.title = parsed.title;
                    if let Some(p) = parsed.priority {
                        input.priority = p;
                    }
                    for tag in parsed.tags {
                        if !input.tags.contains(&tag) {
                            input.tags.push(tag);
                        }
                    }
                    if input.due.is_none() {
                        input.due = parsed.due;
                    }
                    if input.due_time.is_none() {
                        input.due_time = parsed.due_time;
                    }
                    if input.repeat.is_none() {
                        input.repeat = parsed.repeat;
                    }
                }
                let task = ops::add_task(&path, cfg, input)?;
                if json {
                    print_json(&task)?;
                } else {
                    let eff_tags = ops::effective_tags_for(&task, &cfg.sync);
                    println!(
                        "{} {}",
                        painter.badge("+", painter.theme.add),
                        painter.format_task_line(&task, &eff_tags, 0)
                    );
                }
            }
        }
        SubCommand::Edit(args) => {
            let mut input = UpdateTaskInput::default();
            let mut editor_existing: Option<Task> = None;
            if args.open_editor {
                let existing = ops::get_task(&path, cfg, &args.selector)?;
                let doc = editor::TaskDocument::from_task(&existing);
                if let Some(edited) = editor::edit_task_in_editor(&doc, cfg.editor.as_deref())? {
                    input = update_input_from_doc_diff(&existing, &edited);
                }
                editor_existing = Some(existing);
            }
            if let Some(t) = &args.title {
                input.title = Some(t.clone());
            }
            if let Some(d) = &args.description {
                input.description = Some(d.clone());
            }
            if let Some(s) = args.status {
                input.status = Some(s);
            }
            if let Some(p) = args.priority {
                input.priority = Some(p);
            }
            if let Some(t) = args.tags.as_deref() {
                input.tags = Some(parse_tags(t));
            }
            input.add_tags = args.add_tags.iter().flat_map(|t| parse_tags(t)).collect();
            input.remove_tags = args
                .remove_tags
                .iter()
                .flat_map(|t| parse_tags(t))
                .collect();
            if args.clear_due {
                input.due = Some(None);
            } else if let Some(d) = &args.due {
                input.due = Some(Some(d.clone()));
            }
            if args.clear_due_time {
                input.due_time = Some(None);
            } else if let Some(dt) = &args.due_time {
                input.due_time = Some(Some(dt.clone()));
            }
            if args.clear_start {
                input.start = Some(None);
            } else if let Some(st) = &args.start {
                input.start = Some(Some(st.clone()));
            }
            if args.clear_start_time {
                input.start_time = Some(None);
            } else if let Some(stt) = &args.start_time {
                input.start_time = Some(Some(stt.clone()));
            }
            if args.clear_repeat {
                input.repeat = Some(None);
            } else if let Some(r) = &args.repeat {
                input.repeat = Some(Some(r.clone()));
            }
            if args.clear_reminders {
                input.reminders = Some(Vec::new());
            } else if !args.reminders.is_empty() {
                input.reminders = Some(args.reminders.clone());
            }
            if args.clear_parent {
                input.parent_uid = Some(None);
            } else if let Some(p) = &args.parent {
                input.parent_uid = Some(Some(p.clone()));
            }
            if args.clear_location {
                input.location = Some(None);
            } else if let Some(loc) = &args.location {
                input.location = Some(Some(loc.clone()));
            }
            if args.clear_url {
                input.url = Some(None);
            } else if let Some(u) = &args.url {
                input.url = Some(Some(u.clone()));
            }
            if args.clear_progress {
                input.percent_complete = Some(None);
            } else if let Some(pct) = args.progress {
                input.percent_complete = Some(Some(pct));
            }

            if input.is_empty() {
                if let Some(existing) = editor_existing {
                    if json {
                        print_json(&existing)?;
                    } else {
                        println!("{}", painter.muted("No changes."));
                    }
                    return Ok(());
                }
                return Err("no changes specified. Pass at least one field flag (e.g. --title, --status, --due, --start, --remind, --location, --url, --parent, --progress) or --editor.".into());
            }
            let task = ops::update_task(&path, cfg, &args.selector, input)?;
            if json {
                print_json(&task)?;
            } else {
                let eff_tags = ops::effective_tags_for(&task, &cfg.sync);
                println!(
                    "{} {}",
                    painter.badge("✎", painter.theme.accent),
                    painter.format_task_line(&task, &eff_tags, 0)
                );
            }
        }
        SubCommand::Notes(args) => {
            let existing = ops::get_task(&path, cfg, &args.selector)?;
            if args.print {
                if json {
                    print_json(&serde_json::json!({
                        "id": existing.id,
                        "uid": existing.uid,
                        "title": existing.title,
                        "notes": existing.description,
                    }))?;
                } else if !existing.description.is_empty() {
                    println!("{}", existing.description);
                }
                return Ok(());
            }

            let update_input: Option<UpdateTaskInput> = if args.clear {
                Some(UpdateTaskInput {
                    description: Some(String::new()),
                    ..UpdateTaskInput::default()
                })
            } else if let Some(text) = &args.set {
                Some(UpdateTaskInput {
                    description: Some(text.clone()),
                    ..UpdateTaskInput::default()
                })
            } else if args.from_stdin {
                let mut buf = String::new();
                io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| format!("failed to read stdin: {e}"))?;
                Some(UpdateTaskInput {
                    description: Some(buf),
                    ..UpdateTaskInput::default()
                })
            } else {
                let doc = editor::TaskDocument::from_task(&existing);
                editor::edit_task_in_editor(&doc, cfg.editor.as_deref())?
                    .map(|edited| update_input_from_doc_diff(&existing, &edited))
                    .filter(|inp| !inp.is_empty())
            };

            let Some(input) = update_input else {
                if json {
                    print_json(&existing)?;
                } else {
                    println!("{}", painter.muted("Task unchanged."));
                }
                return Ok(());
            };

            let updated = ops::update_task(&path, cfg, &args.selector, input)?;
            if json {
                print_json(&updated)?;
            } else {
                let eff_tags = ops::effective_tags_for(&updated, &cfg.sync);
                println!(
                    "{} {}",
                    painter.badge("✎", painter.theme.accent),
                    painter.format_task_line(&updated, &eff_tags, 0)
                );
            }
        }
        SubCommand::Move { args } => {
            let (status_raw, selectors) = args
                .split_last()
                .ok_or_else(|| "usage: roke move <ID>... <STATUS>".to_string())?;
            let status = Status::parse(status_raw)?;
            let mut updated_tasks = Vec::with_capacity(selectors.len());
            for selector in selectors {
                let task = ops::update_task(
                    &path,
                    cfg,
                    selector,
                    UpdateTaskInput {
                        status: Some(status),
                        ..UpdateTaskInput::default()
                    },
                )?;
                updated_tasks.push(task);
            }
            if json {
                if updated_tasks.len() == 1 {
                    print_json(&updated_tasks[0])?;
                } else {
                    print_json(&updated_tasks)?;
                }
            } else {
                for task in &updated_tasks {
                    let eff_tags = ops::effective_tags_for(task, &cfg.sync);
                    println!(
                        "{} {}",
                        painter.badge("→", painter.status_color(task.status)),
                        painter.format_task_line(task, &eff_tags, 0)
                    );
                }
            }
        }
        SubCommand::Done { selectors } => {
            let mut updated_tasks = Vec::with_capacity(selectors.len());
            for selector in selectors {
                let task = ops::update_task(
                    &path,
                    cfg,
                    selector,
                    UpdateTaskInput {
                        status: Some(Status::Done),
                        ..UpdateTaskInput::default()
                    },
                )?;
                updated_tasks.push(task);
            }
            if json {
                if updated_tasks.len() == 1 {
                    print_json(&updated_tasks[0])?;
                } else {
                    print_json(&updated_tasks)?;
                }
            } else {
                for task in &updated_tasks {
                    let eff_tags = ops::effective_tags_for(task, &cfg.sync);
                    println!(
                        "{} {}",
                        painter.badge("✓", painter.theme.done),
                        painter.format_task_line(task, &eff_tags, 0)
                    );
                }
            }
        }
        SubCommand::Rm { selectors } => {
            let mut removed_tasks = Vec::with_capacity(selectors.len());
            for selector in selectors {
                let removed = ops::delete_task(&path, cfg, selector)?;
                removed_tasks.push(removed);
            }
            if json {
                if removed_tasks.len() == 1 {
                    print_json(&removed_tasks[0])?;
                } else {
                    print_json(&removed_tasks)?;
                }
            } else {
                for removed in &removed_tasks {
                    println!(
                        "{} {} {}",
                        painter.badge("✕", painter.theme.error),
                        painter.muted(&format!("#{}", removed.id)),
                        painter.strikethrough(&removed.title)
                    );
                }
            }
        }
        SubCommand::Tags => {
            let tags = ops::list_tags(&path, cfg)?;
            if json {
                print_json(&tags)?;
            } else if tags.is_empty() {
                println!("{}", painter.muted("No tags."));
            } else {
                let width = tags.iter().map(|t| t.tag.len() + 1).max().unwrap_or(4);
                for TagCount { tag, count } in tags {
                    let padded = format!("{:<width$}", format!("#{tag}"));
                    println!(
                        "{}  {}",
                        painter.colored(&padded, painter.theme.accent),
                        painter.muted(&count.to_string())
                    );
                }
            }
        }
        SubCommand::Stats { oneline } => {
            let stats = ops::board_stats(&path, cfg)?;
            if json {
                print_json(&stats)?;
            } else if *oneline {
                println!("{}", painter.format_stats_oneline(&stats));
            } else {
                println!(
                    "{}  {} {}  {} {}  {} {}",
                    painter.style("roke", painter.theme.text, true, false, false),
                    painter.badge("○", painter.theme.todo),
                    stats.todo,
                    painter.badge("◐", painter.theme.in_progress),
                    stats.in_progress,
                    painter.badge("✓", painter.theme.done),
                    stats.done,
                );
                let mut highlights: Vec<String> = Vec::new();
                if stats.overdue > 0 {
                    highlights.push(painter.style(
                        &format!("{} overdue", stats.overdue),
                        painter.theme.error,
                        true,
                        false,
                        false,
                    ));
                }
                if stats.due_today > 0 {
                    highlights.push(painter.colored(
                        &format!("{} due today", stats.due_today),
                        painter.theme.in_progress,
                    ));
                }
                if stats.high_priority_open > 0 {
                    highlights.push(painter.colored(
                        &format!("{} high priority", stats.high_priority_open),
                        painter.theme.priority_high,
                    ));
                }
                if stats.recurring > 0 {
                    highlights.push(painter.subtle(&format!("↻ {} recurring", stats.recurring)));
                }
                if !highlights.is_empty() {
                    println!("  {}", highlights.join("  ·  "));
                }
                println!(
                    "  {}",
                    painter.muted(&format!(
                        "{} done (7d)  ·  {} archived  ·  {} total active",
                        stats.completed_last_7_days, stats.archived, stats.total
                    ))
                );
                if !stats.top_tags.is_empty() {
                    let tag_line = stats
                        .top_tags
                        .iter()
                        .map(|t| {
                            format!(
                                "{} {}",
                                painter.colored(&format!("#{}", t.tag), painter.theme.accent),
                                painter.muted(&t.count.to_string())
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("  ");
                    println!("  {tag_line}");
                }
            }
        }
        SubCommand::Sync => {
            let report: SyncReport = if dual_all {
                let mut synced_peers = 0usize;
                let mut total_tasks = 0usize;
                let mut errors = Vec::new();
                if !cfg.sync.peers.is_empty() {
                    match ops::sync_tasks(&path, cfg) {
                        Ok(r) => {
                            synced_peers += r.synced_peers;
                            total_tasks += r.total_tasks;
                            errors.extend(r.errors);
                        }
                        Err(e) => errors.push(format!("project: {e}")),
                    }
                }
                if !global_cfg.sync.peers.is_empty() {
                    match ops::sync_tasks(&ctx.global_storage_path, &global_cfg) {
                        Ok(r) => {
                            synced_peers += r.synced_peers;
                            total_tasks += r.total_tasks;
                            errors.extend(r.errors);
                        }
                        Err(e) => errors.push(format!("global: {e}")),
                    }
                }
                if cfg.sync.peers.is_empty() && global_cfg.sync.peers.is_empty() {
                    return Err("no sync peers configured in .roke.toml or config.toml".into());
                }
                SyncReport {
                    synced_peers,
                    total_tasks,
                    errors,
                }
            } else {
                ops::sync_tasks(&path, cfg)?
            };
            if json {
                print_json(&report)?;
            } else {
                let noun = if report.synced_peers == 1 {
                    "peer"
                } else {
                    "peers"
                };
                println!(
                    "{} Synced {} {noun} {}",
                    painter.badge("↻", painter.theme.accent),
                    report.synced_peers,
                    painter.muted(&format!("({} tasks total)", report.total_tasks))
                );
                for err in &report.errors {
                    eprintln!("{} {err}", painter.badge("!", painter.theme.error));
                }
            }
            if !report.errors.is_empty() && report.synced_peers == 0 {
                return Err(report.errors.join("; "));
            }
        }
        SubCommand::Auth { peer } => {
            let state_dir = default_state_dir();
            if let Some(peer_name) = peer {
                let target = cfg
                    .sync
                    .peers
                    .iter()
                    .find(|p| p.name.eq_ignore_ascii_case(peer_name))
                    .ok_or_else(|| {
                        let avail: Vec<&str> = cfg
                            .sync
                            .peers
                            .iter()
                            .filter(|p| p.kind.uses_oauth())
                            .map(|p| p.name.as_str())
                            .collect();
                        if avail.is_empty() {
                            format!("peer '{peer_name}' not found (no OAuth peers configured in config.toml)")
                        } else {
                            format!(
                                "peer '{peer_name}' not found (OAuth peers: {})",
                                avail.join(", ")
                            )
                        }
                    })?;
                let saved_path = oauth::authenticate_peer(target, &state_dir)?;
                if json {
                    print_json(&serde_json::json!({
                        "peer": target.name,
                        "kind": target.kind.label(),
                        "token_path": saved_path.display().to_string(),
                    }))?;
                } else {
                    println!(
                        "{} Authenticated {} ({}) {}",
                        painter.badge("✓", painter.theme.done),
                        painter.style(&target.name, painter.theme.text, true, false, false),
                        target.kind.label(),
                        painter.muted(&format!("→ {}", saved_path.display()))
                    );
                }
            } else {
                let oauth_peers: Vec<_> = cfg
                    .sync
                    .peers
                    .iter()
                    .filter(|p| p.kind.uses_oauth())
                    .collect();
                if json {
                    let entries: Vec<_> = oauth_peers
                        .iter()
                        .map(|p| {
                            let authenticated =
                                p.token_cmd.is_some() || oauth::has_saved_tokens(&state_dir, p);
                            serde_json::json!({
                                "peer": p.name,
                                "kind": p.kind.label(),
                                "authenticated": authenticated,
                            })
                        })
                        .collect();
                    print_json(&entries)?;
                } else if oauth_peers.is_empty() {
                    println!(
                        "{}",
                        painter.muted(
                            "No OAuth peers (ms-todo / google-tasks) configured in config.toml."
                        )
                    );
                } else {
                    for p in oauth_peers {
                        let ok = p.token_cmd.is_some() || oauth::has_saved_tokens(&state_dir, p);
                        let badge = if ok {
                            painter.badge("✓", painter.theme.done)
                        } else {
                            painter.badge("○", painter.theme.muted)
                        };
                        let status = if p.token_cmd.is_some() {
                            "token_cmd"
                        } else if ok {
                            "authenticated"
                        } else {
                            "not authenticated (run `roke auth <peer>`)"
                        };
                        println!(
                            "{badge} {} ({})  {}",
                            painter.style(&p.name, painter.theme.text, true, false, false),
                            p.kind.label(),
                            painter.muted(status)
                        );
                    }
                }
            }
        }
        SubCommand::Gc { days } => {
            let report = ops::prune_tombstones(&path, cfg, *days)?;
            if json {
                print_json(&report)?;
            } else {
                println!(
                    "{} Pruned {} tombstone{} {}",
                    painter.badge("✓", painter.theme.done),
                    report.pruned_tombstones,
                    if report.pruned_tombstones == 1 {
                        ""
                    } else {
                        "s"
                    },
                    painter.muted(&format!(
                        "({} remaining, {} tasks)",
                        report.remaining_tombstones, report.total_tasks
                    ))
                );
            }
        }
        SubCommand::Archive {
            list,
            days,
            dry_run,
        } => {
            if *list {
                let tasks = ops::list_archive(&path, cfg)?;
                if json {
                    print_json(&tasks)?;
                } else if tasks.is_empty() {
                    println!("{}", painter.muted("Archive is empty."));
                } else {
                    let id_width = tasks
                        .iter()
                        .map(|t| t.id.to_string().len() + 1)
                        .max()
                        .unwrap_or(2);
                    for task in &tasks {
                        let eff_tags = ops::effective_tags_for(task, &cfg.sync);
                        println!("{}", painter.format_task_line(task, &eff_tags, id_width));
                    }
                }
            } else {
                let report = ops::archive_done(&path, cfg, *days, *dry_run)?;
                if json {
                    print_json(&report)?;
                } else if report.archived == 0 {
                    println!("{}", painter.muted("No done tasks to archive."));
                } else {
                    let verb = if report.dry_run {
                        "Would archive"
                    } else {
                        "Archived"
                    };
                    println!(
                        "{} {verb} {} done task{} → {}",
                        painter.badge("✓", painter.theme.done),
                        report.archived,
                        if report.archived == 1 { "" } else { "s" },
                        painter.muted(&report.archive_path)
                    );
                }
            }
        }
        SubCommand::Unarchive { selectors } => {
            let mut restored_tasks = Vec::with_capacity(selectors.len());
            for selector in selectors {
                let restored = ops::unarchive_task(&path, cfg, selector)?;
                restored_tasks.push(restored);
            }
            if json {
                if restored_tasks.len() == 1 {
                    print_json(&restored_tasks[0])?;
                } else {
                    print_json(&restored_tasks)?;
                }
            } else {
                for task in &restored_tasks {
                    let eff_tags = ops::effective_tags_for(task, &cfg.sync);
                    println!(
                        "{} {}",
                        painter.badge("↩", painter.theme.accent),
                        painter.format_task_line(task, &eff_tags, 0)
                    );
                }
            }
        }
        SubCommand::Init => {
            let target = match &cli.file {
                Some(p) => p.clone(),
                None => std::env::current_dir()
                    .map_err(|e| format!("failed to read current directory: {e}"))?
                    .join(LOCAL_BOARD_FILENAME),
            };
            if target.exists() {
                if json {
                    print_json(&serde_json::json!({
                        "path": target.display().to_string(),
                        "created": false,
                    }))?;
                } else {
                    println!(
                        "{} Local board already exists: {}",
                        painter.badge("·", painter.theme.subtle),
                        target.display()
                    );
                }
            } else {
                TaskStore::new_empty()
                    .save(&target)
                    .map_err(|e| format!("failed to create {}: {e}", target.display()))?;
                if json {
                    print_json(&serde_json::json!({
                        "path": target.display().to_string(),
                        "created": true,
                    }))?;
                } else {
                    println!(
                        "{} Initialized local board at {}",
                        painter.badge("+", painter.theme.add),
                        target.display()
                    );
                }
            }
        }
        SubCommand::Completions { shell } => {
            let mut cmd = Cli::command();
            generate(*shell, &mut cmd, "roke", &mut io::stdout());
        }
        SubCommand::Mcp { read_only } => {
            let server = McpServer::with_options(path, cfg.clone(), *read_only);
            server
                .serve_stdio()
                .map_err(|e| format!("MCP server I/O error: {e}"))?;
        }
    }

    Ok(())
}

pub fn format_error(msg: &str, color_choice: ColorChoice) -> String {
    if color_choice.should_color_stderr() {
        let prefix = ContentStyle {
            foreground_color: Some(crossterm::style::Color::Red),
            attributes: Attribute::Bold.into(),
            ..Default::default()
        }
        .apply("error:");
        format!("{prefix} {msg}")
    } else {
        format!("error: {msg}")
    }
}

fn print_json<T: serde::Serialize>(val: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(val).map_err(|e| e.to_string())?;
    println!("{text}");
    Ok(())
}

pub struct Painter {
    pub theme: Theme,
    pub enabled: bool,
}

impl Painter {
    pub fn new(theme: Theme, enabled: bool) -> Self {
        Self { theme, enabled }
    }

    fn style(&self, text: &str, color: Color, bold: bool, dim: bool, strike: bool) -> String {
        if !self.enabled {
            return text.to_string();
        }
        let mut style = ContentStyle {
            foreground_color: Some(to_crossterm_color(color)),
            ..Default::default()
        };
        if bold {
            style.attributes.set(Attribute::Bold);
        }
        if dim {
            style.attributes.set(Attribute::Dim);
        }
        if strike {
            style.attributes.set(Attribute::CrossedOut);
        }
        StyledContent::new(style, text).to_string()
    }

    fn colored(&self, text: &str, color: Color) -> String {
        self.style(text, color, false, false, false)
    }

    fn badge(&self, glyph: &str, color: Color) -> String {
        self.style(glyph, color, true, false, false)
    }

    fn muted(&self, text: &str) -> String {
        self.style(text, self.theme.muted, false, false, false)
    }

    fn subtle(&self, text: &str) -> String {
        self.style(text, self.theme.subtle, false, false, false)
    }

    fn strikethrough(&self, text: &str) -> String {
        self.style(text, self.theme.muted, false, true, true)
    }

    fn status_color(&self, status: Status) -> Color {
        match status {
            Status::Todo => self.theme.todo,
            Status::InProgress => self.theme.in_progress,
            Status::Done => self.theme.done,
        }
    }

    fn priority_icon_and_color(&self, priority: Priority) -> (&'static str, Color) {
        match priority {
            Priority::High => ("▲", self.theme.priority_high),
            Priority::Medium => ("•", self.theme.priority_medium),
            Priority::Low => ("·", self.theme.priority_low),
        }
    }

    pub fn format_stats_oneline(&self, stats: &ops::BoardStats) -> String {
        let mut parts: Vec<String> = Vec::new();
        if stats.todo > 0 {
            parts.push(self.colored(&format!("○{}", stats.todo), self.theme.todo));
        }
        if stats.in_progress > 0 {
            parts.push(self.colored(&format!("◐{}", stats.in_progress), self.theme.in_progress));
        }
        if stats.overdue > 0 {
            parts.push(self.style(
                &format!("!{}", stats.overdue),
                self.theme.error,
                true,
                false,
                false,
            ));
        }
        if stats.due_today > 0 {
            parts.push(self.colored(&format!("@{}", stats.due_today), self.theme.in_progress));
        }
        if parts.is_empty() {
            self.badge("✓", self.theme.done)
        } else {
            parts.join(" ")
        }
    }

    pub fn format_task_line(&self, task: &Task, eff_tags: &[String], id_width: usize) -> String {
        let raw_id = format!("#{}", task.id);
        let id_str = if id_width > raw_id.len() {
            format!("{raw_id:>id_width$}")
        } else {
            raw_id
        };
        let id_styled = self.muted(&id_str);
        let status_styled = self.badge(task.status.glyph(), self.status_color(task.status));

        let (prio_icon, prio_color) = self.priority_icon_and_color(task.priority);
        let prio_styled = if task.status == Status::Done {
            self.muted("·")
        } else {
            self.style(
                prio_icon,
                prio_color,
                task.priority == Priority::High,
                false,
                false,
            )
        };

        let title_styled = if task.status == Status::Done {
            self.strikethrough(&task.title)
        } else if task.priority == Priority::High {
            self.style(&task.title, self.theme.text, true, false, false)
        } else {
            self.colored(&task.title, self.theme.text)
        };

        let tags_str = if eff_tags.is_empty() {
            String::new()
        } else {
            let joined = eff_tags
                .iter()
                .map(|t| self.colored(&format!("#{t}"), self.theme.accent))
                .collect::<Vec<_>>()
                .join(" ");
            format!("  {joined}")
        };

        let due_str = if let Some(due) = task.due_display() {
            let today = today_ymd();
            let color = match task.due_state(&today) {
                Some(DueState::Overdue) => self.theme.error,
                Some(DueState::Today) => self.theme.in_progress,
                _ => self.theme.muted,
            };
            format!("  {}", self.colored(&format!("due:{due}"), color))
        } else {
            String::new()
        };

        let repeat_str = if let Some(rep) = &task.repeat {
            format!("  {}", self.colored(&format!("↻ {rep}"), self.theme.subtle))
        } else {
            String::new()
        };

        format!("{id_styled} {status_styled} {prio_styled} {title_styled}{due_str}{repeat_str}{tags_str}")
    }

    pub fn format_task_detail(&self, task: &Task, eff_tags: &[String]) -> String {
        let mut out = String::new();
        let status_color = self.status_color(task.status);
        let (prio_icon, prio_color) = self.priority_icon_and_color(task.priority);

        let header_title = if task.status == Status::Done {
            self.strikethrough(&task.title)
        } else {
            self.style(&task.title, self.theme.text, true, false, false)
        };
        out.push_str(&format!(
            "{} {} {}\n",
            self.badge(task.status.glyph(), status_color),
            self.muted(&format!("#{}", task.id)),
            header_title
        ));
        out.push_str(&format!(
            "  {} {}\n",
            self.muted("uid:     "),
            self.subtle(&task.uid)
        ));
        out.push_str(&format!(
            "  {} {}\n",
            self.muted("status:  "),
            self.colored(
                &format!("{} {}", task.status.glyph(), task.status.label()),
                status_color
            )
        ));
        out.push_str(&format!(
            "  {} {}\n",
            self.muted("priority:"),
            self.colored(
                &format!("{prio_icon} {}", task.priority.label()),
                prio_color
            )
        ));
        if let Some(start) = task.start_display() {
            out.push_str(&format!(
                "  {} {}\n",
                self.muted("start:   "),
                self.subtle(&start)
            ));
        }
        if let Some(due) = task.due_display() {
            let today = today_ymd();
            let color = match task.due_state(&today) {
                Some(DueState::Overdue) => self.theme.error,
                Some(DueState::Today) => self.theme.in_progress,
                _ => self.theme.subtle,
            };
            out.push_str(&format!(
                "  {} {}\n",
                self.muted("due:     "),
                self.colored(&due, color)
            ));
        }
        if let Some(rep) = &task.repeat {
            out.push_str(&format!(
                "  {} {}\n",
                self.muted("repeat:  "),
                self.colored(&format!("↻ {rep}"), self.theme.subtle)
            ));
        }
        if !task.reminders.is_empty() {
            out.push_str(&format!(
                "  {} {}\n",
                self.muted("remind:  "),
                self.subtle(&task.reminders.join(", "))
            ));
        }
        if let Some(pct) = task.percent_complete {
            out.push_str(&format!(
                "  {} {}\n",
                self.muted("progress:"),
                self.colored(&format!("{pct}%"), self.theme.accent)
            ));
        }
        if let Some(parent) = &task.parent_uid {
            out.push_str(&format!(
                "  {} {}\n",
                self.muted("parent:  "),
                self.subtle(parent)
            ));
        }
        if let Some(loc) = &task.location {
            out.push_str(&format!(
                "  {} {}\n",
                self.muted("location:"),
                self.subtle(loc)
            ));
        }
        if let Some(u) = &task.url {
            out.push_str(&format!(
                "  {} {}\n",
                self.muted("url:     "),
                self.colored(u, self.theme.accent)
            ));
        }
        if !eff_tags.is_empty() {
            let tags = eff_tags
                .iter()
                .map(|t| self.colored(&format!("#{t}"), self.theme.accent))
                .collect::<Vec<_>>()
                .join(" ");
            out.push_str(&format!("  {} {tags}\n", self.muted("tags:    ")));
        }
        if let Some(origin) = &task.origin {
            out.push_str(&format!(
                "  {} {}\n",
                self.muted("origin:  "),
                self.subtle(origin)
            ));
        }
        if !task.description.is_empty() {
            let lines: Vec<&str> = task.description.lines().collect();
            if lines.len() <= 1 {
                out.push_str(&format!(
                    "  {} {}\n",
                    self.muted("notes:   "),
                    self.colored(&task.description, self.theme.text)
                ));
            } else {
                out.push_str(&format!("  {}\n", self.muted("notes:")));
                for line in lines {
                    out.push_str(&format!("    {}\n", self.colored(line, self.theme.text)));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn parses_global_options_before_or_after_subcommand() {
        let cli = Cli::try_parse_from([
            "roke",
            "add",
            "Ship",
            "MCP",
            "-p",
            "high",
            "-t",
            "work,ai",
            "--file",
            "/tmp/a.jsonl",
            "--json",
            "--color",
            "never",
        ])
        .unwrap();
        assert_eq!(cli.storage_path(), PathBuf::from("/tmp/a.jsonl"));
        assert!(cli.json);
        assert_eq!(cli.color, ColorChoice::Never);
        match cli.command {
            Some(SubCommand::Add(args)) => {
                assert!(!args.from_stdin);
                assert_eq!(args.title.join(" "), "Ship MCP");
                assert_eq!(args.priority, Priority::High);
                assert_eq!(args.tags, vec!["work,ai"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn painter_respects_color_toggle_and_icons() {
        let task = Task {
            id: 4,
            uid: "local-1".into(),
            title: "Review server details".into(),
            description: String::new(),
            status: Status::Todo,
            priority: Priority::High,
            tags: vec!["work".into()],
            due: None,
            due_time: None,
            start: None,
            start_time: None,
            repeat: None,
            reminders: Vec::new(),
            parent_uid: None,
            location: None,
            url: None,
            percent_complete: None,
            created_at: Some(1),
            completed_at: None,
            updated_at: 1,
            origin: Some("local".into()),
            order: None,
        };

        let plain = Painter::new(Theme::default(), false);
        let line_plain = plain.format_task_line(&task, &task.tags, 0);
        assert_eq!(line_plain, "#4 ○ ▲ Review server details  #work");
        assert!(!line_plain.contains('\x1b'));

        let colored = Painter::new(Theme::default(), true);
        let line_colored = colored.format_task_line(&task, &task.tags, 0);
        assert!(line_colored.contains('\x1b'));
        assert!(line_colored.contains("Review server details"));
        assert!(line_colored.contains("#work"));

        let stats = ops::BoardStats {
            total: 5,
            open: 4,
            todo: 3,
            in_progress: 1,
            done: 1,
            archived: 0,
            overdue: 2,
            due_today: 1,
            recurring: 0,
            high_priority_open: 1,
            completed_last_7_days: 1,
            top_tags: Vec::new(),
        };
        assert_eq!(plain.format_stats_oneline(&stats), "○3 ◐1 !2 @1");
        let clear = ops::BoardStats {
            total: 1,
            open: 0,
            todo: 0,
            in_progress: 0,
            done: 1,
            archived: 0,
            overdue: 0,
            due_today: 0,
            recurring: 0,
            high_priority_open: 0,
            completed_last_7_days: 1,
            top_tags: Vec::new(),
        };
        assert_eq!(plain.format_stats_oneline(&clear), "✓");
    }

    #[test]
    fn parses_stdin_json_array_and_jsonl() {
        let defaults = AddTaskInput {
            priority: Priority::High,
            tags: vec!["agent".into()],
            ..AddTaskInput::default()
        };
        let arr = parse_stdin_tasks(
            r#"[{"title":"One"},{"title":"Two","status":"in-progress","tags":["ops"]}]"#,
            &defaults,
        )
        .unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].priority, Priority::High);
        assert_eq!(arr[1].status, Status::InProgress);
        assert_eq!(arr[1].tags, vec!["agent", "ops"]);

        let jsonl = parse_stdin_tasks(
            "{\"title\":\"Line A\"}\n{\"title\":\"Line B\",\"priority\":\"low\"}\n",
            &defaults,
        )
        .unwrap();
        assert_eq!(jsonl.len(), 2);
        assert_eq!(jsonl[1].priority, Priority::Low);
    }

    #[test]
    fn end_to_end_cli_workflow() {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("roke_cli_{}_{seq}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join("todos.jsonl");
        let file_str = file.to_string_lossy().to_string();
        let cfg = AppConfig::default();

        let run = |argv: &[&str]| {
            let cli = Cli::try_parse_from(argv).unwrap();
            let cmd = cli.command.as_ref().unwrap();
            run_subcommand(&cli, cmd, &cfg).unwrap();
        };

        run(&[
            "roke",
            "-f",
            &file_str,
            "add",
            "Draft RFC",
            "-d",
            "Write design doc",
            "-p",
            "high",
            "-t",
            "docs",
        ]);
        run(&["roke", "-f", &file_str, "move", "1", "in-progress"]);

        let task = ops::get_task(&file, &cfg, "1").unwrap();
        assert_eq!(task.title, "Draft RFC");
        assert_eq!(task.status, Status::InProgress);

        run(&[
            "roke",
            "-f",
            &file_str,
            "edit",
            &task.uid,
            "--add-tag",
            "urgent",
            "--title",
            "Finalize RFC",
        ]);
        run(&["roke", "-f", &file_str, "done", "1"]);

        run(&[
            "roke",
            "-f",
            &file_str,
            "notes",
            "1",
            "--set",
            "# Plan\n- [x] Ship notes",
        ]);
        let with_notes = ops::get_task(&file, &cfg, "1").unwrap();
        assert_eq!(with_notes.description, "# Plan\n- [x] Ship notes");

        let updated = ops::get_task(&file, &cfg, "1").unwrap();
        assert_eq!(updated.title, "Finalize RFC");
        assert_eq!(updated.status, Status::Done);
        assert_eq!(updated.tags, vec!["docs", "urgent"]);

        run(&["roke", "-f", &file_str, "rm", "1"]);
        assert!(ops::list_tasks(&file, &cfg, &TaskFilter::default())
            .unwrap()
            .is_empty());

        // Multi-ID batch move, done, and rm
        run(&["roke", "-f", &file_str, "add", "Batch A"]);
        run(&["roke", "-f", &file_str, "add", "Batch B"]);
        run(&["roke", "-f", &file_str, "move", "1", "2", "in-progress"]);
        assert_eq!(
            ops::get_task(&file, &cfg, "1").unwrap().status,
            Status::InProgress
        );
        assert_eq!(
            ops::get_task(&file, &cfg, "2").unwrap().status,
            Status::InProgress
        );
        run(&["roke", "-f", &file_str, "done", "1", "2"]);
        assert_eq!(
            ops::get_task(&file, &cfg, "1").unwrap().status,
            Status::Done
        );
        assert_eq!(
            ops::get_task(&file, &cfg, "2").unwrap().status,
            Status::Done
        );
        run(&["roke", "-f", &file_str, "rm", "1", "2"]);
        assert!(ops::list_tasks(&file, &cfg, &TaskFilter::default())
            .unwrap()
            .is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn generates_shell_completions_without_error() {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut cmd, "roke", &mut buf);
        let script = String::from_utf8(buf).unwrap();
        assert!(script.contains("#compdef roke"));
        assert!(script.contains("archive"));
        assert!(script.contains("completions"));
    }
}

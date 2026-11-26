use crate::config::{default_config_path, AppConfig};
use crate::editor;
use crate::mcp::McpServer;
use crate::model::{
    default_storage_path, find_local_board, parse_due_date, parse_quick_add, parse_repeat,
    parse_tags, today_ymd, DueState, Priority, SortOrder, Status, Task, TaskStore,
    LOCAL_BOARD_FILENAME,
};
use crate::ops::{self, AddTaskInput, SyncReport, TagCount, TaskFilter, UpdateTaskInput};
use crate::theme::{to_crossterm_color, Theme};
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use crossterm::style::{Attribute, ContentStyle, StyledContent};
use ratatui::style::Color;
use serde::Deserialize;
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;

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

    /// Use a project-local .roke.jsonl board in the current or nearest parent directory
    #[arg(long = "local", global = true, conflicts_with = "global_board")]
    pub local: bool,

    /// Force the global board even when inside a directory containing .roke.jsonl
    #[arg(long = "global", global = true, conflicts_with = "local")]
    pub global_board: bool,

    /// Path to TOML config file (theme + SSH sync)
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

impl Cli {
    pub fn storage_path(&self) -> PathBuf {
        if let Some(file) = &self.file {
            return file.clone();
        }
        if !self.global_board {
            if let Ok(cwd) = std::env::current_dir() {
                if let Some(found) = find_local_board(&cwd) {
                    return found;
                }
                if self.local {
                    return cwd.join(LOCAL_BOARD_FILENAME);
                }
            }
        }
        default_storage_path()
    }

    pub fn config_path(&self) -> PathBuf {
        self.config.clone().unwrap_or_else(default_config_path)
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

    /// Sync tasks over SSH with peers configured in config.toml
    Sync,

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

    /// Optional due date (YYYY-MM-DD, today, tomorrow, +3d, +2w, fri, eow, eom)
    #[arg(long = "due", value_parser = parse_due_date)]
    pub due: Option<String>,

    /// Optional recurrence rule (daily, weekdays, weekly, monthly, every 3d/2w/2m)
    #[arg(long = "repeat", value_parser = parse_repeat)]
    pub repeat: Option<String>,

    /// Override task origin (defaults to local device_name)
    #[arg(long = "origin")]
    pub origin: Option<String>,

    /// Parse quick-add shorthand in title (#tag !high @tomorrow ~weekly)
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

    /// Set due date (YYYY-MM-DD, today, tomorrow, +3d, +2w, fri, eow, eom)
    #[arg(long = "due", value_parser = parse_due_date)]
    pub due: Option<String>,

    /// Clear the task's due date
    #[arg(long = "clear-due")]
    pub clear_due: bool,

    /// Set recurrence rule (daily, weekdays, weekly, monthly, every 3d/2w/2m)
    #[arg(long = "repeat", value_parser = parse_repeat)]
    pub repeat: Option<String>,

    /// Clear the task's recurrence rule
    #[arg(long = "clear-repeat")]
    pub clear_repeat: bool,

    /// Open the task's Markdown notes in $EDITOR (nvim, etc.)
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
    repeat: Option<String>,
    #[serde(default)]
    origin: Option<String>,
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
        let repeat = spec.repeat.or_else(|| defaults.repeat.clone());
        let origin = spec.origin.or_else(|| defaults.origin.clone());
        out.push(AddTaskInput {
            title: spec.title,
            description,
            status,
            priority,
            tags,
            due,
            repeat,
            origin,
        });
    }
    Ok(out)
}

pub fn run_subcommand(cli: &Cli, cmd: &SubCommand, cfg: &AppConfig) -> Result<(), String> {
    let path = cli.storage_path();
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
            let tasks = ops::list_tasks(&path, cfg, &filter)?;
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
                for task in &tasks {
                    let eff_tags = ops::effective_tags_for(task, &cfg.sync);
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
                repeat: args.repeat.clone(),
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
            let mut description = args.description.clone();
            if args.open_editor {
                let existing = ops::get_task(&path, cfg, &args.selector)?;
                let seed = description.as_deref().unwrap_or(&existing.description);
                if let Some(updated_md) =
                    editor::edit_markdown_in_editor(seed, &existing.title, cfg.editor.as_deref())?
                {
                    description = Some(updated_md);
                } else if args.title.is_none()
                    && args.status.is_none()
                    && args.priority.is_none()
                    && args.tags.is_none()
                    && args.add_tags.is_empty()
                    && args.remove_tags.is_empty()
                    && args.due.is_none()
                    && !args.clear_due
                    && args.repeat.is_none()
                    && !args.clear_repeat
                {
                    if json {
                        print_json(&existing)?;
                    } else {
                        println!("{}", painter.muted("No changes."));
                    }
                    return Ok(());
                }
            }
            let due_update = if args.clear_due {
                Some(None)
            } else {
                args.due.clone().map(Some)
            };
            let repeat_update = if args.clear_repeat {
                Some(None)
            } else {
                args.repeat.clone().map(Some)
            };
            let input = UpdateTaskInput {
                title: args.title.clone(),
                description,
                status: args.status,
                priority: args.priority,
                tags: args.tags.as_deref().map(parse_tags),
                add_tags: args.add_tags.iter().flat_map(|t| parse_tags(t)).collect(),
                remove_tags: args
                    .remove_tags
                    .iter()
                    .flat_map(|t| parse_tags(t))
                    .collect(),
                due: due_update,
                repeat: repeat_update,
            };
            if input.is_empty() {
                return Err("no changes specified. Pass at least one of --title, --desc, --status, --priority, --tags, --add-tag, --rm-tag, --due, --clear-due, --repeat, --clear-repeat, --editor.".into());
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

            let new_notes: Option<String> = if args.clear {
                Some(String::new())
            } else if let Some(text) = &args.set {
                Some(text.clone())
            } else if args.from_stdin {
                let mut buf = String::new();
                io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| format!("failed to read stdin: {e}"))?;
                Some(buf)
            } else {
                editor::edit_markdown_in_editor(
                    &existing.description,
                    &existing.title,
                    cfg.editor.as_deref(),
                )?
            };

            let Some(notes) = new_notes else {
                if json {
                    print_json(&existing)?;
                } else {
                    println!("{}", painter.muted("Notes unchanged."));
                }
                return Ok(());
            };

            let updated = ops::update_task(
                &path,
                cfg,
                &args.selector,
                UpdateTaskInput {
                    description: Some(notes),
                    ..UpdateTaskInput::default()
                },
            )?;
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
        SubCommand::Sync => {
            let report: SyncReport = ops::sync_tasks(&path, cfg)?;
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

        let due_str = if let Some(due) = &task.due {
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
        if let Some(due) = &task.due {
            let today = today_ymd();
            let color = match task.due_state(&today) {
                Some(DueState::Overdue) => self.theme.error,
                Some(DueState::Today) => self.theme.in_progress,
                _ => self.theme.subtle,
            };
            out.push_str(&format!(
                "  {} {}\n",
                self.muted("due:     "),
                self.colored(due, color)
            ));
        }
        if let Some(rep) = &task.repeat {
            out.push_str(&format!(
                "  {} {}\n",
                self.muted("repeat:  "),
                self.colored(&format!("↻ {rep}"), self.theme.subtle)
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
            repeat: None,
            created_at: Some(1),
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
        assert_eq!(ops::get_task(&file, &cfg, "1").unwrap().status, Status::InProgress);
        assert_eq!(ops::get_task(&file, &cfg, "2").unwrap().status, Status::InProgress);
        run(&["roke", "-f", &file_str, "done", "1", "2"]);
        assert_eq!(ops::get_task(&file, &cfg, "1").unwrap().status, Status::Done);
        assert_eq!(ops::get_task(&file, &cfg, "2").unwrap().status, Status::Done);
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

use crate::config::{default_config_path, AppConfig};
use crate::mcp::McpServer;
use crate::model::{default_storage_path, parse_tags, Priority, Status, Task};
use crate::ops::{
    self, AddTaskInput, SyncReport, TagCount, TaskFilter, UpdateTaskInput,
};
use serde::Deserialize;
use std::io::{self, Read};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct GlobalOptions {
    pub storage_path: PathBuf,
    pub config_path: PathBuf,
    pub json: bool,
}

#[derive(Debug, Clone)]
pub enum Command {
    Tui,
    PrintConfig,
    Version,
    Help(Option<String>),
    List(TaskFilter),
    Get { selector: String },
    Add {
        input: AddTaskInput,
        from_stdin: bool,
    },
    Edit {
        selector: String,
        input: UpdateTaskInput,
    },
    Move {
        selector: String,
        status: Status,
    },
    Done {
        selector: String,
    },
    Remove {
        selector: String,
    },
    Tags,
    Sync,
    Mcp,
}

#[derive(Debug, Clone)]
pub struct ParsedCli {
    pub global: GlobalOptions,
    pub command: Command,
}

pub fn parse_args(args: &[String]) -> Result<ParsedCli, String> {
    let mut storage_path = default_storage_path();
    let mut config_path = default_config_path();
    let mut json = false;

    // First pass: extract global options while preserving subcommand + rest
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    let mut after_double_dash = false;

    while i < args.len() {
        let arg = &args[i];
        if after_double_dash {
            rest.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            after_double_dash = true;
            rest.push(arg.clone());
            i += 1;
            continue;
        }
        match arg.as_str() {
            "-f" | "--file" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                storage_path = PathBuf::from(val);
            }
            "-c" | "--config" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                config_path = PathBuf::from(val);
            }
            "--json" => {
                json = true;
            }
            "--print-config" => {
                return Ok(ParsedCli {
                    global: GlobalOptions {
                        storage_path,
                        config_path,
                        json,
                    },
                    command: Command::PrintConfig,
                });
            }
            "-V" | "--version" => {
                return Ok(ParsedCli {
                    global: GlobalOptions {
                        storage_path,
                        config_path,
                        json,
                    },
                    command: Command::Version,
                });
            }
            _ => rest.push(arg.clone()),
        }
        i += 1;
    }

    let global = GlobalOptions {
        storage_path,
        config_path,
        json,
    };

    if rest.is_empty() {
        return Ok(ParsedCli {
            global,
            command: Command::Tui,
        });
    }

    let first = rest[0].as_str();
    let sub_args = &rest[1..];

    if first == "-h" || first == "--help" {
        return Ok(ParsedCli {
            global,
            command: Command::Help(None),
        });
    }
    if first == "help" {
        let topic = sub_args.first().cloned();
        return Ok(ParsedCli {
            global,
            command: Command::Help(topic),
        });
    }

    // Per-subcommand help
    if sub_args
        .iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "-h" || a == "--help")
    {
        return Ok(ParsedCli {
            global,
            command: Command::Help(Some(first.to_string())),
        });
    }

    let command = match first {
        "list" | "ls" => parse_list_args(sub_args)?,
        "get" | "show" => parse_single_selector(first, sub_args)
            .map(|selector| Command::Get { selector })?,
        "add" | "new" => parse_add_args(sub_args)?,
        "edit" | "update" => parse_edit_args(sub_args)?,
        "move" | "mv" => parse_move_args(sub_args)?,
        "done" => parse_single_selector("done", sub_args)
            .map(|selector| Command::Done { selector })?,
        "rm" | "delete" | "del" => parse_single_selector(first, sub_args)
            .map(|selector| Command::Remove { selector })?,
        "tags" => {
            if !sub_args.is_empty() {
                return Err(format!("unexpected argument for 'tags': {}", sub_args[0]));
            }
            Command::Tags
        }
        "sync" => {
            if !sub_args.is_empty() {
                return Err(format!("unexpected argument for 'sync': {}", sub_args[0]));
            }
            Command::Sync
        }
        "mcp" => {
            if !sub_args.is_empty() {
                return Err(format!("unexpected argument for 'mcp': {}", sub_args[0]));
            }
            Command::Mcp
        }
        other => {
            return Err(format!(
                "unknown command or option '{other}'. Run `roke --help` for usage."
            ));
        }
    };

    Ok(ParsedCli { global, command })
}

fn parse_single_selector(cmd: &str, args: &[String]) -> Result<String, String> {
    let mut positional = Vec::new();
    let mut after_dd = false;
    for arg in args {
        if !after_dd && arg == "--" {
            after_dd = true;
            continue;
        }
        if !after_dd && arg.starts_with('-') {
            return Err(format!("unknown option '{arg}' for '{cmd}'"));
        }
        positional.push(arg.clone());
    }
    match positional.len() {
        1 => Ok(positional.remove(0)),
        0 => Err(format!("missing task <id|uid> for '{cmd}'")),
        _ => Err(format!(
            "too many arguments for '{cmd}' (expected one <id|uid>)"
        )),
    }
}

fn parse_list_args(args: &[String]) -> Result<Command, String> {
    let mut filter = TaskFilter::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-s" | "--status" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                filter.status = Some(Status::parse(val)?);
            }
            "-p" | "--priority" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                filter.priority = Some(Priority::parse(val)?);
            }
            "-t" | "--tag" | "--tags" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                filter.tags.extend(parse_tags(val));
            }
            "--any-tag" | "--or" => {
                filter.tag_match_all = false;
            }
            "-q" | "--search" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                filter.search = Some(val.clone());
            }
            other => {
                return Err(format!("unknown option '{other}' for 'list'"));
            }
        }
        i += 1;
    }
    Ok(Command::List(filter))
}

fn parse_add_args(args: &[String]) -> Result<Command, String> {
    let mut input = AddTaskInput::default();
    let mut from_stdin = false;
    let mut title_parts: Vec<String> = Vec::new();
    let mut after_dd = false;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if after_dd {
            title_parts.push(arg.clone());
            i += 1;
            continue;
        }
        match arg.as_str() {
            "--" => {
                after_dd = true;
            }
            "--stdin" => {
                from_stdin = true;
            }
            "-d" | "--desc" | "--description" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.description = val.clone();
            }
            "-s" | "--status" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.status = Status::parse(val)?;
            }
            "-p" | "--priority" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.priority = Priority::parse(val)?;
            }
            "-t" | "--tag" | "--tags" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.tags.extend(parse_tags(val));
            }
            "--origin" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.origin = Some(val.clone());
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option '{other}' for 'add'"));
            }
            word => title_parts.push(word.to_string()),
        }
        i += 1;
    }

    input.title = title_parts.join(" ").trim().to_string();
    if !from_stdin && input.title.is_empty() {
        return Err("missing task title. Usage: roke add <title> [OPTIONS]".into());
    }

    Ok(Command::Add { input, from_stdin })
}

fn parse_edit_args(args: &[String]) -> Result<Command, String> {
    let mut selector: Option<String> = None;
    let mut input = UpdateTaskInput::default();
    let mut after_dd = false;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if after_dd {
            if selector.is_none() {
                selector = Some(arg.clone());
            } else {
                return Err(format!("unexpected positional argument '{arg}' for 'edit'"));
            }
            i += 1;
            continue;
        }
        match arg.as_str() {
            "--" => after_dd = true,
            "--title" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.title = Some(val.clone());
            }
            "-d" | "--desc" | "--description" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.description = Some(val.clone());
            }
            "-s" | "--status" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.status = Some(Status::parse(val)?);
            }
            "-p" | "--priority" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.priority = Some(Priority::parse(val)?);
            }
            "-t" | "--tags" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.tags = Some(parse_tags(val));
            }
            "--add-tag" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.add_tags.extend(parse_tags(val));
            }
            "--rm-tag" | "--remove-tag" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                input.remove_tags.extend(parse_tags(val));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option '{other}' for 'edit'"));
            }
            pos => {
                if selector.is_none() {
                    selector = Some(pos.to_string());
                } else {
                    return Err(format!("unexpected positional argument '{pos}' for 'edit'"));
                }
            }
        }
        i += 1;
    }

    let selector = selector
        .ok_or_else(|| "missing task <id|uid>. Usage: roke edit <id|uid> [OPTIONS]".to_string())?;
    if input.is_empty() {
        return Err("no changes specified. Pass at least one of --title, --desc, --status, --priority, --tags, --add-tag, --rm-tag.".into());
    }

    Ok(Command::Edit { selector, input })
}

fn parse_move_args(args: &[String]) -> Result<Command, String> {
    let mut positional = Vec::new();
    let mut after_dd = false;
    for arg in args {
        if !after_dd && arg == "--" {
            after_dd = true;
            continue;
        }
        if !after_dd && arg.starts_with('-') {
            return Err(format!("unknown option '{arg}' for 'move'"));
        }
        positional.push(arg.clone());
    }
    if positional.len() != 2 {
        return Err("Usage: roke move <id|uid> <todo|in-progress|done>".into());
    }
    let selector = positional.remove(0);
    let status = Status::parse(&positional.remove(0))?;
    Ok(Command::Move { selector, status })
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
        let origin = spec.origin.or_else(|| defaults.origin.clone());
        out.push(AddTaskInput {
            title: spec.title,
            description,
            status,
            priority,
            tags,
            origin,
        });
    }
    Ok(out)
}

pub fn run_command(parsed: ParsedCli, cfg: &AppConfig) -> Result<(), String> {
    let path = &parsed.global.storage_path;
    let json = parsed.global.json;

    match parsed.command {
        Command::Tui => unreachable!("TUI is handled by main"),
        Command::PrintConfig => {
            println!("{}", AppConfig::default_template_toml());
        }
        Command::Version => {
            println!("roke {}", env!("CARGO_PKG_VERSION"));
        }
        Command::Help(topic) => {
            print!("{}", help_text(topic.as_deref()));
        }
        Command::List(filter) => {
            let tasks = ops::list_tasks(path, cfg, &filter)?;
            if json {
                print_json(&tasks)?;
            } else if tasks.is_empty() {
                println!("No tasks found.");
            } else {
                for task in &tasks {
                    println!("{}", format_task_line(task));
                }
            }
        }
        Command::Get { selector } => {
            let task = ops::get_task(path, cfg, &selector)?;
            if json {
                print_json(&task)?;
            } else {
                print!("{}", format_task_detail(&task));
            }
        }
        Command::Add { input, from_stdin } => {
            if from_stdin {
                let mut buf = String::new();
                io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| format!("failed to read stdin: {e}"))?;
                let batch = parse_stdin_tasks(&buf, &input)?;
                let created = ops::add_tasks_batch(path, cfg, batch)?;
                if json {
                    print_json(&created)?;
                } else {
                    for task in &created {
                        println!("Added {}", format_task_line(task));
                    }
                }
            } else {
                let task = ops::add_task(path, cfg, input)?;
                if json {
                    print_json(&task)?;
                } else {
                    println!("Added {}", format_task_line(&task));
                }
            }
        }
        Command::Edit { selector, input } => {
            let task = ops::update_task(path, cfg, &selector, input)?;
            if json {
                print_json(&task)?;
            } else {
                println!("Updated {}", format_task_line(&task));
            }
        }
        Command::Move { selector, status } => {
            let task = ops::update_task(
                path,
                cfg,
                &selector,
                UpdateTaskInput {
                    status: Some(status),
                    ..UpdateTaskInput::default()
                },
            )?;
            if json {
                print_json(&task)?;
            } else {
                println!("Moved {}", format_task_line(&task));
            }
        }
        Command::Done { selector } => {
            let task = ops::update_task(
                path,
                cfg,
                &selector,
                UpdateTaskInput {
                    status: Some(Status::Done),
                    ..UpdateTaskInput::default()
                },
            )?;
            if json {
                print_json(&task)?;
            } else {
                println!("Done {}", format_task_line(&task));
            }
        }
        Command::Remove { selector } => {
            let removed = ops::delete_task(path, cfg, &selector)?;
            if json {
                print_json(&removed)?;
            } else {
                println!("Deleted #{} {}", removed.id, removed.title);
            }
        }
        Command::Tags => {
            let tags = ops::list_tags(path, cfg)?;
            if json {
                print_json(&tags)?;
            } else if tags.is_empty() {
                println!("No tags.");
            } else {
                for TagCount { tag, count } in tags {
                    println!("#{tag}\t{count}");
                }
            }
        }
        Command::Sync => {
            let report: SyncReport = ops::sync_tasks(path, cfg)?;
            if json {
                print_json(&report)?;
            } else {
                let noun = if report.synced_peers == 1 {
                    "peer"
                } else {
                    "peers"
                };
                println!(
                    "Synced {} {noun} ({} tasks total)",
                    report.synced_peers, report.total_tasks
                );
                for err in &report.errors {
                    eprintln!("Sync error: {err}");
                }
            }
            if !report.errors.is_empty() && report.synced_peers == 0 {
                return Err(report.errors.join("; "));
            }
        }
        Command::Mcp => {
            let server = McpServer::new(path.clone(), cfg.clone());
            server
                .serve_stdio()
                .map_err(|e| format!("MCP server I/O error: {e}"))?;
        }
    }

    Ok(())
}

fn print_json<T: serde::Serialize>(val: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(val).map_err(|e| e.to_string())?;
    println!("{text}");
    Ok(())
}

fn format_task_line(task: &Task) -> String {
    let tags = if task.tags.is_empty() {
        String::new()
    } else {
        let joined = task
            .tags
            .iter()
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!("  {joined}")
    };
    format!(
        "#{} {} {} {}{}",
        task.id,
        task.status.glyph(),
        task.priority.dot(),
        task.title,
        tags
    )
}

fn format_task_detail(task: &Task) -> String {
    let mut out = String::new();
    out.push_str(&format!("#{} {}\n", task.id, task.title));
    out.push_str(&format!("  uid:      {}\n", task.uid));
    out.push_str(&format!("  status:   {}\n", task.status.label()));
    out.push_str(&format!("  priority: {}\n", task.priority.label()));
    if !task.tags.is_empty() {
        let tags = task
            .tags
            .iter()
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("  tags:     {tags}\n"));
    }
    if let Some(origin) = &task.origin {
        out.push_str(&format!("  origin:   {origin}\n"));
    }
    if !task.description.is_empty() {
        out.push_str(&format!("  desc:     {}\n", task.description));
    }
    out
}

pub fn help_text(topic: Option<&str>) -> &'static str {
    match topic {
        Some("list" | "ls") => {
            "Usage: roke list [OPTIONS]\n\n\
             List tasks from the store.\n\n\
             Options:\n\
               -s, --status <STATUS>      Filter by status (todo, in-progress, done)\n\
               -p, --priority <PRIORITY>  Filter by priority (low, medium, high)\n\
               -t, --tag <TAGS>           Filter by tag(s); repeatable or comma-separated\n\
                   --any-tag              Match any tag (OR) instead of all tags (AND)\n\
               -q, --search <QUERY>       Case-insensitive search in title, description, tags\n\
                   --json                 Output JSON\n"
        }
        Some("get" | "show") => {
            "Usage: roke get <ID|UID> [--json]\n\n\
             Show details for a single task by numeric ID or UID.\n"
        }
        Some("add" | "new") => {
            "Usage: roke add <TITLE> [OPTIONS]\n\
                    roke add --stdin [OPTIONS]\n\n\
             Create one or more tasks.\n\n\
             Options:\n\
               -d, --desc <TEXT>          Task description\n\
               -s, --status <STATUS>      Initial status (default: todo)\n\
               -p, --priority <PRIORITY>  Priority: low, medium, high (default: medium)\n\
               -t, --tag <TAGS>           Tag(s), comma-separated or repeated\n\
                   --origin <NAME>        Override task origin (defaults to device_name)\n\
                   --stdin                Read JSON object, JSON array, or JSONL from stdin\n\
                   --json                 Output created task(s) as JSON\n"
        }
        Some("edit" | "update") => {
            "Usage: roke edit <ID|UID> [OPTIONS]\n\n\
             Update fields on an existing task.\n\n\
             Options:\n\
                   --title <TEXT>         New title\n\
               -d, --desc <TEXT>          New description\n\
               -s, --status <STATUS>      New status (todo, in-progress, done)\n\
               -p, --priority <PRIORITY>  New priority (low, medium, high)\n\
               -t, --tags <TAGS>          Replace all tags (empty string clears)\n\
                   --add-tag <TAGS>       Add tag(s) without removing existing ones\n\
                   --rm-tag <TAGS>        Remove tag(s)\n\
                   --json                 Output updated task as JSON\n"
        }
        Some("move" | "mv") => {
            "Usage: roke move <ID|UID> <todo|in-progress|done> [--json]\n\n\
             Move a task to a different status column.\n"
        }
        Some("done") => {
            "Usage: roke done <ID|UID> [--json]\n\n\
             Mark a task as Done.\n"
        }
        Some("rm" | "delete" | "del") => {
            "Usage: roke rm <ID|UID> [--json]\n\n\
             Delete a task and record a sync tombstone.\n"
        }
        Some("tags") => {
            "Usage: roke tags [--json]\n\n\
             List all tags and their task counts.\n"
        }
        Some("sync") => {
            "Usage: roke sync [--json]\n\n\
             Run two-way SSH sync with peers configured in config.toml.\n"
        }
        Some("mcp") => {
            "Usage: roke mcp [-f <PATH>] [-c <PATH>]\n\n\
             Run a Model Context Protocol (MCP) JSON-RPC 2.0 server over stdio.\n\
             Exposes tools: list_tasks, get_task, add_task, update_task, delete_task,\n\
             list_tags, sync_tasks — and resources: roke://tasks, roke://tags.\n"
        }
        _ => {
            "roke — Terminal Todo & Kanban board\n\n\
             Usage:\n\
               roke [GLOBAL OPTIONS]                      Launch interactive TUI\n\
               roke [GLOBAL OPTIONS] <COMMAND> [ARGS]     Run CLI command or MCP server\n\n\
             Commands:\n\
               list, ls       List tasks (filter by status, priority, tag, search)\n\
               get, show      Show a single task by ID or UID\n\
               add, new       Add a new task (or batch via --stdin)\n\
               edit, update   Edit fields or tags on a task\n\
               move, mv       Move a task to todo / in-progress / done\n\
               done           Mark a task as done\n\
               rm, delete     Delete a task (with sync tombstone)\n\
               tags           List tags and counts\n\
               sync           Sync tasks over SSH with configured peers\n\
               mcp            Run built-in MCP server over stdio for AI agents\n\n\
             Global Options:\n\
               -f, --file <PATH>     Path to JSONL task file\n\
               -c, --config <PATH>   Path to TOML config file (theme + SSH sync)\n\
                   --json            Output structured JSON (for CLI commands)\n\
                   --print-config    Print default config.toml to stdout\n\
               -V, --version         Print version\n\
               -h, --help            Show help (also `roke <command> --help`)\n"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn parses_global_options_before_or_after_subcommand() {
        let p = parse_args(&s(&[
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
        ]))
        .unwrap();
        assert_eq!(p.global.storage_path, PathBuf::from("/tmp/a.jsonl"));
        assert!(p.global.json);
        match p.command {
            Command::Add { input, from_stdin } => {
                assert!(!from_stdin);
                assert_eq!(input.title, "Ship MCP");
                assert_eq!(input.priority, Priority::High);
                assert_eq!(input.tags, vec!["work", "ai"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
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

        let p = parse_args(&s(&[
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
        ]))
        .unwrap();
        run_command(p, &cfg).unwrap();

        let p = parse_args(&s(&["-f", &file_str, "move", "1", "in-progress"])).unwrap();
        run_command(p, &cfg).unwrap();

        let task = ops::get_task(&file, &cfg, "1").unwrap();
        assert_eq!(task.title, "Draft RFC");
        assert_eq!(task.status, Status::InProgress);

        let p = parse_args(&s(&[
            "-f",
            &file_str,
            "edit",
            &task.uid,
            "--add-tag",
            "urgent",
            "--title",
            "Finalize RFC",
        ]))
        .unwrap();
        run_command(p, &cfg).unwrap();

        let p = parse_args(&s(&["-f", &file_str, "done", "1"])).unwrap();
        run_command(p, &cfg).unwrap();

        let updated = ops::get_task(&file, &cfg, "1").unwrap();
        assert_eq!(updated.title, "Finalize RFC");
        assert_eq!(updated.status, Status::Done);
        assert_eq!(updated.tags, vec!["docs", "urgent"]);

        let p = parse_args(&s(&["-f", &file_str, "rm", "1"])).unwrap();
        run_command(p, &cfg).unwrap();
        assert!(ops::list_tasks(&file, &cfg, &TaskFilter::default())
            .unwrap()
            .is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}

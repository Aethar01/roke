use crate::config::normalize_device_tag;
use crate::model::{
    parse_due_spec, parse_due_time, parse_reminders, parse_repeat, parse_tags, Priority, Status,
    Task,
};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static EDITOR_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Full task representation editable as YAML frontmatter + Markdown notes in `$EDITOR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDocument {
    pub title: String,
    pub description: String,
    pub status: Status,
    pub priority: Priority,
    pub tags: Vec<String>,
    pub due: Option<String>,
    pub due_time: Option<String>,
    pub start: Option<String>,
    pub start_time: Option<String>,
    pub repeat: Option<String>,
    pub reminders: Vec<String>,
    pub location: Option<String>,
    pub url: Option<String>,
    pub parent_uid: Option<String>,
    pub percent_complete: Option<u8>,
}

impl TaskDocument {
    pub fn from_task(task: &Task) -> Self {
        Self {
            title: task.title.clone(),
            description: task.description.clone(),
            status: task.status,
            priority: task.priority,
            tags: task.tags.clone(),
            due: task.due.clone(),
            due_time: task.due_time.clone(),
            start: task.start.clone(),
            start_time: task.start_time.clone(),
            repeat: task.repeat.clone(),
            reminders: task.reminders.clone(),
            location: task.location.clone(),
            url: task.url.clone(),
            parent_uid: task.parent_uid.clone(),
            percent_complete: task.percent_complete,
        }
    }

    pub fn apply_to_task(&self, task: &mut Task) {
        task.title = self.title.clone();
        task.description = self.description.clone();
        task.set_status(self.status);
        task.priority = self.priority;
        task.tags = self.tags.clone();
        task.due = self.due.clone();
        task.due_time = self.due_time.clone();
        task.start = self.start.clone();
        task.start_time = self.start_time.clone();
        task.repeat = self.repeat.clone();
        task.reminders = self.reminders.clone();
        task.location = self.location.clone();
        task.url = self.url.clone();
        task.parent_uid = self.parent_uid.clone();
        task.percent_complete = self.percent_complete;
    }

    /// Render the task as a Markdown document with YAML frontmatter.
    pub fn to_markdown(&self) -> String {
        let status_str = match self.status {
            Status::Todo => "todo",
            Status::InProgress => "in-progress",
            Status::Done => "done",
        };
        let prio_str = match self.priority {
            Priority::Low => "low",
            Priority::Medium => "medium",
            Priority::High => "high",
        };
        let due_str = match (&self.due, &self.due_time) {
            (Some(d), Some(t)) => format!("{d} {t}"),
            (Some(d), None) => d.clone(),
            _ => String::new(),
        };
        let start_str = match (&self.start, &self.start_time) {
            (Some(s), Some(t)) => format!("{s} {t}"),
            (Some(s), None) => s.clone(),
            _ => String::new(),
        };
        let tags_str = if self.tags.is_empty() {
            String::new()
        } else {
            format!("[{}]", self.tags.join(", "))
        };
        let reminders_str = if self.reminders.is_empty() {
            String::new()
        } else {
            format!("[{}]", self.reminders.join(", "))
        };

        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("title: {}\n", format_yaml_scalar(&self.title)));
        out.push_str(&format!("status: {status_str}\n"));
        out.push_str(&format!("priority: {prio_str}\n"));
        out.push_str(&format_opt_kv("tags", &tags_str));
        out.push_str(&format_opt_kv("due", &due_str));
        out.push_str(&format_opt_kv("start", &start_str));
        out.push_str(&format_opt_kv(
            "repeat",
            self.repeat.as_deref().unwrap_or(""),
        ));
        out.push_str(&format_opt_kv("reminders", &reminders_str));
        out.push_str(&format_opt_kv(
            "location",
            &self
                .location
                .as_deref()
                .map(format_yaml_scalar)
                .unwrap_or_default(),
        ));
        out.push_str(&format_opt_kv(
            "url",
            &self
                .url
                .as_deref()
                .map(format_yaml_scalar)
                .unwrap_or_default(),
        ));
        out.push_str(&format_opt_kv(
            "parent",
            self.parent_uid.as_deref().unwrap_or(""),
        ));
        out.push_str(&format_opt_kv(
            "progress",
            &self
                .percent_complete
                .map(|p| p.to_string())
                .unwrap_or_default(),
        ));
        out.push_str("---\n");

        let body = normalize_markdown(&self.description);
        if !body.is_empty() {
            out.push('\n');
            out.push_str(&body);
            out.push('\n');
        }
        out
    }

    /// Parse a Markdown document with optional YAML frontmatter back into a [`TaskDocument`].
    /// If no `---` frontmatter header is present, preserves metadata from `base` and replaces only `description`.
    pub fn from_markdown(raw: &str, base: &TaskDocument) -> Result<TaskDocument, String> {
        let normalized = raw
            .strip_prefix('\u{feff}')
            .unwrap_or(raw)
            .replace("\r\n", "\n");
        let lines: Vec<&str> = normalized.lines().collect();

        let first_non_empty = lines.iter().position(|l| !l.trim().is_empty());
        let Some(start_idx) = first_non_empty else {
            let mut out = base.clone();
            out.description.clear();
            return Ok(out);
        };

        if lines[start_idx].trim() != "---" {
            // Plain Markdown without frontmatter: update description only
            let mut out = base.clone();
            out.description = normalize_markdown(&normalized);
            return Ok(out);
        }

        let Some(end_rel) = lines[start_idx + 1..]
            .iter()
            .position(|l| matches!(l.trim(), "---" | "..."))
        else {
            return Err("unclosed YAML frontmatter (missing closing '---')".into());
        };
        let end_idx = start_idx + 1 + end_rel;
        let fm_lines = &lines[start_idx + 1..end_idx];

        let body_lines = &lines[end_idx + 1..];
        let body_trimmed = if body_lines.first().is_some_and(|l| l.trim().is_empty()) {
            &body_lines[1..]
        } else {
            body_lines
        };
        let description = normalize_markdown(&body_trimmed.join("\n"));

        // Parse simple YAML frontmatter (key: value, inline [a, b], or multi-line `- item` lists)
        let mut entries: Vec<(String, Vec<String>, bool)> = Vec::new(); // (key, values, is_list)
        let mut idx = 0usize;
        while idx < fm_lines.len() {
            let line = fm_lines[idx];
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                idx += 1;
                continue;
            }
            let Some((raw_key, raw_val)) = line.split_once(':') else {
                return Err(format!("invalid frontmatter line: '{trimmed}'"));
            };
            let key = raw_key.trim().to_ascii_lowercase();
            let val_trimmed = raw_val.trim();

            if val_trimmed.is_empty() {
                // Check for subsequent YAML list items (`- value`)
                let mut list_items = Vec::new();
                let mut j = idx + 1;
                while j < fm_lines.len() {
                    let next_trim = fm_lines[j].trim();
                    if let Some(item) = next_trim.strip_prefix("- ") {
                        list_items.push(unquote_yaml(item.trim()));
                        j += 1;
                    } else if next_trim == "-" || next_trim.is_empty() || next_trim.starts_with('#')
                    {
                        j += 1;
                    } else {
                        break;
                    }
                }
                if !list_items.is_empty() {
                    entries.push((key, list_items, true));
                    idx = j;
                } else {
                    entries.push((key, Vec::new(), false));
                    idx += 1;
                }
            } else if val_trimmed.starts_with('[') && val_trimmed.ends_with(']') {
                let inner = &val_trimmed[1..val_trimmed.len() - 1];
                let items = split_yaml_inline_list(inner);
                entries.push((key, items, true));
                idx += 1;
            } else {
                entries.push((key, vec![unquote_yaml(val_trimmed)], false));
                idx += 1;
            }
        }

        let mut doc = base.clone();
        doc.description = description;

        for (key, vals, _is_list) in entries {
            let scalar = vals.first().map(String::as_str).unwrap_or("").trim();
            match key.as_str() {
                "title" => {
                    if scalar.is_empty() {
                        return Err("frontmatter 'title' cannot be empty".into());
                    }
                    doc.title = scalar.to_string();
                }
                "status" => {
                    if !scalar.is_empty() {
                        doc.status = Status::parse(scalar)?;
                    }
                }
                "priority" => {
                    if !scalar.is_empty() {
                        doc.priority = Priority::parse(scalar)?;
                    }
                }
                "tags" => {
                    let mut tags = Vec::new();
                    for v in &vals {
                        for t in parse_tags(v) {
                            if !tags.contains(&t) {
                                tags.push(t);
                            }
                        }
                    }
                    doc.tags = tags;
                }
                "due" => {
                    if scalar.is_empty() || scalar.eq_ignore_ascii_case("null") {
                        doc.due = None;
                        doc.due_time = None;
                    } else {
                        let (d, t) = parse_due_spec(scalar)?;
                        doc.due = Some(d);
                        doc.due_time = t;
                    }
                }
                "due_time" => {
                    if scalar.is_empty() || scalar.eq_ignore_ascii_case("null") {
                        doc.due_time = None;
                    } else {
                        doc.due_time = Some(parse_due_time(scalar)?);
                    }
                }
                "start" => {
                    if scalar.is_empty() || scalar.eq_ignore_ascii_case("null") {
                        doc.start = None;
                        doc.start_time = None;
                    } else {
                        let (d, t) = parse_due_spec(scalar)?;
                        doc.start = Some(d);
                        doc.start_time = t;
                    }
                }
                "start_time" => {
                    if scalar.is_empty() || scalar.eq_ignore_ascii_case("null") {
                        doc.start_time = None;
                    } else {
                        doc.start_time = Some(parse_due_time(scalar)?);
                    }
                }
                "repeat" => {
                    if scalar.is_empty() || scalar.eq_ignore_ascii_case("null") {
                        doc.repeat = None;
                    } else {
                        doc.repeat = Some(parse_repeat(scalar)?);
                    }
                }
                "reminders" | "reminder" | "alarms" => {
                    doc.reminders = parse_reminders(&vals)?;
                }
                "location" => {
                    doc.location = (!scalar.is_empty() && !scalar.eq_ignore_ascii_case("null"))
                        .then(|| scalar.to_string());
                }
                "url" | "link" => {
                    doc.url = (!scalar.is_empty() && !scalar.eq_ignore_ascii_case("null"))
                        .then(|| scalar.to_string());
                }
                "parent" | "parent_uid" => {
                    doc.parent_uid = (!scalar.is_empty() && !scalar.eq_ignore_ascii_case("null"))
                        .then(|| scalar.to_string());
                }
                "progress" | "percent_complete" => {
                    if scalar.is_empty() || scalar.eq_ignore_ascii_case("null") {
                        doc.percent_complete = None;
                    } else {
                        let cleaned = scalar.trim_end_matches('%').trim();
                        let pct: u8 = cleaned.parse().map_err(|_| {
                            format!("invalid progress '{scalar}' (expected integer 0..100)")
                        })?;
                        if pct > 100 {
                            return Err(format!(
                                "invalid progress '{scalar}' (must be between 0 and 100)"
                            ));
                        }
                        doc.percent_complete = Some(pct);
                    }
                }
                _ => {
                    // Ignore unknown frontmatter keys so user-added annotations don't break saves
                }
            }
        }

        Ok(doc)
    }
}

fn format_opt_kv(key: &str, val: &str) -> String {
    if val.is_empty() {
        format!("{key}:\n")
    } else {
        format!("{key}: {val}\n")
    }
}

fn format_yaml_scalar(val: &str) -> String {
    let trimmed = val.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let needs_quote = trimmed.contains(": ")
        || trimmed.ends_with(':')
        || trimmed.contains(" #")
        || trimmed.starts_with([
            '#', '-', '?', '[', ']', '{', '}', '"', '\'', '&', '*', '!', '|', '>', '%', '@', '`',
        ])
        || trimmed.contains('\n');
    if needs_quote {
        let escaped = trimmed.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        trimmed.to_string()
    }
}

fn unquote_yaml(raw: &str) -> String {
    let s = raw.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(ch);
            }
        }
        return out;
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1].replace("''", "'");
    }
    s.to_string()
}

fn split_yaml_inline_list(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && in_double {
            cur.push(ch);
            escaped = true;
            continue;
        }
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                cur.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                cur.push(ch);
            }
            ',' if !in_single && !in_double => {
                let item = unquote_yaml(cur.trim());
                if !item.is_empty() {
                    out.push(item);
                }
                cur.clear();
            }
            other => cur.push(other),
        }
    }
    let item = unquote_yaml(cur.trim());
    if !item.is_empty() {
        out.push(item);
    }
    out
}

/// Resolve which editor command to launch:
/// `config.toml` `editor` -> `$VISUAL` -> `$EDITOR` -> `nvim` (if installed) -> `vi` / `notepad`.
pub fn resolve_editor(config_editor: Option<&str>) -> String {
    if let Some(cmd) = config_editor.map(str::trim).filter(|s| !s.is_empty()) {
        return cmd.to_string();
    }
    for key in ["VISUAL", "EDITOR"] {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    if command_exists("nvim") {
        return "nvim".to_string();
    }
    if cfg!(windows) {
        "notepad".to_string()
    } else {
        "vi".to_string()
    }
}

/// Short display name for the resolved editor (e.g. `/opt/homebrew/bin/nvim -u NONE` -> `nvim`).
pub fn editor_display_name(config_editor: Option<&str>) -> String {
    let raw = resolve_editor(config_editor);
    let program = split_editor_command(&raw)
        .ok()
        .and_then(|mut parts| (!parts.is_empty()).then(|| parts.remove(0)))
        .unwrap_or(raw);
    Path::new(&program)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&program)
        .to_string()
}

fn command_exists(program: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file()
    })
}

/// Split an editor command string such as `code --wait` or `nvim -c "set wrap"` into argv parts.
pub fn split_editor_command(raw: &str) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in raw.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }

    if in_single || in_double {
        return Err(format!("unclosed quote in editor command: {raw}"));
    }
    if !current.is_empty() {
        parts.push(current);
    }
    if parts.is_empty() {
        return Err("editor command is empty".into());
    }
    Ok(parts)
}

fn normalize_markdown(raw: &str) -> String {
    raw.replace("\r\n", "\n").trim_end().to_string()
}

/// Open a task as YAML frontmatter + Markdown notes in the resolved external editor.
/// Returns `Ok(Some(updated_doc))` if any field or description changed, `Ok(None)` if unchanged.
pub fn edit_task_in_editor(
    doc: &TaskDocument,
    config_editor: Option<&str>,
) -> Result<Option<TaskDocument>, String> {
    let initial_md = doc.to_markdown();
    let edited = run_editor_on_text(&initial_md, &doc.title, config_editor)?;
    let Some(updated_raw) = edited else {
        return Ok(None);
    };
    let parsed = TaskDocument::from_markdown(&updated_raw, doc)?;
    if &parsed == doc {
        Ok(None)
    } else {
        Ok(Some(parsed))
    }
}

fn run_editor_on_text(
    initial: &str,
    title_hint: &str,
    config_editor: Option<&str>,
) -> Result<Option<String>, String> {
    let editor_cmd = resolve_editor(config_editor);
    let parts = split_editor_command(&editor_cmd)?;
    let (program, extra_args) = (&parts[0], &parts[1..]);

    let slug = normalize_device_tag(title_hint);
    let short_slug: String = slug.chars().take(32).collect();
    let label = if short_slug.is_empty() {
        "task".to_string()
    } else {
        short_slug
    };
    let seq = EDITOR_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path =
        std::env::temp_dir().join(format!("roke-{label}-{}-{seq}.md", std::process::id()));

    let initial_norm = normalize_markdown(initial);
    let file_contents = if initial_norm.is_empty() {
        String::new()
    } else {
        format!("{initial_norm}\n")
    };
    fs::write(&tmp_path, file_contents)
        .map_err(|e| format!("failed to write temporary markdown file: {e}"))?;

    let status_res = Command::new(program)
        .args(extra_args)
        .arg(&tmp_path)
        .status();

    let read_res = match status_res {
        Ok(status) if status.success() => fs::read_to_string(&tmp_path)
            .map_err(|e| format!("failed to read back file from {}: {e}", tmp_path.display())),
        Ok(status) => Err(format!("editor '{program}' exited with {status}")),
        Err(err) => Err(format!("failed to launch editor '{program}': {err}")),
    };

    let _ = fs::remove_file(&tmp_path);

    let updated = normalize_markdown(&read_res?);
    if updated == initial_norm {
        Ok(None)
    } else {
        Ok(Some(updated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_editor_command_with_flags_and_quotes() {
        assert_eq!(
            split_editor_command("code --wait").unwrap(),
            vec!["code", "--wait"]
        );
        assert_eq!(
            split_editor_command(r#"nvim -c "set wrap" -u NONE"#).unwrap(),
            vec!["nvim", "-c", "set wrap", "-u", "NONE"]
        );
        assert_eq!(
            editor_display_name(Some("/opt/homebrew/bin/nvim -u NONE")),
            "nvim"
        );
        assert_eq!(editor_display_name(Some("code --wait")), "code");
    }

    #[test]
    fn frontmatter_round_trips_all_fields_and_markdown_body() {
        let doc = TaskDocument {
            title: "Ship v0.5: CalDAV + frontmatter".into(),
            description: "# Notes\n\n- [x] Sync\n- [ ] Test".into(),
            status: Status::InProgress,
            priority: Priority::High,
            tags: vec!["work".into(), "release".into()],
            due: Some("2026-12-01".into()),
            due_time: Some("14:30".into()),
            start: Some("2026-11-28".into()),
            start_time: Some("09:00".into()),
            repeat: Some("weekly".into()),
            reminders: vec!["at-due".into(), "-15m".into()],
            location: Some("Home Office: Desk".into()),
            url: Some("https://github.com/Aethar01/roke".into()),
            parent_uid: Some("parent-123".into()),
            percent_complete: Some(60),
        };

        let md = doc.to_markdown();
        assert!(md.starts_with("---\n"));
        let parsed = TaskDocument::from_markdown(&md, &doc).unwrap();
        assert_eq!(parsed, doc);

        // Plain markdown without frontmatter preserves metadata and updates only description
        let plain = TaskDocument::from_markdown("Just updated notes", &doc).unwrap();
        assert_eq!(plain.title, doc.title);
        assert_eq!(plain.tags, doc.tags);
        assert_eq!(plain.description, "Just updated notes");
    }

    #[test]
    fn edit_task_in_editor_updates_frontmatter_and_notes() {
        let base = TaskDocument {
            title: "Server check".into(),
            description: "# Heading".into(),
            status: Status::Todo,
            priority: Priority::Medium,
            tags: vec!["ops".into()],
            due: None,
            due_time: None,
            start: None,
            start_time: None,
            repeat: None,
            reminders: Vec::new(),
            location: None,
            url: None,
            parent_uid: None,
            percent_complete: None,
        };

        let script = r#"sh -c 'sed "s/priority: medium/priority: high/" "$1" > "$1.new" && mv "$1.new" "$1" && printf "\n- [ ] Check logs\n" >> "$1"' _"#;
        let updated = edit_task_in_editor(&base, Some(script))
            .unwrap()
            .expect("should detect change");
        assert_eq!(updated.priority, Priority::High);
        assert_eq!(updated.description, "# Heading\n\n- [ ] Check logs");

        // No-op editor reports None (unchanged)
        let unchanged = edit_task_in_editor(&base, Some("true")).unwrap();
        assert_eq!(unchanged, None);
    }
}

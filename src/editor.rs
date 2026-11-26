use crate::config::normalize_device_tag;
use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static EDITOR_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

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

/// Open `initial` Markdown content in the resolved external editor using a temporary `.md` file.
/// Returns `Ok(Some(updated))` if the content changed, `Ok(None)` if unchanged, or `Err(...)` on failure.
pub fn edit_markdown_in_editor(
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
        "note".to_string()
    } else {
        short_slug
    };
    let seq = EDITOR_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = std::env::temp_dir().join(format!(
        "roke-{label}-{}-{seq}.md",
        std::process::id()
    ));

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
            .map_err(|e| format!("failed to read back notes from {}: {e}", tmp_path.display())),
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
    }

    #[test]
    fn round_trips_markdown_through_configured_editor() {
        // Use POSIX sh as a fake editor that appends Markdown content to the .md file
        let script = r#"sh -c 'printf "\n- [ ] Check logs\n" >> "$1"' _"#;
        let updated = edit_markdown_in_editor("# Heading", "Server notes", Some(script))
            .unwrap()
            .expect("should report changed markdown");
        assert_eq!(updated, "# Heading\n\n- [ ] Check logs");

        // No-op editor reports None (unchanged)
        let unchanged = edit_markdown_in_editor("# Heading", "Server notes", Some("true")).unwrap();
        assert_eq!(unchanged, None);
    }
}

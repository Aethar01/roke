use crate::theme::{Theme, ThemeConfig};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// Optional editor command for editing Markdown task notes (overrides $VISUAL / $EDITOR).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    /// Optional startup view ("todos" or "kanban").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_view: Option<String>,
    /// Optional startup filter for Todo List ("all", "todo", "in-progress", "done").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_filter: Option<String>,
    /// Optional startup sort order ("manual", "priority", "due", "updated").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_sort: Option<String>,
    #[serde(default)]
    pub theme: ThemeConfig,
    #[serde(default)]
    pub sync: SyncConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Name of this machine (defaults to local hostname if omitted).
    #[serde(default)]
    pub device_name: Option<String>,
    /// When true, tasks synced from another machine dynamically get a `#<peer>` tag.
    /// Defaults to false (all todos synced without dynamic tags).
    #[serde(default)]
    pub inject_device_tag: bool,
    /// Automatically sync with configured peers on startup.
    #[serde(default = "default_true")]
    pub auto_sync_on_start: bool,
    /// Automatically sync in the background after local changes.
    #[serde(default)]
    pub auto_sync_on_save: bool,
    /// Remote or local peers to sync with (ssh, caldav, webdav, file).
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PeerKind {
    #[default]
    Ssh,
    Caldav,
    Webdav,
    File,
    #[serde(alias = "google_tasks", alias = "googletasks")]
    GoogleTasks,
    #[serde(alias = "ms_todo", alias = "mstodo", alias = "microsoft-todo")]
    MsTodo,
}

impl PeerKind {
    pub fn label(self) -> &'static str {
        match self {
            PeerKind::Ssh => "ssh",
            PeerKind::Caldav => "caldav",
            PeerKind::Webdav => "webdav",
            PeerKind::File => "file",
            PeerKind::GoogleTasks => "google-tasks",
            PeerKind::MsTodo => "ms-todo",
        }
    }

    pub fn uses_oauth(self) -> bool {
        matches!(self, PeerKind::GoogleTasks | PeerKind::MsTodo)
    }
}

fn default_true() -> bool {
    true
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            device_name: None,
            inject_device_tag: false,
            auto_sync_on_start: true,
            auto_sync_on_save: false,
            peers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    /// Friendly name of the peer (also used as the dynamic tag if enabled).
    pub name: String,
    /// Sync backend kind: "ssh" (default), "caldav", "webdav", or "file".
    #[serde(default)]
    pub kind: PeerKind,
    /// SSH destination, e.g. "user@host" or an alias from ~/.ssh/config (used by kind = "ssh").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// Optional remote path to todos.jsonl (defaults to remote XDG_DATA_HOME/roke/todos.jsonl; ssh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_path: Option<String>,
    /// Optional SSH port (ssh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Optional SSH private key path (`-i`; ssh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    /// Remote URL for kind = "caldav" (server root, home-set, or task list URL) or kind = "webdav" (todos.jsonl URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Optional task list name or collection slug for kind = "caldav" auto-discovery (defaults to first VTODO list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<String>,
    /// Local or synced file path for kind = "file" (e.g. Syncthing / Dropbox / iCloud folder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Username for HTTP Basic Auth (caldav / webdav).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Direct password for HTTP Basic Auth (prefer `password_env` or `password_cmd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Environment variable name holding the password for HTTP Basic Auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_env: Option<String>,
    /// Shell command whose stdout provides the password (e.g. `pass show caldav`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_cmd: Option<String>,
    /// OAuth 2.0 client ID for kind = "ms-todo" (Azure App) or kind = "google-tasks" (Google Cloud Desktop App).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Optional OAuth 2.0 client secret (Google Cloud Desktop OAuth client; prefer `client_secret_env` or `client_secret_cmd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Environment variable name holding the OAuth 2.0 client secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_env: Option<String>,
    /// Shell command whose stdout provides the OAuth 2.0 client secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_cmd: Option<String>,
    /// Microsoft Azure AD tenant for kind = "ms-todo" (defaults to "common").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Optional shell command whose stdout provides an OAuth Bearer access token directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_cmd: Option<String>,
    /// Optional per-peer override for `sync.inject_device_tag`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_device_tag: Option<bool>,
}

fn run_secret_cmd(label: &str, cmd_str: &str) -> Result<String, String> {
    #[cfg(windows)]
    let output = std::process::Command::new("cmd")
        .args(["/C", cmd_str])
        .output();
    #[cfg(not(windows))]
    let output = std::process::Command::new("sh")
        .args(["-c", cmd_str])
        .output();
    let output = output.map_err(|e| format!("failed to run {label}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("{label} exited with {}", output.status)
        } else {
            format!("{label} failed: {stderr}")
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\r', '\n'])
        .to_string())
}

fn resolve_secret_triple(
    direct: Option<&str>,
    env_var: Option<&str>,
    cmd: Option<&str>,
    cmd_label: &str,
) -> Result<Option<String>, String> {
    if let Some(val) = direct.filter(|s| !s.is_empty()) {
        return Ok(Some(val.to_string()));
    }
    if let Some(env_name) = env_var.map(str::trim).filter(|s| !s.is_empty()) {
        return match std::env::var(env_name) {
            Ok(val) if !val.is_empty() => Ok(Some(val)),
            Ok(_) => Err(format!("environment variable '{env_name}' is empty")),
            Err(_) => Err(format!("environment variable '{env_name}' is not set")),
        };
    }
    if let Some(cmd_str) = cmd.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(Some(run_secret_cmd(cmd_label, cmd_str)?));
    }
    Ok(None)
}

impl PeerConfig {
    /// Resolve HTTP Basic Auth password from `password`, `password_env`, or `password_cmd`.
    pub fn resolve_password(&self) -> Result<Option<String>, String> {
        resolve_secret_triple(
            self.password.as_deref(),
            self.password_env.as_deref(),
            self.password_cmd.as_deref(),
            "password_cmd",
        )
    }

    /// Resolve OAuth client secret from `client_secret`, `client_secret_env`, or `client_secret_cmd`.
    pub fn resolve_client_secret(&self) -> Result<Option<String>, String> {
        resolve_secret_triple(
            self.client_secret.as_deref(),
            self.client_secret_env.as_deref(),
            self.client_secret_cmd.as_deref(),
            "client_secret_cmd",
        )
    }

    /// Resolve an externally supplied OAuth access token via `token_cmd`, if configured.
    pub fn resolve_token_cmd(&self) -> Result<Option<String>, String> {
        let Some(cmd) = self
            .token_cmd
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(None);
        };
        let tok = run_secret_cmd("token_cmd", cmd)?;
        if tok.trim().is_empty() {
            return Err("token_cmd returned empty output".into());
        }
        Ok(Some(tok.trim().to_string()))
    }
}

impl PeerConfig {
    /// Construct a minimal peer config with only `name` and `kind` populated (useful in templates/tests).
    pub fn minimal(name: impl Into<String>, kind: PeerKind) -> Self {
        Self {
            name: name.into(),
            kind,
            host: String::new(),
            remote_path: None,
            port: None,
            identity_file: None,
            url: None,
            list: None,
            path: None,
            username: None,
            password: None,
            password_env: None,
            password_cmd: None,
            client_id: None,
            client_secret: None,
            client_secret_env: None,
            client_secret_cmd: None,
            tenant: None,
            token_cmd: None,
            inject_device_tag: None,
        }
    }
}

pub fn expand_tilde(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return PathBuf::from(home).join(rest);
            }
        }
    } else if trimmed == "~" {
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return PathBuf::from(home);
            }
        }
    }
    PathBuf::from(trimmed)
}

impl SyncConfig {
    pub fn effective_device_name(&self) -> String {
        if let Some(name) = &self.device_name {
            let slug = normalize_device_tag(name);
            if !slug.is_empty() {
                return slug;
            }
        }
        detect_hostname()
    }

    /// Returns whether tasks originating from `origin` should receive a dynamic `#<origin>` tag.
    pub fn should_inject_tag_for(&self, origin: &str) -> bool {
        let local = self.effective_device_name();
        let origin_slug = normalize_device_tag(origin);
        if origin_slug.is_empty() || origin_slug == local {
            return false;
        }
        for peer in &self.peers {
            if normalize_device_tag(&peer.name) == origin_slug {
                return peer.inject_device_tag.unwrap_or(self.inject_device_tag);
            }
        }
        self.inject_device_tag
    }
}

pub fn normalize_device_tag(raw: &str) -> String {
    let base = raw.split('.').next().unwrap_or(raw).trim();
    base.chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                Some(c.to_ascii_lowercase())
            } else if c.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

pub fn detect_hostname() -> String {
    static CACHED_HOSTNAME: OnceLock<String> = OnceLock::new();
    CACHED_HOSTNAME
        .get_or_init(detect_hostname_uncached)
        .clone()
}

fn detect_hostname_uncached() -> String {
    if let Ok(name) = std::env::var("HOSTNAME") {
        let slug = normalize_device_tag(&name);
        if !slug.is_empty() {
            return slug;
        }
    }
    if let Ok(output) = std::process::Command::new("hostname").output() {
        if output.status.success() {
            if let Ok(s) = String::from_utf8(output.stdout) {
                let slug = normalize_device_tag(&s);
                if !slug.is_empty() {
                    return slug;
                }
            }
        }
    }
    "local".to_string()
}

impl AppConfig {
    pub fn load_or_default(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            serde_json::from_str(&raw).map_err(|e| e.to_string())
        } else {
            toml::from_str(&raw).map_err(|e| e.to_string())
        }
    }

    pub fn resolved_theme(&self) -> Theme {
        Theme::from_config(&self.theme)
    }

    pub fn default_template_toml() -> String {
        let sample = AppConfig {
            editor: None,
            default_view: Some("todos".into()),
            default_filter: Some("all".into()),
            default_sort: Some("manual".into()),
            theme: ThemeConfig::default_template(),
            sync: SyncConfig {
                device_name: Some(detect_hostname()),
                inject_device_tag: false,
                auto_sync_on_start: true,
                auto_sync_on_save: false,
                peers: vec![
                    PeerConfig {
                        host: "user@example.com".into(),
                        ..PeerConfig::minimal("server", PeerKind::Ssh)
                    },
                    PeerConfig {
                        url: Some("https://caldav.tasks.org/".into()),
                        list: Some("Default list".into()),
                        username: Some("user".into()),
                        password_env: Some("ROKE_CALDAV_PASSWORD".into()),
                        ..PeerConfig::minimal("phone", PeerKind::Caldav)
                    },
                    PeerConfig {
                        path: Some("~/Sync/roke/todos.jsonl".into()),
                        ..PeerConfig::minimal("syncthing", PeerKind::File)
                    },
                ],
            },
        };
        toml::to_string_pretty(&sample).unwrap_or_default()
    }
}

pub fn default_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("roke").join("config.toml");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home)
                .join(".config")
                .join("roke")
                .join("config.toml");
        }
    }
    PathBuf::from("config.toml")
}

/// XDG state directory for machine-local sync cursors and OAuth tokens
/// (`$XDG_STATE_HOME/roke` or `~/.local/state/roke`).
pub fn default_state_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("roke");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("roke");
        }
    }
    PathBuf::from(".roke-state")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn partial_config_parses_theme_and_default_sync() {
        let raw = r#"
[theme]
accent = "blue"
"#;
        let cfg: AppConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.resolved_theme().accent, Color::Blue);
        assert!(!cfg.sync.inject_device_tag);
        assert!(cfg.sync.peers.is_empty());
    }

    #[test]
    fn dynamic_device_tag_respects_global_and_peer_override() {
        let cfg: SyncConfig = toml::from_str(
            r#"
device_name = "macbook"
inject_device_tag = false

[[peers]]
name = "vps"
host = "me@vps"
inject_device_tag = true

[[peers]]
name = "desktop"
host = "me@desktop"
"#,
        )
        .unwrap();

        assert!(!cfg.should_inject_tag_for("macbook"));
        assert!(cfg.should_inject_tag_for("vps"));
        assert!(!cfg.should_inject_tag_for("desktop"));
    }

    #[test]
    fn multi_backend_peer_config_defaults_to_ssh_and_parses_all_kinds() {
        let cfg: SyncConfig = toml::from_str(
            r#"
device_name = "macbook"

[[peers]]
name = "vps"
host = "me@vps"

[[peers]]
name = "phone"
kind = "caldav"
url = "https://caldav.tasks.org/"
list = "Default list"
username = "elliott"
password_cmd = "printf secret-pass"

[[peers]]
name = "cloud"
kind = "webdav"
url = "https://dav.example.com/roke/todos.jsonl"
username = "elliott"
password = "inline-pass"

[[peers]]
name = "syncthing"
kind = "file"
path = "~/Sync/roke/todos.jsonl"

[[peers]]
name = "microsoft"
kind = "ms-todo"
client_id = "azure-app-id"
list = "Tasks"

[[peers]]
name = "google"
kind = "google-tasks"
client_id = "google-client-id"
client_secret_cmd = "printf google-secret"
list = "My Tasks"
"#,
        )
        .unwrap();

        assert_eq!(cfg.peers.len(), 6);
        assert_eq!(cfg.peers[0].kind, PeerKind::Ssh);
        assert_eq!(cfg.peers[1].kind, PeerKind::Caldav);
        assert_eq!(cfg.peers[1].list.as_deref(), Some("Default list"));
        assert_eq!(
            cfg.peers[1].resolve_password().unwrap().as_deref(),
            Some("secret-pass")
        );
        assert_eq!(cfg.peers[2].kind, PeerKind::Webdav);
        assert_eq!(
            cfg.peers[2].resolve_password().unwrap().as_deref(),
            Some("inline-pass")
        );
        assert_eq!(cfg.peers[3].kind, PeerKind::File);
        assert_eq!(
            cfg.peers[3].path.as_deref(),
            Some("~/Sync/roke/todos.jsonl")
        );
        assert_eq!(cfg.peers[4].kind, PeerKind::MsTodo);
        assert_eq!(cfg.peers[4].client_id.as_deref(), Some("azure-app-id"));
        assert_eq!(cfg.peers[5].kind, PeerKind::GoogleTasks);
        assert_eq!(
            cfg.peers[5].resolve_client_secret().unwrap().as_deref(),
            Some("google-secret")
        );
    }
}

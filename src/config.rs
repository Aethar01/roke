use crate::theme::{Theme, ThemeConfig};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
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
    /// SSH devices to sync with.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
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
    /// Friendly name of the remote machine (also used as the dynamic tag if enabled).
    pub name: String,
    /// SSH destination, e.g. "user@host" or an alias from ~/.ssh/config.
    pub host: String,
    /// Optional remote path to todos.jsonl (defaults to remote XDG_DATA_HOME/roke/todos.jsonl).
    #[serde(default)]
    pub remote_path: Option<String>,
    /// Optional SSH port.
    #[serde(default)]
    pub port: Option<u16>,
    /// Optional SSH private key path (`-i`).
    #[serde(default)]
    pub identity_file: Option<String>,
    /// Optional per-peer override for `sync.inject_device_tag`.
    #[serde(default)]
    pub inject_device_tag: Option<bool>,
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
            theme: ThemeConfig::default_template(),
            sync: SyncConfig {
                device_name: Some(detect_hostname()),
                inject_device_tag: false,
                auto_sync_on_start: true,
                auto_sync_on_save: false,
                peers: vec![PeerConfig {
                    name: "server".into(),
                    host: "user@example.com".into(),
                    remote_path: None,
                    port: None,
                    identity_file: None,
                    inject_device_tag: None,
                }],
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
}

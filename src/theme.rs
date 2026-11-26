use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub accent: Color,
    pub text: Color,
    pub subtle: Color,
    pub muted: Color,
    pub todo: Color,
    pub in_progress: Color,
    pub done: Color,
    pub priority_high: Color,
    pub priority_medium: Color,
    pub priority_low: Color,
    pub add: Color,
    pub drag: Color,
    pub drop: Color,
    pub error: Color,
    pub tag: Color,
    pub tag_active: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Cyan,
            text: Color::White,
            subtle: Color::Gray,
            muted: Color::DarkGray,
            todo: Color::Blue,
            in_progress: Color::Yellow,
            done: Color::Green,
            priority_high: Color::Red,
            priority_medium: Color::Yellow,
            priority_low: Color::DarkGray,
            add: Color::Green,
            drag: Color::Yellow,
            drop: Color::Green,
            error: Color::Red,
            tag: Color::DarkGray,
            tag_active: Color::Cyan,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThemeConfig {
    pub accent: Option<String>,
    pub text: Option<String>,
    pub subtle: Option<String>,
    pub muted: Option<String>,
    pub todo: Option<String>,
    pub in_progress: Option<String>,
    pub done: Option<String>,
    pub priority_high: Option<String>,
    pub priority_medium: Option<String>,
    pub priority_low: Option<String>,
    pub add: Option<String>,
    pub drag: Option<String>,
    pub drop: Option<String>,
    pub error: Option<String>,
    pub tag: Option<String>,
    pub tag_active: Option<String>,
}

impl ThemeConfig {
    pub fn default_template() -> Self {
        Self {
            accent: Some("cyan".into()),
            text: Some("white".into()),
            subtle: Some("gray".into()),
            muted: Some("dark_gray".into()),
            todo: Some("blue".into()),
            in_progress: Some("yellow".into()),
            done: Some("green".into()),
            priority_high: Some("red".into()),
            priority_medium: Some("yellow".into()),
            priority_low: Some("dark_gray".into()),
            add: Some("green".into()),
            drag: Some("yellow".into()),
            drop: Some("green".into()),
            error: Some("red".into()),
            tag: Some("dark_gray".into()),
            tag_active: None,
        }
    }
}

impl Theme {
    pub fn from_config(cfg: &ThemeConfig) -> Self {
        let mut theme = Self::default();
        if let Some(c) = cfg.accent.as_deref().and_then(parse_color) {
            theme.accent = c;
            theme.tag_active = c;
        }
        if let Some(c) = cfg.text.as_deref().and_then(parse_color) {
            theme.text = c;
        }
        if let Some(c) = cfg.subtle.as_deref().and_then(parse_color) {
            theme.subtle = c;
        }
        if let Some(c) = cfg.muted.as_deref().and_then(parse_color) {
            theme.muted = c;
        }
        if let Some(c) = cfg.todo.as_deref().and_then(parse_color) {
            theme.todo = c;
        }
        if let Some(c) = cfg.in_progress.as_deref().and_then(parse_color) {
            theme.in_progress = c;
        }
        if let Some(c) = cfg.done.as_deref().and_then(parse_color) {
            theme.done = c;
        }
        if let Some(c) = cfg.priority_high.as_deref().and_then(parse_color) {
            theme.priority_high = c;
        }
        if let Some(c) = cfg.priority_medium.as_deref().and_then(parse_color) {
            theme.priority_medium = c;
        }
        if let Some(c) = cfg.priority_low.as_deref().and_then(parse_color) {
            theme.priority_low = c;
        }
        if let Some(c) = cfg.add.as_deref().and_then(parse_color) {
            theme.add = c;
        }
        if let Some(c) = cfg.drag.as_deref().and_then(parse_color) {
            theme.drag = c;
        }
        if let Some(c) = cfg.drop.as_deref().and_then(parse_color) {
            theme.drop = c;
        }
        if let Some(c) = cfg.error.as_deref().and_then(parse_color) {
            theme.error = c;
        }
        if let Some(c) = cfg.tag.as_deref().and_then(parse_color) {
            theme.tag = c;
        }
        if let Some(c) = cfg.tag_active.as_deref().and_then(parse_color) {
            theme.tag_active = c;
        }
        theme
    }
}

pub fn parse_color(input: &str) -> Option<Color> {
    let trimmed = input.trim();
    if let Some(hex) = trimmed.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }

    if let Ok(idx) = trimmed.parse::<u8>() {
        return Some(Color::Indexed(idx));
    }

    let normalized: String = trimmed
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .collect();

    match normalized.as_str() {
        "reset" | "default" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" | "darkblue" => Some(Color::Blue),
        "magenta" | "purple" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" | "brightblack" => Some(Color::DarkGray),
        "lightred" | "brightred" => Some(Color::LightRed),
        "lightgreen" | "brightgreen" => Some(Color::LightGreen),
        "lightyellow" | "brightyellow" => Some(Color::LightYellow),
        "lightblue" | "brightblue" => Some(Color::LightBlue),
        "lightmagenta" | "brightmagenta" => Some(Color::LightMagenta),
        "lightcyan" | "brightcyan" => Some(Color::LightCyan),
        "white" | "brightwhite" => Some(Color::White),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ansi16_indexed_and_hex_colors() {
        assert_eq!(parse_color("blue"), Some(Color::Blue));
        assert_eq!(parse_color("dark_blue"), Some(Color::Blue));
        assert_eq!(parse_color("bright-cyan"), Some(Color::LightCyan));
        assert_eq!(parse_color("reset"), Some(Color::Reset));
        assert_eq!(parse_color("4"), Some(Color::Indexed(4)));
        assert_eq!(parse_color("#1e3a8a"), Some(Color::Rgb(0x1e, 0x3a, 0x8a)));
        assert_eq!(parse_color("not-a-color"), None);
    }

    #[test]
    fn partial_theme_override_updates_accent_and_tag_active() {
        let cfg: ThemeConfig = serde_json::from_str(r#"{ "accent": "blue" }"#).unwrap();
        let theme = Theme::from_config(&cfg);
        assert_eq!(theme.accent, Color::Blue);
        assert_eq!(theme.tag_active, Color::Blue);
        assert_eq!(theme.done, Color::Green);
    }
}

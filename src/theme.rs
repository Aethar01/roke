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
    /// Optional built-in palette preset ("default", "catppuccin", "tokyo-night", "gruvbox", "nord", "minimal").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
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
    /// Overlay any explicitly specified fields from `other` onto `self`.
    pub fn overlay(&mut self, other: &ThemeConfig) {
        if let Some(v) = &other.preset {
            self.preset = Some(v.clone());
        }
        if let Some(v) = &other.accent {
            self.accent = Some(v.clone());
        }
        if let Some(v) = &other.text {
            self.text = Some(v.clone());
        }
        if let Some(v) = &other.subtle {
            self.subtle = Some(v.clone());
        }
        if let Some(v) = &other.muted {
            self.muted = Some(v.clone());
        }
        if let Some(v) = &other.todo {
            self.todo = Some(v.clone());
        }
        if let Some(v) = &other.in_progress {
            self.in_progress = Some(v.clone());
        }
        if let Some(v) = &other.done {
            self.done = Some(v.clone());
        }
        if let Some(v) = &other.priority_high {
            self.priority_high = Some(v.clone());
        }
        if let Some(v) = &other.priority_medium {
            self.priority_medium = Some(v.clone());
        }
        if let Some(v) = &other.priority_low {
            self.priority_low = Some(v.clone());
        }
        if let Some(v) = &other.add {
            self.add = Some(v.clone());
        }
        if let Some(v) = &other.drag {
            self.drag = Some(v.clone());
        }
        if let Some(v) = &other.drop {
            self.drop = Some(v.clone());
        }
        if let Some(v) = &other.error {
            self.error = Some(v.clone());
        }
        if let Some(v) = &other.tag {
            self.tag = Some(v.clone());
        }
        if let Some(v) = &other.tag_active {
            self.tag_active = Some(v.clone());
        }
    }

    pub fn default_template() -> Self {
        Self {
            preset: Some("default".into()),
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
    pub fn preset(name: &str) -> Option<Self> {
        let norm: String = name
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| !matches!(c, '_' | '-' | ' '))
            .collect();
        match norm.as_str() {
            "default" | "ansi" => Some(Self::default()),
            "catppuccin" | "catppuccinmocha" | "mocha" => Some(Self {
                accent: Color::Rgb(0x89, 0xb4, 0xfa),
                text: Color::Rgb(0xcd, 0xd6, 0xf4),
                subtle: Color::Rgb(0xa6, 0xad, 0xc8),
                muted: Color::Rgb(0x6c, 0x70, 0x86),
                todo: Color::Rgb(0x89, 0xb4, 0xfa),
                in_progress: Color::Rgb(0xf9, 0xe2, 0xaf),
                done: Color::Rgb(0xa6, 0xe3, 0xa1),
                priority_high: Color::Rgb(0xf3, 0x8b, 0xa8),
                priority_medium: Color::Rgb(0xfa, 0xb3, 0x87),
                priority_low: Color::Rgb(0x6c, 0x70, 0x86),
                add: Color::Rgb(0xa6, 0xe3, 0xa1),
                drag: Color::Rgb(0xf9, 0xe2, 0xaf),
                drop: Color::Rgb(0xa6, 0xe3, 0xa1),
                error: Color::Rgb(0xf3, 0x8b, 0xa8),
                tag: Color::Rgb(0x7f, 0x84, 0x9c),
                tag_active: Color::Rgb(0x89, 0xb4, 0xfa),
            }),
            "tokyonight" | "tokyo" => Some(Self {
                accent: Color::Rgb(0x7a, 0xa2, 0xf7),
                text: Color::Rgb(0xc0, 0xca, 0xf5),
                subtle: Color::Rgb(0xa9, 0xb1, 0xd6),
                muted: Color::Rgb(0x56, 0x5f, 0x89),
                todo: Color::Rgb(0x7d, 0xcf, 0xff),
                in_progress: Color::Rgb(0xe0, 0xaf, 0x68),
                done: Color::Rgb(0x9e, 0xce, 0x6a),
                priority_high: Color::Rgb(0xf7, 0x76, 0x8e),
                priority_medium: Color::Rgb(0xe0, 0xaf, 0x68),
                priority_low: Color::Rgb(0x56, 0x5f, 0x89),
                add: Color::Rgb(0x9e, 0xce, 0x6a),
                drag: Color::Rgb(0xe0, 0xaf, 0x68),
                drop: Color::Rgb(0x9e, 0xce, 0x6a),
                error: Color::Rgb(0xf7, 0x76, 0x8e),
                tag: Color::Rgb(0x56, 0x5f, 0x89),
                tag_active: Color::Rgb(0x7a, 0xa2, 0xf7),
            }),
            "gruvbox" | "gruvboxdark" => Some(Self {
                accent: Color::Rgb(0x83, 0xa5, 0x98),
                text: Color::Rgb(0xeb, 0xdb, 0xb2),
                subtle: Color::Rgb(0xd5, 0xc4, 0xa1),
                muted: Color::Rgb(0x92, 0x83, 0x74),
                todo: Color::Rgb(0x83, 0xa5, 0x98),
                in_progress: Color::Rgb(0xfa, 0xbd, 0x2f),
                done: Color::Rgb(0xb8, 0xbb, 0x26),
                priority_high: Color::Rgb(0xfb, 0x49, 0x34),
                priority_medium: Color::Rgb(0xfe, 0x80, 0x19),
                priority_low: Color::Rgb(0x92, 0x83, 0x74),
                add: Color::Rgb(0xb8, 0xbb, 0x26),
                drag: Color::Rgb(0xfa, 0xbd, 0x2f),
                drop: Color::Rgb(0xb8, 0xbb, 0x26),
                error: Color::Rgb(0xfb, 0x49, 0x34),
                tag: Color::Rgb(0x92, 0x83, 0x74),
                tag_active: Color::Rgb(0x83, 0xa5, 0x98),
            }),
            "nord" => Some(Self {
                accent: Color::Rgb(0x88, 0xc0, 0xd0),
                text: Color::Rgb(0xec, 0xef, 0xf4),
                subtle: Color::Rgb(0xd8, 0xde, 0xe9),
                muted: Color::Rgb(0x61, 0x6e, 0x88),
                todo: Color::Rgb(0x81, 0xa1, 0xc1),
                in_progress: Color::Rgb(0xeb, 0xcb, 0x8b),
                done: Color::Rgb(0xa3, 0xbe, 0x8c),
                priority_high: Color::Rgb(0xbf, 0x61, 0x6a),
                priority_medium: Color::Rgb(0xd0, 0x87, 0x70),
                priority_low: Color::Rgb(0x61, 0x6e, 0x88),
                add: Color::Rgb(0xa3, 0xbe, 0x8c),
                drag: Color::Rgb(0xeb, 0xcb, 0x8b),
                drop: Color::Rgb(0xa3, 0xbe, 0x8c),
                error: Color::Rgb(0xbf, 0x61, 0x6a),
                tag: Color::Rgb(0x61, 0x6e, 0x88),
                tag_active: Color::Rgb(0x88, 0xc0, 0xd0),
            }),
            "minimal" | "mono" | "monochrome" => Some(Self {
                accent: Color::White,
                text: Color::White,
                subtle: Color::Gray,
                muted: Color::DarkGray,
                todo: Color::Gray,
                in_progress: Color::White,
                done: Color::DarkGray,
                priority_high: Color::White,
                priority_medium: Color::Gray,
                priority_low: Color::DarkGray,
                add: Color::White,
                drag: Color::White,
                drop: Color::White,
                error: Color::Red,
                tag: Color::DarkGray,
                tag_active: Color::White,
            }),
            _ => None,
        }
    }

    pub fn from_config(cfg: &ThemeConfig) -> Self {
        let mut theme = cfg
            .preset
            .as_deref()
            .and_then(Self::preset)
            .unwrap_or_default();
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

pub fn to_crossterm_color(color: Color) -> crossterm::style::Color {
    use crossterm::style::Color as Ct;
    match color {
        Color::Reset => Ct::Reset,
        Color::Black => Ct::Black,
        Color::Red => Ct::DarkRed,
        Color::Green => Ct::DarkGreen,
        Color::Yellow => Ct::DarkYellow,
        Color::Blue => Ct::DarkBlue,
        Color::Magenta => Ct::DarkMagenta,
        Color::Cyan => Ct::DarkCyan,
        Color::Gray => Ct::Grey,
        Color::DarkGray => Ct::DarkGrey,
        Color::LightRed => Ct::Red,
        Color::LightGreen => Ct::Green,
        Color::LightYellow => Ct::Yellow,
        Color::LightBlue => Ct::Blue,
        Color::LightMagenta => Ct::Magenta,
        Color::LightCyan => Ct::Cyan,
        Color::White => Ct::White,
        Color::Rgb(r, g, b) => Ct::Rgb { r, g, b },
        Color::Indexed(i) => Ct::AnsiValue(i),
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

    #[test]
    fn theme_preset_loads_and_allows_key_overrides() {
        let cfg: ThemeConfig = toml::from_str(
            r##"
preset = "catppuccin"
accent = "#ff00aa"
"##,
        )
        .unwrap();
        let theme = Theme::from_config(&cfg);
        assert_eq!(theme.accent, Color::Rgb(0xff, 0x00, 0xaa));
        assert_eq!(theme.tag_active, Color::Rgb(0xff, 0x00, 0xaa));
        assert_eq!(theme.done, Color::Rgb(0xa6, 0xe3, 0xa1));
        assert!(Theme::preset("tokyo-night").is_some());
        assert!(Theme::preset("gruvbox").is_some());
        assert!(Theme::preset("nord").is_some());
        assert!(Theme::preset("minimal").is_some());
    }
}

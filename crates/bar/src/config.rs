use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use windows::Win32::Foundation::{COLORREF, SYSTEMTIME};

pub const DEFAULT_CONFIG: &str = include_str!("../bar.default.toml");
pub const DEFAULT_FONT: &str = "Segoe UI";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Module {
    Workspaces,
    Mode,
    Title,
    Clock,
    Cpu,
    Memory,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bar {
    pub height: i32,
    pub padding: i32,
    pub gap: i32,
    pub workspace_width: i32,
    pub max_workspaces: u32,
    pub widget_padding: i32,
    pub icon_gap: i32,
    pub workspace_inset: i32,
    pub title_max_width: i32,
}

impl Default for Bar {
    fn default() -> Self {
        Self {
            height: 28,
            padding: 8,
            gap: 8,
            workspace_width: 30,
            max_workspaces: 12,
            widget_padding: 6,
            icon_gap: 4,
            workspace_inset: 2,
            title_max_width: 320,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Modules {
    pub left: Vec<Module>,
    pub center: Vec<Module>,
    pub right: Vec<Module>,
}

impl Default for Modules {
    fn default() -> Self {
        Self {
            left: vec![Module::Workspaces, Module::Mode],
            center: vec![Module::Title],
            right: vec![Module::Cpu, Module::Memory, Module::Clock],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub font: String,
    pub font_size: i32,
    pub font_weight: i32,
    pub icon_font: String,
    pub icon_size: i32,
    pub workspace_font_size: i32,
    background: String,
    foreground: String,
    muted: String,
    accent: String,
    active_foreground: String,
    active_background: Option<String>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            font: DEFAULT_FONT.into(),
            font_size: 11,
            font_weight: 400,
            icon_font: "Segoe MDL2 Assets".into(),
            icon_size: 13,
            workspace_font_size: 11,
            background: "#111318".into(),
            foreground: "#DCE0E8".into(),
            muted: "#818898".into(),
            accent: "#89B4FA".into(),
            active_foreground: "#111318".into(),
            active_background: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Clock {
    pub format: String,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            format: "{weekday}, {day} {month_name} {hour}:{minute}".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cpu {
    pub format: String,
    pub icon: String,
    pub update_interval_ms: u32,
}

impl Default for Cpu {
    fn default() -> Self {
        Self {
            format: "{percent}%".into(),
            icon: "\u{e950}".into(),
            update_interval_ms: 2000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Memory {
    pub format: String,
    pub icon: String,
    pub update_interval_ms: u32,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            format: "{used_gib}/{total_gib} GiB".into(),
            icon: "\u{e9d9}".into(),
            update_interval_ms: 5000,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Document {
    bar: Bar,
    modules: Modules,
    theme: Theme,
    clock: Clock,
    cpu: Cpu,
    memory: Memory,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bar: Bar,
    pub modules: Modules,
    pub theme: Theme,
    pub clock: Clock,
    pub cpu: Cpu,
    pub memory: Memory,
    pub background: COLORREF,
    pub foreground: COLORREF,
    pub muted: COLORREF,
    pub active_foreground: COLORREF,
    pub active_background: COLORREF,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut doc: Document = toml::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|error| error.to_string())?;
        // An omitted or blank font uses the same portable UI default.
        doc.theme.font = doc.theme.font.trim().to_owned();
        if doc.theme.font.is_empty() {
            doc.theme.font = DEFAULT_FONT.into();
        }
        let bar = &doc.bar;
        for (name, value, min, max) in [
            ("bar.height", bar.height, 16, 200),
            ("bar.padding", bar.padding, 0, 200),
            ("bar.gap", bar.gap, 0, 200),
            ("bar.workspace_width", bar.workspace_width, 16, 200),
            ("theme.font_size", doc.theme.font_size, 6, 96),
            ("theme.icon_size", doc.theme.icon_size, 6, 96),
            (
                "theme.workspace_font_size",
                doc.theme.workspace_font_size,
                6,
                96,
            ),
            ("theme.font_weight", doc.theme.font_weight, 100, 900),
            ("bar.widget_padding", bar.widget_padding, 0, 100),
            ("bar.icon_gap", bar.icon_gap, 0, 50),
            ("bar.workspace_inset", bar.workspace_inset, 0, 30),
            ("bar.title_max_width", bar.title_max_width, 20, 2000),
        ] {
            if !(min..=max).contains(&value) {
                return Err(format!("{name} must be between {min} and {max}"));
            }
        }
        if !(1..=64).contains(&bar.max_workspaces) {
            return Err("bar.max_workspaces must be between 1 and 64".into());
        }
        for (key, font) in [
            ("theme.font", &doc.theme.font),
            ("theme.icon_font", &doc.theme.icon_font),
        ] {
            if font.trim().is_empty() || font.encode_utf16().count() > 31 || font.contains('\0') {
                return Err(format!(
                    "{key} must be a nonempty Windows font name (at most 31 UTF-16 units)"
                ));
            }
        }
        if [
            doc.theme.font_size,
            doc.theme.icon_size,
            doc.theme.workspace_font_size,
        ]
        .iter()
        .any(|size| *size > bar.height - 4)
        {
            return Err("theme font sizes must fit inside bar.height (with 4px padding)".into());
        }
        if bar.workspace_inset * 2 >= bar.height {
            return Err("bar.workspace_inset must leave visible workspace buttons".into());
        }
        for (key, interval) in [
            ("cpu.update_interval_ms", doc.cpu.update_interval_ms),
            ("memory.update_interval_ms", doc.memory.update_interval_ms),
        ] {
            if !(500..=60_000).contains(&interval) {
                return Err(format!("{key} must be between 500 and 60000"));
            }
        }
        for (key, icon) in [
            ("cpu.icon", &doc.cpu.icon),
            ("memory.icon", &doc.memory.icon),
        ] {
            if icon.encode_utf16().count() > 16 || icon.contains(['\0', '\n', '\r']) {
                return Err(format!(
                    "{key} must be a short, single-line icon (or empty)"
                ));
            }
        }
        let modules: Vec<_> = doc
            .modules
            .left
            .iter()
            .chain(&doc.modules.center)
            .chain(&doc.modules.right)
            .copied()
            .collect();
        for (index, module) in modules.iter().enumerate() {
            if modules[..index].contains(module) {
                return Err(format!("duplicate module: {module:?}"));
            }
        }
        validate_format(
            "clock.format",
            &doc.clock.format,
            &[
                "hour",
                "minute",
                "day",
                "month",
                "year",
                "weekday",
                "month_name",
            ],
        )?;
        validate_format("cpu.format", &doc.cpu.format, &["percent"])?;
        validate_format(
            "memory.format",
            &doc.memory.format,
            &["percent", "used_gib", "total_gib"],
        )?;
        let accent = color("theme.accent", &doc.theme.accent)?;
        Ok(Self {
            background: color("theme.background", &doc.theme.background)?,
            foreground: color("theme.foreground", &doc.theme.foreground)?,
            muted: color("theme.muted", &doc.theme.muted)?,
            active_foreground: color("theme.active_foreground", &doc.theme.active_foreground)?,
            active_background: doc
                .theme
                .active_background
                .as_deref()
                .map(|value| color("theme.active_background", value))
                .transpose()?
                .unwrap_or(accent),
            bar: doc.bar,
            modules: doc.modules,
            theme: doc.theme,
            clock: doc.clock,
            cpu: doc.cpu,
            memory: doc.memory,
        })
    }

    pub fn has(&self, module: Module) -> bool {
        [
            &self.modules.left,
            &self.modules.center,
            &self.modules.right,
        ]
        .iter()
        .any(|group| group.contains(&module))
    }
}

fn color(name: &str, text: &str) -> Result<COLORREF, String> {
    let rgb = text
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6 && hex.is_ascii())
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .ok_or_else(|| format!("{name} must be a #RRGGBB color"))?;
    Ok(COLORREF(
        ((rgb & 0xff) << 16) | (rgb & 0xff00) | (rgb >> 16),
    ))
}

fn validate_format(name: &str, text: &str, allowed: &[&str]) -> Result<(), String> {
    if text.len() > 256 || text.contains(['\0', '\n', '\r']) {
        return Err(format!("{name} must be one line of at most 256 bytes"));
    }
    let mut remaining = text;
    while let Some(start) = remaining.find('{') {
        if remaining[..start].contains('}') {
            return Err(format!("unmatched }} in {name}"));
        }
        let end = remaining[start..]
            .find('}')
            .ok_or_else(|| format!("unmatched {{ in {name}"))?
            + start;
        if !allowed.contains(&&remaining[start + 1..end]) {
            return Err(format!(
                "{name} accepts these placeholders: {}",
                allowed.join(", ")
            ));
        }
        remaining = &remaining[end + 1..];
    }
    if remaining.contains('}') {
        return Err(format!("unmatched }} in {name}"));
    }
    Ok(())
}

pub fn clock_text(format: &str, time: SYSTEMTIME) -> String {
    let text = [
        ("{hour}", time.wHour),
        ("{minute}", time.wMinute),
        ("{day}", time.wDay),
        ("{month}", time.wMonth),
        ("{year}", time.wYear),
    ]
    .into_iter()
    .fold(format.to_owned(), |text, (key, value)| {
        text.replace(key, &format!("{value:02}"))
    });
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    text.replace(
        "{weekday}",
        DAYS.get(time.wDayOfWeek as usize).unwrap_or(&"?"),
    )
    .replace(
        "{month_name}",
        MONTHS
            .get(time.wMonth.wrapping_sub(1) as usize)
            .unwrap_or(&"?"),
    )
}

pub fn default_path() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(|root| PathBuf::from(root).join("wm/bar.toml"))
        .ok_or_else(|| "APPDATA is unset; provide --config PATH".into())
}

pub fn init(path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(DEFAULT_CONFIG.as_bytes())?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_customizations_are_rejected_before_apply() {
        Config::parse(DEFAULT_CONFIG).unwrap();
        for text in [
            "[bar]\nheight=0",
            "[theme]\naccent='red'",
            "[modules]\nright=['title']",
            "[clock]\nformat='{second}'",
            "[clock]\nformat='oops}'",
            "[bar]\nheigth=28",
            "[theme]\nfont_size=80",
            "[cpu]\nupdate_interval_ms=0",
            "[memory]\nupdate_interval_ms=60001",
            "[cpu]\nformat='{used_gib}'",
            "[memory]\nformat='{unknown}'",
        ] {
            assert!(Config::parse(text).is_err(), "accepted {text}");
        }
        let config = Config::parse("[modules]\nright=[]").unwrap();
        assert!(!config.has(Module::Clock));
    }

    #[test]
    fn clock_formats_local_time_with_zero_padding() {
        let time = SYSTEMTIME {
            wYear: 2026,
            wMonth: 11,
            wDay: 16,
            wHour: 8,
            wMinute: 3,
            wDayOfWeek: 1,
            ..Default::default()
        };
        assert_eq!(
            clock_text("{year}-{month}-{day} {hour}:{minute}", time),
            "2026-11-16 08:03"
        );
        assert_eq!(
            clock_text(&Clock::default().format, time),
            "Mon, 16 Nov 08:03"
        );
    }

    #[test]
    fn yasb_profile_uses_compact_metrics_and_independent_sampling_intervals() {
        let config = Config::parse(include_str!("../themes/yasb.toml")).unwrap();
        assert_eq!(config.bar.height, 18);
        assert_eq!(config.theme.font_size, 11);
        assert_eq!(config.modules.center, vec![Module::Clock]);
        assert_eq!(config.modules.right, vec![Module::Cpu, Module::Memory]);
        assert_eq!(config.cpu.update_interval_ms, 2000);
        assert_eq!(config.memory.update_interval_ms, 5000);
        assert_eq!(config.active_background, color("test", "#3D3C3D").unwrap());
    }

    #[test]
    fn font_defaults_and_explicit_installed_family_settings_are_preserved() {
        for text in ["", "[theme]\nfont = ''"] {
            let config = Config::parse(text).unwrap();
            assert_eq!(config.theme.font, DEFAULT_FONT);
            assert_eq!(config.theme.font_size, 11);
            assert_eq!(config.theme.font_weight, 400);
        }
        let config =
            Config::parse("[theme]\nfont = ' Tahoma '\nfont_size = 12\nfont_weight = 400").unwrap();
        assert_eq!(config.theme.font, "Tahoma");
        assert_eq!(config.theme.font_size, 12);
        assert_eq!(
            Config::parse(DEFAULT_CONFIG).unwrap().theme.font_size,
            Config::parse("").unwrap().theme.font_size
        );
    }
}

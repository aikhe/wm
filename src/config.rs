//! User-authored settings, separate from automatically remembered app state.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use windows::Win32::Foundation::COLORREF;

use crate::animation::Animations;
use crate::bindings::{self, Action, Binding, Keys};
use crate::layout::{Fraction, Mode, Options, Rect, Width};
use crate::rules::Rule;
use crate::setups::{self, App, Setup, Spot};

pub const DEFAULT_CONFIG: &str = include_str!("../wm.default.toml");

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Document {
    workspaces: Vec<String>,
    layout: Layout,
    behavior: Behavior,
    overview: Overview,
    animations: Animations,
    hotkeys: BTreeMap<String, Keys>,
    agent: Agent,
    window_rules: Vec<WindowRule>,
    setups: BTreeMap<String, SetupDocument>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupDocument {
    key: Option<Keys>,
    save_key: Option<Keys>,
    /// Without these, the apps come from setups.toml.
    apps: Option<Vec<App>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Agent {
    allow: Vec<String>,
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            allow: bindings::DEFAULTS
                .iter()
                .filter(|(_, _, action)| !matches!(action, Action::Close | Action::Quit))
                .map(|(name, _, _)| (*name).to_string())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    pub gap: i32,
    default_width: String,
    width_presets: Vec<String>,
    min_width_percent: i32,
    pub resize_step_percent: i32,
    reserved_top: i32,
    reserved_bottom: i32,
    reserved_left: i32,
    reserved_right: i32,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            gap: 8,
            default_width: "1/2".into(),
            width_presets: ["1/3", "1/2", "2/3"].map(String::from).into(),
            min_width_percent: 10,
            resize_step_percent: 10,
            reserved_top: 0,
            reserved_bottom: 0,
            reserved_left: 0,
            reserved_right: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Behavior {
    pub wrap_focus: bool,
    pub wrap_move: bool,
    pub remember_modes: bool,
    pub remember_widths: bool,
    pub float_dialogs: bool,
    pub floating_above_tiled: bool,
    pub log_events: bool,
    startup_setup: String,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            wrap_focus: true,
            wrap_move: true,
            remember_modes: true,
            remember_widths: true,
            float_dialogs: true,
            floating_above_tiled: true,
            log_events: true,
            startup_setup: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overview {
    background: String,
    workspace: String,
    active_workspace: String,
    card: String,
    highlight: String,
    pub highlight_width: i32,
    margin: String,
    row_gap: String,
    max_row_height: String,
    pub bare_hotkeys: bool,
    pub arrow_keys: bool,
    pub enter_escape_close: bool,
}

impl Default for Overview {
    fn default() -> Self {
        Self {
            background: "#000000".into(),
            workspace: "#202020".into(),
            active_workspace: "#404040".into(),
            card: "#606060".into(),
            highlight: "#FFFFFF".into(),
            highlight_width: 3,
            margin: "1/20".into(),
            row_gap: "1/40".into(),
            max_row_height: "1/3".into(),
            bare_hotkeys: true,
            arrow_keys: true,
            enter_escape_close: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverviewStyle {
    pub background: COLORREF,
    pub workspace: COLORREF,
    pub active_workspace: COLORREF,
    pub card: COLORREF,
    pub highlight: COLORREF,
    pub highlight_width: i32,
    pub margin: Fraction,
    pub row_gap: Fraction,
    pub max_row_height: Fraction,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowRule {
    exe: Option<String>,
    class: Option<String>,
    title_contains: Option<String>,
    mode: Option<RuleMode>,
    width: Option<i32>,
    full_width: Option<bool>,
    workspace: Option<Spot>,
    focus: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleMode {
    Tiled,
    Float,
    Sticky,
    Top,
    Ignore,
}

/// How a window joins the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Joining {
    pub rule: Rule,
    /// `None` for the workspace on screen.
    pub workspace: Option<Spot>,
    /// Whether the view follows it there and it keeps the keyboard.
    pub focus: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub workspaces: Vec<String>,
    pub layout: Layout,
    pub behavior: Behavior,
    pub overview: Overview,
    pub animations: Animations,
    pub options: Options,
    pub style: OverviewStyle,
    pub bindings: Vec<Binding>,
    pub agent_allow: Vec<Action>,
    pub setups: Vec<Setup>,
    pub startup_setup: Option<usize>,
    window_rules: Vec<WindowRule>,
}

impl Config {
    /// Without any saved setups. See `parse_with`.
    #[cfg(test)]
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::parse_with(text, "")
    }

    /// `saved` is setups.toml, which holds the apps of setups that don't
    /// list them in the config.
    pub fn parse_with(text: &str, saved: &str) -> Result<Self, String> {
        let document: Document = toml::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|error| error.to_string())?;
        let saved: BTreeMap<String, Vec<App>> =
            toml::from_str(saved.trim_start_matches('\u{feff}'))
                .map_err(|error| format!("setups.toml: {error}"))?;
        let Document {
            workspaces,
            layout,
            behavior,
            overview,
            animations,
            hotkeys,
            agent,
            window_rules,
            setups: setup_documents,
        } = document;
        for (index, name) in workspaces.iter().enumerate() {
            if name.trim().is_empty() || workspaces[..index].contains(name) {
                return Err(format!("workspaces: `{name}` must be nonempty and unique"));
            }
        }
        animations.validate()?;
        range("layout.gap", layout.gap, 0, 10_000)?;
        range("layout.min_width_percent", layout.min_width_percent, 1, 90)?;
        range(
            "layout.resize_step_percent",
            layout.resize_step_percent,
            1,
            100,
        )?;
        for (name, value) in [
            ("reserved_top", layout.reserved_top),
            ("reserved_bottom", layout.reserved_bottom),
            ("reserved_left", layout.reserved_left),
            ("reserved_right", layout.reserved_right),
        ] {
            range(&format!("layout.{name}"), value, 0, 100_000)?;
        }
        if layout.width_presets.is_empty() || layout.width_presets.len() > 32 {
            return Err("layout.width_presets must contain 1 to 32 fractions".into());
        }
        let widths: Vec<_> = layout
            .width_presets
            .iter()
            .map(|text| fraction("layout.width_presets", text, false))
            .collect::<Result<_, _>>()?;
        if !widths.windows(2).all(|pair| {
            i64::from(pair[0].0) * i64::from(pair[1].1)
                < i64::from(pair[1].0) * i64::from(pair[0].1)
        }) {
            return Err("layout.width_presets must be strictly increasing".into());
        }
        let options = Options {
            widths,
            default_width: fraction("layout.default_width", &layout.default_width, false)?,
            min_width_percent: layout.min_width_percent,
            wrap_focus: behavior.wrap_focus,
            wrap_move: behavior.wrap_move,
        };
        range("overview.highlight_width", overview.highlight_width, 0, 20)?;
        let style = OverviewStyle {
            background: color("overview.background", &overview.background)?,
            workspace: color("overview.workspace", &overview.workspace)?,
            active_workspace: color("overview.active_workspace", &overview.active_workspace)?,
            card: color("overview.card", &overview.card)?,
            highlight: color("overview.highlight", &overview.highlight)?,
            highlight_width: overview.highlight_width,
            margin: fraction("overview.margin", &overview.margin, true)?,
            row_gap: fraction("overview.row_gap", &overview.row_gap, true)?,
            max_row_height: fraction("overview.max_row_height", &overview.max_row_height, false)?,
        };
        if style.margin.0 * 2 >= style.margin.1 {
            return Err("overview.margin must be less than 1/2".into());
        }
        for (index, rule) in window_rules.iter().enumerate() {
            let conditions = [&rule.exe, &rule.class, &rule.title_contains];
            if !conditions.iter().any(|condition| condition.is_some())
                || conditions.iter().any(|condition| {
                    condition
                        .as_ref()
                        .is_some_and(|value| value.trim().is_empty())
                })
            {
                return Err(format!(
                    "window_rules[{index}] needs nonempty match conditions"
                ));
            }
            let placing = rule.workspace.is_some() || rule.focus.is_some();
            if rule.mode.is_none() && rule.width.is_none() && rule.full_width.is_none() && !placing
            {
                return Err(format!(
                    "window_rules[{index}] needs a mode, width, full_width, workspace, or focus"
                ));
            }
            if let Some(width) = rule.width {
                range(&format!("window_rules[{index}].width"), width, 1, 100_000)?;
            }
            if matches!(rule.mode, Some(RuleMode::Ignore))
                && (rule.width.is_some() || rule.full_width.is_some() || placing)
            {
                return Err(format!(
                    "window_rules[{index}]: ignore cannot specify a width, workspace, or focus"
                ));
            }
            if let Some(spot) = &rule.workspace {
                let app = App {
                    exe: "-".into(),
                    workspace: Some(spot.clone()),
                    ..App::default()
                };
                app.check(Some(&workspaces))
                    .map_err(|error| format!("window_rules[{index}]: {error}"))?;
            }
        }
        let mut setups = Vec::new();
        let mut setup_keys = Vec::new();
        for (index, (name, document)) in setup_documents.into_iter().enumerate() {
            let label = format!("setups.{name}");
            if document.apps.is_some() && document.save_key.is_some() {
                return Err(format!(
                    "{label}: save_key needs the apps to live in setups.toml; remove apps"
                ));
            }
            let (apps, names) = match document.apps {
                Some(apps) => (apps, Some(workspaces.as_slice())),
                None => (saved.get(&name).cloned().unwrap_or_default(), None),
            };
            let apps = apps
                .into_iter()
                .enumerate()
                .map(|(number, app)| {
                    app.check(names)
                        .map_err(|error| format!("{label}.apps[{number}]: {error}"))
                })
                .collect::<Result<_, _>>()?;
            if let Some(keys) = document.key {
                setup_keys.push((format!("{label}.key"), keys, Action::LoadSetup(index)));
            }
            if let Some(keys) = document.save_key {
                setup_keys.push((format!("{label}.save_key"), keys, Action::SaveSetup(index)));
            }
            setups.push(Setup {
                saved: names.is_none(),
                name,
                apps,
            });
        }
        let startup_setup = match behavior.startup_setup.as_str() {
            "" => None,
            name => Some(
                setups
                    .iter()
                    .position(|setup| setup.name == name)
                    .ok_or_else(|| format!("behavior.startup_setup: unknown setup `{name}`"))?,
            ),
        };
        let bindings = bindings::compile(&hotkeys, &setup_keys)?;
        let mut agent_allow = Vec::new();
        for name in agent.allow {
            let index = bindings::action_index(&name)
                .ok_or_else(|| format!("unknown agent.allow action `{name}`"))?;
            let action = bindings::DEFAULTS[index].2;
            if agent_allow.contains(&action) {
                return Err(format!("duplicate agent.allow action `{name}`"));
            }
            agent_allow.push(action);
        }
        Ok(Self {
            workspaces,
            layout,
            behavior,
            overview,
            animations,
            options,
            style,
            bindings,
            agent_allow,
            setups,
            startup_setup,
            window_rules,
        })
    }

    /// Reject impossible geometry before committing any part of a reload.
    pub fn viewport(&self, work_area: Rect) -> Result<Rect, String> {
        let layout = &self.layout;
        let viewport = Rect {
            x: work_area.x + layout.reserved_left,
            y: work_area.y + layout.reserved_top,
            width: work_area.width - layout.reserved_left - layout.reserved_right,
            height: work_area.height - layout.reserved_top - layout.reserved_bottom,
        };
        let width = viewport.width - 2 * layout.gap;
        if width < 1
            || viewport.height - 2 * layout.gap < 1
            || viewport.width * layout.min_width_percent / 100 > width
        {
            return Err(
                "layout gaps/reserved space leave too little room in the monitor's work area"
                    .into(),
            );
        }
        Ok(viewport)
    }

    pub fn resolve_rule(
        &self,
        exe: &str,
        class: &str,
        title: &str,
        mut learned: Rule,
        opening: Mode,
        default_pixels: i32,
    ) -> Option<Joining> {
        if !self.behavior.remember_modes {
            learned.mode = opening;
        }
        if !self.behavior.remember_widths {
            learned.width = None;
        }
        let Some(rule) = self.window_rules.iter().find(|rule| {
            rule.exe
                .as_ref()
                .is_none_or(|name| name.eq_ignore_ascii_case(exe))
                && rule.class.as_ref().is_none_or(|name| name == class)
                && rule
                    .title_contains
                    .as_ref()
                    .is_none_or(|part| title.contains(part))
        }) else {
            return Some(Joining {
                rule: learned,
                workspace: None,
                focus: true,
            });
        };
        if let Some(mode) = rule.mode {
            learned.mode = mode.mode()?;
        }
        if rule.width.is_some() || rule.full_width.is_some() {
            let before = learned.width.unwrap_or(Width {
                pixels: default_pixels,
                full: false,
            });
            learned.width = Some(Width {
                pixels: rule.width.unwrap_or(before.pixels),
                full: rule.full_width.unwrap_or(before.full),
            });
        }
        Some(Joining {
            rule: learned,
            workspace: rule.workspace.clone(),
            focus: rule.focus.unwrap_or(true),
        })
    }
}

fn range(name: &str, value: i32, min: i32, max: i32) -> Result<(), String> {
    if !(min..=max).contains(&value) {
        return Err(format!("{name} must be between {min} and {max}"));
    }
    Ok(())
}

fn fraction(name: &str, text: &str, zero: bool) -> Result<Fraction, String> {
    let parsed = text
        .split_once('/')
        .and_then(|(n, d)| Some((n.trim().parse::<i32>().ok()?, d.trim().parse::<i32>().ok()?)));
    match parsed {
        Some((n, d)) if (1..=10_000).contains(&d) && (i32::from(!zero)..=d).contains(&n) => {
            Ok((n, d))
        }
        _ => Err(format!(
            "{name}: `{text}` must be a fraction {}..1, e.g. \"1/2\" (denominator 1..10000)",
            if zero { "0" } else { ">0" }
        )),
    }
}

fn color(name: &str, text: &str) -> Result<COLORREF, String> {
    let rgb = text
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6 && hex.is_ascii())
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .ok_or_else(|| format!("{name} must be a #RRGGBB color"))?;
    Ok(COLORREF(
        ((rgb & 0xff) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 0xff),
    ))
}

pub fn default_path() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(|folder| PathBuf::from(folder).join("wm").join("config.toml"))
        .ok_or_else(|| "APPDATA is unset; provide --config PATH".into())
}

/// Create once, without truncating an existing user's file.
pub fn init(path: &Path) -> Result<bool, String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(DEFAULT_CONFIG.as_bytes())
                .map_err(|error| format!("{}: {error}", path.display()))?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

pub fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// The config and setups.toml beside it, which may not exist yet.
pub fn read_all(path: &Path) -> Result<(String, String), String> {
    let saved = setups::path(path);
    let setups = match fs::read_to_string(&saved) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        other => other.map_err(|error| format!("{}: {error}", saved.display()))?,
    };
    Ok((read(path)?, setups))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_defaults_match_omitted_settings_and_every_binding() {
        let example = Config::parse(DEFAULT_CONFIG).unwrap();
        let empty = Config::parse("").unwrap();
        assert_eq!(example.bindings, empty.bindings);
        assert_eq!(example.agent_allow, empty.agent_allow);
        assert_eq!(example.options, empty.options);
        assert_eq!(example.style, empty.style);
        assert_eq!(example.animations, empty.animations);
        // Serialize-free comparison catches every extra field's default too.
        let explicit: Document = toml::from_str(DEFAULT_CONFIG).unwrap();
        let omitted = Document::default();
        assert_eq!(
            format!("{:?}", explicit.layout),
            format!("{:?}", omitted.layout)
        );
        assert_eq!(
            format!("{:?}", explicit.behavior),
            format!("{:?}", omitted.behavior)
        );
        assert_eq!(
            format!("{:?}", explicit.overview),
            format!("{:?}", omitted.overview)
        );
        assert_eq!(explicit.hotkeys.len(), bindings::DEFAULTS.len());
    }

    #[test]
    fn invalid_settings_are_rejected_before_application() {
        for text in [
            "[layuot]",
            "[layout]\ngap=-1",
            "[behavior]\nwrap_foucs=true",
            "[layout]\nwidth_presets=[]",
            "[layout]\nwidth_presets=['2/3','1/2']",
            "[layout]\ndefault_width='1/0'",
            "[overview]\nhighlight='white'",
            "[animations]\neasing='linear'",
            "[animations.horizontal]\nduration_ms=-1",
            "[animations.workspaces]\nduration_ms=2001",
            "[animations.overview]\nenabeld=true",
            "[hotkeys]\nquit=''",
            "[hotkeys]\nfocas_left='Alt+H'",
            "[hotkeys]\nfocus_left=1",
            "[hotkeys]\nfocus_left=['Alt+H', 'Alt+Nope']",
            "[[window_rules]]\nmode='float'",
            "[[window_rules]]\nexe='a.exe'\nmode='typo'",
        ] {
            assert!(Config::parse(text).is_err(), "accepted {text}");
        }
        let config = Config::parse("[layout]\nreserved_top=900").unwrap();
        let doubled = Config::parse("[hotkeys]\nfocus_left=['Alt+H', 'Alt+A']").unwrap();
        assert_eq!(doubled.bindings.len(), bindings::DEFAULTS.len() + 1);
        assert!(
            config
                .viewport(Rect {
                    x: 0,
                    y: 0,
                    width: 1000,
                    height: 600
                })
                .is_err()
        );
    }

    #[test]
    fn agent_allow_list_defaults_and_validation() {
        let default = Config::parse("").unwrap();
        assert_eq!(default.agent_allow.len(), bindings::DEFAULTS.len() - 2);
        assert!(!default.agent_allow.contains(&Action::Close));
        assert!(!default.agent_allow.contains(&Action::Quit));
        assert!(Config::parse("[agent]\nallow=['unknown']").is_err());
        assert!(Config::parse("[agent]\nallow=['close', 'close']").is_err());
        assert_eq!(
            Config::parse("[agent]\nallow=['close']")
                .unwrap()
                .agent_allow,
            vec![Action::Close]
        );
    }

    #[test]
    fn animation_switches_and_zero_duration_disable_only_the_selected_effect() {
        let config = Config::parse(
            "[animations.horizontal]\nenabled=false\n[animations.workspaces]\nduration_ms=0",
        )
        .unwrap();
        let settings = config.animations;
        assert_eq!(
            settings.horizontal.duration_ms, 300,
            "kept when only switched off"
        );
        assert!(settings.rate(settings.horizontal).is_none());
        assert!(settings.rate(settings.workspaces).is_none());
        assert!(settings.rate(settings.overview).is_some());
        let disabled = Config::parse("[animations]\nenabled=false")
            .unwrap()
            .animations;
        assert!(disabled.rate(disabled.overview).is_none());
    }

    #[test]
    fn explicit_rule_fields_override_learning_and_ignore_is_specific() {
        let config = Config::parse("[[window_rules]]\nexe='a.EXE'\nclass='A'\nmode='float'\n[[window_rules]]\nexe='a.exe'\nmode='ignore'").unwrap();
        let learned = Rule {
            mode: Mode::OnTop,
            width: Some(Width {
                pixels: 640,
                full: false,
            }),
        };
        let matched = config
            .resolve_rule("A.exe", "A", "title", learned, Mode::Tiled, 500)
            .unwrap()
            .rule;
        assert_eq!(matched.mode, Mode::Floating);
        assert_eq!(matched.width, learned.width);
        assert!(
            config
                .resolve_rule("a.exe", "B", "title", learned, Mode::Tiled, 500)
                .is_none()
        );
        assert_eq!(
            config
                .resolve_rule("b.exe", "A", "title", learned, Mode::Tiled, 500)
                .map(|joining| joining.rule),
            Some(learned)
        );
        let off = Config::parse("[behavior]\nremember_modes=false\nremember_widths=false").unwrap();
        assert_eq!(
            off.resolve_rule("b.exe", "A", "title", learned, Mode::Floating, 500)
                .map(|joining| joining.rule),
            Some(Rule {
                mode: Mode::Floating,
                width: None
            }),
            "falls back to the window's default mode"
        );
    }

    #[test]
    fn rules_place_windows_on_named_workspaces() {
        let text = "workspaces=['web','chat']
[[window_rules]]
exe='discord.exe'
workspace='chat'
focus=false";
        let config = Config::parse(text).unwrap();
        let joining = config
            .resolve_rule(
                "Discord.exe",
                "C",
                "t",
                Rule {
                    mode: Mode::Tiled,
                    width: None,
                },
                Mode::Tiled,
                500,
            )
            .unwrap();
        assert_eq!(joining.workspace, Some(Spot::Name("chat".into())));
        assert!(!joining.focus);
        for text in [
            "workspaces=['web','web']",
            "[[window_rules]]
exe='a.exe'
workspace='nowhere'",
            "[[window_rules]]
exe='a.exe'
workspace=0",
            "[[window_rules]]
exe='a.exe'
mode='ignore'
workspace=2",
        ] {
            assert!(Config::parse(text).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn setups_bind_keys_and_take_saved_apps_only_without_their_own() {
        let text = "workspaces=['web']
[behavior]
startup_setup='saved'
            [setups.saved]
key='Ctrl+Alt+1'
save_key=['Ctrl+Alt+Shift+1']
            [setups.written]
key='Ctrl+Alt+2'
apps=[{run='C:/x/firefox.exe', workspace='web'}]";
        let saved = "[[saved]]
exe='a.exe'
workspace='gone'
[[written]]
exe='ignored.exe'";
        let config = Config::parse_with(text, saved).unwrap();
        let [first, second] = &config.setups[..] else {
            panic!("two setups");
        };
        assert!(first.saved && !second.saved);
        assert_eq!(
            first.apps[0].exe, "a.exe",
            "saved names are checked on load"
        );
        assert_eq!(second.apps[0].exe, "firefox.exe");
        assert_eq!(config.startup_setup, Some(0));
        let actions: Vec<_> = config
            .bindings
            .iter()
            .map(|binding| binding.action)
            .collect();
        for action in [
            Action::LoadSetup(0),
            Action::SaveSetup(0),
            Action::LoadSetup(1),
        ] {
            assert!(actions.contains(&action));
        }
        for text in [
            "[setups.a]
save_key='Ctrl+Alt+1'
apps=[]",
            "[setups.a]
key='Alt+H'",
            "[setups.a]
apps=[{workspace='web'}]",
            "[behavior]
startup_setup='missing'",
        ] {
            assert!(Config::parse(text).is_err(), "accepted {text}");
        }
    }
}

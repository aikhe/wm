//! Setups: named lists of apps, each with the workspace, mode and width it
//! belongs at. Loading one moves the open windows into place and launches
//! the apps that aren't open; their windows are placed as they appear.
//!
//! A setup lists its apps in config.toml, or else in `setups.toml` beside
//! it, which its save key rewrites from the layout.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, thread};

use serde::Deserialize;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

use crate::config::RuleMode;
use crate::layout::{Mode, Width};

/// A workspace by its name, or by its number counted from 1 at the top.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Spot {
    Name(String),
    Number(u32),
}

/// One app in a setup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct App {
    /// The program file name its windows belong to, case insensitive.
    /// Defaults to the file name in `run`.
    pub exe: String,
    pub class: Option<String>,
    pub title_contains: Option<String>,
    /// What to launch when no window matches: a path, a name Windows knows
    /// (as in the Run dialog), a shortcut or a URL.
    pub run: Option<String>,
    pub args: Option<String>,
    /// `None` for the workspace on screen.
    pub workspace: Option<Spot>,
    pub mode: Option<RuleMode>,
    pub width: Option<i32>,
    pub full_width: Option<bool>,
}

impl App {
    /// Fills in `exe` and rejects what can't work. `names` are the named
    /// workspaces, or `None` to leave names unchecked.
    pub fn check(mut self, names: Option<&[String]>) -> Result<Self, String> {
        if self.exe.trim().is_empty() {
            let run = self.run.as_deref().unwrap_or_default();
            self.exe = Path::new(run.trim())
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        if self.exe.is_empty() {
            return Err("needs an exe or a run".into());
        }
        if self.mode == Some(RuleMode::Ignore) {
            return Err("mode cannot be ignore".into());
        }
        if self
            .width
            .is_some_and(|width| !(1..=100_000).contains(&width))
        {
            return Err("width must be between 1 and 100000".into());
        }
        match (&self.workspace, names) {
            (Some(Spot::Number(0)), _) => return Err("workspaces are numbered from 1".into()),
            (Some(Spot::Name(name)), Some(names)) if !names.contains(name) => {
                return Err(format!("unknown workspace `{name}`"));
            }
            _ => {}
        }
        Ok(self)
    }

    pub fn matches(&self, exe: &str, class: &str, title: &str) -> bool {
        self.exe.eq_ignore_ascii_case(exe)
            && self.class.as_ref().is_none_or(|name| name == class)
            && self
                .title_contains
                .as_ref()
                .is_none_or(|part| title.contains(part.as_str()))
    }

    /// The width set here, completed from `before` or the default.
    pub fn width(&self, before: Option<Width>, default_pixels: i32) -> Option<Width> {
        if self.width.is_none() && self.full_width.is_none() {
            return before;
        }
        let before = before.unwrap_or(Width {
            pixels: default_pixels,
            full: false,
        });
        Some(Width {
            pixels: self.width.unwrap_or(before.pixels),
            full: self.full_width.unwrap_or(before.full),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    pub name: String,
    pub apps: Vec<App>,
    /// Its apps live in setups.toml, so its save key may rewrite them.
    pub saved: bool,
}

pub fn path(config: &Path) -> PathBuf {
    config.with_file_name("setups.toml")
}

/// The saved setups as setups.toml holds them.
pub fn render(setups: &[Setup]) -> String {
    let mut text = String::from(
        "# Written by wm when a setup's save_key is pressed, from the layout at the time.\n\
         # Edit freely. Setups that list apps in config.toml are not kept here.\n",
    );
    for setup in setups.iter().filter(|setup| setup.saved) {
        for app in &setup.apps {
            let _ = writeln!(text, "\n[[{}]]", key(&setup.name));
            let mut field = |name: &str, value: String| {
                let _ = writeln!(text, "{name} = {value}");
            };
            field("exe", quote(&app.exe));
            if let Some(class) = &app.class {
                field("class", quote(class));
            }
            if let Some(part) = &app.title_contains {
                field("title_contains", quote(part));
            }
            if let Some(run) = &app.run {
                field("run", quote(run));
            }
            if let Some(args) = &app.args {
                field("args", quote(args));
            }
            match &app.workspace {
                Some(Spot::Name(name)) => field("workspace", quote(name)),
                Some(Spot::Number(number)) => field("workspace", number.to_string()),
                None => {}
            }
            if let Some(mode) = app.mode {
                field("mode", quote(mode.word()));
            }
            if let Some(width) = app.width {
                field("width", width.to_string());
            }
            if let Some(full) = app.full_width {
                field("full_width", full.to_string());
            }
        }
    }
    text
}

/// Writes the file on its own thread, so no key press waits on the disk.
/// Saves made in quick succession write one at a time, and only the latest.
pub fn save(path: PathBuf, text: String) {
    static LATEST: AtomicU64 = AtomicU64::new(0);
    static WRITING: Mutex<()> = Mutex::new(());
    let save = LATEST.fetch_add(1, Ordering::SeqCst) + 1;
    thread::spawn(move || {
        let _writing = WRITING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if LATEST.load(Ordering::SeqCst) != save {
            return;
        }
        let temporary = path.with_extension("toml.tmp");
        let written = fs::write(&temporary, text).and_then(|()| fs::rename(&temporary, &path));
        if let Err(error) = written {
            eprintln!("couldn't save {}: {error}", path.display());
        }
    });
}

/// Starts each `(run, args)` the way the Run dialog would, on its own
/// thread, since the shell can take its time.
pub fn launch(commands: Vec<(String, Option<String>)>) {
    if commands.is_empty() {
        return;
    }
    thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        for (run, args) in commands {
            let file = HSTRING::from(run.as_str());
            let args = args.map(|args| HSTRING::from(args.as_str()));
            let parameters = args
                .as_ref()
                .map_or(PCWSTR::null(), |args| PCWSTR(args.as_ptr()));
            let result = unsafe {
                ShellExecuteW(
                    None,
                    w!("open"),
                    &file,
                    parameters,
                    PCWSTR::null(),
                    SW_SHOWNORMAL,
                )
            };
            // Values up to 32 are errors.
            if result.0 as usize <= 32 {
                eprintln!("couldn't launch {run} (error {})", result.0 as usize);
            }
        }
    });
}

/// A TOML key, quoted only when it has to be.
fn key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare { name.to_owned() } else { quote(name) }
}

fn quote(text: &str) -> String {
    let mut quoted = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(quoted, "\\u{:04X}", c as u32);
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

impl RuleMode {
    pub fn word(self) -> &'static str {
        match self {
            RuleMode::Tiled => "tiled",
            RuleMode::Float => "float",
            RuleMode::Sticky => "sticky",
            RuleMode::Top => "top",
            RuleMode::Ignore => "ignore",
        }
    }

    /// `None` for ignore.
    pub fn mode(self) -> Option<Mode> {
        Some(match self {
            RuleMode::Tiled => Mode::Tiled,
            RuleMode::Float => Mode::Floating,
            RuleMode::Sticky => Mode::Sticky,
            RuleMode::Top => Mode::OnTop,
            RuleMode::Ignore => return None,
        })
    }

    pub fn of(mode: Mode) -> Self {
        match mode {
            Mode::Tiled => RuleMode::Tiled,
            Mode::Floating => RuleMode::Float,
            Mode::Sticky => RuleMode::Sticky,
            Mode::OnTop => RuleMode::Top,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_comes_from_run_and_matching_ignores_its_case() {
        let app = App {
            run: Some(r"C:\Program Files\Mozilla Firefox\firefox.exe".into()),
            ..App::default()
        }
        .check(None)
        .unwrap();
        assert_eq!(app.exe, "firefox.exe");
        assert!(app.matches("Firefox.EXE", "MozillaWindowClass", "Tab"));
        assert!(App::default().check(None).is_err());
        let named = App {
            exe: "a.exe".into(),
            workspace: Some(Spot::Name("web".into())),
            ..App::default()
        };
        assert!(named.clone().check(Some(&[])).is_err());
        assert!(
            named.check(None).is_ok(),
            "saved setups aren't held to names"
        );
    }

    #[test]
    fn saved_setups_render_as_toml_they_are_read_back_from() {
        let app = App {
            exe: "wezterm-gui.exe".into(),
            class: Some("org.wezfurlong.wezterm".into()),
            run: Some(r#"C:\Program Files\WezTerm\wezterm-gui.exe"#.into()),
            workspace: Some(Spot::Number(3)),
            mode: Some(RuleMode::Tiled),
            width: Some(1264),
            full_width: Some(true),
            ..App::default()
        };
        let setups = [
            Setup {
                name: "my setup".into(),
                apps: vec![app.clone()],
                saved: true,
            },
            Setup {
                name: "written".into(),
                apps: vec![app.clone()],
                saved: false,
            },
        ];
        let text = render(&setups);
        let read: std::collections::BTreeMap<String, Vec<App>> = toml::from_str(&text).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read["my setup"], [app]);
    }
}

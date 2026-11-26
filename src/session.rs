//! Where each window was when wm last changed or exited, so that the windows
//! still open when it starts again (or after a crash) go back to their
//! workspaces and columns, kept as they were.
//!
//! The session lives in `%APPDATA%\wm\session.txt`, an optional `# active\tN`
//! header followed by one window per line in layout order, with tabs between
//! its mode (`tiled`, `float`, `sticky`, `top`, or `scratch`), its workspace
//! counted from 0 at the top, its handle, its program's file name, its window
//! class and, when tiled, its column width in pixels and then `full`.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fs, thread};

use windows::Win32::Foundation::HWND;

use crate::layout::{Mode, Width};
use crate::rules::MODES;
use crate::window::Window;

/// A window as the layout kept it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub window: Window,
    pub mode: Mode,
    /// Whether this window belongs to the scratchpad.
    pub scratchpad: bool,
    /// Meaningless for windows shown on every workspace or in the scratchpad.
    pub workspace: usize,
    /// Its program's file name and window class, since Windows reuses the
    /// handles of closed windows.
    pub exe: String,
    pub class: String,
    pub width: Option<Width>,
}

impl Entry {
    /// Whether this is the window as it was saved, and not a later one that
    /// was given its handle.
    pub fn is(&self, window: Window, exe: &str, class: &str) -> bool {
        self.window == window && self.exe == exe && self.class == class
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Saved {
    pub active: Option<usize>,
    pub entries: Vec<Entry>,
}

pub fn default_path() -> Option<PathBuf> {
    env::var_os("APPDATA").map(|folder| PathBuf::from(folder).join("wm").join("session.txt"))
}

/// The windows saved last. A missing or unreadable file means none.
pub fn load() -> Saved {
    default_path().map_or_else(Saved::default, |path| load_from(&path))
}

pub fn load_from(path: &Path) -> Saved {
    let text = fs::read_to_string(path).ok();
    parse(&text.unwrap_or_default())
}

/// Renders the session to its exact on-disk text.
pub fn render(active: usize, entries: &[Entry]) -> String {
    let mut text = format!("# active\t{active}\n");
    for entry in entries {
        let word = if entry.scratchpad {
            "scratch"
        } else {
            let Some((_, word)) = MODES.into_iter().find(|(mode, _)| *mode == entry.mode) else {
                continue;
            };
            word
        };
        let handle = entry.window.0.0 as usize;
        let width = match entry.width {
            None => String::new(),
            Some(Width { pixels, full: true }) => format!("\t{pixels}\tfull"),
            Some(Width { pixels, .. }) => format!("\t{pixels}"),
        };
        let Entry {
            workspace,
            exe,
            class,
            ..
        } = entry;
        text.push_str(&format!(
            "{word}\t{workspace}\t{handle}\t{exe}\t{class}{width}\n"
        ));
    }
    text
}

/// Writes `text` atomically via a temporary file and rename.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("txt.tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path)
}

/// Synchronously flushes the session on shutdown, superseding any queued
/// background save.
pub fn save_sync(path: &Path, text: &str) {
    LATEST.fetch_add(1, Ordering::SeqCst);
    let _writing = WRITING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Err(error) = write_atomic(path, text) {
        eprintln!("couldn't save {}: {error}", path.display());
    }
}

/// Writes `text` atomically on a background thread so hotkeys never wait on
/// the disk. Rapid saves coalesce and only the latest is written.
pub fn save_async(path: PathBuf, text: String) {
    let ticket = LATEST.fetch_add(1, Ordering::SeqCst) + 1;
    thread::spawn(move || {
        let _writing = WRITING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if LATEST.load(Ordering::SeqCst) != ticket {
            return;
        }
        if let Err(error) = write_atomic(&path, &text) {
            eprintln!("couldn't save {}: {error}", path.display());
        }
    });
}

static LATEST: AtomicU64 = AtomicU64::new(0);
static WRITING: Mutex<()> = Mutex::new(());

/// Lines that aren't a window or `# active` header are skipped.
pub fn parse(text: &str) -> Saved {
    let mut active = None;
    let mut entries = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# active\t") {
            if let Ok(index) = rest.trim().parse::<usize>() {
                active = Some(index);
            }
            continue;
        }
        let mut fields = line.split('\t');
        let Some(word) = fields.next() else {
            continue;
        };
        let (mode, scratchpad) = if word == "scratch" {
            (Mode::Floating, true)
        } else {
            let Some((mode, _)) = MODES.into_iter().find(|(_, known)| *known == word) else {
                continue;
            };
            (mode, false)
        };
        let Some(workspace) = fields.next().and_then(|value| value.parse().ok()) else {
            continue;
        };
        let Some(handle) = fields.next().and_then(|value| value.parse::<usize>().ok()) else {
            continue;
        };
        let (Some(exe), Some(class)) = (fields.next(), fields.next()) else {
            continue;
        };
        let pixels = fields.next().and_then(|pixels| pixels.parse().ok());
        let full = fields.next() == Some("full");
        entries.push(Entry {
            window: Window(HWND(handle as *mut c_void)),
            mode,
            scratchpad,
            workspace,
            exe: exe.to_owned(),
            class: class.to_owned(),
            width: pixels.map(|pixels| Width { pixels, full }),
        });
    }
    Saved { active, entries }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn sessions_survive_being_saved_and_read_back() {
        let text = "# active\t2\n\
                    tiled\t0\t1180\tfirefox.exe\tMozillaWindowClass\t1264\n\
                    tiled\t2\t65602\twezterm-gui.exe\torg.wezfurlong.wezterm\t960\tfull\n\
                    float\t2\t4242\tscrcpy.exe\tSDL_app\n\
                    top\t0\t77\tmpv.exe\tmpv\n\
                    scratch\t0\t9999\tWindowsTerminal.exe\tCASCADIA_HOSTING_WINDOW_CLASS\n";
        let saved = parse(text);
        assert_eq!(saved.active, Some(2));
        assert_eq!(saved.entries.len(), 5);
        assert_eq!(render(2, &saved.entries), text);
        let wezterm = &saved.entries[1];
        assert!(wezterm.is(
            Window(HWND(65602 as *mut c_void)),
            "wezterm-gui.exe",
            "org.wezfurlong.wezterm"
        ));
        assert!(!wezterm.is(Window(HWND(65602 as *mut c_void)), "notepad.exe", "Notepad"));
        assert!(saved.entries[4].scratchpad);
        assert_eq!(saved.entries[4].mode, Mode::Floating);
    }

    #[test]
    fn atomic_write_replaces_existing_session_cleanly() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("wm-session-test-{}-{unique}", std::process::id()));
        let file = dir.join("session.txt");
        let first = render(
            0,
            &[Entry {
                window: Window(HWND(101 as *mut c_void)),
                mode: Mode::Tiled,
                scratchpad: false,
                workspace: 0,
                exe: "a.exe".into(),
                class: "A".into(),
                width: Some(Width {
                    pixels: 800,
                    full: false,
                }),
            }],
        );
        save_sync(&file, &first);
        assert_eq!(load_from(&file).active, Some(0));
        let second = render(
            1,
            &[Entry {
                window: Window(HWND(202 as *mut c_void)),
                mode: Mode::Floating,
                scratchpad: true,
                workspace: 0,
                exe: "b.exe".into(),
                class: "B".into(),
                width: None,
            }],
        );
        save_sync(&file, &second);
        let loaded = load_from(&file);
        assert_eq!(loaded.active, Some(1));
        assert_eq!(loaded.entries.len(), 1);
        assert!(loaded.entries[0].scratchpad);
        let _ = fs::remove_dir_all(&dir);
    }
}

//! Where each window was when wm last exited, so that the windows still open
//! when it starts again go back to their workspaces and columns, kept as they
//! were.
//!
//! The session lives in `%APPDATA%\wm\session.txt`, one window per line in
//! layout order, with tabs between its mode, its workspace counted from 0 at
//! the top, its handle, its program's file name, its window class and, when
//! tiled, its column width in pixels and then `full`.

use std::ffi::c_void;
use std::path::PathBuf;
use std::{env, fs};

use windows::Win32::Foundation::HWND;

use crate::layout::{Mode, Width};
use crate::rules::MODES;
use crate::window::Window;

/// A window as the layout kept it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub window: Window,
    pub mode: Mode,
    /// Meaningless for windows shown on every workspace.
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

fn path() -> Option<PathBuf> {
    env::var_os("APPDATA").map(|folder| PathBuf::from(folder).join("wm").join("session.txt"))
}

/// The windows saved last. A missing or unreadable file means none.
pub fn load() -> Vec<Entry> {
    let text = path().and_then(|path| fs::read_to_string(path).ok());
    parse(&text.unwrap_or_default())
}

pub fn save(entries: &[Entry]) {
    let Some(path) = path() else {
        return;
    };
    let folder = path.parent().map_or(Ok(()), fs::create_dir_all);
    if let Err(error) = folder.and_then(|()| fs::write(&path, render(entries))) {
        eprintln!("couldn't save {}: {error}", path.display());
    }
}

/// Lines that aren't a window are skipped.
fn parse(text: &str) -> Vec<Entry> {
    let entry = |line: &str| {
        let mut fields = line.split('\t');
        let word = fields.next()?;
        let (mode, _) = MODES.into_iter().find(|(_, known)| *known == word)?;
        let workspace = fields.next()?.parse().ok()?;
        let handle: usize = fields.next()?.parse().ok()?;
        let (exe, class) = (fields.next()?.to_owned(), fields.next()?.to_owned());
        let pixels = fields.next().and_then(|pixels| pixels.parse().ok());
        let full = fields.next() == Some("full");
        Some(Entry {
            window: Window(HWND(handle as *mut c_void)),
            mode,
            workspace,
            exe,
            class,
            width: pixels.map(|pixels| Width { pixels, full }),
        })
    };
    text.lines().filter_map(entry).collect()
}

fn render(entries: &[Entry]) -> String {
    let line = |entry: &Entry| {
        let (_, word) = MODES.into_iter().find(|(mode, _)| *mode == entry.mode)?;
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
        Some(format!(
            "{word}\t{workspace}\t{handle}\t{exe}\t{class}{width}\n"
        ))
    };
    entries.iter().filter_map(line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_survive_being_saved_and_read_back() {
        let text = "tiled\t0\t1180\tfirefox.exe\tMozillaWindowClass\t1264\n\
                    tiled\t2\t65602\twezterm-gui.exe\torg.wezfurlong.wezterm\t960\tfull\n\
                    float\t2\t4242\tscrcpy.exe\tSDL_app\n\
                    top\t0\t77\tmpv.exe\tmpv\n";
        let entries = parse(text);
        assert_eq!(entries.len(), 4);
        assert_eq!(render(&entries), text);
        let wezterm = &entries[1];
        assert!(wezterm.is(
            Window(HWND(65602 as *mut c_void)),
            "wezterm-gui.exe",
            "org.wezfurlong.wezterm"
        ));
        assert!(!wezterm.is(Window(HWND(65602 as *mut c_void)), "notepad.exe", "Notepad"));
    }
}

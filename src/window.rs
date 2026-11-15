use std::ffi::OsString;
use std::fmt;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWINDOWATTRIBUTE, DwmGetWindowAttribute,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetWindow, GetWindowLongW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, WINDOW_EX_STYLE, WS_EX_APPWINDOW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PWSTR, Result};

/// A top-level window, usually owned by another process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window(HWND);

impl Window {
    /// Every visible top-level window, in z-order from the top.
    pub fn visible() -> Result<Vec<Window>> {
        unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let windows = unsafe { &mut *(lparam.0 as *mut Vec<Window>) };
            if unsafe { IsWindowVisible(hwnd) }.as_bool() {
                windows.push(Window(hwnd));
            }
            true.into()
        }

        let mut windows = Vec::new();
        unsafe { EnumWindows(Some(collect), LPARAM(&raw mut windows as isize))? };
        Ok(windows)
    }

    /// Reads the window's properties. None of these calls wait on the owning app,
    /// so this stays fast when that app is hung.
    pub fn info(self) -> Info {
        let hwnd = self.0;
        let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) };

        Info {
            title: text(|buffer| unsafe { GetWindowTextW(hwnd, buffer) }),
            class: text(|buffer| unsafe { GetClassNameW(hwnd, buffer) }),
            exe: self.exe(),
            bounds: self
                .dwm_attribute(DWMWA_EXTENDED_FRAME_BOUNDS)
                .unwrap_or_default(),
            cloaked: self
                .dwm_attribute::<u32>(DWMWA_CLOAKED)
                .is_some_and(|cloaked| cloaked != 0),
            minimized: unsafe { IsIconic(hwnd) }.as_bool(),
            owned: unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|owner| !owner.is_invalid()),
            ex_style: WINDOW_EX_STYLE(ex_style.cast_unsigned()),
        }
    }

    fn exe(self) -> Option<String> {
        let mut process_id = 0;
        unsafe { GetWindowThreadProcessId(self.0, Some(&mut process_id)) };
        let process =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;

        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let read = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut len,
            )
        };
        // Nothing useful to do if closing fails; the handle is ours either way.
        let _ = unsafe { CloseHandle(process) };
        read.ok()?;

        let path = PathBuf::from(OsString::from_wide(&buffer[..len as usize]));
        Some(path.file_name()?.to_string_lossy().into_owned())
    }

    /// `T` must be the type DWM documents for `attribute`.
    fn dwm_attribute<T: Default>(self, attribute: DWMWINDOWATTRIBUTE) -> Option<T> {
        let mut value = T::default();
        unsafe {
            DwmGetWindowAttribute(
                self.0,
                attribute,
                (&raw mut value).cast(),
                size_of::<T>() as u32,
            )
        }
        .ok()?;
        Some(value)
    }
}

/// Decodes a string from a Win32 call that fills a buffer and returns its length.
fn text(read: impl FnOnce(&mut [u16]) -> i32) -> String {
    let mut buffer = [0u16; 256];
    let len = usize::try_from(read(&mut buffer)).unwrap_or_default();
    String::from_utf16_lossy(&buffer[..len])
}

/// A window's properties, read once so the tiling decision is a pure function.
#[derive(Debug, Default)]
pub struct Info {
    pub title: String,
    pub class: String,
    /// `None` when the process can't be opened, as with elevated apps.
    pub exe: Option<String>,
    /// Visible bounds in physical pixels, excluding the invisible resize border.
    pub bounds: RECT,
    pub cloaked: bool,
    pub minimized: bool,
    pub owned: bool,
    pub ex_style: WINDOW_EX_STYLE,
}

/// Why a visible window is left out of the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Cloaked,
    Minimized,
    NoActivate,
    ToolWindow,
    Owned,
    Untitled,
}

impl Info {
    /// The reason this window isn't tiled, or `None` if it is.
    ///
    /// Follows the taskbar's rule: unowned, non-tool windows are apps, and
    /// `WS_EX_APPWINDOW` forces a window to count as one.
    pub fn skip(&self) -> Option<Skip> {
        let has = |style| self.ex_style.contains(style);
        let app_window = has(WS_EX_APPWINDOW);

        [
            (self.cloaked, Skip::Cloaked),
            (self.minimized, Skip::Minimized),
            (has(WS_EX_NOACTIVATE), Skip::NoActivate),
            (has(WS_EX_TOOLWINDOW) && !app_window, Skip::ToolWindow),
            (self.owned && !app_window, Skip::Owned),
            (self.title.is_empty(), Skip::Untitled),
        ]
        .into_iter()
        .find_map(|(applies, skip)| applies.then_some(skip))
    }
}

impl fmt::Display for Info {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let RECT {
            left,
            top,
            right,
            bottom,
        } = self.bounds;
        write!(
            f,
            "{:<22} {:>4}x{:<4} at {:>5},{:<5} {:<28} {}",
            self.exe.as_deref().unwrap_or("?"),
            right - left,
            bottom - top,
            left,
            top,
            self.class,
            self.title,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> Info {
        Info {
            title: "Editor".into(),
            ..Info::default()
        }
    }

    #[test]
    fn plain_app_window_is_tiled() {
        assert_eq!(app().skip(), None);
    }

    #[test]
    fn owned_and_tool_windows_are_skipped() {
        assert_eq!(
            Info {
                owned: true,
                ..app()
            }
            .skip(),
            Some(Skip::Owned)
        );
        assert_eq!(
            Info {
                ex_style: WS_EX_TOOLWINDOW,
                ..app()
            }
            .skip(),
            Some(Skip::ToolWindow)
        );
    }

    #[test]
    fn app_window_style_overrides_owner_and_tool_window() {
        let forced = Info {
            owned: true,
            ex_style: WS_EX_TOOLWINDOW | WS_EX_APPWINDOW,
            ..app()
        };
        assert_eq!(forced.skip(), None);
    }
}

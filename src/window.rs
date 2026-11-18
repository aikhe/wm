use std::ffi::OsString;
use std::fmt;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_TRANSITIONS_FORCEDISABLED,
    DWMWINDOWATTRIBUTE, DwmGetWindowAttribute, DwmSetWindowAttribute,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_MOUSE, SendInput};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetClassNameW, GetForegroundWindow,
    GetShellWindow, GetWindow, GetWindowLongW, GetWindowPlacement, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, IsIconic, IsWindowVisible,
    IsZoomed, PostMessageW, SW_HIDE, SW_SHOWMAXIMIZED, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE,
    SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow,
    SetWindowPlacement, SetWindowPos, ShowWindowAsync, WINDOW_EX_STYLE, WINDOW_STYLE,
    WINDOWPLACEMENT, WM_CLOSE, WPF_ASYNCWINDOWPLACEMENT, WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW,
    WS_EX_DLGMODALFRAME, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_THICKFRAME,
};
use windows::core::{BOOL, PWSTR, Result};

use crate::layout::Rect;

/// A window's position and state at a moment when the layout wasn't placing it.
pub type Snapshot = WINDOWPLACEMENT;

/// A window, usually owned by another process.
///
/// Nothing here waits on the owning app, so a hung app can't stall the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window(pub HWND);

impl Window {
    /// Stops Windows fading or zooming the window as it shows and hides.
    /// For our own surfaces, which animate what's on them themselves.
    pub fn disable_native_transitions(self) {
        let disabled = BOOL(1);
        let _ = unsafe {
            DwmSetWindowAttribute(
                self.0,
                DWMWA_TRANSITIONS_FORCEDISABLED,
                (&raw const disabled).cast(),
                size_of::<BOOL>() as u32,
            )
        };
    }

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

    /// The desktop itself, which can hold focus when no window should.
    pub fn desktop() -> Window {
        Window(unsafe { GetShellWindow() })
    }

    /// The window the user is typing into, if any.
    pub fn foreground() -> Option<Window> {
        let hwnd = unsafe { GetForegroundWindow() };
        (!hwnd.is_invalid()).then_some(Window(hwnd))
    }

    pub fn info(self) -> Info {
        let hwnd = self.0;
        let style = |index| unsafe { GetWindowLongW(hwnd, index) }.cast_unsigned();

        Info {
            title: self.title(),
            class: self.class(),
            bounds: self.bounds(),
            visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
            cloaked: self.cloaked(),
            minimized: unsafe { IsIconic(hwnd) }.as_bool(),
            owned: unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|owner| !owner.is_invalid()),
            style: WINDOW_STYLE(style(GWL_STYLE)),
            ex_style: WINDOW_EX_STYLE(style(GWL_EXSTYLE)),
        }
    }

    /// Whether Windows is keeping the window off the screen right now,
    /// whoever asked it to. Windows shows a cloaked window again by itself
    /// once it takes the keyboard.
    pub fn cloaked(self) -> bool {
        self.dwm_attribute::<u32>(DWMWA_CLOAKED)
            .is_some_and(|cloaked| cloaked != 0)
    }

    pub fn title(self) -> String {
        text(|buffer| unsafe { GetWindowTextW(self.0, buffer) })
    }

    /// The window class its app registered, the same for every such window.
    pub fn class(self) -> String {
        text(|buffer| unsafe { GetClassNameW(self.0, buffer) })
    }

    /// Why this window is left out of the layout, or `None` if it's tiled.
    pub fn skip(self, info: &Info) -> Option<Skip> {
        // Checked last because, unlike the rest, it sends the window a request.
        info.skip()
            .or_else(|| self.refuses_moves().then_some(Skip::Elevated))
    }

    pub fn is_tileable(self) -> bool {
        self.skip(&self.info()).is_none()
    }

    /// Whether Windows blocks us from moving it, as it does for the windows of
    /// elevated apps when we aren't elevated ourselves.
    fn refuses_moves(self) -> bool {
        let nothing = SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER;
        unsafe { SetWindowPos(self.0, None, 0, 0, 0, 0, nothing) }
            .is_err_and(|error| error.code() == E_ACCESSDENIED)
    }

    /// The file name of the owning process, or `None` if it can't be opened.
    pub fn exe(self) -> Option<String> {
        Some(self.exe_path()?.file_name()?.to_string_lossy().into_owned())
    }

    /// The full path of the owning process's program.
    pub fn exe_path(self) -> Option<PathBuf> {
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

        Some(PathBuf::from(OsString::from_wide(&buffer[..len as usize])))
    }

    /// Moves the window so its visible frame fills `rect`.
    ///
    /// Returns `false` for a maximized window. It gets restored, but its frame
    /// can't be measured until the app has done that, so place it again later.
    pub fn place(self, rect: Rect) -> bool {
        let hwnd = self.0;
        let maximized = unsafe { IsZoomed(hwnd) }.as_bool();
        if maximized {
            let _ = unsafe { ShowWindowAsync(hwnd, SW_SHOWNOACTIVATE) };
        }

        // Windows 10 windows carry an invisible resize border outside the
        // frame, and SetWindowPos positions that outer edge.
        let mut outer = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut outer) }.is_err() {
            return true;
        }
        let frame = self.frame().unwrap_or(outer);
        let (left, top) = (frame.left - outer.left, frame.top - outer.top);
        let (right, bottom) = (outer.right - frame.right, outer.bottom - frame.bottom);

        // Fails only if the window is gone, and an event will tell us that.
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                None,
                rect.x - left,
                rect.y - top,
                rect.width + left + right,
                rect.height + top + bottom,
                SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOZORDER,
            )
        };
        !maximized
    }

    /// Brings the window above the other ordinary windows without focusing it.
    pub fn raise(self) {
        self.reorder(HWND_TOP);
    }

    /// Keeps the window above every ordinary window, or stops doing so.
    pub fn set_topmost(self, topmost: bool) {
        self.reorder(if topmost {
            HWND_TOPMOST
        } else {
            HWND_NOTOPMOST
        });
    }

    /// Changes the window's place in the z-order and nothing else about it.
    fn reorder(self, place: HWND) {
        let flags = SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE;
        // Fails only if the window is gone.
        let _ = unsafe { SetWindowPos(self.0, Some(place), 0, 0, 0, 0, flags) };
    }

    /// Asks the app to close the window, as its close button would.
    pub fn close(self) {
        // Fails only if the window is already gone.
        let _ = unsafe { PostMessageW(Some(self.0), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }

    /// Makes this the foreground window.
    pub fn focus(self) {
        // Windows only lets a process take the foreground just after it has
        // received input, so send ourselves an empty mouse event first.
        let input = [INPUT {
            r#type: INPUT_MOUSE,
            ..Default::default()
        }];
        unsafe {
            SendInput(&input, size_of::<INPUT>() as i32);
            let _ = SetForegroundWindow(self.0);
        }
    }

    pub fn snapshot(self) -> Option<Snapshot> {
        let mut snapshot = Snapshot {
            length: size_of::<Snapshot>() as u32,
            ..Default::default()
        };
        unsafe { GetWindowPlacement(self.0, &mut snapshot) }.ok()?;
        Some(snapshot)
    }

    /// Puts the window back where `saved` had it. A window that has since been
    /// hidden or minimized stays that way and returns there when next shown.
    pub fn restore(self, saved: Snapshot) {
        let hwnd = self.0;
        let show = if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            SW_HIDE
        } else if unsafe { IsIconic(hwnd) }.as_bool() {
            SW_SHOWMINNOACTIVE
        } else if saved.showCmd == SW_SHOWMAXIMIZED.0 as u32 {
            SW_SHOWMAXIMIZED
        } else {
            SW_SHOWNOACTIVATE
        };
        let placement = Snapshot {
            showCmd: show.0 as u32,
            flags: saved.flags | WPF_ASYNCWINDOWPLACEMENT,
            ..saved
        };
        // Fails only if the window is gone.
        let _ = unsafe { SetWindowPlacement(hwnd, &placement) };
    }

    /// Where the window's visible frame is on screen, or an empty rectangle
    /// if it can't be measured.
    pub fn bounds(self) -> Rect {
        self.frame().unwrap_or_default().into()
    }

    /// Where the window's picture belongs on the desktop: in the layout's
    /// `slot` at the size the window really is, so that it's never scaled. A
    /// floating window has no slot, and is wherever it is.
    pub fn pose(self, slot: Option<Rect>) -> Rect {
        let actual = self.bounds();
        match slot {
            Some(slot) if actual.width > 0 && actual.height > 0 => Rect {
                width: actual.width,
                height: actual.height,
                ..slot
            },
            Some(slot) => slot,
            None => actual,
        }
    }

    /// The visible frame, which excludes the invisible resize border.
    fn frame(self) -> Option<RECT> {
        self.dwm_attribute(DWMWA_EXTENDED_FRAME_BOUNDS)
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

impl From<Rect> for RECT {
    fn from(rect: Rect) -> Self {
        RECT {
            left: rect.x,
            top: rect.y,
            right: rect.x + rect.width,
            bottom: rect.y + rect.height,
        }
    }
}

impl From<RECT> for Rect {
    fn from(rect: RECT) -> Self {
        Rect {
            x: rect.left,
            y: rect.top,
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
        }
    }
}

/// A window's properties, read once so the tiling decision is a pure function.
#[derive(Debug, Default)]
pub struct Info {
    pub title: String,
    pub class: String,
    /// The visible frame, excluding the invisible resize border.
    pub bounds: Rect,
    pub visible: bool,
    pub cloaked: bool,
    pub minimized: bool,
    pub owned: bool,
    pub style: WINDOW_STYLE,
    pub ex_style: WINDOW_EX_STYLE,
}

/// Why a window is left out of the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Hidden,
    Cloaked,
    Minimized,
    Child,
    NoActivate,
    ToolWindow,
    Owned,
    Dialog,
    Uncaptioned,
    Untitled,
    Elevated,
}

impl Info {
    /// The reason these properties rule the window out, or `None` if they don't.
    ///
    /// Starts from the taskbar's rule: unowned, non-tool windows are apps, and
    /// `WS_EX_APPWINDOW` forces a window to count as one. Requiring a caption
    /// then drops shell surfaces such as the Start menu and the Alt+Tab switcher.
    pub fn skip(&self) -> Option<Skip> {
        let has = |style| self.ex_style.contains(style);
        let app_window = has(WS_EX_APPWINDOW);

        [
            (!self.visible, Skip::Hidden),
            (self.cloaked, Skip::Cloaked),
            (self.minimized, Skip::Minimized),
            (self.style.contains(WS_CHILD), Skip::Child),
            (has(WS_EX_NOACTIVATE), Skip::NoActivate),
            (has(WS_EX_TOOLWINDOW) && !app_window, Skip::ToolWindow),
            (self.owned && !app_window, Skip::Owned),
            (has(WS_EX_DLGMODALFRAME), Skip::Dialog),
            (!self.style.contains(WS_CAPTION), Skip::Uncaptioned),
            (self.title.is_empty(), Skip::Untitled),
        ]
        .into_iter()
        .find_map(|(applies, skip)| applies.then_some(skip))
    }

    /// Whether it looks like a dialog, which opens floating as in niri: it
    /// can't be resized, or it uses the standard dialog class.
    pub fn dialog_like(&self) -> bool {
        !self.style.contains(WS_THICKFRAME) || self.class == "#32770"
    }
}

impl fmt::Display for Info {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Rect {
            x,
            y,
            width,
            height,
        } = self.bounds;
        write!(
            f,
            "{width:>4}x{height:<4} at {x:>5},{y:<5} {:<28} {}",
            self.class, self.title
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> Info {
        Info {
            title: "Editor".into(),
            visible: true,
            style: WS_CAPTION,
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

    #[test]
    fn child_controls_and_captionless_surfaces_are_skipped() {
        assert_eq!(
            Info {
                style: WS_CAPTION | WS_CHILD,
                ..app()
            }
            .skip(),
            Some(Skip::Child)
        );
        assert_eq!(
            Info {
                style: WINDOW_STYLE(0),
                ..app()
            }
            .skip(),
            Some(Skip::Uncaptioned)
        );
    }

    #[test]
    fn fixed_size_and_dialog_class_windows_are_dialog_like() {
        let resizable = Info {
            style: WS_CAPTION | WS_THICKFRAME,
            ..app()
        };
        assert!(!resizable.dialog_like());
        assert!(app().dialog_like(), "no resize border");
        let dialog = Info {
            class: "#32770".into(),
            ..resizable
        };
        assert!(dialog.dialog_like());
    }
}

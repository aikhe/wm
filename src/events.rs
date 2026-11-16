use std::ffi::c_void;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_SHOW,
    EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, MSG, OBJID_WINDOW, PostThreadMessageW,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP,
};

use crate::window::Window;

/// The thread message that carries a window event to the message loop.
pub const MESSAGE: u32 = WM_APP;

/// The event ranges to hook. Kept narrow so that noisy events, such as every
/// cursor move, never wake the process.
const RANGES: [(u32, u32); 5] = [
    (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
    (EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND),
    (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
    (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
    (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
];

/// Something that happened to a window. It may be any window on the desktop,
/// including child controls, so check it against the layout before acting.
#[derive(Debug, Clone, Copy)]
pub enum Event {
    /// Shown, uncloaked or restored from minimized.
    Appeared(Window),
    /// Hidden, cloaked or minimized, but still alive.
    Vanished(Window),
    Destroyed(Window),
    Focused(Window),
    /// The user finished dragging or resizing it.
    Moved(Window),
}

impl Event {
    /// Decodes a `MESSAGE` from the calling thread's queue.
    pub fn from_message(message: &MSG) -> Option<Event> {
        let window = Window(HWND(message.lParam.0 as *mut c_void));

        Some(match message.wParam.0 as u32 {
            EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED | EVENT_SYSTEM_MINIMIZEEND => {
                Event::Appeared(window)
            }
            EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED | EVENT_SYSTEM_MINIMIZESTART => {
                Event::Vanished(window)
            }
            EVENT_OBJECT_DESTROY => Event::Destroyed(window),
            EVENT_SYSTEM_FOREGROUND => Event::Focused(window),
            EVENT_SYSTEM_MOVESIZEEND => Event::Moved(window),
            _ => return None,
        })
    }
}

/// Starts posting window events to the calling thread's queue as `MESSAGE`s.
/// The hooks last until the process exits.
pub fn listen() {
    for (first, last) in RANGES {
        unsafe {
            SetWinEventHook(
                first,
                last,
                None,
                Some(forward),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
        }
    }
}

/// Windows calls this on the hooking thread while it waits in `GetMessage`, and
/// posting is what makes that call return.
unsafe extern "system" fn forward(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    let is_window_itself = object == OBJID_WINDOW.0 && child == CHILDID_SELF as i32;
    if is_window_itself && !hwnd.is_invalid() {
        // A full queue drops the event; there's nothing better to do with it.
        let _ = unsafe {
            PostThreadMessageW(
                GetCurrentThreadId(),
                MESSAGE,
                WPARAM(event as usize),
                LPARAM(hwnd.0 as isize),
            )
        };
    }
}

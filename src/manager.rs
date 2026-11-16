use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_SHIFT, RegisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, MSG, PostThreadMessageW, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SystemParametersInfoW, WM_HOTKEY, WM_QUIT,
};
use windows::core::{BOOL, Result};

use crate::cloak::{self, Cloaker};
use crate::events::{self, Event};
use crate::layout::{Direction, Placement, Rect, Workspaces};
use crate::window::{Snapshot, Window};

/// Space between columns and around the edge of the screen, in pixels.
const GAP: i32 = 8;

#[derive(Clone, Copy)]
enum Action {
    Focus(Direction),
    Move(Direction),
    CycleWidth,
    Close,
    Quit,
}

const ALT_SHIFT: HOT_KEY_MODIFIERS = HOT_KEY_MODIFIERS(MOD_ALT.0 | MOD_SHIFT.0);

/// Hotkeys as modifiers, key and action. A binding's index is its hotkey id.
const BINDINGS: [(HOT_KEY_MODIFIERS, char, Action); 11] = [
    (MOD_ALT, 'H', Action::Focus(Direction::Left)),
    (MOD_ALT, 'L', Action::Focus(Direction::Right)),
    (MOD_ALT, 'K', Action::Focus(Direction::Up)),
    (MOD_ALT, 'J', Action::Focus(Direction::Down)),
    (ALT_SHIFT, 'H', Action::Move(Direction::Left)),
    (ALT_SHIFT, 'L', Action::Move(Direction::Right)),
    (ALT_SHIFT, 'K', Action::Move(Direction::Up)),
    (ALT_SHIFT, 'J', Action::Move(Direction::Down)),
    (MOD_ALT, 'R', Action::CycleWidth),
    (ALT_SHIFT, 'W', Action::Close),
    (ALT_SHIFT, 'Q', Action::Quit),
];

/// The thread running the message loop, so Ctrl+C can ask it to stop.
static LOOP_THREAD: AtomicU32 = AtomicU32::new(0);

/// Tiles the desktop until the quit hotkey or Ctrl+C, then puts every window
/// back where it was.
pub fn run() -> Result<()> {
    LOOP_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    unsafe { SetConsoleCtrlHandler(Some(stop_loop), true)? };

    for (id, (modifiers, key, _)) in BINDINGS.into_iter().enumerate() {
        unsafe { RegisterHotKey(None, id as i32, modifiers, key as u32)? };
    }
    events::listen();

    let mut manager = Manager::new()?;
    println!(
        "Tiling. Alt+H/J/K/L focus, Alt+Shift+H/J/K/L move, Alt+R width, Alt+Shift+W close,          Alt+Shift+Q quit and restore."
    );

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        match message.message {
            WM_HOTKEY => {
                let binding = BINDINGS.get(message.wParam.0);
                if binding.is_some_and(|(_, _, action)| manager.perform(*action).is_break()) {
                    break;
                }
            }
            events::MESSAGE => {
                if let Some(event) = Event::from_message(&message) {
                    manager.handle(event);
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Runs on a thread Windows creates for Ctrl+C or a closing console.
unsafe extern "system" fn stop_loop(_control: u32) -> BOOL {
    let thread = LOOP_THREAD.load(Ordering::Relaxed);
    let _ = unsafe { PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
    // Returning lets Windows kill the process, so give the loop time to restore
    // the windows and exit by itself first.
    std::thread::sleep(Duration::from_secs(1));
    false.into()
}

struct Manager {
    layout: Workspaces<Window>,
    cloaker: Cloaker,
    /// Where each window was last sent, so unchanged ones aren't moved again.
    placed: Vec<(Window, Rect)>,
    /// Where each window was before we first moved it, kept until it's destroyed.
    originals: Vec<(Window, Snapshot)>,
}

impl Manager {
    /// Adopts the windows already open, left to right as they sit on screen.
    fn new() -> Result<Self> {
        let mut manager = Manager {
            layout: Workspaces::new(work_area()?, GAP),
            cloaker: Cloaker::new()?,
            placed: Vec::new(),
            originals: Vec::new(),
        };
        // A run that was killed leaves its off-screen windows cloaked, and
        // cloaked windows aren't adopted.
        manager.cloaker.uncloak_all();

        let mut open: Vec<_> = Window::visible()?
            .into_iter()
            .map(|window| (window, window.info()))
            .filter(|(window, info)| window.skip(info).is_none())
            .collect();
        open.sort_by_key(|(_, info)| info.bounds.x);
        open.into_iter()
            .for_each(|(window, _)| manager.adopt(window));

        if let Some(window) = Window::foreground() {
            manager.layout.focus(window);
        }
        manager.apply();
        Ok(manager)
    }

    /// Carries out a hotkey. Breaks when the manager should exit.
    fn perform(&mut self, action: Action) -> ControlFlow<()> {
        let changed = match action {
            Action::Focus(direction) => self.layout.focus_toward(direction),
            Action::Move(direction) => self.layout.move_toward(direction),
            Action::CycleWidth => self.layout.cycle_width(),
            Action::Close => {
                // Only a tiled window: the desktop can have focus too, and
                // closing that asks to shut Windows down.
                let tiled = Window::foreground().filter(|window| self.layout.contains(*window));
                if let Some(window) = tiled {
                    window.close();
                }
                false
            }
            Action::Quit => return ControlFlow::Break(()),
        };

        if changed {
            self.apply();
            self.focus_layout();
        }
        ControlFlow::Continue(())
    }

    fn handle(&mut self, event: Event) {
        let focused = self.layout.focused();

        let changed = match event {
            Event::Appeared(window) => {
                let new = !self.layout.contains(window) && window.is_tileable();
                if new {
                    self.adopt(window);
                }
                new
            }
            Event::Vanished(window) => self.release(window),
            // Our own cloaking of an off-screen column lands here too.
            Event::Cloaked(window) => !cloak::is_cloaked(window) && self.release(window),
            Event::Destroyed(window) => {
                self.originals.retain(|(known, _)| *known != window);
                self.layout.remove(window)
            }
            // Windows reports each window it passes through on the way to the
            // one that ends up focused; only that last one matters.
            Event::Focused(window) => {
                Window::foreground() == Some(window) && self.layout.focus(window)
            }
            Event::Moved(window) => {
                // Forget where it was sent so it snaps back into its column.
                self.placed.retain(|(placed, _)| *placed != window);
                self.layout.contains(window)
            }
        };

        if changed {
            self.apply();
            // When a window goes away Windows activates one of its own
            // choosing, which may be off screen. Ours takes over.
            if !matches!(event, Event::Focused(_)) && self.layout.focused() != focused {
                self.focus_layout();
            }
        }
    }

    /// Gives keyboard focus to the layout's focused window, or to the desktop
    /// on an empty workspace so that no hidden window keeps it.
    fn focus_layout(&self) {
        self.layout
            .focused()
            .unwrap_or_else(Window::desktop)
            .focus();
    }

    fn adopt(&mut self, window: Window) {
        let known = self.originals.iter().any(|(known, _)| *known == window);
        if let (false, Some(snapshot)) = (known, window.snapshot()) {
            self.originals.push((window, snapshot));
        }
        self.layout.insert(window);
    }

    /// Drops a live window from the layout, uncloaking it so it isn't left
    /// invisible. Returns `false` if it wasn't in the layout.
    fn release(&mut self, window: Window) -> bool {
        let removed = self.layout.remove(window);
        if removed && cloak::is_cloaked(window) {
            self.cloaker.set(window, false);
        }
        removed
    }

    /// Moves the windows whose rectangle changed since the last call, and
    /// cloaks the ones that are off screen or on another workspace.
    fn apply(&mut self) {
        let placed = std::mem::take(&mut self.placed);

        for placement in self.layout.placements() {
            let Placement {
                window,
                rect,
                visible,
            } = placement;
            if visible == cloak::is_cloaked(window) {
                self.cloaker.set(window, !visible);
            }
            if placed.contains(&(window, rect)) || window.place(rect) {
                self.placed.push((window, rect));
            }
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.cloaker.uncloak_all();
        for (window, snapshot) in &self.originals {
            window.restore(*snapshot);
        }
    }
}

/// The primary monitor's area, minus the taskbar.
fn work_area() -> Result<Rect> {
    let mut area = RECT::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&raw mut area).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
    }
    Ok(area.into())
}

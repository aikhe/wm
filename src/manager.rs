use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, RegisterHotKey, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, MSG, PostThreadMessageW, SPI_GETWORKAREA, SW_SHOWNORMAL,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW, WM_HOTKEY, WM_QUIT,
};
use windows::core::{BOOL, Result};

use crate::cloak::{self, Cloaker};
use crate::events::{self, Event};
use crate::layout::{Direction, Mode, Placement, Rect, Width, Workspaces};
use crate::rules::{Rule, Rules};
use crate::window::{Snapshot, Window};

/// Space between columns and around the viewport, in pixels.
const GAP: i32 = 8;
const RESIZE_STEP: i32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Focus(Direction),
    Move(Direction),
    Workspace(usize),
    Send(usize),
    CycleWidth,
    Resize(i32),
    ToggleFull,
    Toggle(Mode),
    Close,
    Quit,
}

const ALT_SHIFT: HOT_KEY_MODIFIERS = HOT_KEY_MODIFIERS(MOD_ALT.0 | MOD_SHIFT.0);
const CTRL_ALT_SHIFT: HOT_KEY_MODIFIERS = HOT_KEY_MODIFIERS(MOD_CONTROL.0 | ALT_SHIFT.0);

const BINDINGS: [(HOT_KEY_MODIFIERS, VIRTUAL_KEY, Action); 35] = [
    (
        MOD_ALT,
        VIRTUAL_KEY(b'H' as u16),
        Action::Focus(Direction::Left),
    ),
    (
        MOD_ALT,
        VIRTUAL_KEY(b'L' as u16),
        Action::Focus(Direction::Right),
    ),
    (
        MOD_ALT,
        VIRTUAL_KEY(b'K' as u16),
        Action::Focus(Direction::Up),
    ),
    (
        MOD_ALT,
        VIRTUAL_KEY(b'J' as u16),
        Action::Focus(Direction::Down),
    ),
    (
        ALT_SHIFT,
        VIRTUAL_KEY(b'H' as u16),
        Action::Move(Direction::Left),
    ),
    (
        ALT_SHIFT,
        VIRTUAL_KEY(b'L' as u16),
        Action::Move(Direction::Right),
    ),
    (
        ALT_SHIFT,
        VIRTUAL_KEY(b'K' as u16),
        Action::Move(Direction::Up),
    ),
    (
        ALT_SHIFT,
        VIRTUAL_KEY(b'J' as u16),
        Action::Move(Direction::Down),
    ),
    (MOD_ALT, VIRTUAL_KEY(b'1' as u16), Action::Workspace(0)),
    (MOD_ALT, VIRTUAL_KEY(b'2' as u16), Action::Workspace(1)),
    (MOD_ALT, VIRTUAL_KEY(b'3' as u16), Action::Workspace(2)),
    (MOD_ALT, VIRTUAL_KEY(b'4' as u16), Action::Workspace(3)),
    (MOD_ALT, VIRTUAL_KEY(b'5' as u16), Action::Workspace(4)),
    (MOD_ALT, VIRTUAL_KEY(b'6' as u16), Action::Workspace(5)),
    (MOD_ALT, VIRTUAL_KEY(b'7' as u16), Action::Workspace(6)),
    (MOD_ALT, VIRTUAL_KEY(b'8' as u16), Action::Workspace(7)),
    (MOD_ALT, VIRTUAL_KEY(b'9' as u16), Action::Workspace(8)),
    (ALT_SHIFT, VIRTUAL_KEY(b'1' as u16), Action::Send(0)),
    (ALT_SHIFT, VIRTUAL_KEY(b'2' as u16), Action::Send(1)),
    (ALT_SHIFT, VIRTUAL_KEY(b'3' as u16), Action::Send(2)),
    (ALT_SHIFT, VIRTUAL_KEY(b'4' as u16), Action::Send(3)),
    (ALT_SHIFT, VIRTUAL_KEY(b'5' as u16), Action::Send(4)),
    (ALT_SHIFT, VIRTUAL_KEY(b'6' as u16), Action::Send(5)),
    (ALT_SHIFT, VIRTUAL_KEY(b'7' as u16), Action::Send(6)),
    (ALT_SHIFT, VIRTUAL_KEY(b'8' as u16), Action::Send(7)),
    (ALT_SHIFT, VIRTUAL_KEY(b'9' as u16), Action::Send(8)),
    (MOD_ALT, VIRTUAL_KEY(b'R' as u16), Action::CycleWidth),
    (MOD_ALT, VIRTUAL_KEY(0xbd), Action::Resize(-RESIZE_STEP)),
    (MOD_ALT, VIRTUAL_KEY(0xbb), Action::Resize(RESIZE_STEP)),
    (MOD_ALT, VIRTUAL_KEY(b'F' as u16), Action::ToggleFull),
    (
        MOD_ALT,
        VIRTUAL_KEY(b'T' as u16),
        Action::Toggle(Mode::Floating),
    ),
    (
        ALT_SHIFT,
        VIRTUAL_KEY(b'T' as u16),
        Action::Toggle(Mode::Sticky),
    ),
    (
        CTRL_ALT_SHIFT,
        VIRTUAL_KEY(b'T' as u16),
        Action::Toggle(Mode::OnTop),
    ),
    (ALT_SHIFT, VIRTUAL_KEY(b'W' as u16), Action::Close),
    (ALT_SHIFT, VIRTUAL_KEY(b'E' as u16), Action::Quit),
];

/// The thread running the message loop, so Ctrl+C can ask it to stop.
static LOOP_THREAD: AtomicU32 = AtomicU32::new(0);

/// Tiles the desktop until the quit hotkey or Ctrl+C, then puts every window
/// back where it was.
pub fn run() -> Result<()> {
    LOOP_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    unsafe { SetConsoleCtrlHandler(Some(stop_loop), true)? };
    for (id, (modifiers, key, _)) in BINDINGS.into_iter().enumerate() {
        unsafe { RegisterHotKey(None, id as i32, modifiers, key.0.into())? };
    }
    events::listen();
    let mut manager = Manager::new()?;
    println!("Tiling. Alt+H/J/K/L focus; add Shift to move. Alt+1..9 workspace; Shift sends.");
    println!(
        "Alt+T float, Alt+Shift+T sticky, Ctrl+Alt+Shift+T on top. Alt+R/-/= width, Alt+F full."
    );
    println!("Alt+Shift+W close, Alt+Shift+E quit and restore.");
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        let action = match message.message {
            WM_HOTKEY => BINDINGS.get(message.wParam.0).map(|(_, _, action)| *action),
            events::MESSAGE => {
                if let Some(event) = Event::from_message(&message) {
                    manager.handle(event);
                }
                None
            }
            _ => None,
        };
        if action.is_some_and(|action| manager.perform(action).is_break()) {
            break;
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
    /// Where each window sits when it isn't tiled: taken before we first move
    /// it and again whenever it stops floating, kept until it's destroyed.
    untiled: Vec<(Window, Snapshot)>,
    /// The mode and width each kind of window was last given, which new ones
    /// open with.
    rules: Rules,
}

impl Manager {
    /// Adopts the windows already open, left to right as they sit on screen.
    fn new() -> Result<Self> {
        let mut manager = Manager {
            layout: Workspaces::new(work_area()?, GAP),
            cloaker: Cloaker::new()?,
            placed: Vec::new(),
            untiled: Vec::new(),
            rules: Rules::load(),
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
        for (window, _) in open {
            if manager.adopt(window) {
                manager.log("adopted");
            }
        }

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
            Action::Workspace(index) => self.layout.focus_workspace(index),
            Action::Send(index) => self.layout.move_to_workspace(index),
            Action::CycleWidth => self.layout.cycle_width(),
            Action::Resize(sign) => self.layout.resize(sign),
            Action::ToggleFull => self.layout.toggle_full(),
            Action::Toggle(mode) => self.toggle(mode),
            Action::Close => {
                // Never close the desktop or an unmanaged window.
                let target = Window::foreground().filter(|window| self.layout.contains(*window));
                if let Some(window) = target {
                    window.close();
                }
                false
            }
            Action::Quit => return ControlFlow::Break(()),
        };

        let name = format!("{action:?}");
        if changed {
            if matches!(
                action,
                Action::CycleWidth | Action::Resize(_) | Action::ToggleFull
            ) {
                self.record_width();
            }
            self.apply();
            self.focus_layout();
            self.log(&name);
        } else if !matches!(action, Action::Close) {
            println!("{name:<17} nothing to do");
        }
        ControlFlow::Continue(())
    }

    fn handle(&mut self, event: Event) {
        let focused = self.layout.focused();
        let name = match event {
            Event::Appeared(_) => "opened",
            Event::Vanished(_) | Event::Cloaked(_) => "hidden",
            Event::Destroyed(_) => "closed",
            Event::Focused(_) => "focused",
            Event::Moved(_) => "moved",
        };

        let changed = match event {
            Event::Appeared(window) => {
                let new = !self.layout.contains(window) && window.is_tileable();
                new && self.adopt(window)
            }
            Event::Vanished(window) => self.release(window),
            // Our own cloaking lands here too, and so late that the window may
            // be back on screen by now. Only a window that someone else has
            // cloaked, and that still is, has left.
            Event::Cloaked(window) => {
                !cloak::is_cloaked(window) && window.info().cloaked && self.release(window)
            }
            Event::Destroyed(window) => {
                self.untiled.retain(|(known, _)| *known != window);
                self.layout.remove(window)
            }
            // Windows reports each window it passes through on the way to the
            // one that ends up focused; only that last one matters.
            Event::Focused(window) => {
                let current = Window::foreground() == Some(window);
                let focused = current && self.layout.focus(window);
                if focused && self.layout.mode(window) == Some(Mode::Tiled) {
                    self.raise_floating();
                }
                focused
            }
            Event::Moved(window) => {
                // Forget where it was sent so it snaps back into its column.
                self.placed.retain(|(placed, _)| *placed != window);
                self.layout.contains(window)
            }
        };

        if changed {
            self.apply();
            let refocused = self.layout.focused() != focused;
            let focus_event = matches!(event, Event::Focused(_));
            // When a window goes away Windows activates one of its own
            // choosing, which may be off screen. Ours takes over.
            if refocused && !focus_event {
                self.focus_layout();
            }
            // A hotkey has already logged the focus changes it caused.
            if refocused || !focus_event {
                self.log(name);
            }
        }
    }

    /// Prints what just happened, then the focused window and how it's kept.
    fn log(&self, event: &str) {
        let (active, count) = self.layout.active();
        let workspace = active + 1;
        let Some(window) = self.layout.focused() else {
            println!("{event:<17} workspace {workspace}/{count}  empty");
            return;
        };
        let mode = match self.layout.mode(window) {
            Some(Mode::Floating) => "floating",
            Some(Mode::Sticky) => "sticky",
            Some(Mode::OnTop) => "on top",
            Some(Mode::Tiled) | None => "tiled",
        };
        let width = match self.layout.width(window) {
            Some(Width { full: true, .. }) => " full".to_owned(),
            Some(Width { pixels, .. }) => format!(" {pixels}px"),
            None => String::new(),
        };
        let state = format!("{mode}{width}");
        let exe = window.exe().unwrap_or_else(|| "?".into());
        let title = window.title();
        println!("{event:<17} workspace {workspace}/{count}  {state:<13} {exe}  {title:.48}");
    }

    /// Makes the focused column's width the one new windows of its kind open at.
    fn record_width(&mut self) {
        let Some(window) = self.layout.focused() else {
            return;
        };
        let width = self.layout.width(window);
        let rule = self.rules.get(window);
        self.rules.set(window, Rule { width, ..rule });
    }

    /// Gives keyboard focus to the layout's focused window, or to the desktop
    /// on an empty workspace so that no hidden window keeps it.
    fn focus_layout(&self) {
        self.focus_target().focus();
    }

    /// The window that should have keyboard focus.
    fn focus_target(&self) -> Window {
        self.layout.focused().unwrap_or_else(Window::desktop)
    }

    /// Puts the floating windows on screen back above the tiled ones. A tiled
    /// window comes to the top when it takes focus, which would bury them.
    fn raise_floating(&self) {
        self.layout
            .placements()
            .filter(|placement| placement.rect.is_none() && placement.visible)
            .for_each(|placement| placement.window.raise());
    }

    /// Presses a mode key on the focused window, and makes that the mode new
    /// windows of its kind open in. A window that stops being tiled goes back
    /// to where it sat before, and one that gets tiled has its spot
    /// remembered for the next time and takes the width its column last had.
    /// Returns `false` when nothing has focus.
    fn toggle(&mut self, target: Mode) -> bool {
        let Some(window) = self.layout.focused() else {
            return false;
        };
        let Some(before) = self.layout.mode(window) else {
            return false;
        };
        let after = before.toggled(target);
        // Its own width while it's tiled, or else the one its kind last had.
        let width = self.layout.width(window);
        let width = width.or(self.rules.get(window).width);
        self.layout.set_mode(window, after, width);

        let spot = self.untiled.iter().find(|(known, _)| *known == window);
        if after == Mode::Tiled {
            self.remember(window);
        } else if let (Mode::Tiled, Some((_, snapshot))) = (before, spot) {
            // Never maximized, which would cover everything it floats above.
            window.restore(Snapshot {
                showCmd: SW_SHOWNORMAL.0 as u32,
                ..*snapshot
            });
        }
        if (before == Mode::OnTop) != (after == Mode::OnTop) {
            window.set_topmost(after == Mode::OnTop);
        }
        self.rules.set(window, Rule { mode: after, width });
        true
    }

    /// Takes the window into the layout, in the mode and at the width its
    /// kind was last given.
    fn adopt(&mut self, window: Window) -> bool {
        let Rule { mode, width } = self.rules.get(window);
        if !self.untiled.iter().any(|(known, _)| *known == window) {
            self.remember(window);
        }
        self.layout.insert(window, mode, width);
        if mode == Mode::OnTop {
            window.set_topmost(true);
        }
        true
    }

    /// Records where the window sits now as its place when it isn't tiled.
    fn remember(&mut self, window: Window) {
        if let Some(snapshot) = window.snapshot() {
            self.untiled.retain(|(known, _)| *known != window);
            self.untiled.push((window, snapshot));
        }
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

    /// Moves the tiled windows whose rectangle changed since the last call,
    /// and cloaks every window that is off screen or on another workspace.
    fn apply(&mut self) {
        let placed = std::mem::take(&mut self.placed);

        for placement in self.layout.placements() {
            let Placement {
                window,
                rect,
                visible,
                ..
            } = placement;
            if visible == cloak::is_cloaked(window) {
                self.cloaker.set(window, !visible);
            }
            let Some(rect) = rect else {
                continue;
            };
            if placed.contains(&(window, rect)) || window.place(rect) {
                self.placed.push((window, rect));
            }
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.rules.save();
        self.cloaker.uncloak_all();
        for (window, snapshot) in &self.untiled {
            match self.layout.mode(*window) {
                // Floating windows are already where they were left.
                Some(Mode::Floating | Mode::Sticky) => {}
                // Nothing could take it back off the top once we're gone.
                Some(Mode::OnTop) => window.set_topmost(false),
                Some(Mode::Tiled) | None => window.restore(*snapshot),
            }
        }
    }
}

/// The primary monitor's area, minus the taskbar.
pub fn work_area() -> Result<Rect> {
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

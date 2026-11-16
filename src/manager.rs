use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SPI_GETWORKAREA,
    SW_SHOWNORMAL, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW, WM_HOTKEY,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_PAINT, WM_QUIT,
};
use windows::core::{BOOL, Result};

use crate::bindings::{Action, Binding, Hotkeys};
use crate::cloak::{self, Cloaker};
use crate::config::{self, Config};
use crate::config_watch::{self, Watcher};
use crate::events::{self, Event};
use crate::layout::{Direction, Mode, Placement, Rect, Width, Workspaces};
use crate::overview::{Overview, Target};
use crate::rules::{Rule, Rules};
use crate::window::{Snapshot, Window};

/// The thread running the message loop, so Ctrl+C can ask it to stop.
static LOOP_THREAD: AtomicU32 = AtomicU32::new(0);

/// Tiles the desktop until the quit hotkey or Ctrl+C, then puts every window
/// back where it was.
pub fn run(path: PathBuf) -> std::result::Result<(), Box<dyn std::error::Error>> {
    config::init(&path)?;
    let initial = config::read(&path)?;
    let config = Config::parse(&initial).map_err(|error| format!("{}: {error}", path.display()))?;
    let viewport = config.viewport(work_area()?)?;
    let thread = unsafe { GetCurrentThreadId() };
    LOOP_THREAD.store(thread, Ordering::Relaxed);
    unsafe { SetConsoleCtrlHandler(Some(stop_loop), true)? };
    // Make the queue before the file watcher can post its first update.
    let mut queued = MSG::default();
    let _ = unsafe { PeekMessageW(&mut queued, None, 0, 0, PM_NOREMOVE) };
    let mut hotkeys = Hotkeys::default();
    hotkeys.replace(config.bindings.clone())?;
    let watcher = Watcher::new(path.clone(), initial, Some(thread))?;
    events::listen();
    println!("Config: {} (reloads on save)", path.display());
    println!(
        "Tiling. Hotkeys and all defaults are documented in that file. Ctrl+C quits and restores."
    );
    let mut manager = Manager::new(config, viewport)?;

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        if message.message == config_watch::MESSAGE {
            if let Some(update) = watcher.take() {
                manager.reload(update, &mut hotkeys);
            }
            continue;
        }
        let in_overview = manager.overview.owns(&message);
        let action = match message.message {
            WM_HOTKEY => hotkeys.action(message.wParam.0),
            WM_KEYDOWN if in_overview => {
                let held = [(0x10, 4), (0x11, 2), (0x12, 1), (0x5b, 8), (0x5c, 8)]
                    .into_iter()
                    .filter(|(key, _)| unsafe { GetKeyState(*key) } < 0)
                    .fold(0, |all, (_, modifier)| all | modifier);
                overview_action(
                    message.wParam.0 as u16,
                    held,
                    &hotkeys.bindings,
                    &manager.config.overview,
                )
            }
            WM_LBUTTONDOWN if in_overview => {
                // The low and high words of the parameter, each signed.
                let point = message.lParam.0;
                manager.click(i32::from(point as i16), i32::from((point >> 16) as i16));
                None
            }
            WM_PAINT if in_overview => {
                manager.overview.paint();
                None
            }
            events::MESSAGE => {
                if let Some(event) = Event::from_message(&message) {
                    manager.handle(event);
                }
                None
            }
            _ => None,
        };
        if action == Some(Action::Reload) {
            watcher.reload();
            continue;
        }
        if action.is_some_and(|action| manager.perform(action).is_break()) {
            break;
        }
    }

    Ok(())
}

/// The action for a key pressed in the overview. Every hotkey works there
/// without Alt, the arrow keys stand in for H/J/K/L, and Enter or Escape
/// closes it. Quitting still takes the whole hotkey.
fn overview_action(
    key: u16,
    held: u32,
    bindings: &[Binding],
    options: &config::Overview,
) -> Option<Action> {
    if options.enter_escape_close && held == 0 && matches!(key, 13 | 27) {
        return Some(Action::Overview);
    }
    if options.arrow_keys && matches!(held, 0 | 4) {
        let direction = match key {
            0x25 => Some(Direction::Left),
            0x26 => Some(Direction::Up),
            0x27 => Some(Direction::Right),
            0x28 => Some(Direction::Down),
            _ => None,
        };
        if let Some(direction) = direction {
            return Some(if held == 4 {
                Action::Move(direction)
            } else {
                Action::Focus(direction)
            });
        }
    }
    if !options.bare_hotkeys {
        return None;
    }
    bindings
        .iter()
        .find(|binding| {
            binding.chord.key == key
                && binding.chord.modifiers & !1 == held
                && binding.action != Action::Quit
        })
        .map(|binding| binding.action)
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
    config: Config,
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
    overview: Overview,
}

impl Manager {
    /// Adopts the windows already open, left to right as they sit on screen.
    fn new(config: Config, viewport: Rect) -> Result<Self> {
        let mut layout = Workspaces::new(viewport, config.layout.gap);
        layout.configure(viewport, config.layout.gap, config.options.clone());
        let mut manager = Manager {
            layout,
            cloaker: Cloaker::new()?,
            placed: Vec::new(),
            untiled: Vec::new(),
            rules: Rules::load(),
            overview: Overview::new(config.style.clone())?,
            config,
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

    fn reload(&mut self, update: std::result::Result<Config, String>, hotkeys: &mut Hotkeys) {
        let prepared = update.and_then(|config| {
            let area = work_area().map_err(|error| error.to_string())?;
            let viewport = config.viewport(area)?;
            hotkeys.replace(config.bindings.clone())?;
            Ok((config, viewport))
        });
        match prepared {
            Ok((config, viewport)) => {
                self.layout
                    .configure(viewport, config.layout.gap, config.options.clone());
                self.overview.configure(config.style.clone());
                self.config = config;
                self.apply();
                self.raise_floating();
                println!("Config reloaded. App rules/default widths apply when windows next join.");
            }
            Err(error) => eprintln!("Config reload rejected; previous settings kept: {error}"),
        }
    }

    /// Carries out a hotkey. Breaks when the manager should exit.
    fn perform(&mut self, action: Action) -> ControlFlow<()> {
        let changed = match action {
            Action::Focus(direction) => self.layout.focus_toward(direction),
            Action::Move(direction) => self.layout.move_toward(direction),
            Action::Workspace(index) => self.layout.focus_workspace(index),
            Action::Send(index) => self.layout.move_to_workspace(index),
            Action::CycleWidth => self.layout.cycle_width(),
            Action::Resize(sign) => self
                .layout
                .resize(sign * self.config.layout.resize_step_percent),
            Action::ToggleFull => self.layout.toggle_full(),
            Action::Toggle(mode) => self.toggle(mode),
            Action::Overview => {
                if self.overview.is_open() {
                    self.close_overview();
                } else {
                    self.overview.open();
                }
                true
            }
            Action::Close => {
                // Only a window of ours: the desktop can have focus too, and
                // closing that asks to shut Windows down. While the overview
                // has the keyboard, it's the window highlighted there.
                let ours = Window::foreground().filter(|window| self.layout.contains(*window));
                let target = match self.overview.is_open() {
                    true => self.layout.focused(),
                    false => ours,
                };
                if let Some(window) = target {
                    window.close();
                }
                false
            }
            Action::Quit => return ControlFlow::Break(()),
            Action::Reload => return ControlFlow::Continue(()),
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
        } else if self.config.behavior.log_events && !matches!(action, Action::Close) {
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
                // Another window taking the keyboard ends the overview.
                if current && self.overview.is_open() {
                    self.overview.close();
                }
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
        if !self.config.behavior.log_events {
            return;
        }
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
        if !self.config.behavior.remember_widths {
            return;
        }
        let Some(window) = self.layout.focused() else {
            return;
        };
        let width = self.layout.width(window);
        let rule = self.rules.get(window);
        self.rules.set(window, Rule { width, ..rule });
    }

    /// Gives keyboard focus to the layout's focused window, or to the desktop
    /// on an empty workspace so that no hidden window keeps it. The overview
    /// keeps the keyboard for as long as it's open.
    fn focus_layout(&self) {
        if !self.overview.is_open() {
            self.focus_target().focus();
        }
    }

    /// The window the keyboard belongs to when the overview doesn't have it.
    fn focus_target(&self) -> Window {
        self.layout.focused().unwrap_or_else(Window::desktop)
    }

    /// Hands the keyboard back, then hides the overview. In that order:
    /// hiding the window that has the keyboard leaves Windows to choose the
    /// next one.
    fn close_overview(&mut self) {
        self.focus_target().focus();
        self.overview.close();
    }

    /// Goes to the window or workspace clicked in the overview and closes
    /// it. A click on nothing only closes it.
    fn click(&mut self, x: i32, y: i32) {
        match self.overview.target(x, y) {
            Some(Target::Window(window)) => self.layout.focus(window),
            Some(Target::Workspace(index)) => self.layout.focus_workspace(index),
            None => false,
        };
        // The windows are put in place before the one clicked takes focus.
        self.apply();
        self.close_overview();
        self.log("Click");
    }

    /// Puts the floating windows on screen back above the tiled ones. A tiled
    /// window comes to the top when it takes focus, which would bury them.
    fn raise_floating(&self) {
        if !self.config.behavior.floating_above_tiled {
            return;
        }
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
        let width = width.or_else(|| {
            self.config
                .behavior
                .remember_widths
                .then(|| self.rules.get(window).width)
                .flatten()
        });
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
        let previous = self.rules.get(window);
        self.rules.set(
            window,
            Rule {
                mode: if self.config.behavior.remember_modes {
                    after
                } else {
                    previous.mode
                },
                width: if self.config.behavior.remember_widths {
                    width
                } else {
                    previous.width
                },
            },
        );
        true
    }

    /// Takes the window into the layout, in the mode and at the width its
    /// kind was last given.
    fn adopt(&mut self, window: Window) -> bool {
        let Some(Rule { mode, width }) = self.config.resolve_rule(
            &window.exe().unwrap_or_default(),
            &window.class(),
            &window.title(),
            self.rules.get(window),
            self.layout.default_width(),
        ) else {
            return false;
        };
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
        self.overview.show(&self.layout);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_uses_rebound_keys_and_keeps_navigation_and_exit_available() {
        let config = Config::parse("[hotkeys]\nfocus_left='Alt+U'\noverview='Alt+P'").unwrap();
        let action = |key, held| overview_action(key, held, &config.bindings, &config.overview);
        assert_eq!(
            action(u16::from(b'U'), 0),
            Some(Action::Focus(Direction::Left))
        );
        assert_eq!(action(u16::from(b'H'), 0), None);
        assert_eq!(action(u16::from(b'P'), 0), Some(Action::Overview));
        assert_eq!(action(0x25, 4), Some(Action::Move(Direction::Left)));
        assert_eq!(action(13, 0), Some(Action::Overview));
        assert_eq!(action(27, 0), Some(Action::Overview));
        assert_eq!(
            action(u16::from(b'E'), 4),
            None,
            "quit needs the full shortcut"
        );
    }
}

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetSystemMetrics, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW,
    PostThreadMessageW, SM_CYVIRTUALSCREEN, SPI_GETWORKAREA, SW_SHOWNORMAL,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW, WM_HOTKEY, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_PAINT, WM_QUIT,
};
use windows::core::{BOOL, Error, Result};
use wm_bar_protocol::{Command as BarCommand, Host as BarHost, Snapshot as BarSnapshot};

use crate::bindings::{Action, Binding, Hotkeys};
use crate::cloak::{self, Cloaker};
use crate::config::{self, Config};
use crate::config_watch::{self, Watcher};
use crate::events::{self, Event};
use crate::frame_clock::FrameClock;
use crate::layout::{Direction, Mode, Placement, Rect, Width, Workspaces};
use crate::overview::{Overview, Target};
use crate::rules::{Rule, Rules};
use crate::stage::{self, SceneWindow, Stage};
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
    manager.bar.announce();

    let mut message = MSG::default();
    let mut clock = FrameClock::default();
    'run: loop {
        if !manager.animating() {
            clock.rest();
            match unsafe { GetMessageW(&mut message, None, 0, 0) }.0 {
                -1 => return Err(Error::from_thread().into()),
                0 => break,
                _ => {}
            }
            if manager
                .dispatch(&message, &mut hotkeys, &watcher)
                .is_break()
            {
                break;
            }
            continue;
        }
        // While something moves, one frame is made for each one the
        // compositor presents. Input is taken between frames and bends the
        // motion from where it is, so nothing queues behind an animation.
        let seconds = clock.wait();
        manager.stage.composed();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            let quit = message.message == WM_QUIT;
            if quit
                || manager
                    .dispatch(&message, &mut hotkeys, &watcher)
                    .is_break()
            {
                break 'run;
            }
        }
        manager.tick(seconds);
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
    bar: BarHost,
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
    /// Shows the windows' pictures while they move and in the overview.
    /// Before `overview`, so that its window goes before the one it's above.
    stage: Stage,
    overview: Overview,
    /// The keyboard is to go to the layout's focused window as soon as the
    /// pictures allow it.
    deferred: bool,
}

impl Manager {
    /// Acts on one message from the queue. Breaks when the manager should exit.
    fn dispatch(
        &mut self,
        message: &MSG,
        hotkeys: &mut Hotkeys,
        watcher: &Watcher,
    ) -> ControlFlow<()> {
        if message.hwnd == self.bar.window {
            return match self.bar.handle(message) {
                Some(BarCommand::Workspace(index)) => {
                    self.perform(Action::Workspace(index as usize))
                }
                Some(BarCommand::Overview) => self.perform(Action::Overview),
                Some(BarCommand::Agent(_)) => ControlFlow::Continue(()),
                None => ControlFlow::Continue(()),
            };
        }
        if self.stage.owns(message) {
            // They take no input, and a paint request left unanswered would
            // be asked again forever.
            unsafe { DispatchMessageW(message) };
            return ControlFlow::Continue(());
        }
        if message.message == config_watch::MESSAGE {
            if let Some(update) = watcher.take() {
                self.reload(update, hotkeys);
            }
            return ControlFlow::Continue(());
        }
        let in_overview = self.overview.owns(message);
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
                    &self.config.overview,
                )
            }
            WM_LBUTTONDOWN if in_overview => {
                // The low and high words of the parameter, each signed.
                let point = message.lParam.0;
                self.click(i32::from(point as i16), i32::from((point >> 16) as i16));
                None
            }
            WM_PAINT if in_overview => {
                self.overview.paint();
                None
            }
            events::MESSAGE => {
                if let Some(event) = Event::from_message(message) {
                    self.handle(event);
                }
                None
            }
            _ => None,
        };
        match action {
            Some(Action::Reload) => {
                watcher.reload();
                ControlFlow::Continue(())
            }
            Some(action) => self.perform(action),
            None => ControlFlow::Continue(()),
        }
    }

    /// Adopts the windows already open, left to right as they sit on screen.
    fn new(config: Config, viewport: Rect) -> Result<Self> {
        let mut layout = Workspaces::new(viewport, config.layout.gap);
        layout.configure(viewport, config.layout.gap, config.options.clone());
        let overview = Overview::new(config.style.clone(), config.animations.clone())?;
        let mut manager = Manager {
            bar: BarHost::new()?,
            layout,
            cloaker: Cloaker::new()?,
            placed: Vec::new(),
            untiled: Vec::new(),
            rules: Rules::load(),
            stage: Stage::new(Some(overview.window().0))?,
            overview,
            deferred: false,
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
                self.stage.finish();
                self.layout
                    .configure(viewport, config.layout.gap, config.options.clone());
                self.overview
                    .configure(config.style.clone(), config.animations.clone());
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
        let navigation = matches!(
            action,
            Action::Focus(_) | Action::Move(_) | Action::Workspace(_) | Action::Send(_)
        );
        let active = self.layout.active().0;
        let before = (navigation && !self.overview.is_open() && self.config.animations.enabled)
            .then(|| self.scene(true));
        // A window that changes size or mode does so at once, at rest. In the
        // overview its card just moves.
        let reshapes = matches!(
            action,
            Action::CycleWidth | Action::Resize(_) | Action::ToggleFull | Action::Toggle(_)
        );
        if reshapes && !self.overview.is_open() {
            self.settle();
        }
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
            if let Some(before) = before {
                let vertical = match action {
                    Action::Focus(direction @ (Direction::Up | Direction::Down))
                    | Action::Move(direction @ (Direction::Up | Direction::Down)) => {
                        Some(direction)
                    }
                    Action::Workspace(index) | Action::Send(index) if index != active => {
                        Some(if index < active {
                            Direction::Up
                        } else {
                            Direction::Down
                        })
                    }
                    _ => None,
                };
                self.slide(before, vertical);
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
        let active = self.layout.active().0;
        let in_overview = self.overview.is_open();
        let before =
            (matches!(event, Event::Focused(_)) && !in_overview && self.config.animations.enabled)
                .then(|| self.scene(true));
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
                // Another window taking the keyboard ends the overview, which
                // zooms back out to the desktop as that window has it.
                let closes = current && in_overview;
                if closes {
                    self.overview.close(&self.layout, &mut self.stage);
                }
                if focused && self.layout.mode(window) == Some(Mode::Tiled) {
                    self.raise_floating();
                }
                focused || closes
            }
            Event::Moved(window) => {
                // Forget where it was sent so it snaps back into its column.
                self.placed.retain(|(placed, _)| *placed != window);
                self.layout.contains(window)
            }
        };

        if changed {
            // A window that opens, closes or is dropped somewhere takes its
            // place at once, so the others stop moving first.
            if !matches!(event, Event::Focused(_)) && !in_overview {
                self.settle();
            }
            // Windows has already shown a window it brought forward from
            // another workspace, so that switch is made at once. Within a
            // workspace the strip scrolls to it.
            if let Some(before) = before {
                if self.layout.active().0 != active {
                    self.settle();
                } else if self.layout.focused() != focused {
                    self.slide(before, None);
                }
            }
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
    ///
    /// Windows shows a cloaked window that takes the keyboard, so the focused
    /// window must be out of sight first. While pictures are going up it
    /// waits the one frame until `apply` has sent that window off stage, and
    /// a floating one, which is never moved, waits until its picture goes.
    fn focus_layout(&mut self) {
        if self.overview.is_open() {
            return;
        }
        let target = self.focus_target();
        let floating = self
            .layout
            .mode(target)
            .is_some_and(|mode| mode != Mode::Tiled);
        self.deferred = self.stage.covering() || (floating && self.stage.veils(target));
        if !self.deferred {
            target.focus();
        }
    }

    /// The window that has the keyboard, or is about to, unless that's the
    /// overview or the desktop.
    fn keyboard(&self) -> Option<Window> {
        self.layout.focused().filter(|_| !self.overview.is_open())
    }

    /// The window the keyboard belongs to when the overview doesn't have it.
    fn focus_target(&self) -> Window {
        self.layout.focused().unwrap_or_else(Window::desktop)
    }

    /// Starts the overview's way out and hands the keyboard back. The view
    /// only hides once it has faded, well after: hiding the window that has
    /// the keyboard would leave Windows to choose the next one.
    fn close_overview(&mut self) {
        self.overview.close(&self.layout, &mut self.stage);
        // Sends the window off stage before it takes the keyboard.
        self.apply();
        self.focus_layout();
    }

    /// Goes to the window or workspace clicked in the overview and closes
    /// it. A click on nothing only closes it.
    fn click(&mut self, x: i32, y: i32) {
        // Still on screen only because it's fading out.
        if !self.overview.is_open() {
            self.settle();
            return;
        }
        match self.overview.target(x, y) {
            Some(Target::Window(window)) => self.layout.focus(window),
            Some(Target::Workspace(index)) => self.layout.focus_workspace(index),
            None => false,
        };
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
    /// and cloaks every window that is off screen, on another workspace, or
    /// shown as a picture on the stage for now.
    ///
    /// The window with the keyboard is the exception among those shown as
    /// pictures. Windows uncloaks whichever window takes the keyboard, a
    /// moment after and without a word, and takes the keyboard away from one
    /// left cloaked. So that window waits off stage instead: uncloaked, a
    /// desktop's height below where it belongs.
    fn apply(&mut self) {
        let (active, count) = self.layout.active();
        let focused = self.layout.focused();
        self.bar.publish(BarSnapshot {
            active: active as u32,
            workspaces: count as u32,
            focused: focused.map_or(0, |window| window.0.0 as u64),
            mode: match focused.and_then(|window| self.layout.mode(window)) {
                None => 0,
                Some(Mode::Tiled) => 1,
                Some(Mode::Floating) => 2,
                Some(Mode::Sticky) => 3,
                Some(Mode::OnTop) => 4,
            },
            overview: self.overview.is_open(),
        });
        self.overview.show(&self.layout, &mut self.stage);
        // The real windows stay as they are until the pictures cover them;
        // `tick` comes back here then.
        if self.stage.covering() {
            return;
        }
        let placed = std::mem::take(&mut self.placed);
        let keyboard = self.keyboard();

        for placement in self.layout.placements() {
            let Placement {
                window,
                rect,
                visible,
                ..
            } = placement;
            let veiled = self.stage.veils(window);
            let waiting = veiled && Some(window) == keyboard;
            let marked = cloak::is_cloaked(window);
            // Our mark outlives the cloak when Windows lifts it by itself.
            let hidden = marked && window.cloaked();
            if waiting {
                // Left to Windows to uncloak once it has the keyboard, which
                // gives it time to get off the screen first.
                if marked && !hidden {
                    self.cloaker.set(window, false);
                }
            } else if !visible || veiled {
                if !hidden {
                    self.cloaker.set(window, true);
                }
            } else if marked {
                self.cloaker.set(window, false);
            }
            let Some(rect) = rect else {
                continue;
            };
            let rect = if waiting { off_stage(rect) } else { rect };
            if placed.contains(&(window, rect)) || window.place(rect) {
                self.placed.push((window, rect));
            }
        }
    }

    /// The desktop as the layout has it, bottom-most window first. With
    /// `shown`, the windows on screen are where they really are right now,
    /// which is where their pictures must first appear.
    fn scene(&self, shown: bool) -> Vec<SceneWindow<Window>> {
        let active = self.layout.active().0;
        let mut placements: Vec<_> = self.layout.placements().collect();
        placements.sort_by_key(|item| {
            (
                self.layout.mode(item.window) == Some(Mode::OnTop),
                self.config.behavior.floating_above_tiled && item.rect.is_none(),
            )
        });
        placements
            .into_iter()
            .map(|item| {
                let real = (shown && item.visible).then(|| item.window.bounds());
                SceneWindow {
                    window: item.window,
                    // Not one that's minimized, or still on its way back
                    // from off stage.
                    rect: real
                        .filter(|rect| rect.width > 0 && rect.height > 0)
                        .filter(|rect| !is_off_stage(*rect, self.layout.viewport()))
                        .unwrap_or_else(|| item.window.pose(item.rect)),
                    visible: item.visible,
                    active: item.workspace == active,
                }
            })
            .collect()
    }

    /// Slides the windows' pictures from the arrangement `before` to the
    /// layout's, across workspaces with `vertical`. Call it before `apply`.
    fn slide(&mut self, before: Vec<SceneWindow<Window>>, vertical: Option<Direction>) {
        let settings = &self.config.animations;
        let effect = if vertical.is_some() {
            settings.workspaces
        } else {
            settings.horizontal
        };
        let Some(rate) = settings.rate(effect) else {
            self.settle();
            return;
        };
        let viewport = self.layout.viewport();
        let slides = stage::slides(&before, &self.scene(false), vertical, viewport);
        // Focus can change without anything having to move.
        let still = slides.iter().all(|slide| slide.from == slide.to);
        if still && !self.stage.is_presenting() {
            return;
        }
        self.stage.present(&slides, viewport, Some(rate), false);
    }

    /// Ends whatever is moving, with every window where it belongs.
    fn settle(&mut self) {
        self.stage.finish();
        self.overview.finish();
        self.apply();
        if self.deferred {
            self.focus_layout();
        }
    }

    fn animating(&self) -> bool {
        self.stage.animating() || self.overview.animating()
    }

    /// Makes the next frame, `seconds` on from the last.
    fn tick(&mut self, seconds: f64) {
        self.overview.tick(seconds);
        if self.stage.take_cover() {
            self.apply();
            if self.deferred {
                self.focus_layout();
            }
        }
        // The window with the keyboard comes back from off stage in its
        // app's own time, and the pictures stay up until it has.
        let layout = &self.layout;
        let landed = || {
            layout
                .focused()
                .is_none_or(|window| !is_off_stage(window.bounds(), layout.viewport()))
        };
        if self.stage.step(seconds, landed) {
            self.apply();
            // If Windows refused the focused window the keyboard while it
            // was off stage and left it with another of ours, ask again.
            let stray = Window::foreground().is_some_and(|window| {
                self.layout.contains(window) && window != self.focus_target()
            });
            if stray || self.deferred {
                self.focus_layout();
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

/// Where a window that belongs at `rect` waits while its picture stands in
/// for it and it can't be cloaked: straight down, past the bottom of every
/// screen.
fn off_stage(rect: Rect) -> Rect {
    Rect {
        y: rect.y + unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) },
        ..rect
    }
}

/// Whether a window whose frame is `bounds` is still waiting off stage.
fn is_off_stage(bounds: Rect, viewport: Rect) -> bool {
    bounds.y >= viewport.y + viewport.height
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

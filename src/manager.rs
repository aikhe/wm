use std::cmp::Reverse;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW,
    SPI_GETWORKAREA, SW_SHOWNORMAL, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    WM_HOTKEY, WM_KEYDOWN, WM_LBUTTONDOWN, WM_PAINT, WM_QUIT,
};
use windows::core::{BOOL, Error, Result};
use wm_bar_protocol::{Command as BarCommand, DiagKind, Snapshot as BarSnapshot};

use crate::animation::Slide;
use crate::bindings::{self, Action, Binding, Hotkeys};
use crate::config::{self, Config, Joining, RuleMode};
use crate::config_watch::{self, Watcher};
use crate::events::{self, Event};
use crate::frame_clock::FrameClock;
use crate::gestures::{Axis, SwipeEvent};
use crate::layout::{Arranged, Direction, Mode, Placement, Rect, Width, Workspaces};
use crate::overview::Target;
use crate::platform::{Platform, Win32Platform};
use crate::rules::{Rule, Rules};
use crate::session::{self, Entry};
use crate::setups::{self, App, Spot};
use crate::stage::{self, SceneWindow};
use crate::window::{Snapshot, Window};

/// The thread running the message loop, so Ctrl+C can ask it to stop.
static LOOP_THREAD: AtomicU32 = AtomicU32::new(0);

/// Tiles the desktop until the quit hotkey or Ctrl+C, then puts every window
/// back where it was.
pub fn run(path: PathBuf) -> std::result::Result<(), Box<dyn std::error::Error>> {
    config::init(&path)?;
    let initial = config::read_all(&path)?;
    let config = Config::parse_with(&initial.0, &initial.1)
        .map_err(|error| format!("{}: {error}", path.display()))?;
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
    let platform = Win32Platform::new(&config)?;
    println!("Config: {} (reloads on save)", path.display());
    println!(
        "Tiling. Hotkeys and all defaults are documented in that file. Ctrl+C quits and restores."
    );
    let mut manager = Manager::new_with(platform, config, viewport, setups::path(&path));
    manager.platform.bar.announce();

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
        manager.platform.stage.composed();
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

struct Manager<P: Platform> {
    platform: P,
    config: Config,
    layout: Workspaces<Window>,
    /// Where each window was last sent, so unchanged ones aren't moved again.
    placed: Vec<(Window, Rect)>,
    /// Where each window sits when it isn't tiled: taken before we first move
    /// it and again whenever it stops floating, kept until it's destroyed.
    untiled: Vec<(Window, Snapshot)>,
    /// The mode and width each kind of window was last given, which new ones
    /// open with.
    rules: Rules,
    /// The order the overview stacks its pictures in, top first: the desktop's
    /// when it opened, with each window chosen there since brought to the top
    /// as it would be on the desktop.
    overview_order: Vec<Window>,
    /// The window last brought to the top of `overview_order`.
    overview_chosen: Option<Window>,
    /// The keyboard is to go to the layout's focused window as soon as the
    /// pictures allow it.
    deferred: bool,
    /// Where the save key writes setups.
    setups_file: PathBuf,
    loading: Option<Loading>,
    /// Windows placed without focus, and when. Their app may take the
    /// keyboard as they open, and ours takes it back.
    quiet: Vec<(Window, Duration)>,
    /// When paused, all windows are restored and tiling is suspended until resumed.
    paused: bool,
    /// Last session text persisted so we only write when the layout changes.
    last_session: String,
    /// Snapshot captured when a touchpad swipe gesture begins scrubbing.
    swipe_base: Option<Vec<SceneWindow<Window>>>,
}

/// The setup loaded last, whose apps take their places as their windows
/// appear, for a while.
struct Loading {
    apps: Vec<App>,
    /// The window each app has, once one has appeared.
    claimed: Vec<Option<Window>>,
    since: Duration,
}

/// How long a setup's launched apps have to show their windows. Apps are
/// slow just after signing in.
const LAUNCH_WAIT: Duration = Duration::from_secs(120);

/// How long after a window is placed without focus its app taking the
/// keyboard is undone.
const QUIET: Duration = Duration::from_secs(3);

impl Manager<Win32Platform> {
    /// Acts on one message from the queue. Breaks when the manager should exit.
    fn dispatch(
        &mut self,
        message: &MSG,
        hotkeys: &mut Hotkeys,
        watcher: &Watcher,
    ) -> ControlFlow<()> {
        if message.hwnd == self.platform.bar.window {
            return match self.platform.bar.handle(message) {
                Some(BarCommand::Workspace(index)) => {
                    self.perform(Action::Workspace(index as usize), Some(hotkeys))
                }
                Some(BarCommand::Overview) => self.perform(Action::Overview, Some(hotkeys)),
                Some(BarCommand::Agent(index)) => {
                    if let Some((name, _, action)) = bindings::DEFAULTS.get(index) {
                        if !self.config.agent_allow.contains(action) {
                            self.platform
                                .record_diag(DiagKind::AgentRefused, 0, index as u32);
                            self.log_always(&format!("Agent refused {name}"));
                            ControlFlow::Continue(())
                        } else if *action == Action::Reload {
                            watcher.reload();
                            ControlFlow::Continue(())
                        } else {
                            self.perform(*action, Some(hotkeys))
                        }
                    } else {
                        ControlFlow::Continue(())
                    }
                }
                None => ControlFlow::Continue(()),
            };
        }
        if self.platform.stage.owns(message) {
            // They take no input, and a paint request left unanswered would
            // be asked again forever.
            unsafe { DispatchMessageW(message) };
            return ControlFlow::Continue(());
        }
        if self.platform.touchpad.owns(message) {
            let viewport = self.layout.viewport();
            if let Some(event) =
                self.platform
                    .touchpad
                    .handle(message, viewport.width, viewport.height)
            {
                self.handle_swipe(event, Some(hotkeys));
            }
            return ControlFlow::Continue(());
        }
        if message.message == config_watch::MESSAGE {
            if let Some(update) = watcher.take() {
                self.reload(update, Some(hotkeys));
            }
            return ControlFlow::Continue(());
        }
        let in_overview = self.platform.overview.owns(message);
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
                self.platform.overview.paint();
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
            Some(action) => self.perform(action, Some(hotkeys)),
            None => ControlFlow::Continue(()),
        }
    }

    fn animating(&self) -> bool {
        self.platform.stage.animating() || self.platform.overview.animating()
    }

    /// Makes the next frame, `seconds` on from the last.
    fn tick(&mut self, seconds: f64) {
        self.platform.overview.tick(seconds);
        if self.platform.stage.take_cover() {
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
        if self.platform.stage.step(seconds, landed) {
            self.apply();
            // If Windows refused the focused window the keyboard while it
            // was off stage and left it with another of ours, ask again.
            let stray = self.platform.foreground().is_some_and(|window| {
                self.layout.contains(window) && window != self.focus_target()
            });
            if stray || self.deferred {
                self.focus_layout();
            }
        }
    }
}

impl<P: Platform> Manager<P> {
    /// Puts the windows already open back where the last run left them,
    /// recovers any off-stage windows from an interrupted animation, and adopts
    /// the others left to right as they sit on screen.
    fn new_with(platform: P, config: Config, viewport: Rect, setups_file: PathBuf) -> Self {
        let mut layout = Workspaces::new(viewport, config.layout.gap);
        layout.configure(viewport, config.layout.gap, config.options.clone());
        layout.name(&config.workspaces);
        let rules = platform.load_rules();
        let mut manager = Manager {
            platform,
            layout,
            placed: Vec::new(),
            untiled: Vec::new(),
            rules,
            overview_order: Vec::new(),
            overview_chosen: None,
            deferred: false,
            setups_file,
            loading: None,
            quiet: Vec::new(),
            paused: false,
            last_session: String::new(),
            swipe_base: None,
            config,
        };
        // A run that was killed leaves its off-screen windows cloaked, and
        // cloaked windows aren't adopted.
        manager.platform.uncloak_all();
        manager.recover_parked();

        let mut open: Vec<_> = manager
            .platform
            .visible_windows()
            .into_iter()
            .map(|window| (window, manager.platform.info(window)))
            .filter(|(window, info)| manager.platform.skip(*window, info).is_none())
            .collect();
        let restored = manager.restore(open.iter().map(|(window, _)| *window).collect());
        if !restored.is_empty() {
            manager.log(&format!("restored {}", restored.len()));
        }
        open.retain(|(window, _)| !restored.contains(window));
        open.sort_by_key(|(_, info)| info.bounds.x);
        for (window, _) in open {
            if manager.adopt(window) {
                manager.log("adopted");
            }
        }

        if let Some(window) = manager.platform.foreground()
            && !manager.quiet.iter().any(|(quiet, _)| *quiet == window)
            && !manager.layout.is_scratchpad(window)
        {
            manager.layout.focus(window);
        }
        // A restart keeps the layout it restored; the setup is for a fresh start.
        if let Some(index) = manager.config.startup_setup
            && restored.is_empty()
        {
            manager.load_setup(index);
        }
        manager.apply();
        manager
    }

    /// Pulls back any window left parked off-stage beneath the screen by a
    /// crash mid-animation.
    fn recover_parked(&mut self) {
        let viewport = self.layout.viewport();
        let screen_h = self.platform.screen_height().max(viewport.height);
        for window in self.platform.visible_windows() {
            if !self.platform.is_parked(window) {
                continue;
            }
            self.platform.mark_parked(window, false);
            let bounds = self.platform.bounds(window);
            if is_off_stage(bounds, viewport) {
                let recovered = Rect {
                    x: bounds.x.clamp(viewport.x, viewport.x + viewport.width - 100),
                    y: (bounds.y - screen_h)
                        .clamp(viewport.y, viewport.y + viewport.height - 100),
                    width: bounds.width.clamp(100, viewport.width),
                    height: bounds.height.clamp(100, viewport.height),
                };
                self.platform.place(window, recovered);
                self.platform
                    .record_diag(DiagKind::OffstageRecovered, window.0.0 as u64, 0);
            }
        }
    }

    fn reload(&mut self, update: std::result::Result<Config, String>, hotkeys: Option<&mut Hotkeys>) {
        let paused = self.paused;
        let prepared = update.and_then(|config| {
            let area = self.platform.work_area()?;
            let viewport = config.viewport(area)?;
            if let Some(hotkeys) = hotkeys {
                let target = if paused {
                    bindings::pause_bindings(&config.bindings)
                } else {
                    config.bindings.clone()
                };
                hotkeys.replace(target)?;
            }
            Ok((config, viewport))
        });
        match prepared {
            Ok((config, viewport)) => {
                self.platform.configure_presentation(&config);
                self.layout
                    .configure(viewport, config.layout.gap, config.options.clone());
                self.layout.name(&config.workspaces);
                self.config = config;
                if !self.paused {
                    self.apply();
                    self.stack_floats_above_tiles();
                } else {
                    self.publish_state();
                }
                println!("Config reloaded. App rules/default widths apply when windows next join.");
            }
            Err(error) => {
                self.platform.record_diag(DiagKind::ReloadRejected, 0, 0);
                eprintln!("Config reload rejected; previous settings kept: {error}");
            }
        }
    }

    fn toggle_pause(&mut self, hotkeys: Option<&mut Hotkeys>) {
        if !self.paused {
            self.settle();
            if self.platform.overview_open() {
                let placements = self.stacked(self.layout.focused(), &self.overview_order);
                self.platform.close_overview(&self.layout, &placements);
                self.platform.settle_presentation();
            }
            self.platform.cancel_touchpad();
            self.swipe_base = None;
            self.paused = true;
            self.platform.set_hooks(false);
            if let Some(hotkeys) = hotkeys {
                let _ = hotkeys.replace(bindings::pause_bindings(&self.config.bindings));
            }
            self.platform.uncloak_all();
            self.restore_windows();
            self.publish_state();
            self.log_always("Paused");
            return;
        }

        self.paused = false;
        if let Some(hotkeys) = hotkeys
            && let Err(error) = hotkeys.replace(self.config.bindings.clone())
        {
            self.platform.record_diag(DiagKind::HotkeyFailed, 0, 0);
            eprintln!("Resume hotkey warning: {error}");
        }
        self.platform.set_hooks(true);

        // Reconcile against whatever opened or closed while paused.
        let live: Vec<_> = self
            .platform
            .visible_windows()
            .into_iter()
            .map(|window| (window, self.platform.info(window)))
            .filter(|(window, info)| self.platform.skip(*window, info).is_none())
            .collect();
        let live_handles: Vec<Window> = live.iter().map(|(w, _)| *w).collect();
        let tracked: Vec<Window> = self.layout.placements().map(|p| p.window).collect();
        for window in tracked {
            if !live_handles.contains(&window) {
                self.untiled.retain(|(known, _)| *known != window);
                self.layout.remove(window);
            } else if self.layout.mode(window) == Some(Mode::OnTop) {
                self.platform.set_topmost(window, true);
            }
        }
        for (window, _) in live {
            if !self.layout.contains(window) {
                self.adopt(window);
            }
        }
        self.placed.clear();
        self.apply();
        self.stack_floats_above_tiles();
        self.focus_layout();
        self.log_always("Resumed");
    }

    fn restore_windows(&mut self) {
        for (window, snapshot) in &self.untiled {
            if self.platform.is_parked(*window) {
                self.platform.mark_parked(*window, false);
            }
            match self.layout.mode(*window) {
                Some(Mode::Floating | Mode::Sticky) => {}
                Some(Mode::OnTop) => self.platform.set_topmost(*window, false),
                Some(Mode::Tiled) | None => self.platform.restore(*window, *snapshot),
            }
        }
    }

    /// Carries out a hotkey or agent action. Breaks when the manager should exit.
    fn perform(&mut self, action: Action, hotkeys: Option<&mut Hotkeys>) -> ControlFlow<()> {
        if action == Action::Quit {
            return ControlFlow::Break(());
        }
        if action == Action::TogglePause {
            self.toggle_pause(hotkeys);
            return ControlFlow::Continue(());
        }
        if self.paused {
            return ControlFlow::Continue(());
        }
        let navigation = matches!(
            action,
            Action::Focus(_) | Action::Move(_) | Action::Workspace(_) | Action::Send(_)
        );
        let active = self.layout.active().0;
        let in_overview = self.platform.overview_open();
        let before = (navigation && !in_overview && self.config.animations.enabled)
            .then(|| self.scene(true));
        // A window that changes size or mode does so at once, at rest. In the
        // overview its card just moves.
        let reshapes = matches!(
            action,
            Action::CycleWidth
                | Action::Resize(_)
                | Action::ToggleFull
                | Action::Toggle(_)
                | Action::ScratchpadMark
                | Action::ScratchpadToggle
                | Action::LoadSetup(_)
        );
        if reshapes && !in_overview {
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
            Action::ScratchpadMark => self.scratchpad_mark(),
            Action::ScratchpadToggle => self.scratchpad_toggle(),
            Action::TogglePause => unreachable!(),
            Action::Overview => {
                if self.platform.overview_open() {
                    self.close_overview();
                } else {
                    self.platform.open_overview();
                    self.overview_order = self.platform.visible_windows();
                    self.overview_chosen = self.layout.focused();
                }
                true
            }
            Action::Close => {
                // Only a window of ours: the desktop can have focus too, and
                // closing that asks to shut Windows down. While the overview
                // has the keyboard, it's the window highlighted there.
                let ours = self
                    .platform
                    .foreground()
                    .filter(|window| self.layout.contains(*window));
                let target = match self.platform.overview_open() {
                    true => self.layout.focused(),
                    false => ours,
                };
                if let Some(window) = target {
                    self.platform.close(window);
                }
                false
            }
            Action::Quit => return ControlFlow::Break(()),
            Action::Reload => return ControlFlow::Continue(()),
            Action::LoadSetup(index) => self.load_setup(index),
            Action::SaveSetup(index) => {
                self.save_setup(index);
                return ControlFlow::Continue(());
            }
        };

        let name = match action {
            Action::LoadSetup(index) => {
                let setup = self.config.setups.get(index);
                format!("Setup {}", setup.map_or("?", |setup| &setup.name))
            }
            _ => format!("{action:?}"),
        };
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
            if matches!(action, Action::ScratchpadToggle) {
                self.stack_floats_above_tiles();
            }
            self.focus_layout();
            self.log(&name);
            if matches!(
                action,
                Action::Send(_) | Action::Move(Direction::Up | Direction::Down)
            ) && let Some(window) = self.layout.focused()
            {
                self.record_workspace(window);
            }
        } else if self.config.behavior.log_events && !matches!(action, Action::Close) {
            println!("{name:<17} nothing to do");
        }
        ControlFlow::Continue(())
    }

    fn scratchpad_mark(&mut self) -> bool {
        let Some(focused) = self.layout.focused() else {
            return false;
        };
        let was_mode = self.layout.mode(focused);
        let width = self.layout.width(focused).or_else(|| {
            let opening = self.opening_mode(focused);
            self.config
                .behavior
                .remember_widths
                .then(|| self.rule_get(focused, opening).width)
                .flatten()
        });
        let Some((window, stashed)) = self.layout.scratchpad_mark(width) else {
            return false;
        };
        if stashed {
            if was_mode == Some(Mode::OnTop) {
                self.platform.set_topmost(window, false);
            }
            if was_mode == Some(Mode::Tiled) {
                // Keep a clean floating rect ready for when it is summoned.
                let rect =
                    scratchpad_rect(self.layout.viewport(), self.platform.bounds(window), true);
                self.platform.place(window, rect);
            }
        } else {
            // Returning to tiled on the current workspace.
            self.remember(window);
        }
        true
    }

    fn scratchpad_toggle(&mut self) -> bool {
        let Some((window, shown)) = self.layout.scratchpad_toggle() else {
            return false;
        };
        if shown {
            let current = self.platform.bounds(window);
            let rect = scratchpad_rect(self.layout.viewport(), current, false);
            if rect != current {
                self.platform.place(window, rect);
            }
        }
        true
    }

    fn handle_swipe(&mut self, event: SwipeEvent, hotkeys: Option<&mut Hotkeys>) {
        if self.paused || self.platform.overview_open() {
            self.swipe_base = None;
            return;
        }
        let viewport = self.layout.viewport();
        match event {
            SwipeEvent::Start(_) => {
                let base = self.scene(true);
                let slides: Vec<_> = base
                    .iter()
                    .filter(|item| item.visible)
                    .map(|item| Slide {
                        window: item.window,
                        from: item.rect,
                        to: item.rect,
                    })
                    .collect();
                self.swipe_base = Some(base);
                if !slides.is_empty() {
                    self.platform.stage_present(&slides, viewport, None, true);
                }
            }
            SwipeEvent::Update { axis, offset_px } => {
                let Some(base) = &self.swipe_base else {
                    return;
                };
                let (dx, dy) = match axis {
                    Axis::Horizontal => (offset_px, 0),
                    Axis::Vertical => (0, offset_px),
                };
                let slides: Vec<_> = base
                    .iter()
                    .filter(|item| item.visible || (axis == Axis::Horizontal && item.active))
                    .map(|item| {
                        let shifted = Rect {
                            x: item.rect.x + dx,
                            y: item.rect.y + dy,
                            ..item.rect
                        };
                        Slide {
                            window: item.window,
                            from: shifted,
                            to: shifted,
                        }
                    })
                    .collect();
                if !slides.is_empty() {
                    self.platform.stage_present(&slides, viewport, None, true);
                }
            }
            SwipeEvent::Commit { direction } => {
                let base = self.swipe_base.take();
                let before_active = self.layout.active().0;
                let before_focused = self.layout.focused();
                let _ = self.perform(Action::Focus(direction), hotkeys);
                let moved = self.layout.active().0 != before_active
                    || self.layout.focused() != before_focused;
                if !moved
                    && let Some(base) = base
                {
                    self.cancel_swipe(&base);
                }
            }
            SwipeEvent::Cancel => {
                if let Some(base) = self.swipe_base.take() {
                    self.cancel_swipe(&base);
                }
            }
        }
    }

    fn cancel_swipe(&mut self, base: &[SceneWindow<Window>]) {
        let viewport = self.layout.viewport();
        let slides: Vec<_> = base
            .iter()
            .filter(|item| item.visible)
            .map(|item| Slide {
                window: item.window,
                from: item.rect,
                to: item.rect,
            })
            .collect();
        let rate = self.config.animations.rate(self.config.animations.horizontal);
        self.platform.stage_present(&slides, viewport, rate, false);
        self.apply();
    }

    fn handle(&mut self, event: Event) {
        if self.paused {
            return;
        }
        // An app taking the keyboard as its window opens somewhere else.
        if let Event::Focused(window) = event
            && self.platform.foreground() == Some(window)
            && self.take_quiet(window)
        {
            self.apply();
            if let Some(overview_win) = self
                .platform
                .overview_window()
                .filter(|_| self.platform.overview_open())
            {
                self.platform.focus(overview_win);
            } else {
                self.focus_layout();
            }
            return;
        }
        let focused = self.layout.focused();
        let active = self.layout.active().0;
        let in_overview = self.platform.overview_open();
        let before =
            (matches!(event, Event::Focused(_)) && !in_overview && self.config.animations.enabled)
                .then(|| self.scene(true));
        let name = match event {
            Event::Appeared(_) => "opened",
            Event::Hidden(_) | Event::MinimizeStarted(_) | Event::Cloaked(_) => "hidden",
            Event::Destroyed(_) => "closed",
            Event::Focused(_) => "focused",
            Event::Moved(_) => "moved",
        };

        let changed = match event {
            // Windows uncloaks a window that takes the keyboard. One of ours
            // that has since lost it goes back out of sight.
            Event::Appeared(window) if self.layout.contains(window) => {
                // Brought forward from another workspace, as a link does to
                // the browser: follow it there.
                if self.platform.foreground() == Some(window)
                    && self.keyboard() != Some(window)
                    && !self.interim_focus(window)
                {
                    return self.handle(Event::Focused(window));
                }
                if self.platform.is_marked_cloaked(window)
                    && !self.platform.cloaked(window)
                    && self.keyboard() != Some(window)
                {
                    self.platform
                        .record_diag(DiagKind::UncloakTrap, window.0.0 as u64, 0);
                    self.apply();
                }
                false
            }
            Event::Appeared(window) => self.platform.is_tileable(window) && self.adopt(window),
            // A queued hide can outlive an app showing its existing window again.
            Event::Hidden(window) => !self.platform.is_visible(window) && self.release(window),
            Event::MinimizeStarted(window) => self.release(window),
            // Our own cloaking lands here too, and so late that the window may
            // be back on screen by now. Only a window that someone else has
            // cloaked, and that still is, has left.
            Event::Cloaked(window) => {
                if self.platform.is_marked_cloaked(window) || !self.platform.info(window).cloaked {
                    if self.layout.contains(window) {
                        self.platform
                            .record_diag(DiagKind::StaleEvent, window.0.0 as u64, 1);
                    }
                    false
                } else {
                    self.release(window)
                }
            }
            Event::Destroyed(window) => {
                self.untiled.retain(|(known, _)| *known != window);
                self.layout.remove(window)
            }
            // Windows reports each window it passes through on the way to the
            // one that ends up focused; only that last one matters.
            Event::Focused(window) => {
                let current = self.platform.foreground() == Some(window);
                if !current && self.layout.contains(window) {
                    self.platform
                        .record_diag(DiagKind::StaleEvent, window.0.0 as u64, 2);
                }
                let focused = current && !self.interim_focus(window) && self.layout.focus(window);
                // Another window taking the keyboard ends the overview, which
                // zooms back out to the desktop as that window has it.
                let closes = current && in_overview;
                if closes {
                    let placements =
                        self.stacked(self.layout.focused(), &self.platform.visible_windows());
                    self.platform.close_overview(&self.layout, &placements);
                }
                if focused && self.layout.mode(window) == Some(Mode::Tiled) {
                    self.stack_floats_above_tiles();
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
            let hushed = match event {
                Event::Appeared(window) => {
                    self.platform.foreground() == Some(window)
                        && self.quiet.iter().any(|(quiet, _)| *quiet == window)
                }
                _ => false,
            };
            if (refocused && !focus_event) || hushed {
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
        self.log_always(event);
    }

    fn log_always(&self, event: &str) {
        let (active, count) = self.layout.active();
        let workspace = active + 1;
        let Some(window) = self.layout.focused() else {
            println!("{event:<17} workspace {workspace}/{count}  empty");
            return;
        };
        let mode = if self.layout.is_scratchpad(window) {
            "scratchpad"
        } else {
            match self.layout.mode(window) {
                Some(Mode::Floating) => "floating",
                Some(Mode::Sticky) => "sticky",
                Some(Mode::OnTop) => "on top",
                Some(Mode::Tiled) | None => "tiled",
            }
        };
        let width = match self.layout.width(window) {
            Some(Width { full: true, .. }) => " full".to_owned(),
            Some(Width { pixels, .. }) => format!(" {pixels}px"),
            None => String::new(),
        };
        let state = format!("{mode}{width}");
        let exe = self.platform.exe(window).unwrap_or_else(|| "?".into());
        let title = self.platform.title(window);
        println!("{event:<17} workspace {workspace}/{count}  {state:<13} {exe}  {title:.48}");
    }

    fn rule_get(&self, window: Window, opening: Mode) -> Rule {
        let exe = self.platform.exe(window);
        let class = self.platform.class(window);
        self.rules.get_by(exe.as_deref(), &class, opening)
    }

    fn rule_set(&mut self, window: Window, rule: Rule, opening: Mode) {
        let exe = self.platform.exe(window);
        let class = self.platform.class(window);
        self.rules.set_by(exe, class, rule, opening);
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
        let opening = self.opening_mode(window);
        let rule = self.rule_get(window, opening);
        self.rule_set(window, Rule { width, ..rule }, opening);
    }

    /// The mode a window opens in when no rule says otherwise: floating if
    /// it looks like a dialog, as in niri, or else tiled.
    fn opening_mode(&self, window: Window) -> Mode {
        if self.config.behavior.float_dialogs && self.platform.info(window).dialog_like() {
            Mode::Floating
        } else {
            Mode::Tiled
        }
    }

    /// Gives keyboard focus to the layout's focused window, or to the desktop
    /// on an empty workspace so that no hidden window keeps it. The overview
    /// keeps the keyboard for as long as it's open.
    fn focus_layout(&mut self) {
        if self.platform.overview_open() {
            return;
        }
        let target = self.focus_target();
        let floating = self
            .layout
            .mode(target)
            .is_some_and(|mode| mode != Mode::Tiled);
        self.deferred =
            self.platform.stage_covering() || (floating && self.platform.stage_veils(target));
        if !self.deferred {
            self.platform.focus(target);
        }
    }

    /// The window that has the keyboard, or is about to, unless that's the
    /// overview or the desktop.
    fn keyboard(&self) -> Option<Window> {
        self.layout
            .focused()
            .filter(|_| !self.platform.overview_open())
    }

    /// Cloaking the outgoing window can temporarily give another staged
    /// window focus before the destination can take it.
    fn interim_focus(&self, window: Window) -> bool {
        self.deferred
            && !self.platform.overview_open()
            && self.layout.focused() != Some(window)
            && self.platform.stage_shows(window)
    }

    /// The window the keyboard belongs to when the overview doesn't have it.
    fn focus_target(&self) -> Window {
        self.layout
            .focused()
            .unwrap_or_else(|| self.platform.desktop())
    }

    /// Starts the overview's way out and hands the keyboard back.
    fn close_overview(&mut self) {
        let placements = self.stacked(self.layout.focused(), &self.overview_order);
        self.platform.close_overview(&self.layout, &placements);
        // Sends the window off stage before it takes the keyboard.
        self.apply();
        self.focus_layout();
    }

    /// Goes to the window or workspace clicked in the overview and closes it.
    fn click(&mut self, x: i32, y: i32) {
        // Still on screen only because it's fading out.
        if !self.platform.overview_open() {
            self.settle();
            return;
        }
        match self.platform.overview_target(x, y) {
            Some(Target::Window(window)) => self.layout.focus(window),
            Some(Target::Workspace(index)) => self.layout.focus_workspace(index),
            None => false,
        };
        self.close_overview();
        self.log("Click");
    }

    fn stack_floats_above_tiles(&mut self) {
        if !self.config.behavior.floating_above_tiled
            || self.platform.overview_open()
            || self.platform.stage_covering()
        {
            return;
        }
        let mut visible: Vec<_> = self
            .layout
            .placements()
            .filter(|item| item.visible && !self.platform.stage_veils(item.window))
            .collect();
        if !visible.iter().any(|p| p.rect.is_none()) {
            return;
        }
        // Reordering across the topmost band can change a window's topmost style.
        visible.retain(|p| !self.platform.topmost(p.window));
        let z_order = self.platform.visible_windows();
        lower_occluding_tiles(&visible, &z_order, |tile, anchor| {
            self.platform.place_behind(tile, anchor);
        });
    }

    fn toggle(&mut self, target: Mode) -> bool {
        let Some(window) = self.layout.focused() else {
            return false;
        };
        let Some(before) = self.layout.mode(window) else {
            return false;
        };
        let was_scratchpad = self.layout.is_scratchpad(window);
        let after = if was_scratchpad && target == Mode::Floating {
            Mode::Tiled
        } else {
            before.toggled(target)
        };
        let opening = self.opening_mode(window);
        let previous = self.rule_get(window, opening);
        // Its own width while it's tiled, or else the one its kind last had.
        let width = self.layout.width(window);
        let width = width.or_else(|| {
            self.config
                .behavior
                .remember_widths
                .then_some(previous.width)
                .flatten()
        });
        self.layout.set_mode(window, after, width);
        self.changed_mode(window, before, after);
        self.rule_set(
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
                ..previous
            },
            opening,
        );
        true
    }

    fn changed_mode(&mut self, window: Window, before: Mode, after: Mode) {
        if before == after {
            return;
        }
        let spot = self
            .untiled
            .iter()
            .find(|(known, _)| *known == window)
            .map(|(_, snapshot)| *snapshot);
        if after == Mode::Tiled {
            self.remember(window);
        } else if let (Mode::Tiled, Some(snapshot)) = (before, spot) {
            // Never maximized, which would cover everything it floats above.
            self.platform.restore(
                window,
                Snapshot {
                    showCmd: SW_SHOWNORMAL.0 as u32,
                    ..snapshot
                },
            );
        }
        if (before == Mode::OnTop) != (after == Mode::OnTop) {
            self.platform.set_topmost(window, after == Mode::OnTop);
        }
    }

    fn adopt(&mut self, window: Window) -> bool {
        let (exe, class, title) = (
            self.platform.exe(window).unwrap_or_default(),
            self.platform.class(window),
            self.platform.title(window),
        );
        let opening = self.opening_mode(window);
        let mut learned = self.rule_get(window, opening);
        // Another window of an app that's open, as a browser opens one, stays
        // on the workspace on screen.
        let first = !self.kind_open(window, &exe, &class);
        if !first {
            learned.workspace = None;
        }
        let joining = self.config.resolve_rule(
            &exe,
            &class,
            &title,
            learned,
            opening,
            self.layout.default_width(),
        );
        let (item, focus) = match self.claim(window, &exe, &class, &title, joining.as_ref()) {
            Some(item) => (item, false),
            None => {
                let Some(Joining {
                    rule: Rule { mode, width, .. },
                    workspace,
                    focus,
                }) = joining
                else {
                    return false;
                };
                let index = self.workspace_index(workspace.as_ref());
                let item = Arranged {
                    window,
                    mode,
                    width,
                    workspace: index,
                    after: self.layout.column_focus(index),
                };
                (item, focus)
            }
        };
        if !self.untiled.iter().any(|(known, _)| *known == window) {
            self.remember(window);
        }
        self.layout.arrange(&[item]);
        if first {
            self.record_workspace(window);
        }
        if focus {
            self.layout.focus(window);
        } else {
            let now = self.platform.now();
            self.quiet
                .retain(|(_, since)| now.saturating_sub(*since) < QUIET);
            self.quiet.push((window, now));
        }
        if item.mode == Mode::OnTop {
            self.platform.set_topmost(window, true);
        }
        true
    }

    /// Whether the window was placed without focus a moment ago, forgetting it either way.
    fn take_quiet(&mut self, window: Window) -> bool {
        let now = self.platform.now();
        let found = self.quiet.iter().position(|(quiet, _)| *quiet == window);
        found.is_some_and(|index| now.saturating_sub(self.quiet.swap_remove(index).1) < QUIET)
    }

    fn workspace_index(&self, spot: Option<&Spot>) -> usize {
        match spot {
            Some(Spot::Name(name)) => self.layout.named(name),
            Some(Spot::Number(number)) => Some(*number as usize - 1),
            None => None,
        }
        .unwrap_or(self.layout.active().0)
    }

    fn destination(&self, spot: Option<&Spot>) -> Option<u32> {
        match spot? {
            Spot::Number(number) => Some(*number),
            Spot::Name(name) => self
                .config
                .workspaces
                .iter()
                .position(|known| known == name)
                .map(|index| index as u32 + 1),
        }
    }

    fn claim(
        &mut self,
        window: Window,
        exe: &str,
        class: &str,
        title: &str,
        joining: Option<&Joining>,
    ) -> Option<Arranged<Window>> {
        let now = self.platform.now();
        let loading = self
            .loading
            .as_mut()
            .filter(|loading| now.saturating_sub(loading.since) < LAUNCH_WAIT)?;
        let number = (0..loading.apps.len()).find(|number| {
            loading.claimed[*number].is_none() && loading.apps[*number].matches(exe, class, title)
        })?;
        loading.claimed[number] = Some(window);
        let app = loading.apps[number].clone();
        let others: Vec<_> = loading
            .claimed
            .iter()
            .zip(&loading.apps)
            .enumerate()
            .filter_map(|(other, (claimed, other_app))| {
                Some((other, (*claimed)?, other_app.workspace.clone()))
            })
            .collect();
        let destination = self.destination(app.workspace.as_ref());
        let mates: Vec<_> = others
            .into_iter()
            .filter(|(other, mate, spot)| {
                *other != number
                    && self.layout.contains(*mate)
                    && self.destination(spot.as_ref()) == destination
            })
            .collect();
        let after = mates
            .iter()
            .rev()
            .filter(|(other, ..)| *other < number)
            .map(|(_, mate, _)| *mate)
            .find(|mate| self.layout.mode(*mate) == Some(Mode::Tiled));
        let workspace = after
            .into_iter()
            .chain(mates.iter().map(|(_, mate, _)| *mate))
            .find_map(|mate| self.layout.workspace_of(mate))
            .unwrap_or_else(|| self.workspace_index(app.workspace.as_ref()));
        let learned = joining.map(|joining| &joining.rule);
        Some(Arranged {
            window,
            mode: app
                .mode
                .and_then(RuleMode::mode)
                .or(learned.map(|rule| rule.mode))
                .unwrap_or(Mode::Tiled),
            width: app.width(
                learned.and_then(|rule| rule.width),
                self.layout.default_width(),
            ),
            workspace,
            after,
        })
    }

    fn load_setup(&mut self, index: usize) -> bool {
        let Some(setup) = self.config.setups.get(index) else {
            return false;
        };
        let (name, apps) = (setup.name.clone(), setup.apps.clone());
        let open: Vec<_> = self
            .layout
            .placements()
            .map(|placement| {
                let window = placement.window;
                let exe = self.platform.exe(window).unwrap_or_default();
                (
                    window,
                    exe,
                    self.platform.class(window),
                    self.platform.title(window),
                )
            })
            .collect();
        let mut claimed: Vec<Option<Window>> = vec![None; apps.len()];
        for (number, app) in apps.iter().enumerate() {
            claimed[number] = open
                .iter()
                .find(|(window, exe, class, title)| {
                    !claimed.contains(&Some(*window)) && app.matches(exe, class, title)
                })
                .map(|(window, ..)| *window);
        }

        let default_width = self.layout.default_width();
        let mut items = Vec::new();
        let mut modes = Vec::new();
        for (app, window) in apps.iter().zip(&claimed) {
            let Some(window) = *window else {
                continue;
            };
            let Some(before) = self.layout.mode(window) else {
                continue;
            };
            let width = self.layout.width(window).or_else(|| {
                let opening = self.opening_mode(window);
                self.rule_get(window, opening).width
            });
            let item = Arranged {
                window,
                mode: app.mode.and_then(RuleMode::mode).unwrap_or(before),
                width: app.width(width, default_width),
                workspace: self.workspace_index(app.workspace.as_ref()),
                after: None,
            };
            modes.push((window, before, item.mode));
            items.push(item);
        }
        self.layout.arrange(&items);
        for (window, before, after) in modes {
            self.changed_mode(window, before, after);
            self.record_workspace(window);
        }

        let launches: Vec<_> = apps
            .iter()
            .zip(&claimed)
            .filter(|(_, window)| window.is_none())
            .filter_map(|(app, _)| Some((app.run.clone()?, app.args.clone())))
            .collect();
        self.log_always(&format!(
            "Setup {name}: {} placed, {} launching",
            items.len(),
            launches.len()
        ));
        self.platform.launch_setup(launches);
        self.loading = Some(Loading {
            apps,
            claimed,
            since: self.platform.now(),
        });
        true
    }

    fn save_setup(&mut self, index: usize) {
        let names: Vec<Option<String>> = self
            .layout
            .names()
            .map(|name| name.map(str::to_owned))
            .collect();
        let apps: Vec<App> = self
            .layout
            .placements()
            .filter_map(|placement| {
                let window = placement.window;
                if self.layout.is_scratchpad(window) {
                    return None;
                }
                let mode = self.layout.mode(window)?;
                let path = self.platform.exe_path(window)?;
                let exe = path.file_name()?.to_string_lossy().into_owned();
                let pinned = matches!(mode, Mode::Sticky | Mode::OnTop);
                let workspace = (!pinned).then(|| match &names[placement.workspace] {
                    Some(name) => Spot::Name(name.clone()),
                    None => Spot::Number(placement.workspace as u32 + 1),
                });
                let width = self.layout.width(window);
                // Store apps can't be started from their frame's program.
                let launchable = !exe.eq_ignore_ascii_case("ApplicationFrameHost.exe");
                Some(App {
                    run: launchable.then(|| path.to_string_lossy().into_owned()),
                    exe,
                    class: Some(self.platform.class(window)),
                    workspace,
                    mode: Some(RuleMode::of(mode)),
                    width: width.map(|width| width.pixels),
                    full_width: width.map(|width| width.full),
                    ..App::default()
                })
            })
            .collect();
        let Some(setup) = self.config.setups.get_mut(index) else {
            return;
        };
        let count = apps.len();
        setup.apps = apps;
        let name = setup.name.clone();
        self.platform
            .save_setups(self.setups_file.clone(), setups::render(&self.config.setups));
        self.log_always(&format!(
            "Saved setup {name}: {count} windows to {}",
            self.setups_file.display()
        ));
    }

    /// Puts the open windows saved in the last session back as they were kept,
    /// including scratchpad windows and active workspace.
    fn restore(&mut self, open: Vec<Window>) -> Vec<Window> {
        let saved = self.platform.load_session();
        let mut items = Vec::new();
        let mut scratchpads = Vec::new();
        for entry in saved.entries {
            let window = entry.window;
            let Some(exe) = self.platform.exe(window) else {
                continue;
            };
            let class = self.platform.class(window);
            if !open.contains(&window) || !entry.is(window, &exe, &class) {
                continue;
            }
            // A rule made since may have it ignored.
            let opening = self.opening_mode(window);
            let title = self.platform.title(window);
            if self
                .config
                .resolve_rule(
                    &exe,
                    &class,
                    &title,
                    self.rule_get(window, opening),
                    opening,
                    self.layout.default_width(),
                )
                .is_none()
            {
                continue;
            }
            if entry.scratchpad {
                scratchpads.push(window);
            } else {
                items.push(Arranged {
                    window,
                    mode: entry.mode,
                    width: entry.width,
                    workspace: entry.workspace,
                    after: None,
                });
            }
        }
        self.layout.arrange(&items);
        let mut restored = Vec::with_capacity(items.len() + scratchpads.len());
        for item in &items {
            self.remember(item.window);
            if item.mode == Mode::OnTop {
                self.platform.set_topmost(item.window, true);
            }
            restored.push(item.window);
        }
        for window in scratchpads {
            self.remember(window);
            self.layout.add_scratchpad(window);
            restored.push(window);
        }
        if let Some(active) = saved.active
            && !restored.is_empty()
        {
            self.layout.focus_workspace(active);
        }
        restored
    }

    /// The layout as `restore` takes it back.
    fn session(&self) -> Vec<Entry> {
        self.layout
            .placements()
            .filter_map(|placement| {
                let window = placement.window;
                let scratchpad = self.layout.is_scratchpad(window);
                Some(Entry {
                    window,
                    mode: self.layout.mode(window)?,
                    scratchpad,
                    // Not the one on screen for windows shown on every one,
                    // which would make `arrange` open a workspace for it.
                    workspace: self.layout.workspace_of(window).unwrap_or(0),
                    exe: self.platform.exe(window)?,
                    class: self.platform.class(window),
                    width: self.layout.width(window),
                })
            })
            .collect()
    }

    fn persist_session(&mut self, sync: bool) {
        let active = self.layout.active().0;
        let text = session::render(active, &self.session());
        if sync || text != self.last_session {
            self.last_session = text.clone();
            self.platform.save_session(text, sync);
        }
    }

    fn record_workspace(&mut self, window: Window) {
        if !self.config.behavior.remember_workspaces {
            return;
        }
        let Some(index) = self.layout.workspace_of(window) else {
            return;
        };
        let workspace = self.layout.names().nth(index).flatten().map(str::to_owned);
        let opening = self.opening_mode(window);
        let rule = self.rule_get(window, opening);
        self.rule_set(window, Rule { workspace, ..rule }, opening);
    }

    fn kind_open(&self, window: Window, exe: &str, class: &str) -> bool {
        self.layout.placements().any(|placement| {
            let other = placement.window;
            other != window
                && self.platform.class(other) == class
                && self.platform.exe(other).as_deref() == Some(exe)
        })
    }

    fn remember(&mut self, window: Window) {
        if let Some(snapshot) = self.platform.snapshot(window) {
            self.untiled.retain(|(known, _)| *known != window);
            self.untiled.push((window, snapshot));
        }
    }

    fn release(&mut self, window: Window) -> bool {
        let removed = self.layout.remove(window);
        if removed && self.platform.is_marked_cloaked(window) {
            let _ = self.platform.set_cloak(window, false);
        }
        removed
    }

    fn publish_state(&mut self) {
        let (active, count) = self.layout.active();
        let focused = self.layout.focused();
        let (tiled, floating, scratchpad) = self.layout.counts();
        self.platform.publish(BarSnapshot {
            active: active as u32,
            workspaces: count as u32,
            focused: focused.map_or(0, |window| window.0.0 as u64),
            mode: match focused {
                None => 0,
                Some(window) if self.layout.is_scratchpad(window) => 5,
                Some(window) => match self.layout.mode(window) {
                    None => 0,
                    Some(Mode::Tiled) => 1,
                    Some(Mode::Floating) => 2,
                    Some(Mode::Sticky) => 3,
                    Some(Mode::OnTop) => 4,
                },
            },
            overview: self.platform.overview_open(),
            paused: self.paused,
            tiled: tiled as u32,
            floating: floating as u32,
            scratchpad: scratchpad as u32,
            scratchpad_shown: self.layout.scratchpad_shown().is_some(),
        });
    }

    fn apply(&mut self) {
        self.publish_state();
        // Choosing a window in the view leaves the real ones where they are,
        // so only its picture comes to the top.
        if self.platform.overview_open() {
            let focused = self.layout.focused();
            if focused != self.overview_chosen {
                self.overview_chosen = focused;
                if let Some(window) = focused {
                    self.overview_order.retain(|&other| other != window);
                    self.overview_order.insert(0, window);
                }
            }
            let placements = self.stacked(None, &self.overview_order);
            self.platform.overview_show(&self.layout, &placements);
        }
        // The real windows stay as they are until the pictures cover them;
        // `tick` comes back here then.
        if self.platform.stage_covering() {
            return;
        }
        if !self.paused {
            self.persist_session(false);
        }
        let placed = std::mem::take(&mut self.placed);
        let keyboard = self.keyboard();
        let holder = self.platform.foreground();
        let screen_h = self.platform.screen_height();
        let placements: Vec<_> = self.layout.placements().collect();
        let mut revealed = false;

        for placement in placements {
            let Placement {
                window,
                rect,
                visible,
                ..
            } = placement;
            let veiled = self.platform.stage_veils(window);
            let waiting = veiled && Some(window) == keyboard;
            let marked = self.platform.is_marked_cloaked(window);
            // Our mark outlives the cloak when Windows lifts it by itself.
            let hidden = marked && self.platform.cloaked(window);
            if waiting {
                // Left to Windows to uncloak once it has the keyboard, which
                // gives it time to get off the screen first.
                if marked && !hidden && !self.platform.set_cloak(window, false) {
                    self.platform
                        .record_diag(DiagKind::CloakFailed, window.0.0 as u64, 0);
                }
            } else if !visible || veiled {
                if !hidden {
                    // Windows shows a cloaked window again while it has the
                    // keyboard, so it goes to the desktop until the layout's
                    // focused window takes it.
                    if holder == Some(window) {
                        let desktop = self.platform.desktop();
                        self.platform.focus(desktop);
                    }
                    if !self.platform.set_cloak(window, true) {
                        self.platform
                            .record_diag(DiagKind::CloakFailed, window.0.0 as u64, 1);
                    }
                }
            } else if marked {
                if !self.platform.set_cloak(window, false) {
                    self.platform
                        .record_diag(DiagKind::CloakFailed, window.0.0 as u64, 0);
                } else {
                    revealed = true;
                }
            }
            let Some(rect) = rect else {
                continue;
            };
            let target = if waiting {
                self.platform.mark_parked(window, true);
                Rect {
                    y: rect.y + screen_h,
                    ..rect
                }
            } else {
                rect
            };
            if placed.contains(&(window, target)) || self.platform.place(window, target) {
                self.placed.push((window, target));
                if !waiting && self.platform.is_parked(window) {
                    self.platform.mark_parked(window, false);
                }
            }
        }
        // A tile may already have taken focus while the floats were cloaked.
        // Restore their order under the thumbnails before the stage clears.
        if revealed {
            self.stack_floats_above_tiles();
        }
    }

    /// Every placement, bottom-most picture first, stacked as in `z_order`
    /// (top first) once `raised` has the keyboard.
    fn stacked(&self, raised: Option<Window>, z_order: &[Window]) -> Vec<Placement<Window>> {
        let mut placements: Vec<_> = self.layout.placements().collect();
        stack_placements(
            &mut placements,
            &self.layout,
            self.config.behavior.floating_above_tiled,
            raised,
            z_order,
        );
        placements
    }

    /// With `shown`, the windows on screen are where they really are right
    /// now, which is where their pictures must first appear.
    fn scene(&self, shown: bool) -> Vec<SceneWindow<Window>> {
        let active = self.layout.active().0;
        self.stacked(self.layout.focused(), &self.platform.visible_windows())
            .into_iter()
            .map(|item| {
                let real = (shown && item.visible).then(|| self.platform.bounds(item.window));
                SceneWindow {
                    window: item.window,
                    // Not one that's minimized, or still on its way back
                    // from off stage.
                    rect: real
                        .filter(|rect| rect.width > 0 && rect.height > 0)
                        .filter(|rect| !is_off_stage(*rect, self.layout.viewport()))
                        .unwrap_or_else(|| self.platform.pose(item.window, item.rect)),
                    visible: item.visible,
                    active: item.workspace == active,
                }
            })
            .collect()
    }

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
        if still && !self.platform.stage_is_presenting() {
            return;
        }
        self.platform
            .stage_present(&slides, viewport, Some(rate), false);
    }

    /// Ends whatever is moving, with every window where it belongs.
    fn settle(&mut self) {
        self.platform.settle_presentation();
        self.apply();
        if self.deferred {
            self.focus_layout();
        }
    }
}

impl<P: Platform> Drop for Manager<P> {
    fn drop(&mut self) {
        self.platform.save_rules(&mut self.rules);
        self.persist_session(true);
        self.platform.uncloak_all();
        self.restore_windows();
    }
}

/// HWND_TOP can leave a background float below the foreground tile. Move the
/// tiles behind the lowest visible float instead, keeping the keyboard where
/// it is. Walk bottom to top so successive inserts preserve tile order.
fn lower_occluding_tiles<W: Copy + Eq>(
    visible: &[Placement<W>],
    z_order: &[W],
    mut place_behind: impl FnMut(W, W),
) {
    let mut lowest_float = None;
    for &window in z_order.iter().rev() {
        let Some(item) = visible.iter().find(|p| p.window == window) else {
            continue;
        };
        if item.rect.is_none() {
            lowest_float.get_or_insert(window);
        } else if let Some(anchor) = lowest_float {
            place_behind(window, anchor);
        }
    }
}

/// Bottom-most thumbnail first. Cloaked windows keep their place in the
/// system z-order.
fn stack_placements<W: Copy + Eq>(
    placements: &mut [Placement<W>],
    layout: &Workspaces<W>,
    floating_above_tiled: bool,
    raised: Option<W>,
    z_order: &[W],
) {
    placements.sort_by_key(|item| {
        (
            layout.mode(item.window) == Some(Mode::OnTop),
            floating_above_tiled && item.rect.is_none(),
            Some(item.window) == raised,
            Reverse(
                z_order
                    .iter()
                    .position(|window| *window == item.window)
                    .unwrap_or(usize::MAX),
            ),
        )
    });
}

/// Centers a scratchpad window in `viewport`. When `force_default_size` is set
/// (or the window has no valid smaller floating size), sizes it to 3/4 of the
/// viewport so previously tiled windows summon cleanly.
fn scratchpad_rect(viewport: Rect, current: Rect, force_default_size: bool) -> Rect {
    let max_w = (viewport.width * 9 / 10).max(200);
    let max_h = (viewport.height * 9 / 10).max(200);
    let (width, height) = if !force_default_size
        && current.width > 100
        && current.height > 100
        && current.width <= max_w
        && current.height <= max_h
    {
        (current.width, current.height)
    } else {
        (
            (viewport.width * 3 / 4).max(200),
            (viewport.height * 3 / 4).max(200),
        )
    };
    Rect {
        x: viewport.x + (viewport.width - width) / 2,
        y: viewport.y + (viewport.height - height) / 2,
        width,
        height,
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
    use crate::window::{Info, Skip};
    use std::collections::BTreeMap;
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{WS_CAPTION, WS_THICKFRAME};

    const VIEWPORT: Rect = Rect {
        x: 0,
        y: 0,
        width: 1000,
        height: 800,
    };

    fn win(id: usize) -> Window {
        Window(HWND(id as *mut c_void))
    }

    #[derive(Clone)]
    struct FakeWindow {
        exe: String,
        class: String,
        title: String,
        bounds: Rect,
        cloaked: bool,
        marked_cloaked: bool,
        parked: bool,
        topmost: bool,
        visible: bool,
        closed: bool,
    }

    impl FakeWindow {
        fn new(exe: &str, x: i32) -> Self {
            Self {
                exe: exe.into(),
                class: "AppClass".into(),
                title: exe.into(),
                bounds: Rect {
                    x,
                    y: 0,
                    width: 500,
                    height: 600,
                },
                cloaked: false,
                marked_cloaked: false,
                parked: false,
                topmost: false,
                visible: true,
                closed: false,
            }
        }
    }

    struct FakePlatform {
        windows: BTreeMap<usize, FakeWindow>,
        foreground: Option<Window>,
        desktop: Window,
        now: Duration,
        saved_session: session::Saved,
        persisted_sessions: Vec<(String, bool)>,
        published: Vec<BarSnapshot>,
        diags: Vec<(DiagKind, u64, u32)>,
        hooks_enabled: bool,
        overview_open: bool,
        stage_slides: Vec<Slide<Window>>,
        stage_presenting: bool,
        restored: Vec<Window>,
    }

    impl FakePlatform {
        fn new() -> Self {
            Self {
                windows: BTreeMap::new(),
                foreground: None,
                desktop: win(9999),
                now: Duration::ZERO,
                saved_session: session::Saved::default(),
                persisted_sessions: Vec::new(),
                published: Vec::new(),
                diags: Vec::new(),
                hooks_enabled: true,
                overview_open: false,
                stage_slides: Vec::new(),
                stage_presenting: false,
                restored: Vec::new(),
            }
        }

        fn add(&mut self, id: usize, exe: &str, x: i32) -> Window {
            self.windows.insert(id, FakeWindow::new(exe, x));
            win(id)
        }

        fn key(window: Window) -> usize {
            window.0.0 as usize
        }
    }

    impl Platform for FakePlatform {
        fn visible_windows(&self) -> Vec<Window> {
            self.windows
                .iter()
                .filter(|(_, w)| w.visible && !w.closed)
                .map(|(id, _)| win(*id))
                .collect()
        }

        fn foreground(&self) -> Option<Window> {
            self.foreground
        }

        fn is_visible(&self, window: Window) -> bool {
            self.windows
                .get(&Self::key(window))
                .is_some_and(|w| w.visible && !w.closed)
        }

        fn desktop(&self) -> Window {
            self.desktop
        }

        fn info(&self, window: Window) -> Info {
            let Some(w) = self.windows.get(&Self::key(window)) else {
                return Info::default();
            };
            Info {
                title: w.title.clone(),
                class: w.class.clone(),
                bounds: w.bounds,
                visible: w.visible && !w.closed,
                cloaked: w.cloaked,
                style: WS_CAPTION | WS_THICKFRAME,
                ..Info::default()
            }
        }

        fn skip(&self, _window: Window, info: &Info) -> Option<Skip> {
            info.skip()
        }

        fn exe(&self, window: Window) -> Option<String> {
            self.windows.get(&Self::key(window)).map(|w| w.exe.clone())
        }

        fn exe_path(&self, window: Window) -> Option<PathBuf> {
            self.exe(window).map(PathBuf::from)
        }

        fn class(&self, window: Window) -> String {
            self.windows
                .get(&Self::key(window))
                .map(|w| w.class.clone())
                .unwrap_or_default()
        }

        fn title(&self, window: Window) -> String {
            self.windows
                .get(&Self::key(window))
                .map(|w| w.title.clone())
                .unwrap_or_default()
        }

        fn bounds(&self, window: Window) -> Rect {
            self.windows
                .get(&Self::key(window))
                .map(|w| w.bounds)
                .unwrap_or_default()
        }

        fn pose(&self, window: Window, slot: Option<Rect>) -> Rect {
            slot.unwrap_or_else(|| self.bounds(window))
        }

        fn cloaked(&self, window: Window) -> bool {
            self.windows
                .get(&Self::key(window))
                .is_some_and(|w| w.cloaked)
        }

        fn is_marked_cloaked(&self, window: Window) -> bool {
            self.windows
                .get(&Self::key(window))
                .is_some_and(|w| w.marked_cloaked)
        }

        fn set_cloak(&mut self, window: Window, cloaked: bool) -> bool {
            if let Some(w) = self.windows.get_mut(&Self::key(window)) {
                w.cloaked = cloaked;
                w.marked_cloaked = cloaked;
                true
            } else {
                false
            }
        }

        fn uncloak_all(&mut self) {
            for w in self.windows.values_mut() {
                if w.marked_cloaked {
                    w.cloaked = false;
                    w.marked_cloaked = false;
                }
            }
        }

        fn mark_parked(&mut self, window: Window, parked: bool) {
            if let Some(w) = self.windows.get_mut(&Self::key(window)) {
                w.parked = parked;
            }
        }

        fn is_parked(&self, window: Window) -> bool {
            self.windows
                .get(&Self::key(window))
                .is_some_and(|w| w.parked)
        }

        fn place(&mut self, window: Window, rect: Rect) -> bool {
            if let Some(w) = self.windows.get_mut(&Self::key(window)) {
                w.bounds = rect;
                true
            } else {
                false
            }
        }

        fn topmost(&self, window: Window) -> bool {
            self.windows
                .get(&Self::key(window))
                .is_some_and(|w| w.topmost)
        }

        fn place_behind(&mut self, _window: Window, _other: Window) {}

        fn set_topmost(&mut self, window: Window, topmost: bool) {
            if let Some(w) = self.windows.get_mut(&Self::key(window)) {
                w.topmost = topmost;
            }
        }

        fn close(&mut self, window: Window) {
            if let Some(w) = self.windows.get_mut(&Self::key(window)) {
                w.closed = true;
            }
        }

        fn focus(&mut self, window: Window) {
            self.foreground = Some(window);
        }

        fn snapshot(&self, window: Window) -> Option<Snapshot> {
            self.windows.get(&Self::key(window)).map(|_| Snapshot::default())
        }

        fn restore(&mut self, window: Window, _snapshot: Snapshot) {
            self.restored.push(window);
        }

        fn screen_height(&self) -> i32 {
            VIEWPORT.height
        }

        fn work_area(&self) -> std::result::Result<Rect, String> {
            Ok(VIEWPORT)
        }

        fn now(&self) -> Duration {
            self.now
        }

        fn publish(&mut self, state: BarSnapshot) {
            self.published.push(state);
        }

        fn record_diag(&mut self, kind: DiagKind, hwnd: u64, code: u32) {
            self.diags.push((kind, hwnd, code));
        }

        fn save_session(&mut self, text: String, sync: bool) {
            self.persisted_sessions.push((text, sync));
        }

        fn load_session(&self) -> session::Saved {
            self.saved_session.clone()
        }

        fn load_rules(&self) -> Rules {
            Rules::empty()
        }

        fn save_rules(&mut self, _rules: &mut Rules) {}

        fn save_setups(&mut self, _file: PathBuf, _text: String) {}

        fn launch_setup(&mut self, _commands: Vec<(String, Option<String>)>) {}

        fn set_hooks(&mut self, enabled: bool) {
            self.hooks_enabled = enabled;
        }

        fn configure_presentation(&mut self, _config: &Config) {}

        fn cancel_touchpad(&mut self) {}

        fn overview_open(&self) -> bool {
            self.overview_open
        }

        fn open_overview(&mut self) {
            self.overview_open = true;
        }

        fn close_overview(&mut self, _layout: &Workspaces<Window>, _placements: &[Placement<Window>]) {
            self.overview_open = false;
        }

        fn overview_show(&mut self, _layout: &Workspaces<Window>, _placements: &[Placement<Window>]) {}

        fn overview_target(&self, _x: i32, _y: i32) -> Option<Target> {
            None
        }

        fn overview_window(&self) -> Option<Window> {
            None
        }

        fn stage_covering(&self) -> bool {
            false
        }

        fn stage_veils(&self, _window: Window) -> bool {
            false
        }

        fn stage_shows(&self, window: Window) -> bool {
            self.stage_slides.iter().any(|slide| slide.window == window)
        }

        fn stage_is_presenting(&self) -> bool {
            self.stage_presenting
        }

        fn stage_present(
            &mut self,
            slides: &[Slide<Window>],
            _bounds: Rect,
            _rate: Option<f64>,
            hold: bool,
        ) {
            self.stage_slides = slides.to_vec();
            self.stage_presenting = hold || !slides.is_empty();
        }

        fn settle_presentation(&mut self) {
            self.stage_presenting = false;
            self.stage_slides.clear();
        }
    }

    fn make_manager(platform: FakePlatform, toml: &str) -> Manager<FakePlatform> {
        let config = Config::parse(toml).unwrap();
        let viewport = config.viewport(VIEWPORT).unwrap();
        Manager::new_with(platform, config, viewport, PathBuf::from("setups.toml"))
    }

    #[test]
    fn returning_to_a_workspace_stacks_real_floats_as_their_thumbnails_were_shown() {
        let mut layout = Workspaces::new(
            Rect {
                width: 1000,
                height: 600,
                ..Rect::default()
            },
            10,
        );
        layout.insert(1, Mode::Tiled, None);
        layout.focus_workspace(1);
        layout.insert(2, Mode::Tiled, None);
        layout.insert(5, Mode::Tiled, None);
        layout.insert(3, Mode::Floating, None);
        layout.insert(4, Mode::Floating, None);
        layout.focus(2);
        layout.focus_workspace(0);
        layout.focus_workspace(1);

        // Foreground tile, another workspace's tile, then a second tile
        // between this workspace's floats.
        let mut z_order = vec![2, 1, 3, 5, 4];
        let mut visible: Vec<_> = layout.placements().filter(|p| p.visible).collect();
        lower_occluding_tiles(&visible, &z_order.clone(), |tile, float| {
            z_order.retain(|&w| w != tile);
            let after = z_order.iter().position(|&w| w == float).unwrap() + 1;
            z_order.insert(after, tile);
        });
        assert_eq!(z_order, [1, 3, 4, 2, 5]);
        assert_eq!(layout.focused(), Some(2));
        lower_occluding_tiles(&visible, &z_order, |_, _| panic!("already stacked"));

        let focused = layout.focused();
        stack_placements(&mut visible, &layout, true, focused, &z_order);
        assert_eq!(
            visible.iter().rev().map(|p| p.window).collect::<Vec<_>>(),
            [3, 4, 2, 5]
        );
        stack_placements(&mut visible, &layout, false, focused, &z_order);
        assert_eq!(visible.last().unwrap().window, 2);
    }

    #[test]
    fn pictures_follow_the_given_order_until_a_window_is_raised() {
        let mut layout = Workspaces::new(
            Rect {
                width: 1000,
                height: 600,
                ..Rect::default()
            },
            10,
        );
        layout.insert(1, Mode::Tiled, None);
        layout.insert(2, Mode::Floating, None);
        layout.focus(1);

        // The float is on top though the tile has focus.
        let z_order = [2, 1];
        let mut placements: Vec<_> = layout.placements().collect();
        stack_placements(&mut placements, &layout, false, None, &z_order);
        assert_eq!(placements.last().unwrap().window, 2);

        stack_placements(&mut placements, &layout, false, Some(1), &z_order);
        assert_eq!(placements.last().unwrap().window, 1);
    }

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

    #[test]
    fn browser_raised_after_a_delayed_hide_keeps_its_workspace_and_focus() {
        let mut p = FakePlatform::new();
        let source = p.add(1, "mail.exe", 0);
        let browser = p.add(2, "browser.exe", 500);
        p.foreground = Some(browser);
        let mut m = make_manager(
            p,
            "[animations]\nenabled=false\n[behavior]\nlog_events=false",
        );
        let _ = m.perform(Action::Move(Direction::Down), None);
        let _ = m.perform(Action::Workspace(0), None);
        assert_eq!(m.platform.foreground, Some(source));
        assert!(m.platform.cloaked(browser));

        // Windows has already shown and activated the browser for the link.
        m.platform.windows.get_mut(&2).unwrap().cloaked = false;
        m.platform.foreground = Some(browser);
        m.handle(Event::Hidden(browser));
        m.handle(Event::Appeared(browser));
        m.handle(Event::Focused(source));

        assert_eq!(m.layout.workspace_of(browser), Some(1));
        assert_eq!(m.layout.active().0, 1);
        assert_eq!(m.layout.focused(), Some(browser));
        assert_eq!(m.platform.foreground, Some(browser));
        assert!(!m.platform.is_marked_cloaked(browser));
        assert!(m.platform.cloaked(source));
    }

    #[test]
    fn hidden_and_minimizing_windows_still_leave_the_layout() {
        let mut p = FakePlatform::new();
        let hidden = p.add(1, "mail.exe", 0);
        let minimizing = p.add(2, "browser.exe", 500);
        let mut m = make_manager(
            p,
            "[animations]\nenabled=false\n[behavior]\nlog_events=false",
        );

        m.platform.windows.get_mut(&1).unwrap().visible = false;
        m.handle(Event::Hidden(hidden));
        assert!(!m.layout.contains(hidden));

        // Windows announces minimization while the window can still be visible.
        m.handle(Event::MinimizeStarted(minimizing));
        assert!(!m.layout.contains(minimizing));
    }

    #[test]
    fn late_cloaked_event_and_windows_uncloak_trap_are_handled_safely() {
        let mut p = FakePlatform::new();
        let a = p.add(1, "a.exe", 0);
        let b = p.add(2, "b.exe", 500);
        p.foreground = Some(b);
        let mut m = make_manager(p, "[animations]\nenabled=false\n[behavior]\nlog_events=false");

        // Move B to workspace 2: A becomes cloaked on workspace 1.
        let _ = m.perform(Action::Move(Direction::Down), None);
        assert!(m.platform.cloaked(a));
        assert!(!m.platform.cloaked(b));

        // Switch back to workspace 1, then a stale Cloaked(a) arrives late.
        let _ = m.perform(Action::Workspace(0), None);
        assert!(!m.platform.cloaked(a));
        m.handle(Event::Cloaked(a));
        assert!(
            m.layout.contains(a),
            "late Cloaked event must not evict an uncloaked window"
        );
        assert!(
            m.platform
                .diags
                .iter()
                .any(|(kind, hwnd, _)| *kind == DiagKind::StaleEvent && *hwnd == 1)
        );

        // Windows spontaneously uncloaks B while A holds focus -> uncloak trap re-cloaks B.
        m.platform.windows.get_mut(&2).unwrap().cloaked = false;
        m.handle(Event::Appeared(b));
        assert!(m.platform.cloaked(b), "uncloak trap re-cloaks background window");
        assert!(
            m.platform
                .diags
                .iter()
                .any(|(kind, hwnd, _)| *kind == DiagKind::UncloakTrap && *hwnd == 2)
        );
    }

    #[test]
    fn scratchpad_stashes_cloaks_summons_centered_and_persists_live_session() {
        let mut p = FakePlatform::new();
        let a = p.add(1, "a.exe", 0);
        let b = p.add(2, "b.exe", 500);
        p.foreground = Some(b);
        let mut m = make_manager(p, "[animations]\nenabled=false\n[behavior]\nlog_events=false");

        // Mark B for scratchpad -> cloaked, A focused, session persisted.
        let _ = m.perform(Action::ScratchpadMark, None);
        assert!(m.layout.is_scratchpad(b));
        assert!(m.platform.cloaked(b));
        assert_eq!(m.layout.focused(), Some(a));
        assert_eq!(m.platform.published.last().unwrap().scratchpad, 1);
        assert!(!m.platform.published.last().unwrap().scratchpad_shown);
        assert!(
            m.platform
                .persisted_sessions
                .last()
                .unwrap()
                .0
                .contains("scratch\t0\t2\tb.exe")
        );

        // Switch to empty workspace 2 and summon B -> uncloaked, centered, focused, mode=5.
        let _ = m.perform(Action::Focus(Direction::Down), None);
        let _ = m.perform(Action::ScratchpadToggle, None);
        assert!(!m.platform.cloaked(b));
        assert_eq!(m.layout.focused(), Some(b));
        let bounds = m.platform.bounds(b);
        assert_eq!(bounds.x, (VIEWPORT.width - bounds.width) / 2);
        assert_eq!(m.platform.published.last().unwrap().mode, 5);

        // Toggle again hides and cloaks B.
        let _ = m.perform(Action::ScratchpadToggle, None);
        assert!(m.platform.cloaked(b));
    }

    #[test]
    fn pause_unhooks_restores_windows_and_resume_reconciles_live_desktop() {
        let mut p = FakePlatform::new();
        let a = p.add(1, "a.exe", 0);
        let b = p.add(2, "b.exe", 500);
        p.foreground = Some(b);
        let mut m = make_manager(p, "[animations]\nenabled=false\n[behavior]\nlog_events=false");
        let _ = m.perform(Action::Move(Direction::Down), None);
        assert!(m.platform.cloaked(a));

        // Pause: uncloaks A, restores tiled windows, unhooks WinEvents, publishes paused=true.
        let _ = m.perform(Action::TogglePause, None);
        assert!(m.paused);
        assert!(!m.platform.hooks_enabled);
        assert!(!m.platform.cloaked(a));
        assert!(m.platform.published.last().unwrap().paused);

        // Events while paused are ignored.
        let c = m.platform.add(3, "c.exe", 100);
        m.handle(Event::Appeared(c));
        assert!(!m.layout.contains(c));

        // Close B while paused, keep A and new C.
        m.platform.windows.get_mut(&2).unwrap().closed = true;

        // Resume: re-hooks, drops closed B, adopts C, re-tiles A.
        let _ = m.perform(Action::TogglePause, None);
        assert!(!m.paused);
        assert!(m.platform.hooks_enabled);
        assert!(!m.layout.contains(b));
        assert!(m.layout.contains(a));
        assert!(m.layout.contains(c));
        assert!(!m.platform.published.last().unwrap().paused);
    }

    #[test]
    fn crash_recovery_uncloaks_pulls_offstage_window_and_restores_session_and_active_workspace() {
        let mut p = FakePlatform::new();
        let a = p.add(1, "a.exe", 0);
        let b = p.add(2, "b.exe", 500);
        let c = p.add(3, "c.exe", 200);
        // Simulate crash mid-animation: A was cloaked on ws 0, B was parked off-stage on ws 1.
        p.windows.get_mut(&1).unwrap().cloaked = true;
        p.windows.get_mut(&1).unwrap().marked_cloaked = true;
        p.windows.get_mut(&2).unwrap().parked = true;
        p.windows.get_mut(&2).unwrap().bounds.y = VIEWPORT.height + 50;
        p.saved_session = session::parse(
            "# active\t1\n\
             tiled\t0\t1\ta.exe\tAppClass\t600\n\
             tiled\t1\t2\tb.exe\tAppClass\t700\n\
             scratch\t0\t3\tc.exe\tAppClass\n",
        );

        let m = make_manager(p, "[animations]\nenabled=false\n[behavior]\nlog_events=false");
        assert!(!m.platform.is_parked(b));
        assert!(
            m.platform
                .diags
                .iter()
                .any(|(kind, hwnd, _)| *kind == DiagKind::OffstageRecovered && *hwnd == 2)
        );
        assert_eq!(m.layout.active().0, 1, "active workspace restored");
        assert_eq!(m.layout.workspace_of(a), Some(0));
        assert_eq!(m.layout.workspace_of(b), Some(1));
        assert!(m.layout.is_scratchpad(c));
        assert!(m.platform.cloaked(c), "restored scratchpad starts hidden");
    }

    #[test]
    fn quiet_window_focus_steal_is_undone_within_window_and_reload_rejection_preserves_state() {
        let mut p = FakePlatform::new();
        let a = p.add(1, "a.exe", 0);
        p.foreground = Some(a);
        let mut m = make_manager(
            p,
            "workspaces=['web','chat']\n\
             [animations]\nenabled=false\n\
             [behavior]\nlog_events=false\n\
             [[window_rules]]\nexe='chat.exe'\nworkspace='chat'\nfocus=false",
        );
        let before_ws = m.layout.active().0;
        let chat = m.platform.add(2, "chat.exe", 0);
        m.handle(Event::Appeared(chat));
        assert_eq!(m.layout.workspace_of(chat), Some(1));
        assert_eq!(m.layout.active().0, before_ws);
        // Chat steals focus 1s later -> WM takes focus back to A.
        m.platform.now = Duration::from_secs(1);
        m.platform.foreground = Some(chat);
        m.handle(Event::Focused(chat));
        assert_eq!(m.platform.foreground, Some(a));
        assert_eq!(m.layout.active().0, before_ws);

        // Invalid reload is rejected, keeps existing config, and logs ReloadRejected.
        m.reload(Err("bad syntax".into()), None);
        assert_eq!(m.config.workspaces, ["web", "chat"]);
        assert!(
            m.platform
                .diags
                .iter()
                .any(|(kind, _, _)| *kind == DiagKind::ReloadRejected)
        );
    }

    #[test]
    fn swipe_scrub_presents_shifted_thumbnails_and_commits_focus() {
        let mut p = FakePlatform::new();
        let a = p.add(1, "a.exe", 0);
        let b = p.add(2, "b.exe", 500);
        p.foreground = Some(a);
        let mut m = make_manager(p, "[behavior]\nlog_events=false");
        m.layout.focus(a);
        m.apply();

        m.handle_swipe(SwipeEvent::Start(Axis::Horizontal), None);
        assert!(m.swipe_base.is_some());
        m.handle_swipe(
            SwipeEvent::Update {
                axis: Axis::Horizontal,
                offset_px: -120,
            },
            None,
        );
        assert!(!m.platform.stage_slides.is_empty());
        m.handle_swipe(
            SwipeEvent::Commit {
                direction: Direction::Right,
            },
            None,
        );
        assert!(m.swipe_base.is_none());
        assert_eq!(m.layout.focused(), Some(b));
    }
}

//! Thin OS boundary between `Manager` and Win32 so lifecycle, race, recovery,
//! scratchpad, and pause/resume behavior can be tested deterministically
//! without touching the live desktop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CYVIRTUALSCREEN};
use windows::core::Result;
use wm_bar_protocol::{DiagKind, Host as BarHost, Snapshot as BarSnapshot};

use crate::animation::Slide;
use crate::config::Config;
use crate::events::Hooks;
use crate::gestures::Touchpad;
use crate::layout::{Placement, Rect, Workspaces};
use crate::overview::{Overview, Target};
use crate::rules::Rules;
use crate::session;
use crate::setups;
use crate::stage::Stage;
use crate::window::{Info, Skip, Snapshot, Window};

pub trait Platform {
    fn visible_windows(&self) -> Vec<Window>;
    fn is_visible(&self, window: Window) -> bool;
    fn foreground(&self) -> Option<Window>;
    fn desktop(&self) -> Window;
    fn info(&self, window: Window) -> Info;
    fn skip(&self, window: Window, info: &Info) -> Option<Skip>;
    fn is_tileable(&self, window: Window) -> bool {
        let info = self.info(window);
        self.skip(window, &info).is_none()
    }
    fn exe(&self, window: Window) -> Option<String>;
    fn exe_path(&self, window: Window) -> Option<PathBuf>;
    fn class(&self, window: Window) -> String;
    fn title(&self, window: Window) -> String;
    fn bounds(&self, window: Window) -> Rect;
    fn pose(&self, window: Window, slot: Option<Rect>) -> Rect;
    fn cloaked(&self, window: Window) -> bool;
    fn is_marked_cloaked(&self, window: Window) -> bool;
    fn set_cloak(&mut self, window: Window, cloaked: bool) -> bool;
    fn uncloak_all(&mut self);
    fn mark_parked(&mut self, window: Window, parked: bool);
    fn is_parked(&self, window: Window) -> bool;
    fn place(&mut self, window: Window, rect: Rect) -> bool;
    fn topmost(&self, window: Window) -> bool;
    fn place_behind(&mut self, window: Window, other: Window);
    fn set_topmost(&mut self, window: Window, topmost: bool);
    fn close(&mut self, window: Window);
    fn focus(&mut self, window: Window);
    fn snapshot(&self, window: Window) -> Option<Snapshot>;
    fn restore(&mut self, window: Window, snapshot: Snapshot);
    fn screen_height(&self) -> i32;
    fn work_area(&self) -> std::result::Result<Rect, String>;
    fn now(&self) -> Duration;

    fn publish(&mut self, state: BarSnapshot);
    fn record_diag(&mut self, kind: DiagKind, hwnd: u64, code: u32);
    fn save_session(&mut self, text: String, sync: bool);
    fn load_session(&self) -> session::Saved;
    fn load_rules(&self) -> Rules;
    fn save_rules(&mut self, rules: &mut Rules);
    fn save_setups(&mut self, file: PathBuf, text: String);
    fn launch_setup(&mut self, commands: Vec<(String, Option<String>)>);
    fn set_hooks(&mut self, enabled: bool);
    fn configure_presentation(&mut self, config: &Config);
    fn cancel_touchpad(&mut self);
    fn overview_open(&self) -> bool;
    fn open_overview(&mut self);
    fn close_overview(&mut self, layout: &Workspaces<Window>, placements: &[Placement<Window>]);
    fn overview_show(&mut self, layout: &Workspaces<Window>, placements: &[Placement<Window>]);
    fn overview_target(&self, x: i32, y: i32) -> Option<Target>;
    fn overview_window(&self) -> Option<Window>;
    fn stage_covering(&self) -> bool;
    fn stage_veils(&self, window: Window) -> bool;
    fn stage_shows(&self, window: Window) -> bool;
    fn stage_is_presenting(&self) -> bool;
    fn stage_present(
        &mut self,
        slides: &[Slide<Window>],
        bounds: Rect,
        rate: Option<f64>,
        hold: bool,
    );
    fn settle_presentation(&mut self);
}

pub struct Win32Platform {
    pub bar: BarHost,
    pub cloaker: crate::cloak::Cloaker,
    /// Before `overview`, so that its window drops before the one it's above.
    pub stage: Stage,
    pub overview: Overview,
    pub touchpad: Touchpad,
    pub hooks: Hooks,
    session_path: Option<PathBuf>,
    started: Instant,
}

impl Win32Platform {
    pub fn new(config: &Config) -> Result<Self> {
        let overview = Overview::new(config.style.clone(), config.animations.clone())?;
        let stage = Stage::new(Some(overview.window().0))?;
        let touchpad = Touchpad::new(config.gestures.clone())?;
        let bar = BarHost::new()?;
        let cloaker = crate::cloak::Cloaker::new()?;
        let hooks = Hooks::listen();
        Ok(Self {
            bar,
            cloaker,
            stage,
            overview,
            touchpad,
            hooks,
            session_path: session::default_path(),
            started: Instant::now(),
        })
    }
}

impl Platform for Win32Platform {
    fn visible_windows(&self) -> Vec<Window> {
        Window::visible().unwrap_or_default()
    }

    fn is_visible(&self, window: Window) -> bool {
        window.is_visible()
    }

    fn foreground(&self) -> Option<Window> {
        Window::foreground()
    }

    fn desktop(&self) -> Window {
        Window::desktop()
    }

    fn info(&self, window: Window) -> Info {
        window.info()
    }

    fn skip(&self, window: Window, info: &Info) -> Option<Skip> {
        window.skip(info)
    }

    fn exe(&self, window: Window) -> Option<String> {
        window.exe()
    }

    fn exe_path(&self, window: Window) -> Option<PathBuf> {
        window.exe_path()
    }

    fn class(&self, window: Window) -> String {
        window.class()
    }

    fn title(&self, window: Window) -> String {
        window.title()
    }

    fn bounds(&self, window: Window) -> Rect {
        window.bounds()
    }

    fn pose(&self, window: Window, slot: Option<Rect>) -> Rect {
        window.pose(slot)
    }

    fn cloaked(&self, window: Window) -> bool {
        window.cloaked()
    }

    fn is_marked_cloaked(&self, window: Window) -> bool {
        crate::cloak::is_cloaked(window)
    }

    fn set_cloak(&mut self, window: Window, cloaked: bool) -> bool {
        self.cloaker.set(window, cloaked)
    }

    fn uncloak_all(&mut self) {
        self.cloaker.uncloak_all();
    }

    fn mark_parked(&mut self, window: Window, parked: bool) {
        crate::cloak::mark_parked(window, parked);
    }

    fn is_parked(&self, window: Window) -> bool {
        crate::cloak::is_parked(window)
    }

    fn place(&mut self, window: Window, rect: Rect) -> bool {
        window.place(rect)
    }

    fn topmost(&self, window: Window) -> bool {
        window.topmost()
    }

    fn place_behind(&mut self, window: Window, other: Window) {
        window.place_behind(other);
    }

    fn set_topmost(&mut self, window: Window, topmost: bool) {
        window.set_topmost(topmost);
    }

    fn close(&mut self, window: Window) {
        window.close();
    }

    fn focus(&mut self, window: Window) {
        window.focus();
    }

    fn snapshot(&self, window: Window) -> Option<Snapshot> {
        window.snapshot()
    }

    fn restore(&mut self, window: Window, snapshot: Snapshot) {
        window.restore(snapshot);
    }

    fn screen_height(&self) -> i32 {
        unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }
    }

    fn work_area(&self) -> std::result::Result<Rect, String> {
        crate::manager::work_area().map_err(|error| error.to_string())
    }

    fn now(&self) -> Duration {
        self.started.elapsed()
    }

    fn publish(&mut self, state: BarSnapshot) {
        self.bar.publish(state);
    }

    fn record_diag(&mut self, kind: DiagKind, hwnd: u64, code: u32) {
        self.bar.record_diag(kind, hwnd, code);
    }

    fn save_session(&mut self, text: String, sync: bool) {
        let Some(path) = self.session_path.clone() else {
            return;
        };
        if sync {
            session::save_sync(&path, &text);
        } else {
            session::save_async(path, text);
        }
    }

    fn load_session(&self) -> session::Saved {
        session::load()
    }

    fn load_rules(&self) -> Rules {
        Rules::load()
    }

    fn save_rules(&mut self, rules: &mut Rules) {
        rules.save();
    }

    fn save_setups(&mut self, file: PathBuf, text: String) {
        setups::save(file, text);
    }

    fn launch_setup(&mut self, commands: Vec<(String, Option<String>)>) {
        setups::launch(commands);
    }

    fn set_hooks(&mut self, enabled: bool) {
        if enabled {
            self.hooks.rehook();
        } else {
            self.hooks.unhook();
        }
    }

    fn configure_presentation(&mut self, config: &Config) {
        self.stage.finish();
        self.overview
            .configure(config.style.clone(), config.animations.clone());
        self.touchpad.configure(config.gestures.clone());
    }

    fn cancel_touchpad(&mut self) {
        self.touchpad.cancel();
    }

    fn overview_open(&self) -> bool {
        self.overview.is_open()
    }

    fn open_overview(&mut self) {
        self.overview.open();
    }

    fn close_overview(&mut self, layout: &Workspaces<Window>, placements: &[Placement<Window>]) {
        self.overview.close(layout, placements, &mut self.stage);
    }

    fn overview_show(&mut self, layout: &Workspaces<Window>, placements: &[Placement<Window>]) {
        self.overview.show(layout, placements, &mut self.stage);
    }

    fn overview_target(&self, x: i32, y: i32) -> Option<Target> {
        self.overview.target(x, y)
    }

    fn overview_window(&self) -> Option<Window> {
        Some(self.overview.window())
    }

    fn stage_covering(&self) -> bool {
        self.stage.covering()
    }

    fn stage_veils(&self, window: Window) -> bool {
        self.stage.veils(window)
    }

    fn stage_shows(&self, window: Window) -> bool {
        self.stage.shows(window)
    }

    fn stage_is_presenting(&self) -> bool {
        self.stage.is_presenting()
    }

    fn stage_present(
        &mut self,
        slides: &[Slide<Window>],
        bounds: Rect,
        rate: Option<f64>,
        hold: bool,
    ) {
        self.stage.present(slides, bounds, rate, hold);
    }

    fn settle_presentation(&mut self) {
        self.stage.finish();
        self.overview.finish();
    }
}

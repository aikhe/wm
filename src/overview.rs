//! A zoomed-out view of every workspace, stacked top to bottom, with a live
//! picture of each window where it sits in its strip.
//!
//! The view's own window is only the backdrop: the rows and their numbers.
//! The pictures are the stage's, which sits above it, and so is the highlight
//! round the focused one, which has to move with its picture and show above
//! the ones beside it. Opening is the desktop's windows shrinking into their
//! rows while the backdrop fades in behind them, and closing is the reverse.
//! Because hidden windows are cloaked rather than minimized their pictures
//! stay live, and showing the view never waits on another app.
//!
//! It keeps nothing of its own beyond what it last drew. The highlighted
//! window is the layout's focused one, so every hotkey works on the view as
//! it does on the desktop.

use windows::Win32::Foundation::{
    COLORREF, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT,
    WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, BeginPaint, DC_BRUSH, DEFAULT_GUI_FONT, EndPaint, FillRect, GetStockObject,
    HBRUSH, InvalidateRect, PAINTSTRUCT, SelectObject, SetBkMode, SetDCBrushColor, SetTextColor,
    TRANSPARENT, TextOutW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, IDC_ARROW, LWA_ALPHA,
    LoadCursorW, MSG, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOW,
    SetLayeredWindowAttributes, ShowWindow, WNDCLASSW, WS_EX_LAYERED, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{Error, PCWSTR, Result, w};

use crate::animation::{Animations, Slide, Spring};
use crate::config::OverviewStyle;
use crate::layout::{Placement, Rect, Workspaces};
use crate::stage::{Ring, Stage};
use crate::window::Window;

const CLASS: PCWSTR = w!("wm.overview");

/// Fits every workspace on the screen as a row: a scaled-down copy of the
/// viewport, centred, with the rows stacked from the top.
struct Map {
    viewport: Rect,
    /// Where the top workspace's viewport is drawn.
    first: Rect,
    /// The distance from the top of one row to the top of the next.
    pitch: i32,
}

impl Map {
    fn new(screen: Rect, viewport: Rect, rows: usize, style: &OverviewStyle) -> Self {
        let rows = rows.max(1) as i32;
        let scale = |fraction: (i32, i32)| {
            (i64::from(screen.height) * i64::from(fraction.0) / i64::from(fraction.1)) as i32
        };
        let margin = scale(style.margin);
        // Keep dense overviews usable rather than producing negative rectangles.
        let gap = scale(style.row_gap).min((screen.height - 2 * margin) / rows / 2);
        let available = screen.height - 2 * margin - gap * (rows - 1);
        // A third of the screen at most, however few rows there are, so the
        // columns to either side of the viewport have room to show.
        let height = (available / rows).min(scale(style.max_row_height)).max(1);
        let width = viewport.width * height / viewport.height;
        let used = height * rows + gap * (rows - 1);

        Map {
            viewport,
            first: Rect {
                x: screen.x + (screen.width - width) / 2,
                y: screen.y + (screen.height - used) / 2,
                width,
                height,
            },
            pitch: height + gap,
        }
    }

    /// Where the workspace's viewport is drawn.
    fn row(&self, workspace: usize) -> Rect {
        Rect {
            y: self.first.y + self.pitch * workspace as i32,
            ..self.first
        }
    }

    /// Where a window at `rect` on the real screen is drawn in the
    /// workspace's row.
    fn card(&self, workspace: usize, rect: Rect) -> Rect {
        let row = self.row(workspace);
        let scaled = |length: i32| length * row.height / self.viewport.height;
        Rect {
            x: row.x + scaled(rect.x - self.viewport.x),
            y: row.y + scaled(rect.y - self.viewport.y),
            width: scaled(rect.width),
            height: scaled(rect.height),
        }
    }
}

/// What the view last drew, which clicks are matched against.
#[derive(Default, PartialEq)]
struct Scene {
    /// Each workspace's viewport, from the top.
    rows: Vec<Rect>,
    active: usize,
    /// Where each window's picture comes to rest, bottom-most first.
    cards: Vec<(Window, Rect)>,
    /// The cards of windows that Windows gave no picture of.
    blank: Vec<Rect>,
}

/// What a click in the view landed on.
pub enum Target {
    Window(Window),
    Workspace(usize),
}

pub struct Overview {
    window: Window,
    screen: Rect,
    open: bool,
    /// Opened, but not yet on screen with the keyboard: `show` does that.
    opening: bool,
    scene: Scene,
    style: OverviewStyle,
    animations: Animations,
    /// How opaque the backdrop is, from 0 to 255. It hides once faded out.
    fade: Spring,
}

impl Overview {
    /// Creates the view's window, which stays hidden until `open`.
    pub fn new(style: OverviewStyle, animations: Animations) -> Result<Self> {
        let screen = Rect {
            x: 0,
            y: 0,
            width: unsafe { GetSystemMetrics(SM_CXSCREEN) },
            height: unsafe { GetSystemMetrics(SM_CYSCREEN) },
        };
        let hwnd = unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            let class = WNDCLASSW {
                lpfnWndProc: Some(default_behaviour),
                hInstance: instance,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                lpszClassName: CLASS,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(Error::from_thread());
            }
            // A tool window, so it stays out of the taskbar and Alt+Tab, and
            // layered so that it can fade.
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
                CLASS,
                w!("wm overview"),
                WS_POPUP,
                screen.x,
                screen.y,
                screen.width,
                screen.height,
                None,
                None,
                Some(instance),
                None,
            )?
        };

        Window(hwnd).disable_native_transitions();
        Ok(Overview {
            window: Window(hwnd),
            screen,
            open: false,
            opening: false,
            scene: Scene::default(),
            style,
            animations,
            fade: Spring::at(0.0),
        })
    }

    /// The view's window, which the stage is kept above.
    pub fn window(&self) -> Window {
        self.window
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn configure(&mut self, style: OverviewStyle, animations: Animations) {
        self.finish();
        self.style = style;
        self.animations = animations;
        let _ = unsafe { InvalidateRect(Some(self.window.0), None, false) };
    }

    /// Whether the message was meant for the view's window.
    pub fn owns(&self, message: &MSG) -> bool {
        message.hwnd == self.window.0
    }

    /// Opens the view. It comes on screen and takes the keyboard when `show`
    /// is next called.
    pub fn open(&mut self) {
        self.open = true;
        self.opening = true;
    }

    /// Sends every window's picture to its place in the view, the highlight
    /// with the focused one, and repaints the backdrop if it changed. The
    /// `placements` are the layout's, bottom-most picture first. Does nothing
    /// while the view is closed.
    pub fn show(
        &mut self,
        layout: &Workspaces<Window>,
        placements: &[Placement<Window>],
        stage: &mut Stage,
    ) {
        if !self.open {
            return;
        }
        let (active, count) = layout.active();
        let map = Map::new(self.screen, layout.viewport(), count, &self.style);
        let slides: Vec<_> = placements
            .iter()
            .map(|placement| {
                let window = placement.window;
                let rect = placement.rect.unwrap_or_else(|| window.bounds());
                Slide {
                    window,
                    from: self.desktop(active, placement),
                    to: map.card(placement.workspace, rect),
                }
            })
            .collect();
        stage.highlight(layout.focused().map(|window| Ring {
            window,
            colour: self.style.highlight,
            width: self.style.highlight_width,
        }));
        stage.present(&slides, self.screen, self.rate(), true);

        let scene = Scene {
            rows: (0..count).map(|workspace| map.row(workspace)).collect(),
            active,
            cards: slides
                .iter()
                .map(|slide| (slide.window, slide.to))
                .collect(),
            blank: slides
                .iter()
                .filter(|slide| !stage.shows(slide.window))
                .map(|slide| slide.to)
                .collect(),
        };
        if scene != self.scene {
            self.scene = scene;
            let _ = unsafe { InvalidateRect(Some(self.window.0), None, false) };
        }
        if self.opening {
            self.opening = false;
            self.fade.aim(255.0);
            if self.rate().is_none() {
                self.fade.finish();
            }
            self.blend();
            let _ = unsafe { ShowWindow(self.window.0, SW_SHOW) };
            self.window.focus();
        }
    }

    /// Zooms back out to the desktop: the pictures return to where the real
    /// windows are and the backdrop fades away. Give another window the
    /// keyboard first, or Windows picks one itself. The `placements` are
    /// stacked as for `show`.
    pub fn close(
        &mut self,
        layout: &Workspaces<Window>,
        placements: &[Placement<Window>],
        stage: &mut Stage,
    ) {
        self.open = false;
        self.opening = false;
        let active = layout.active().0;
        let slides: Vec<_> = placements
            .iter()
            .map(|placement| {
                let rect = self.desktop(active, placement);
                Slide {
                    window: placement.window,
                    from: rect,
                    to: rect,
                }
            })
            .collect();
        stage.highlight(None);
        stage.present(&slides, self.screen, self.rate(), false);
        self.fade.aim(0.0);
        if self.rate().is_none() {
            self.finish();
        }
    }

    /// Where the window's picture is on the desktop, seen from the view: the
    /// workspaces are stacked a screen apart, so the ones that aren't on
    /// screen lie above and below it.
    fn desktop(&self, active: usize, placement: &Placement<Window>) -> Rect {
        let rect = placement.window.pose(placement.rect);
        let below = placement.workspace as i32 - active as i32;
        Rect {
            y: rect.y + below * self.screen.height,
            ..rect
        }
    }

    fn rate(&self) -> Option<f64> {
        self.animations.rate(self.animations.overview)
    }

    pub fn animating(&self) -> bool {
        !self.fade.settled()
    }

    /// Advances the backdrop's fade by `seconds`.
    pub fn tick(&mut self, seconds: f64) {
        if self.fade.settled() {
            return;
        }
        match self.rate() {
            Some(rate) => self.fade.step(rate, seconds),
            None => self.fade.finish(),
        }
        self.blend();
        self.hide_when_faded();
    }

    /// Ends the fade now.
    pub fn finish(&mut self) {
        self.fade.finish();
        self.blend();
        self.hide_when_faded();
    }

    fn blend(&self) {
        let alpha = self.fade.value().clamp(0, 255) as u8;
        let _ = unsafe { SetLayeredWindowAttributes(self.window.0, COLORREF(0), alpha, LWA_ALPHA) };
    }

    fn hide_when_faded(&mut self) {
        if !self.open && self.fade.settled() {
            self.scene = Scene::default();
            let _ = unsafe { ShowWindow(self.window.0, SW_HIDE) };
        }
    }

    /// Paints the backdrop: the rows, their numbers, and a plain card for
    /// each window there's no picture of. Call it when Windows asks the view
    /// to paint.
    pub fn paint(&self) {
        unsafe {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(self.window.0, &mut paint);
            let brush = HBRUSH(GetStockObject(DC_BRUSH).0);
            let fill = |rect: Rect, colour: COLORREF| {
                SetDCBrushColor(dc, colour);
                FillRect(dc, &RECT::from(rect), brush);
            };

            SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
            SetBkMode(dc, TRANSPARENT);
            fill(self.screen, self.style.background);
            SetTextColor(dc, self.style.highlight);
            for (workspace, row) in self.scene.rows.iter().enumerate() {
                let active = workspace == self.scene.active;
                fill(
                    *row,
                    if active {
                        self.style.active_workspace
                    } else {
                        self.style.workspace
                    },
                );
                let number: Vec<u16> = (workspace + 1).to_string().encode_utf16().collect();
                let _ = TextOutW(dc, row.x - row.height / 8, row.y, &number);
            }
            for rect in &self.scene.blank {
                fill(*rect, self.style.card);
            }

            let _ = EndPaint(self.window.0, &paint);
        }
    }

    /// What is drawn at a point in the view, the window on top winning.
    /// Anywhere level with a workspace's row counts as that workspace.
    pub fn target(&self, x: i32, y: i32) -> Option<Target> {
        let mut cards = self.scene.cards.iter().rev();
        let level = |row: &Rect| (row.y..row.y + row.height).contains(&y);
        cards
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(window, _)| Target::Window(*window))
            .or_else(|| {
                self.scene
                    .rows
                    .iter()
                    .position(level)
                    .map(Target::Workspace)
            })
    }
}

impl Drop for Overview {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.window.0) };
    }
}

/// The view's window procedure. The messages the view acts on are queued
/// ones, which the manager's loop hands to it directly, so every message that
/// reaches the window itself only needs the default treatment.
unsafe extern "system" fn default_behaviour(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_are_drawn_as_scaled_rows_with_windows_in_place() {
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1200,
            height: 900,
        };
        let map = Map::new(
            screen,
            screen,
            2,
            &crate::config::Config::parse("").unwrap().style,
        );
        let row = |y| Rect {
            x: 400,
            y,
            width: 400,
            height: 300,
        };
        assert_eq!([map.row(0), map.row(1)], [row(139), row(461)]);

        let column = |x| Rect {
            x,
            y: 0,
            width: 600,
            height: 900,
        };
        let card = |x| Rect {
            x,
            y: 461,
            width: 200,
            height: 300,
        };
        assert_eq!(map.card(1, column(600)), card(600));
        assert_eq!(
            map.card(1, column(-600)),
            card(200),
            "a column scrolled off the screen sits beside the viewport"
        );
    }
}

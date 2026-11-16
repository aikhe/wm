//! A zoomed-out view of every workspace, stacked top to bottom, with a live
//! picture of each window where it sits in its strip.
//!
//! The view is one full-screen window of ours. Windows draws the pictures
//! into it (DWM thumbnails, as Alt+Tab uses), so they stay live at no cost to
//! us, and because hidden windows are cloaked rather than minimized they keep
//! updating too. Showing the view never waits on another app.
//!
//! It keeps nothing of its own beyond what it last drew. The highlighted
//! window is the layout's focused one, so every hotkey works on the view as
//! it does on the desktop.

use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE, DwmRegisterThumbnail,
    DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
};
use windows::Win32::Graphics::Gdi::{
    BLACK_BRUSH, BeginPaint, DC_BRUSH, DEFAULT_GUI_FONT, EndPaint, FillRect, GetStockObject,
    HBRUSH, InvalidateRect, PAINTSTRUCT, SelectObject, SetBkMode, SetDCBrushColor, SetTextColor,
    TRANSPARENT, TextOutW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetSystemMetrics, IDC_ARROW, LoadCursorW, MSG, RegisterClassW,
    SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOW, ShowWindow, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{Error, PCWSTR, Result, w};

use crate::config::OverviewStyle;
use crate::layout::{Rect, Workspaces};
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

/// A window as drawn in the view.
struct Card {
    window: Window,
    rect: Rect,
    /// The live picture Windows draws over `rect`, if it would give us one.
    thumbnail: Option<isize>,
}

/// What the view last drew, which clicks are matched against.
#[derive(Default)]
struct Scene {
    /// Each workspace's viewport, from the top.
    rows: Vec<Rect>,
    active: usize,
    /// Bottom-most first.
    cards: Vec<Card>,
    focused: Option<Window>,
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
    scene: Scene,
    style: OverviewStyle,
}

impl Overview {
    /// Creates the view's window, which stays hidden until `open`.
    pub fn new(style: OverviewStyle) -> Result<Self> {
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
            if RegisterClassW(&class) == 0 {
                return Err(Error::from_thread());
            }
            // A tool window, so it stays out of the taskbar and Alt+Tab.
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
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

        Ok(Overview {
            window: Window(hwnd),
            screen,
            open: false,
            scene: Scene::default(),
            style,
        })
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn configure(&mut self, style: OverviewStyle) {
        self.style = style;
    }

    /// Whether the message was meant for the view's window.
    pub fn owns(&self, message: &MSG) -> bool {
        message.hwnd == self.window.0
    }

    /// Puts the view on screen and gives it the keyboard. It's blank until
    /// `show` is next called.
    pub fn open(&mut self) {
        self.open = true;
        let _ = unsafe { ShowWindow(self.window.0, SW_SHOW) };
        self.window.focus();
    }

    /// Takes the view off screen. Give another window the keyboard first, or
    /// Windows picks one itself.
    pub fn close(&mut self) {
        self.open = false;
        self.drop_thumbnails();
        self.scene = Scene::default();
        let _ = unsafe { ShowWindow(self.window.0, SW_HIDE) };
    }

    /// Redraws the view from the layout. Does nothing while it's closed.
    pub fn show(&mut self, layout: &Workspaces<Window>) {
        if !self.open {
            return;
        }
        let (active, count) = layout.active();
        let focused = layout.focused();
        let map = Map::new(self.screen, layout.viewport(), count, &self.style);
        let drawn: Vec<_> = layout
            .placements()
            .map(|placement| {
                let window = placement.window;
                let rect = placement.rect.unwrap_or_else(|| window.bounds());
                (window, map.card(placement.workspace, rect))
            })
            .collect();

        // Thumbnails stack in the order they were made, so they are only
        // kept while the windows and their order are unchanged.
        let before = self.scene.cards.iter().map(|card| card.window);
        let unchanged = before.eq(drawn.iter().map(|(window, _)| *window));
        let thumbnails: Vec<_> = if unchanged {
            let kept = self.scene.cards.iter();
            kept.map(|card| card.thumbnail).collect()
        } else {
            self.drop_thumbnails();
            let register =
                |window: Window| unsafe { DwmRegisterThumbnail(self.window.0, window.0) };
            let made = drawn.iter().map(|(window, _)| register(*window).ok());
            made.collect()
        };

        let cards: Vec<_> = drawn
            .into_iter()
            .zip(thumbnails)
            .map(|((window, rect), thumbnail)| Card {
                window,
                rect,
                thumbnail,
            })
            .collect();
        for card in &cards {
            // The focused window's picture is drawn smaller, so the
            // highlight painted behind it shows all the way round.
            let rect = if Some(card.window) == focused {
                card.rect.inset(
                    self.style
                        .highlight_width
                        .min((card.rect.width.min(card.rect.height) - 1).max(0) / 2),
                )
            } else {
                card.rect
            };
            let properties = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE,
                rcDestination: rect.into(),
                fVisible: true.into(),
                ..Default::default()
            };
            if let Some(thumbnail) = card.thumbnail {
                // Fails only if the window is gone, and an event will tell us.
                let _ = unsafe { DwmUpdateThumbnailProperties(thumbnail, &properties) };
            }
        }

        self.scene = Scene {
            rows: (0..count).map(|workspace| map.row(workspace)).collect(),
            active,
            cards,
            focused,
        };
        let _ = unsafe { InvalidateRect(Some(self.window.0), None, true) };
    }

    /// Paints what the thumbnails don't cover: the rows, their numbers, and
    /// the highlight. Call it when Windows asks the view to paint.
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
            for card in &self.scene.cards {
                let focused = Some(card.window) == self.scene.focused;
                fill(
                    card.rect,
                    if focused {
                        self.style.highlight
                    } else {
                        self.style.card
                    },
                );
            }

            let _ = EndPaint(self.window.0, &paint);
        }
    }

    /// What was drawn at a point in the view, the window on top winning.
    /// Anywhere level with a workspace's row counts as that workspace.
    pub fn target(&self, x: i32, y: i32) -> Option<Target> {
        let mut cards = self.scene.cards.iter().rev();
        let level = |row: &Rect| (row.y..row.y + row.height).contains(&y);
        cards
            .find(|card| card.rect.contains(x, y))
            .map(|card| Target::Window(card.window))
            .or_else(|| {
                self.scene
                    .rows
                    .iter()
                    .position(level)
                    .map(Target::Workspace)
            })
    }

    fn drop_thumbnails(&mut self) {
        for card in &mut self.scene.cards {
            if let Some(thumbnail) = card.thumbnail.take() {
                let _ = unsafe { DwmUnregisterThumbnail(thumbnail) };
            }
        }
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

    /// Needs a desktop session with an app window open, so it doesn't run by
    /// default. Nothing shows on screen: the view stays hidden.
    #[test]
    #[ignore]
    fn windows_gives_the_view_a_picture_of_another_apps_window() {
        let overview = Overview::new(crate::config::Config::parse("").unwrap().style)
            .expect("the view's window is created");
        let open = Window::visible().expect("windows can be listed");
        let window = open
            .into_iter()
            .find(|window| window.is_tileable())
            .expect("an app window");

        let thumbnail = unsafe { DwmRegisterThumbnail(overview.window.0, window.0) };
        assert!(thumbnail.is_ok(), "{thumbnail:?}");
    }
}

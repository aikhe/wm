//! Where windows are shown while they move: a see-through surface above the
//! desktop holding a live picture of each one (DWM thumbnails).
//!
//! Other apps' windows can't be moved smoothly, so their pictures move
//! instead. The pictures first appear exactly over the real windows, the real
//! ones are then cloaked and put in their final places at once, and when the
//! pictures come to rest the real ones are shown beneath them before the
//! pictures go. Two copies of a window in one spot look like one, so neither
//! swap can be seen, and nothing but the windows is ever drawn: the wallpaper
//! stays as it is and clicks pass through.
//!
//! The one thing drawn besides is the ring round a chosen picture, the
//! overview's highlight. It has to lie above the pictures and move with one,
//! so it's made of pictures too: four of a small window filled with its
//! colour, stretched along the edges.

use std::ffi::c_void;

use windows::Win32::Foundation::{
    COLORREF, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT,
    SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, BeginPaint,
    CreateCompatibleDC, CreateDIBSection, DC_BRUSH, DIB_RGB_COLORS, DeleteDC, DeleteObject,
    EndPaint, FillRect, GetStockObject, HBRUSH, InvalidateRect, PAINTSTRUCT, SelectObject,
    SetDCBrushColor, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, HWND_TOPMOST,
    MSG, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WINDOW_EX_STYLE, WM_ERASEBKGND,
    WM_PAINT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{BOOL, Error, PCWSTR, Result, w};

use crate::animation::{Motion, Slide};
use crate::layout::{Direction, Rect};
use crate::thumbnail::Thumbnail;
use crate::window::Window;

/// A window as one arrangement of the desktop has it.
#[derive(Clone, Copy)]
pub struct SceneWindow<W> {
    pub window: W,
    pub rect: Rect,
    pub visible: bool,
    /// On the workspace that's on screen, in view or scrolled out of it.
    pub active: bool,
}

/// The slides that turn one arrangement of the desktop into the next, in
/// `after`'s order. With `vertical`, workspaces pass like pages one viewport
/// apart and the windows on both stay put. Otherwise the columns of the strip
/// scroll, and one coming into view starts where it sat off screen.
pub fn slides<W: Copy + Eq>(
    before: &[SceneWindow<W>],
    after: &[SceneWindow<W>],
    vertical: Option<Direction>,
    viewport: Rect,
) -> Vec<Slide<W>> {
    let shift = match vertical {
        Some(Direction::Up) => -viewport.height,
        Some(Direction::Down) => viewport.height,
        _ => 0,
    };
    let lowered = |rect: Rect, by| Rect {
        y: rect.y + by,
        ..rect
    };
    // A wrap or a long scroll carries columns through the viewport that
    // aren't in view at either end.
    let passes = |old: Rect, new: Rect| {
        old.x.min(new.x) < viewport.x + viewport.width
            && (old.x + old.width).max(new.x + new.width) > viewport.x
    };
    let gone = before
        .iter()
        .filter(|old| !after.iter().any(|new| new.window == old.window));
    let windows = after.iter().chain(gone).map(|item| item.window);

    windows
        .filter_map(|window| {
            let old = before.iter().find(|item| item.window == window);
            let new = after.iter().find(|item| item.window == window);
            let shown = |item: Option<&SceneWindow<W>>| {
                item.filter(|item| item.visible).map(|item| item.rect)
            };
            let scrolled = old
                .zip(new)
                .filter(|(old, new)| shift == 0 && old.active && new.active)
                .map(|(old, new)| (old.rect, new.rect))
                .filter(|(old, new)| passes(*old, *new));
            let (from, to) = match (scrolled, shown(old), shown(new)) {
                (Some(ends), _, _) => ends,
                (None, Some(old), Some(new)) => (old, new),
                (None, None, Some(new)) => (lowered(new, shift), new),
                (None, Some(old), None) if shift != 0 => (old, lowered(old, -shift)),
                _ => return None,
            };
            Some(Slide { window, from, to })
        })
        .collect()
}

enum Phase {
    Idle,
    /// The pictures are submitted over the real windows, which stay as they
    /// are until the compositor has `shown` them.
    Cover {
        shown: bool,
    },
    /// The pictures stand in for the real windows, which are cloaked.
    Live,
    /// The real windows are coming back. The pictures stay over them for
    /// `frames` more once they've all landed, until the compositor is certain
    /// to be showing both, and wait at most `patience` frames for that.
    Uncover {
        frames: u8,
        patience: u8,
    },
}

/// The start of the last phase: two frames, after waiting up to half a
/// second for a window whose app is slow to move it back.
const UNCOVER: Phase = Phase::Uncover {
    frames: 2,
    patience: 30,
};

/// A coloured border drawn round one window's picture, above all of them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ring {
    pub window: Window,
    pub colour: COLORREF,
    pub width: i32,
}

/// The side, in pixels, of the window the ring's colour is taken from.
/// Windows gives an empty picture of a window much smaller.
const SWATCH: i32 = 64;

pub struct Stage {
    window: Window,
    /// Filled with the ring's colour and never seen itself.
    swatch: Window,
    ring: Option<Ring>,
    /// Pictures of `swatch`, one for each side of the ring.
    edges: Vec<Thumbnail>,
    /// The part of the screen the surface covers, which clips the pictures.
    bounds: Rect,
    motion: Motion<Window>,
    /// One for each picture in `motion`, in the same order. Windows stacks
    /// them in the order they were made.
    thumbnails: Vec<Thumbnail>,
    rate: f64,
    phase: Phase,
    /// Stay up once the pictures come to rest, as the overview's cards do.
    hold: bool,
}

impl Stage {
    /// Creates the surface, hidden. It stays above `owner`.
    pub fn new(owner: Option<HWND>) -> Result<Self> {
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
        let make = |class: PCWSTR, procedure, style, size, owner| unsafe {
            let registered = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                lpszClassName: class,
                ..Default::default()
            };
            if RegisterClassW(&registered) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(Error::from_thread());
            }
            CreateWindowExW(
                style | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class,
                class,
                WS_POPUP,
                0,
                0,
                size,
                size,
                owner,
                None,
                Some(instance),
                None,
            )
        };
        // Layered and transparent together let every click through, and a
        // tool window stays out of the taskbar and Alt+Tab.
        let surface = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST;
        let window = Window(make(w!("wm.stage"), default_behaviour, surface, 1, owner)?);
        window.disable_native_transitions();
        // Windows gives no picture of a layered window's content, so the
        // swatch is an ordinary one that paints itself.
        let plain = WINDOW_EX_STYLE::default();
        let swatch =
            make(w!("wm.swatch"), paint_swatch, plain, SWATCH, None).inspect_err(|_| {
                let _ = unsafe { DestroyWindow(window.0) };
            })?;
        let stage = Stage {
            window,
            swatch: Window(swatch),
            ring: None,
            edges: Vec::new(),
            bounds: Rect::default(),
            motion: Motion::default(),
            thumbnails: Vec::new(),
            rate: 1.0,
            phase: Phase::Idle,
            hold: false,
        };
        // After `stage` exists, so the windows are destroyed if this fails.
        clear_surface(window.0)?;
        // Windows only keeps a picture of a window that's shown, so the
        // swatch is, but cloaked: pictures are all it's for.
        let cloak = BOOL::from(true);
        unsafe {
            DwmSetWindowAttribute(
                swatch,
                DWMWA_CLOAK,
                (&raw const cloak).cast::<c_void>(),
                size_of::<BOOL>() as u32,
            )?;
            let _ = ShowWindow(swatch, SW_SHOWNOACTIVATE);
        }
        Ok(stage)
    }

    /// Draws `ring` round its window's picture from the next `present` on,
    /// or no ring with `None`.
    pub fn highlight(&mut self, ring: Option<Ring>) {
        let colour = |ring: Option<Ring>| ring.map(|ring| ring.colour);
        if let Some(COLORREF(new)) = colour(ring).filter(|new| Some(*new) != colour(self.ring)) {
            let swatch = self.swatch.0;
            // Our own window, so painting it now waits on nothing.
            unsafe {
                SetWindowLongPtrW(swatch, GWLP_USERDATA, new as isize);
                let _ = InvalidateRect(Some(swatch), None, false);
                let _ = UpdateWindow(swatch);
            }
        }
        self.ring = ring;
    }

    /// Whether the window has a picture on the stage.
    pub fn shows(&self, window: Window) -> bool {
        self.motion.contains(window)
    }

    /// Whether the message was meant for one of the stage's windows.
    pub fn owns(&self, message: &MSG) -> bool {
        message.hwnd == self.window.0 || message.hwnd == self.swatch.0
    }

    /// Shows the slides' pictures over `bounds` and sends them on their way
    /// at `rate`, or straight to their ends without one. With `hold` they
    /// stay up when they get there; otherwise the real windows take over.
    pub fn present(
        &mut self,
        slides: &[Slide<Window>],
        bounds: Rect,
        rate: Option<f64>,
        hold: bool,
    ) {
        self.hold = hold;
        self.motion.retarget(slides);
        match rate {
            Some(rate) => self.rate = rate,
            None => self.motion.finish(),
        }
        let idle = matches!(self.phase, Phase::Idle);
        if idle || bounds != self.bounds {
            self.bounds = bounds;
            let Rect {
                x,
                y,
                width,
                height,
            } = bounds;
            let _ = unsafe {
                SetWindowPos(
                    self.window.0,
                    Some(HWND_TOPMOST),
                    x,
                    y,
                    width,
                    height,
                    SWP_NOACTIVATE,
                )
            };
        }

        // A new set of pictures is drawn before the old one goes, so there's
        // never a frame with neither.
        let replaced = self.register();
        self.draw();
        drop(replaced);
        if self.motion.is_empty() {
            self.clear();
            return;
        }
        match self.phase {
            Phase::Idle => {
                let _ = unsafe { ShowWindow(self.window.0, SW_SHOWNOACTIVATE) };
                self.phase = Phase::Cover { shown: false };
            }
            // The pictures are still over the real windows.
            Phase::Uncover { .. } => self.phase = Phase::Live,
            Phase::Cover { .. } | Phase::Live => {}
        }
    }

    /// Makes `thumbnails` match the pictures in `motion`. Ones that are kept
    /// can't be reordered, so when the order has changed they're all made
    /// again, and the old ones are returned to be dropped once the new are
    /// drawn. A window Windows won't give a picture of leaves the motion.
    fn register(&mut self) -> Vec<Thumbnail> {
        let wanted: Vec<_> = self.motion.frames().map(|(window, _)| window).collect();
        self.thumbnails
            .retain(|thumbnail| wanted.contains(&thumbnail.window));
        let kept = self.thumbnails.iter().map(|thumbnail| thumbnail.window);
        let mut replaced = if kept.eq(wanted.iter().copied().take(self.thumbnails.len())) {
            Vec::new()
        } else {
            std::mem::take(&mut self.thumbnails)
        };
        let before = self.thumbnails.len();
        for window in &wanted[before..] {
            match Thumbnail::new(self.window.0, *window) {
                Ok(thumbnail) => self.thumbnails.push(thumbnail),
                Err(_) => self.motion.remove(*window),
            }
        }
        // The ring lies above every picture, so it's made again after any.
        if self.ring.is_none() || self.thumbnails.len() > before {
            replaced.append(&mut self.edges);
        }
        if self.ring.is_some() && self.edges.is_empty() {
            let edge = |_| Thumbnail::new(self.window.0, self.swatch).ok();
            self.edges = (0..4).filter_map(edge).collect();
        }
        replaced
    }

    fn draw(&self) {
        let clip = Rect {
            x: 0,
            y: 0,
            ..self.bounds
        };
        let mut sides = [Rect::default(); 4];
        for (window, rect) in self.motion.frames() {
            let mut made = self.thumbnails.iter();
            let Some(thumbnail) = made.find(|made| made.window == window) else {
                continue;
            };
            let rect = Rect {
                x: rect.x - self.bounds.x,
                y: rect.y - self.bounds.y,
                ..rect
            };
            // Fails only once the window is gone, and an event says so.
            let _ = thumbnail.draw(rect, clip);
            if let Some(ring) = self.ring.filter(|ring| ring.window == window) {
                sides = border(rect, ring.width);
            }
        }
        // An empty side hides its picture, as all four are without a ring.
        for (edge, side) in self.edges.iter().zip(sides) {
            let _ = edge.draw(side, clip);
        }
    }

    /// Tells the surface that the compositor has presented a frame.
    pub fn composed(&mut self) {
        if let Phase::Cover { shown } = &mut self.phase {
            *shown = true;
        }
    }

    /// Returns `true` once, when the pictures are on screen over the real
    /// windows. Cloak and place the real ones before anything else moves.
    pub fn take_cover(&mut self) -> bool {
        let covered = matches!(self.phase, Phase::Cover { shown: true });
        if covered {
            self.phase = Phase::Live;
        }
        covered
    }

    /// Advances the pictures by `seconds`. Returns `true` when they have
    /// come to rest and the real windows should be shown again. The pictures
    /// then stay up until every real window has `landed` beneath its own.
    pub fn step(&mut self, seconds: f64, landed: impl FnOnce() -> bool) -> bool {
        match self.phase {
            Phase::Live => {
                self.motion.step(self.rate, seconds);
                self.thumbnails
                    .retain(|thumbnail| self.motion.contains(thumbnail.window));
                self.draw();
                let done = self.motion.settled() && !self.hold;
                if done {
                    self.phase = UNCOVER;
                }
                done
            }
            Phase::Uncover { frames, patience } => {
                if patience > 0 && !landed() {
                    let patience = patience - 1;
                    self.phase = Phase::Uncover { frames, patience };
                } else if frames > 0 {
                    self.phase = Phase::Uncover {
                        frames: frames - 1,
                        patience: 0,
                    };
                } else {
                    self.clear();
                }
                false
            }
            Phase::Idle | Phase::Cover { .. } => false,
        }
    }

    /// Puts every picture where it was heading, now. Unless they're held,
    /// show the real windows again straight after.
    pub fn finish(&mut self) {
        match self.phase {
            // The real windows were never hidden.
            Phase::Cover { .. } if !self.hold => self.clear(),
            Phase::Cover { .. } | Phase::Live => {
                self.motion.finish();
                self.thumbnails
                    .retain(|thumbnail| self.motion.contains(thumbnail.window));
                self.draw();
                if !self.hold && matches!(self.phase, Phase::Live) {
                    self.phase = UNCOVER;
                }
            }
            Phase::Idle | Phase::Uncover { .. } => {}
        }
    }

    /// Whether there are frames still to make.
    pub fn animating(&self) -> bool {
        match self.phase {
            Phase::Idle => false,
            Phase::Live => !(self.hold && self.motion.settled()),
            Phase::Cover { .. } | Phase::Uncover { .. } => true,
        }
    }

    pub fn is_presenting(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// Whether the real windows must be left alone for now: the pictures
    /// aren't on screen yet to hide a change behind.
    pub fn covering(&self) -> bool {
        matches!(self.phase, Phase::Cover { .. })
    }

    /// Whether the window's picture is standing in for it, so the real one
    /// should be cloaked.
    pub fn veils(&self, window: Window) -> bool {
        matches!(self.phase, Phase::Live) && self.motion.contains(window)
    }

    fn clear(&mut self) {
        self.phase = Phase::Idle;
        self.motion.clear();
        let _ = unsafe { ShowWindow(self.window.0, SW_HIDE) };
        self.thumbnails.clear();
        self.edges.clear();
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        self.clear();
        unsafe {
            let _ = DestroyWindow(self.window.0);
            let _ = DestroyWindow(self.swatch.0);
        }
    }
}

/// The four sides of a border `width` thick round `rect`: top, bottom, left
/// and right.
fn border(rect: Rect, width: i32) -> [Rect; 4] {
    let outer = rect.inset(-width);
    let across = |y| Rect {
        y,
        height: width,
        ..outer
    };
    let down = |x| Rect { x, width, ..rect };
    [
        across(outer.y),
        across(rect.y + rect.height),
        down(outer.x),
        down(rect.x + rect.width),
    ]
}

/// Gives the layered window a surface with nothing on it: one clear pixel.
/// Windows composes the pictures over whatever size the window is then given.
fn clear_surface(hwnd: HWND) -> Result<()> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: 1,
            biHeight: 1,
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
        ..Default::default()
    };
    let size = SIZE { cx: 1, cy: 1 };
    let origin = POINT::default();
    unsafe {
        let dc = CreateCompatibleDC(None);
        let mut pixels = std::ptr::null_mut();
        // The section's pixels start out zeroed, which is fully clear.
        let made = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0);
        let updated = made.and_then(|bitmap| {
            let previous = SelectObject(dc, bitmap.into());
            let updated = UpdateLayeredWindow(
                hwnd,
                None,
                None,
                Some(&raw const size),
                Some(dc),
                Some(&raw const origin),
                COLORREF(0),
                Some(&raw const blend),
                ULW_ALPHA,
            );
            SelectObject(dc, previous);
            let _ = DeleteObject(bitmap.into());
            updated
        });
        let _ = DeleteDC(dc);
        updated
    }
}

/// Fills the swatch with the colour kept in its user data.
unsafe extern "system" fn paint_swatch(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => unsafe {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let colour = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u32;
            SetDCBrushColor(dc, COLORREF(colour));
            FillRect(dc, &paint.rcPaint, HBRUSH(GetStockObject(DC_BRUSH).0));
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        },
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

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

    const VIEWPORT: Rect = Rect {
        x: 0,
        y: 20,
        width: 1000,
        height: 800,
    };

    /// Needs a desktop session with an app window open, so it doesn't run by
    /// default. Nothing shows on screen: the stage stays hidden.
    #[test]
    #[ignore]
    fn windows_gives_the_stage_a_picture_of_another_apps_window() {
        let stage = Stage::new(None).expect("the stage's window is created");
        let open = Window::visible().expect("windows can be listed");
        let window = open
            .into_iter()
            .find(|window| window.is_tileable())
            .expect("an app window");

        let _thumbnail = Thumbnail::new(stage.window.0, window).expect("a live thumbnail");
    }

    fn ends(slides: &[Slide<i32>], window: i32) -> Option<((i32, i32), (i32, i32))> {
        let slide = slides.iter().find(|slide| slide.window == window)?;
        Some(((slide.from.x, slide.from.y), (slide.to.x, slide.to.y)))
    }

    #[test]
    fn workspaces_pass_like_pages_while_windows_on_both_stay_put() {
        let item = |window, visible| SceneWindow {
            window,
            rect: VIEWPORT,
            visible,
            active: visible,
        };
        let slides = slides(
            &[item(1, true), item(2, false), item(3, true), item(4, false)],
            &[item(1, false), item(2, true), item(3, true), item(4, false)],
            Some(Direction::Down),
            VIEWPORT,
        );
        assert_eq!(ends(&slides, 1), Some(((0, 20), (0, -780))));
        assert_eq!(ends(&slides, 2), Some(((0, 820), (0, 20))));
        assert_eq!(ends(&slides, 3), Some(((0, 20), (0, 20))));
        assert_eq!(ends(&slides, 4), None, "never on screen");
    }

    #[test]
    fn a_scroll_brings_columns_from_where_they_sat_off_screen() {
        let item = |window, x, visible, active| SceneWindow {
            window,
            rect: Rect { x, ..VIEWPORT },
            visible,
            active,
        };
        let before = [
            item(1, 0, true, true),
            item(2, 1000, false, true),
            item(3, 2000, false, true),
            item(4, 0, false, false),
        ];
        let after = [
            item(1, -2000, false, true),
            item(2, -1000, false, true),
            item(3, 0, true, true),
            item(4, 0, false, false),
        ];
        let slides = slides(&before, &after, None, VIEWPORT);
        assert_eq!(ends(&slides, 1), Some(((0, 20), (-2000, 20))));
        assert_eq!(
            ends(&slides, 2),
            Some(((1000, 20), (-1000, 20))),
            "in view at neither end, but it passes through"
        );
        assert_eq!(ends(&slides, 3), Some(((2000, 20), (0, 20))));
        assert_eq!(ends(&slides, 4), None, "another workspace's");
    }

    #[test]
    fn a_window_that_only_appears_or_goes_without_a_slide_isnt_moved() {
        let item = |visible| SceneWindow {
            window: 1,
            rect: VIEWPORT,
            visible,
            active: false,
        };
        let appears = slides(&[item(false)], &[item(true)], None, VIEWPORT);
        assert_eq!(ends(&appears, 1), Some(((0, 20), (0, 20))));
        assert!(slides(&[item(true)], &[item(false)], None, VIEWPORT).is_empty());
        assert_eq!(
            ends(
                &slides(&[item(true)], &[], Some(Direction::Up), VIEWPORT),
                1
            ),
            Some(((0, 20), (0, 820))),
            "a window that's gone still leaves with its workspace"
        );
    }
}

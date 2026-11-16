//! Checks on the real desktop that the stage does what the animations rely
//! on, by reading back the pixels Windows composes. It shows three small
//! windows of its own for about two seconds and never adopts, moves, focuses
//! or cloaks another app's.
use std::time::Duration;

use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BeginPaint, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DC_BRUSH, DIB_RGB_COLORS, DeleteDC, DeleteObject, EndPaint, FillRect,
    GetDC, GetDIBits, GetStockObject, HBRUSH, PAINTSTRUCT, ReleaseDC, SRCCOPY, SelectObject,
    SetDCBrushColor, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect,
    GetWindowLongPtrW, RegisterClassW, SW_SHOWNOACTIVATE, SetWindowLongPtrW, ShowWindow, WM_PAINT,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_POPUP,
    WindowFromPoint,
};
use windows::core::w;

use crate::animation::{Animations, Slide};
use crate::frame_clock::FrameClock;
use crate::layout::Rect;
use crate::stage::{Ring, Stage};
use crate::window::Window;

const FLOOR: u32 = 0x00C0_20C0;
const RING: u32 = 0x0020_E040;

struct Owned(HWND);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// Fills the window with the colour in its user data, or with a grid when
/// that is zero, so a picture that is offset or scaled can't match.
unsafe extern "system" fn paint(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if message != WM_PAINT {
        return unsafe { DefWindowProcW(hwnd, message, wp, lp) };
    }
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut area = RECT::default();
        let _ = GetClientRect(hwnd, &mut area);
        let brush = HBRUSH(GetStockObject(DC_BRUSH).0);
        let colour = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u32;
        SetDCBrushColor(dc, COLORREF(if colour == 0 { 0x0030_2010 } else { colour }));
        FillRect(dc, &area, brush);
        if colour == 0 {
            SetDCBrushColor(dc, COLORREF(0x0000_CCFF));
            for x in (0..area.right).step_by(40) {
                let line = RECT {
                    left: x,
                    right: x + 2,
                    ..area
                };
                FillRect(dc, &line, brush);
            }
            for y in (0..area.bottom).step_by(40) {
                let line = RECT {
                    top: y,
                    bottom: y + 2,
                    ..area
                };
                FillRect(dc, &line, brush);
            }
        }
        let _ = EndPaint(hwnd, &ps);
    }
    LRESULT(0)
}

/// The pixels Windows is showing in `rect` of the screen.
fn capture(rect: Rect) -> Vec<u32> {
    let mut pixels = vec![0u32; (rect.width * rect.height) as usize];
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: rect.width,
            biHeight: -rect.height,
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        let screen = GetDC(None);
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, rect.width, rect.height);
        let previous = SelectObject(memory, bitmap.into());
        // CAPTUREBLT includes layered windows, as the stage is.
        let copy = SRCCOPY | CAPTUREBLT;
        BitBlt(
            memory,
            0,
            0,
            rect.width,
            rect.height,
            Some(screen),
            rect.x,
            rect.y,
            copy,
        )
        .unwrap();
        SelectObject(memory, previous);
        let rows = GetDIBits(
            memory,
            bitmap,
            0,
            rect.height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        assert_ne!(rows, 0);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
    }
    pixels
}

fn close(a: u32, b: u32) -> bool {
    [0, 8, 16]
        .into_iter()
        .all(|shift| ((a >> shift) & 255).abs_diff((b >> shift) & 255) <= 2)
}

/// How many of the points sampled across two same-sized captures differ.
/// The outer pixels are left out: a window's edge is drawn by Windows.
fn mismatches(a: &[u32], b: &[u32], size: Rect) -> (usize, usize) {
    let mut count = 0;
    let mut wrong = 0;
    for y in (12..size.height - 12).step_by(13) {
        for x in (12..size.width - 12).step_by(11) {
            let index = (y * size.width + x) as usize;
            count += 1;
            wrong += usize::from(!close(a[index], b[index]));
        }
    }
    (wrong, count)
}

/// Lets what was submitted reach the screen before it's read back.
fn settle() {
    unsafe {
        DwmFlush().unwrap();
    }
    std::thread::sleep(Duration::from_millis(100));
    unsafe {
        DwmFlush().unwrap();
    }
}

#[test]
#[ignore = "shows three small windows of its own for about two seconds"]
fn the_stage_shows_true_pictures_over_a_clear_click_through_surface() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).unwrap();
    }
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None).unwrap() }.0);
    let class = WNDCLASSW {
        hInstance: instance,
        lpfnWndProc: Some(paint),
        lpszClassName: w!("wm.stage.probe"),
        ..Default::default()
    };
    assert_ne!(unsafe { RegisterClassW(&class) }, 0);
    // Tool windows that take no focus, which a running manager leaves alone.
    let quiet = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
    let window = |title, style, colour: u32, rect: Rect| {
        let hwnd = unsafe {
            CreateWindowExW(
                quiet,
                class.lpszClassName,
                title,
                style,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                None,
                None,
                Some(instance),
                None,
            )
            .unwrap()
        };
        let owned = Owned(hwnd);
        Window(hwnd).disable_native_transitions();
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, colour as isize);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = UpdateWindow(hwnd);
        }
        owned
    };

    // A framed window to take pictures of, and a plain floor for them to be
    // shown over, so the test knows what's behind the stage.
    let source = window(
        w!("wm stage probe source"),
        WS_OVERLAPPEDWINDOW,
        0,
        Rect {
            x: 60,
            y: 160,
            width: 420,
            height: 300,
        },
    );
    let floor_area = Rect {
        x: 520,
        y: 120,
        width: 700,
        height: 420,
    };
    let floor = window(w!("wm stage probe floor"), WS_POPUP, FLOOR, floor_area);
    settle();
    let frame = Window(source.0).bounds();
    let original = capture(frame);

    let mut stage = Stage::new(Some(floor.0)).unwrap();
    let place = |x, y| Rect { x, y, ..frame };
    let first = place(floor_area.x + 30, floor_area.y + 30);
    let slide = |from, to| Slide {
        window: Window(source.0),
        from,
        to,
    };
    let rate = Animations::default().rate(Default::default());
    stage.present(&[slide(first, first)], floor_area, rate, true);
    assert!(stage.covering() && !stage.veils(Window(source.0)));
    settle();
    stage.composed();
    assert!(stage.take_cover(), "the pictures are up once composed");
    assert!(stage.veils(Window(source.0)));

    let shown = capture(first);
    let (wrong, count) = mismatches(&original, &shown, frame);
    let edge = original
        .iter()
        .zip(&shown)
        .filter(|(real, picture)| !close(**real, **picture))
        .count();
    println!(
        "picture at rest: {}/{count} sampled pixels match; {edge} of all {} differ",
        count - wrong,
        original.len()
    );
    assert!(wrong * 100 <= count, "the picture is offset or scaled");
    let corner = capture(Rect {
        width: 20,
        height: 20,
        ..floor_area
    });
    assert!(
        corner.iter().all(|pixel| close(*pixel, bgr(FLOOR))),
        "the surface is clear, and above its owner, where there's no picture: {:08x}",
        corner[0]
    );
    let under = unsafe {
        WindowFromPoint(POINT {
            x: first.x + 100,
            y: first.y + 100,
        })
    };
    assert_eq!(under, floor.0, "clicks pass through the surface");

    // The ring is drawn outside the picture, and goes where it goes.
    let beside = |picture: Rect| {
        capture(Rect {
            x: picture.x - 4,
            y: picture.y + 40,
            width: 4,
            height: 20,
        })
    };
    let all = |pixels: Vec<u32>, colour| pixels.iter().all(|pixel| close(*pixel, bgr(colour)));
    stage.highlight(Some(Ring {
        window: Window(source.0),
        colour: COLORREF(RING),
        width: 4,
    }));
    stage.present(&[slide(first, first)], floor_area, rate, true);
    settle();
    assert!(all(beside(first), RING), "the ring is round the picture");

    // Slide it across, a frame for each the compositor presents.
    let second = place(floor_area.x + 230, floor_area.y + 50);
    stage.present(&[slide(first, second)], floor_area, rate, true);
    let mut clock = FrameClock::default();
    let mut frames = 0;
    while stage.animating() {
        stage.step(clock.wait(), || true);
        frames += 1;
        assert!(frames < 200, "the slide never came to rest");
    }
    settle();
    assert!(all(beside(second), RING), "the ring went with the picture");
    stage.highlight(None);
    stage.present(&[slide(second, second)], floor_area, rate, true);
    settle();
    assert!(all(beside(second), FLOOR), "the ring is gone");
    let (wrong, count) = mismatches(&original, &capture(second), frame);
    println!(
        "picture after a {frames}-frame slide: {}/{count} sampled pixels match",
        count - wrong
    );
    assert!(
        wrong * 100 <= count,
        "the picture didn't land where it was sent"
    );
    let left = capture(Rect {
        width: 20,
        height: 20,
        ..first
    });
    assert!(left.iter().all(|pixel| close(*pixel, bgr(FLOOR))));

    stage.finish();
    drop(stage);
}

/// A `COLORREF` as the captured pixels have it, red and blue swapped.
fn bgr(colour: u32) -> u32 {
    (colour & 0xFF00) | ((colour & 0xFF) << 16) | ((colour >> 16) & 0xFF)
}

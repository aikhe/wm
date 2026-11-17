use std::cell::Cell;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{
    COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, RECT, SIZE, WAIT_FAILED, WAIT_OBJECT_0,
    WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleBitmap,
    CreateCompatibleDC, CreateFontW, DC_BRUSH, DEFAULT_CHARSET, DT_CENTER, DT_END_ELLIPSIS,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, EndPaint,
    FF_DONTCARE, FillRect, GetDC, GetStockObject, GetTextExtentPoint32W, GetTextFaceW, HBITMAP,
    HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect, OUT_TT_ONLY_PRECIS, PAINTSTRUCT, ReleaseDC,
    SRCCOPY, SelectObject, SetBkMode, SetDCBrushColor, SetTextColor, TRANSPARENT,
};
use windows::Win32::Storage::FileSystem::{
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::INFINITE;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    ABM_NEW, ABM_REMOVE, ABN_FULLSCREENAPP, APPBARDATA, SHAppBarMessage,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_NAMECHANGE, FindWindowW, GetWindowTextW,
    GetWindowThreadProcessId, HWND_BOTTOM, HWND_TOPMOST, KillTimer, MA_NOACTIVATE, MSG,
    MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, OBJID_WINDOW, PM_REMOVE, PeekMessageW,
    PostMessageW, PostQuitMessage, QS_ALLINPUT, SPI_GETWORKAREA, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetTimer, SetWindowPos,
    ShowWindow, SystemParametersInfoW, TranslateMessage, WINEVENT_OUTOFCONTEXT, WM_APP, WM_CLOSE,
    WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_MOUSEACTIVATE,
    WM_MOUSEWHEEL, WM_PAINT, WM_QUIT, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMECHANGE, WM_TIMER,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{Error, PCWSTR, w};
use wm_bar_protocol::{self as protocol, BAR_CLASS, Client, Command, Snapshot};

use crate::config::{self, Config, DEFAULT_FONT, Module};
use crate::layout::{arrange, workspace_page};
use crate::stats::{CpuSampler, MemoryUsage};

const TITLE_CHANGED: u32 = WM_APP + 10;
const HOST_DIED: u32 = WM_APP + 11;
const SYSTEM_CHANGED: u32 = WM_APP + 12;
const APPBAR_CALLBACK: u32 = WM_APP + 13;
const FULLSCREEN_CHANGED: u32 = WM_APP + 14;
const CLOCK_TIMER: usize = 1;
const RELOAD_TIMER: usize = 2;
const CPU_TIMER: usize = 3;
const MEMORY_TIMER: usize = 4;

thread_local! {
    static BAR: Cell<isize> = const { Cell::new(0) };
    static TITLE_WINDOW: Cell<isize> = const { Cell::new(0) };
    static HOST_WINDOW: Cell<isize> = const { Cell::new(0) };
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_ERASEBKGND => LRESULT(1), // Every repaint copies a complete backbuffer.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE | WM_TIMECHANGE => {
            // Broadcasts are often sent, not queued. Defer them into the loop
            // instead of trying to access/re-enter View from a window procedure.
            let _ = unsafe {
                PostMessageW(
                    Some(hwnd),
                    SYSTEM_CHANGED,
                    WPARAM(message as usize),
                    LPARAM(0),
                )
            };
            LRESULT(0)
        }
        APPBAR_CALLBACK if wparam.0 as u32 == ABN_FULLSCREENAPP => {
            let _ = unsafe { PostMessageW(Some(hwnd), FULLSCREEN_CHANGED, WPARAM(0), lparam) };
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

unsafe extern "system" fn event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if object != OBJID_WINDOW.0 || child != CHILDID_SELF as i32 {
        return;
    }
    let message = if event == EVENT_OBJECT_NAMECHANGE && TITLE_WINDOW.get() == hwnd.0 as isize {
        TITLE_CHANGED
    } else if event == EVENT_OBJECT_DESTROY && HOST_WINDOW.get() == hwnd.0 as isize {
        HOST_DIED
    } else {
        return;
    };
    let _ = unsafe {
        PostMessageW(
            Some(HWND(BAR.get() as *mut _)),
            message,
            WPARAM(hwnd.0 as usize),
            LPARAM(0),
        )
    };
}

struct Hook(HWINEVENTHOOK);
impl Hook {
    fn for_window(event: u32, hwnd: HWND) -> Option<Self> {
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0 || thread == 0 {
            return None;
        }
        let hook = unsafe {
            SetWinEventHook(
                event,
                event,
                None,
                Some(event_proc),
                pid,
                thread,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if hook.is_invalid() {
            eprintln!("Couldn't watch window changes: {}", Error::from_thread());
            None
        } else {
            Some(Self(hook))
        }
    }
}
impl Drop for Hook {
    fn drop(&mut self) {
        let _ = unsafe { UnhookWinEvent(self.0) };
    }
}

struct Changes(HANDLE);
impl Changes {
    fn new(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let parent = path
            .parent()
            .ok_or("configuration has no parent directory")?;
        let name: Vec<_> = parent.as_os_str().encode_wide().chain(Some(0)).collect();
        Ok(Self(unsafe {
            FindFirstChangeNotificationW(
                PCWSTR(name.as_ptr()),
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_SIZE,
            )?
        }))
    }
}
impl Drop for Changes {
    fn drop(&mut self) {
        let _ = unsafe { FindCloseChangeNotification(self.0) };
    }
}

struct Font(HFONT);
impl Font {
    fn new(face: &str, size: i32, weight: i32) -> Result<Self, Box<dyn std::error::Error>> {
        let name: Vec<_> = face.encode_utf16().chain(Some(0)).collect();
        let font = unsafe {
            CreateFontW(
                -size,
                // Zero means the family's natural glyph widths. Never scale
                // text horizontally to fit a widget; clip/ellipsize instead.
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                // Never let a missing family select legacy bitmap fonts.
                OUT_TT_ONLY_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                FF_DONTCARE.0 as u32,
                PCWSTR(name.as_ptr()),
            )
        };
        if font.is_invalid() {
            return Err("couldn't create the bar font".into());
        }
        let font = Self(font);
        let actual = font.face()?;
        if !actual.eq_ignore_ascii_case(face) && !face.eq_ignore_ascii_case(DEFAULT_FONT) {
            eprintln!(
                "Font `{face}` resolved to `{actual}`; using {DEFAULT_FONT} instead. Run `wm-bar fonts` for installed family names."
            );
            return Self::new(DEFAULT_FONT, size, weight);
        }
        Ok(font)
    }

    fn face(&self) -> Result<String, Box<dyn std::error::Error>> {
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return Err("couldn't check the bar font".into());
        }
        let previous = unsafe { SelectObject(dc, self.0.into()) };
        let mut name = [0u16; 128];
        let count = unsafe { GetTextFaceW(dc, Some(&mut name)) };
        unsafe {
            SelectObject(dc, previous);
            let _ = DeleteDC(dc);
        }
        if count == 0 {
            return Err("couldn't identify the bar font".into());
        }
        let end = name
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(name.len());
        Ok(String::from_utf16_lossy(&name[..end]))
    }
}

struct Fonts {
    text: Font,
    icons: Font,
    workspace: Font,
}

impl Fonts {
    fn new(config: &Config) -> Result<Self, Box<dyn std::error::Error>> {
        let theme = &config.theme;
        Ok(Self {
            text: Font::new(&theme.font, theme.font_size, theme.font_weight)?,
            icons: Font::new(&theme.icon_font, theme.icon_size, 400)?,
            workspace: Font::new(&theme.font, theme.workspace_font_size, theme.font_weight)?,
        })
    }
}
impl Drop for Font {
    fn drop(&mut self) {
        let _ = unsafe { DeleteObject(self.0.into()) };
    }
}

/// Cached GDI objects, recreated only when the bar's dimensions change.
struct Backbuffer {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    width: i32,
    height: i32,
}
impl Backbuffer {
    fn new(width: i32, height: i32) -> Result<Self, Box<dyn std::error::Error>> {
        let screen = unsafe { GetDC(None) };
        if screen.is_invalid() {
            return Err("couldn't acquire a screen DC".into());
        }
        let dc = unsafe { CreateCompatibleDC(Some(screen)) };
        let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
        unsafe { ReleaseDC(None, screen) };
        if dc.is_invalid() || bitmap.is_invalid() {
            let _ = unsafe { DeleteDC(dc) };
            let _ = unsafe { DeleteObject(bitmap.into()) };
            return Err("couldn't create the bar backbuffer".into());
        }
        let original = unsafe { SelectObject(dc, bitmap.into()) };
        Ok(Self {
            dc,
            bitmap,
            original,
            width,
            height,
        })
    }
}
impl Drop for Backbuffer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.original);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

#[derive(Clone, Copy)]
struct Hit {
    rect: RECT,
    command: Command,
    revision: u64,
}

/// Registers or removes the bar as a shell appbar. It never reserves space
/// (no `ABM_SETPOS`); registering only makes the shell tell the bar when a
/// fullscreen app opens or closes.
fn appbar(message: u32, window: HWND) {
    let mut data = APPBARDATA {
        cbSize: size_of::<APPBARDATA>() as u32,
        hWnd: window,
        uCallbackMessage: APPBAR_CALLBACK,
        ..Default::default()
    };
    unsafe { SHAppBarMessage(message, &mut data) };
}

struct BarWindow(HWND);
impl Drop for BarWindow {
    fn drop(&mut self) {
        appbar(ABM_REMOVE, self.0);
        let _ = unsafe { DestroyWindow(self.0) };
        BAR.set(0);
    }
}

struct View {
    window: BarWindow,
    config: Config,
    fonts: Fonts,
    buffer: Backbuffer,
    client: Option<Client>,
    host_hook: Option<Hook>,
    title_hook: Option<Hook>,
    state: Snapshot,
    revision: u64,
    title_window: u64,
    title: String,
    clock: String,
    cpu_sampler: CpuSampler,
    cpu: String,
    memory: String,
    hits: Vec<Hit>,
    /// A fullscreen app is open, so the bar sits at the bottom instead of on top.
    fullscreen: bool,
}

fn work_area() -> windows::core::Result<RECT> {
    let mut rect = RECT::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&raw mut rect).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?
    };
    Ok(rect)
}

fn check_geometry(config: &Config, area: RECT) -> Result<(), Box<dyn std::error::Error>> {
    if area.right - area.left <= 2 * config.bar.padding
        || area.bottom - area.top < config.bar.height
    {
        return Err("bar height/padding do not fit the primary monitor's work area".into());
    }
    Ok(())
}

impl View {
    fn new(config: Config) -> Result<Self, Box<dyn std::error::Error>> {
        if unsafe { FindWindowW(BAR_CLASS, None) }.is_ok() {
            return Err("wm-bar is already running in this session".into());
        }
        let area = work_area()?;
        check_geometry(&config, area)?;
        let fonts = Fonts::new(&config)?;
        let buffer = Backbuffer::new(area.right - area.left, config.bar.height)?;
        protocol::register_class(BAR_CLASS, Some(window_proc))?;
        let window = BarWindow(unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                BAR_CLASS,
                w!("wm-bar"),
                WS_POPUP,
                area.left,
                area.top,
                area.right - area.left,
                config.bar.height,
                None,
                None,
                Some(HINSTANCE(GetModuleHandleW(None)?.0)),
                None,
            )?
        });
        BAR.set(window.0.0 as isize);
        appbar(ABM_NEW, window.0);
        let mut view = Self {
            window,
            config,
            fonts,
            buffer,
            client: None,
            host_hook: None,
            title_hook: None,
            state: Snapshot::default(),
            revision: 0,
            title_window: 0,
            title: String::new(),
            clock: String::new(),
            cpu_sampler: CpuSampler::default(),
            cpu: String::new(),
            memory: String::new(),
            hits: Vec::new(),
            fullscreen: false,
        };
        view.connect();
        view.update_clock();
        view.configure_stats();
        let _ = unsafe { ShowWindow(view.window.0, SW_SHOWNOACTIVATE) };
        Ok(view)
    }

    /// Where the bar belongs in the z-order: behind everything while a
    /// fullscreen app is open, above everything otherwise.
    fn layer(&self) -> HWND {
        if self.fullscreen {
            HWND_BOTTOM
        } else {
            HWND_TOPMOST
        }
    }

    fn set_fullscreen(&mut self, fullscreen: bool) {
        self.fullscreen = fullscreen;
        let _ = unsafe {
            SetWindowPos(
                self.window.0,
                Some(self.layer()),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };
    }

    fn invalidate(&self) {
        let _ = unsafe { InvalidateRect(Some(self.window.0), None, false) };
    }

    fn connect(&mut self) {
        self.host_hook = None;
        self.client = None;
        self.state = Snapshot::default();
        self.revision = 0;
        HOST_WINDOW.set(0);
        if let Some(host) = protocol::find_host() {
            match Client::connect(host, self.window.0) {
                Ok(client) => {
                    HOST_WINDOW.set(host.0 as isize);
                    self.host_hook = Hook::for_window(EVENT_OBJECT_DESTROY, host);
                    self.client = Some(client);
                }
                Err(error) => eprintln!("Couldn't connect to wm: {error}"),
            }
        }
        self.update_state(false);
    }

    fn update_state(&mut self, acknowledge: bool) {
        if let Some(client) = &self.client {
            if let Some((revision, state)) = client.read() {
                self.revision = revision;
                self.state = state;
            }
            if acknowledge {
                client.acknowledge(self.window.0, self.revision);
            }
        } else {
            self.state = Snapshot::default();
            self.revision = 0;
        }
        self.track_title();
        self.invalidate();
    }

    fn track_title(&mut self) {
        let target = if self.config.has(Module::Title) {
            self.state.focused
        } else {
            0
        };
        if target != self.title_window {
            self.title_hook = None;
            self.title_window = target;
            TITLE_WINDOW.set(target as isize);
            if target != 0 {
                self.title_hook = Hook::for_window(EVENT_OBJECT_NAMECHANGE, HWND(target as *mut _));
            }
        }
        self.update_title();
    }

    fn update_title(&mut self) {
        let text = if self.revision == 0 {
            "wm is not running".into()
        } else if self.title_window == 0 {
            "Empty workspace".into()
        } else {
            // Other-process top-level captions are copied by Windows;
            // this does not ask the target application to paint/respond.
            let mut text = [0u16; 4096];
            let count = unsafe { GetWindowTextW(HWND(self.title_window as *mut _), &mut text) };
            String::from_utf16_lossy(&text[..count.max(0) as usize])
        };
        if text != self.title {
            self.title = text;
            self.invalidate();
        }
    }

    fn update_clock(&mut self) {
        let _ = unsafe { KillTimer(Some(self.window.0), CLOCK_TIMER) };
        if self.config.has(Module::Clock) {
            let time = unsafe { GetLocalTime() };
            let text = config::clock_text(&self.config.clock.format, time);
            if text != self.clock {
                self.clock = text;
                self.invalidate();
            }
            let delay = 60_000 - u32::from(time.wSecond) * 1000 - u32::from(time.wMilliseconds);
            if unsafe { SetTimer(Some(self.window.0), CLOCK_TIMER, delay, None) } == 0 {
                eprintln!("Couldn't arm the clock timer: {}", Error::from_thread());
            }
        }
    }

    fn configure_stats(&mut self) {
        for timer in [CPU_TIMER, MEMORY_TIMER] {
            let _ = unsafe { KillTimer(Some(self.window.0), timer) };
        }
        if self.config.has(Module::Cpu) {
            self.update_cpu();
            self.stats_timer(CPU_TIMER, self.config.cpu.update_interval_ms);
        } else {
            self.cpu_sampler = CpuSampler::default();
            self.cpu.clear();
        }
        if self.config.has(Module::Memory) {
            self.update_memory();
            self.stats_timer(MEMORY_TIMER, self.config.memory.update_interval_ms);
        } else {
            self.memory.clear();
        }
    }

    fn stats_timer(&self, timer: usize, interval: u32) {
        if unsafe { SetTimer(Some(self.window.0), timer, interval, None) } == 0 {
            eprintln!("Couldn't arm a statistics timer: {}", Error::from_thread());
        }
    }

    fn update_cpu(&mut self) {
        if !self.config.has(Module::Cpu) {
            return;
        }
        let value = self
            .cpu_sampler
            .sample()
            .ok()
            .flatten()
            .map_or_else(|| "--".into(), |value| value.to_string());
        let text = self.config.cpu.format.replace("{percent}", &value);
        if text != self.cpu {
            self.cpu = text;
            self.invalidate();
        }
    }

    fn update_memory(&mut self) {
        if !self.config.has(Module::Memory) {
            return;
        }
        let text = MemoryUsage::read().map_or_else(
            |_| "--".into(),
            |value| value.format(&self.config.memory.format),
        );
        if text != self.memory {
            self.memory = text;
            self.invalidate();
        }
    }

    fn reload(&mut self, path: &Path) {
        let prepared = (|| -> Result<_, Box<dyn std::error::Error>> {
            let config = Config::parse(&std::fs::read_to_string(path)?)?;
            let area = work_area()?;
            check_geometry(&config, area)?;
            let fonts = Fonts::new(&config)?;
            let buffer = Backbuffer::new(area.right - area.left, config.bar.height)?;
            unsafe {
                SetWindowPos(
                    self.window.0,
                    Some(self.layer()),
                    area.left,
                    area.top,
                    area.right - area.left,
                    config.bar.height,
                    SWP_NOACTIVATE,
                )?
            };
            Ok((config, fonts, buffer))
        })();
        match prepared {
            Ok((config, fonts, buffer)) => {
                let height_changed = config.bar.height != self.config.bar.height;
                self.config = config;
                self.fonts = fonts;
                self.buffer = buffer;
                self.track_title();
                self.update_clock();
                self.configure_stats();
                self.invalidate();
                if height_changed {
                    println!(
                        "Bar height is {}px; set wm's layout.reserved_top to match.",
                        self.config.bar.height
                    );
                }
            }
            Err(error) => eprintln!("Bar config reload rejected; previous settings kept: {error}"),
        }
    }

    fn reposition(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let area = work_area()?;
        check_geometry(&self.config, area)?;
        let buffer = Backbuffer::new(area.right - area.left, self.config.bar.height)?;
        unsafe {
            SetWindowPos(
                self.window.0,
                Some(self.layer()),
                area.left,
                area.top,
                area.right - area.left,
                self.config.bar.height,
                SWP_NOACTIVATE,
            )?
        };
        self.buffer = buffer;
        self.invalidate();
        Ok(())
    }

    fn text(&self, module: Module) -> &str {
        match module {
            Module::Title => &self.title,
            Module::Clock => &self.clock,
            Module::Cpu => &self.cpu,
            Module::Memory => &self.memory,
            Module::Mode if self.revision == 0 => "Offline",
            Module::Mode if self.state.overview => "Overview",
            Module::Mode => match self.state.mode {
                1 => "Tiled",
                2 => "Floating",
                3 => "Sticky",
                4 => "On top",
                _ => "Empty",
            },
            Module::Workspaces => "",
        }
    }

    fn icon(&self, module: Module) -> &str {
        match module {
            Module::Cpu => &self.config.cpu.icon,
            Module::Memory => &self.config.memory.icon,
            _ => "",
        }
    }

    fn render(&mut self) {
        let dc = self.buffer.dc;
        let height = self.buffer.height;
        let original = unsafe { SelectObject(dc, self.fonts.text.0.into()) };
        unsafe { SetBkMode(dc, TRANSPARENT) };
        fill(
            dc,
            RECT {
                left: 0,
                top: 0,
                right: self.buffer.width,
                bottom: height,
            },
            self.config.background,
        );
        let page = workspace_page(
            self.state.active,
            self.state.workspaces,
            self.config.bar.max_workspaces,
        );
        let groups = [
            &self.config.modules.left,
            &self.config.modules.center,
            &self.config.modules.right,
        ]
        .map(|items| {
            items
                .iter()
                .map(|&module| {
                    let width = if module == Module::Workspaces {
                        page.len() as i32 * self.config.bar.workspace_width
                    } else {
                        let mut width = measure(dc, self.text(module));
                        if module == Module::Cpu {
                            // Reserve room for 100% so utilization changes do
                            // not make neighboring widgets jump sideways.
                            width = width.max(measure(
                                dc,
                                &self.config.cpu.format.replace("{percent}", "100"),
                            ));
                        }
                        if module == Module::Title {
                            width = width.min(self.config.bar.title_max_width);
                        }
                        if !self.icon(module).is_empty() {
                            let previous = unsafe { SelectObject(dc, self.fonts.icons.0.into()) };
                            width += measure(dc, self.icon(module)) + self.config.bar.icon_gap;
                            unsafe { SelectObject(dc, previous) };
                        }
                        width + self.config.bar.widget_padding * 2
                    };
                    (module, width)
                })
                .collect()
        });
        let cells = arrange(
            &groups,
            self.buffer.width,
            self.config.bar.padding,
            self.config.bar.gap,
        );
        self.hits.clear();
        for cell in cells {
            let rect = RECT {
                left: cell.x,
                top: 0,
                right: cell.x + cell.width,
                bottom: height,
            };
            if cell.module == Module::Workspaces {
                for (position, index) in page.clone().enumerate() {
                    let left = rect.left + position as i32 * self.config.bar.workspace_width;
                    let button = RECT {
                        left,
                        top: self.config.bar.workspace_inset,
                        right: (left + self.config.bar.workspace_width).min(rect.right),
                        bottom: height - self.config.bar.workspace_inset,
                    };
                    if button.right <= button.left {
                        break;
                    }
                    let active = self.revision != 0 && index == self.state.active;
                    if active {
                        fill(dc, button, self.config.active_background);
                    }
                    let previous = unsafe { SelectObject(dc, self.fonts.workspace.0.into()) };
                    draw_text(
                        dc,
                        &(index + 1).to_string(),
                        button,
                        if active {
                            self.config.active_foreground
                        } else {
                            self.config.muted
                        },
                    );
                    unsafe { SelectObject(dc, previous) };
                    self.hits.push(Hit {
                        rect: button,
                        command: Command::Workspace(index),
                        revision: self.revision,
                    });
                }
            } else {
                self.draw_module(dc, cell.module, rect);
                if matches!(cell.module, Module::Title | Module::Mode) {
                    self.hits.push(Hit {
                        rect,
                        command: Command::Overview,
                        revision: self.revision,
                    });
                }
            }
        }
        unsafe { SelectObject(dc, original) };
    }

    fn draw_module(&self, dc: HDC, module: Module, mut rect: RECT) {
        let padding = self
            .config
            .bar
            .widget_padding
            .min((rect.right - rect.left) / 2);
        rect.left += padding;
        rect.right -= padding;
        if rect.left >= rect.right {
            return;
        }
        let icon = self.icon(module);
        if !icon.is_empty() {
            let previous = unsafe { SelectObject(dc, self.fonts.icons.0.into()) };
            let icon_width = measure(dc, icon);
            let icon_rect = RECT {
                right: (rect.left + icon_width).min(rect.right),
                ..rect
            };
            draw_text(dc, icon, icon_rect, self.config.foreground);
            unsafe { SelectObject(dc, previous) };
            rect.left = (icon_rect.right + self.config.bar.icon_gap).min(rect.right);
        }
        if rect.left < rect.right {
            draw_text(dc, self.text(module), rect, self.config.foreground);
        }
    }

    fn paint(&mut self) {
        self.render();
        let mut paint = PAINTSTRUCT::default();
        let dc = unsafe { BeginPaint(self.window.0, &mut paint) };
        let _ = unsafe {
            BitBlt(
                dc,
                0,
                0,
                self.buffer.width,
                self.buffer.height,
                Some(self.buffer.dc),
                0,
                0,
                SRCCOPY,
            )
        };
        let _ = unsafe { EndPaint(self.window.0, &paint) };
    }

    fn click(&self, x: i32, y: i32) {
        if let Some(client) = &self.client
            && let Some(hit) = self.hits.iter().find(|hit| contains(hit.rect, x, y))
        {
            client.command(hit.command, hit.revision);
        }
    }
}

fn contains(rect: RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn fill(dc: HDC, rect: RECT, color: COLORREF) {
    unsafe {
        SetDCBrushColor(dc, color);
        FillRect(dc, &rect, HBRUSH(GetStockObject(DC_BRUSH).0));
    }
}

fn measure(dc: HDC, text: &str) -> i32 {
    let text: Vec<_> = text.encode_utf16().collect();
    if text.is_empty() {
        return 0;
    }
    let mut size = SIZE::default();
    let _ = unsafe { GetTextExtentPoint32W(dc, &text, &mut size) };
    size.cx
}

fn draw_text(dc: HDC, text: &str, mut rect: RECT, color: COLORREF) {
    let mut text: Vec<_> = text.encode_utf16().collect();
    if text.is_empty() {
        return;
    }
    unsafe {
        SetTextColor(dc, color);
        DrawTextW(
            dc,
            &mut text,
            &raw mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }
}

pub fn run(path: PathBuf, config: Config) -> Result<(), Box<dyn std::error::Error>> {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)? };
    let changes = Changes::new(&path)?;
    let mut view = View::new(config)?;
    let update = protocol::update_message();
    println!(
        "Bar config: {} (reloads on save). Right-click to quit.",
        path.display()
    );
    println!(
        "Set wm's layout.reserved_top = {}. Start/stop either process independently.",
        view.config.bar.height
    );
    loop {
        // Wait for UI input OR directory changes. No polling and no worker.
        let event = unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(&[changes.0]),
                INFINITE,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        if event == WAIT_FAILED {
            return Err(Error::from_thread().into());
        }
        if event == WAIT_OBJECT_0 {
            unsafe { FindNextChangeNotification(changes.0)? };
            if unsafe { SetTimer(Some(view.window.0), RELOAD_TIMER, 150, None) } == 0 {
                return Err("couldn't debounce the bar configuration reload".into());
            }
        }
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            if message.message == WM_QUIT {
                return Ok(());
            }
            if message.message == update {
                if message.wParam.0 == 0
                    || view
                        .client
                        .as_ref()
                        .is_none_or(|client| client.host.0 as usize != message.wParam.0)
                {
                    view.connect();
                } else {
                    view.update_state(true);
                }
                continue;
            }
            if message.hwnd == view.window.0 {
                match message.message {
                    WM_CLOSE | WM_RBUTTONUP => return Ok(()),
                    WM_PAINT => {
                        view.paint();
                        continue;
                    }
                    WM_LBUTTONDOWN => {
                        view.click(
                            i32::from(message.lParam.0 as i16),
                            i32::from((message.lParam.0 >> 16) as i16),
                        );
                        continue;
                    }
                    WM_MOUSEWHEEL => {
                        // Only scroll when over workspace buttons. The message
                        // carries screen coordinates; convert via the work area.
                        let area = work_area()?;
                        let x = i32::from(message.lParam.0 as i16) - area.left;
                        let y = i32::from((message.lParam.0 >> 16) as i16) - area.top;
                        if view.hits.iter().any(|hit| {
                            matches!(hit.command, Command::Workspace(_)) && contains(hit.rect, x, y)
                        }) && let Some(client) = &view.client
                        {
                            let delta = (message.wParam.0 >> 16) as i16;
                            let target = if delta > 0 {
                                view.state.active.saturating_sub(1)
                            } else {
                                (view.state.active + 1).min(view.state.workspaces - 1)
                            };
                            client.command(Command::Workspace(target), view.revision);
                        }
                        continue;
                    }
                    TITLE_CHANGED if message.wParam.0 as u64 == view.title_window => {
                        view.update_title();
                        continue;
                    }
                    HOST_DIED if HOST_WINDOW.get() as usize == message.wParam.0 => {
                        view.connect();
                        continue;
                    }
                    WM_TIMER if message.wParam.0 == CLOCK_TIMER => {
                        view.update_clock();
                        continue;
                    }
                    WM_TIMER if message.wParam.0 == RELOAD_TIMER => {
                        let _ = unsafe { KillTimer(Some(view.window.0), RELOAD_TIMER) };
                        view.reload(&path);
                        continue;
                    }
                    WM_TIMER if message.wParam.0 == CPU_TIMER => {
                        view.update_cpu();
                        continue;
                    }
                    WM_TIMER if message.wParam.0 == MEMORY_TIMER => {
                        view.update_memory();
                        continue;
                    }
                    WM_TIMECHANGE => {
                        view.update_clock();
                        continue;
                    }
                    FULLSCREEN_CHANGED => {
                        view.set_fullscreen(message.lParam.0 != 0);
                        continue;
                    }
                    SYSTEM_CHANGED => {
                        view.update_clock();
                        if let Err(error) = view.reposition() {
                            eprintln!("Couldn't reposition bar: {error}");
                        }
                        continue;
                    }
                    WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => {
                        if let Err(error) = view.reposition() {
                            eprintln!("Couldn't reposition bar: {error}");
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            let _ = unsafe { TranslateMessage(&message) };
            unsafe { DispatchMessageW(&message) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::GetPixel;

    #[test]
    fn missing_fonts_use_segoe_ui_instead_of_bitmap_substitutes() {
        let font = Font::new("wm font that does not exist", 11, 600).unwrap();
        assert_eq!(font.face().unwrap(), "Segoe UI");
        let dc = unsafe { CreateCompatibleDC(None) };
        let previous = unsafe { SelectObject(dc, font.0.into()) };
        let bytes = unsafe { windows::Win32::Graphics::Gdi::GetFontData(dc, 0, 0, None, 0) };
        unsafe {
            SelectObject(dc, previous);
            let _ = DeleteDC(dc);
        }
        assert_ne!(
            bytes,
            u32::MAX,
            "the selected font must contain scalable font data"
        );
        assert!(bytes > 0);
        // An explicit installed family must be honored rather than always
        // selecting the default. Use names from the renderer's enumeration.
        let installed = crate::fonts::installed().unwrap();
        assert!(installed.iter().any(|name| name == "Arial"));
        assert_eq!(
            Font::new("Arial", 11, 400).unwrap().face().unwrap(),
            "Arial"
        );
    }

    #[test]
    fn renderer_draws_unicode_and_workspace_hits_without_a_desktop_window() {
        let config = Config::parse(config::DEFAULT_CONFIG).unwrap();
        let fonts = Fonts::new(&config).unwrap();
        let buffer = Backbuffer::new(800, 28).unwrap();
        // HWND(0) is never a real window. This fixture only renders into its
        // private bitmap; it never shows a bar, registers keys or moves apps.
        let mut view = View {
            window: BarWindow(HWND::default()),
            config,
            fonts,
            buffer,
            client: None,
            host_hook: None,
            title_hook: None,
            state: Snapshot {
                active: 1,
                workspaces: 3,
                ..Default::default()
            },
            revision: 42,
            title_window: 0,
            title: "Editor — 日本語 & Rust".into(),
            clock: "08:03".into(),
            cpu_sampler: CpuSampler::default(),
            cpu: "24%".into(),
            memory: "6.0/16.0 GiB".into(),
            hits: Vec::new(),
            fullscreen: false,
        };
        view.render();
        assert_eq!(
            unsafe { GetPixel(view.buffer.dc, 0, 0) },
            view.config.background
        );
        let workspaces: Vec<_> = view
            .hits
            .iter()
            .filter(|hit| matches!(hit.command, Command::Workspace(_)))
            .collect();
        assert_eq!(workspaces.len(), 3);
        assert_eq!(workspaces[1].command, Command::Workspace(1));
        assert_eq!(workspaces[1].revision, 42);
        assert!(!contains(workspaces[0].rect, workspaces[0].rect.right, 5));
    }

    #[test]
    fn compact_yasb_profile_renders_centered_date_and_right_hand_stats() {
        let config = Config::parse(include_str!("../themes/yasb.toml")).unwrap();
        let fonts = Fonts::new(&config).unwrap();
        let buffer = Backbuffer::new(1920, config.bar.height).unwrap();
        let mut view = View {
            window: BarWindow(HWND::default()),
            config,
            fonts,
            buffer,
            client: None,
            host_hook: None,
            title_hook: None,
            state: Snapshot {
                active: 1,
                workspaces: 3,
                ..Default::default()
            },
            revision: 42,
            title_window: 0,
            title: "Editor".into(),
            clock: "Mon, 16 Nov 20:03".into(),
            cpu_sampler: CpuSampler::default(),
            cpu: "24%".into(),
            memory: "6.0/16.0 GiB".into(),
            hits: Vec::new(),
            fullscreen: false,
        };
        view.render();
        let active = view
            .hits
            .iter()
            .find(|hit| hit.command == Command::Workspace(1))
            .unwrap();
        assert_eq!(active.rect.top, 0);
        assert_eq!(active.rect.bottom, 18);
        assert_eq!(
            unsafe { GetPixel(view.buffer.dc, active.rect.left, 0) },
            view.config.active_background
        );
        let has_text = |left, right| {
            (left..right).any(|x| {
                (1..17)
                    .any(|y| (unsafe { GetPixel(view.buffer.dc, x, y) }) != view.config.background)
            })
        };
        assert!(
            has_text(870, 1050),
            "centered date/time must be painted in the 18px bar"
        );
        assert!(
            has_text(1710, 1830),
            "CPU icon and usage must be visible on the right"
        );
        assert!(
            has_text(1830, 1918),
            "RAM icon and usage must be visible on the right"
        );
    }
}

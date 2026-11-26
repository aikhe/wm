//! Private protocol for two binaries built together. No cross-version ABI promise.
//! The WM is the sole writer. Every mapped word is atomic: a sequence counter
//! around ordinary Rust fields would still allow data races/torn reads.
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

use windows::Win32::Foundation::{
    CloseHandle, E_FAIL, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE, HWND,
    INVALID_HANDLE_VALUE, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowW, GetClassNameW,
    GetWindowThreadProcessId, HWND_BROADCAST, MSG, PostMessageW, RegisterClassW,
    RegisterWindowMessageW, WINDOW_EX_STYLE, WM_APP, WNDCLASSW, WS_POPUP,
};
use windows::core::{Error, PCWSTR, Result, w};

pub const HOST_CLASS: PCWSTR = w!("wm.bar.host.v1");
pub const BAR_CLASS: PCWSTR = w!("wm.bar.window.v1");
pub const ATTACH: u32 = WM_APP + 64;
pub const ACK: u32 = WM_APP + 65;
pub const COMMAND: u32 = WM_APP + 66;
const MAGIC: u64 = 0x574d_4241_5200_0002;
const WORDS: usize = 7;
pub const DIAG_CAPACITY: usize = 32;
pub const DIAG_KINDS: usize = 8;

/// Categories of runtime anomalies tracked in the lock-free doctor ring buffer.
/// Never carries window titles or user data.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagKind {
    PlaceFailed = 0,
    CloakFailed = 1,
    ReloadRejected = 2,
    StaleEvent = 3,
    AgentRefused = 4,
    OffstageRecovered = 5,
    UncloakTrap = 6,
    HotkeyFailed = 7,
}

impl DiagKind {
    pub fn from_u32(value: u32) -> Option<Self> {
        Some(match value {
            0 => Self::PlaceFailed,
            1 => Self::CloakFailed,
            2 => Self::ReloadRejected,
            3 => Self::StaleEvent,
            4 => Self::AgentRefused,
            5 => Self::OffstageRecovered,
            6 => Self::UncloakTrap,
            7 => Self::HotkeyFailed,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::PlaceFailed => "place_failed",
            Self::CloakFailed => "cloak_failed",
            Self::ReloadRejected => "reload_rejected",
            Self::StaleEvent => "stale_event",
            Self::AgentRefused => "agent_refused",
            Self::OffstageRecovered => "offstage_recovered",
            Self::UncloakTrap => "uncloak_trap",
            Self::HotkeyFailed => "hotkey_failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagEvent {
    pub sequence: u64,
    pub kind: DiagKind,
    pub hwnd: u64,
    pub code: u32,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostics {
    pub started_unix_ms: u64,
    pub total: u64,
    pub counts: [u64; DIAG_KINDS],
    pub recent: Vec<DiagEvent>,
}

/// Small enough to publish after every logical change, even during animations.
/// `focused` is an opaque HWND value, never a pointer to dereference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub active: u32,
    pub workspaces: u32,
    pub focused: u64,
    /// 0: empty, 1: tiled, 2: floating, 3: sticky, 4: on top, 5: scratchpad.
    pub mode: u32,
    pub overview: bool,
    pub paused: bool,
    pub tiled: u32,
    pub floating: u32,
    pub scratchpad: u32,
    pub scratchpad_shown: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            active: 0,
            workspaces: 1,
            focused: 0,
            mode: 0,
            overview: false,
            paused: false,
            tiled: 0,
            floating: 0,
            scratchpad: 0,
            scratchpad_shown: false,
        }
    }
}

impl Snapshot {
    fn words(self) -> [u64; WORDS] {
        let flags = u64::from(self.overview)
            | (u64::from(self.paused) << 1)
            | (u64::from(self.scratchpad_shown) << 2);
        [
            u64::from(self.active),
            u64::from(self.workspaces),
            self.focused,
            u64::from(self.mode),
            flags,
            (u64::from(self.tiled) << 32) | u64::from(self.floating),
            u64::from(self.scratchpad),
        ]
    }

    fn decode(words: [u64; WORDS]) -> Option<Self> {
        let active = u32::try_from(words[0]).ok()?;
        let workspaces = u32::try_from(words[1]).ok()?;
        let flags = words[4];
        let scratchpad = u32::try_from(words[6]).ok()?;
        (active < workspaces && words[3] <= 5 && flags & !0b111 == 0).then_some(Self {
            active,
            workspaces,
            focused: words[2],
            mode: words[3] as u32,
            overview: flags & 1 != 0,
            paused: flags & 2 != 0,
            scratchpad_shown: flags & 4 != 0,
            tiled: (words[5] >> 32) as u32,
            floating: words[5] as u32,
            scratchpad,
        })
    }
}

#[repr(C)]
struct DiagSlot {
    /// Odd while the writer updates this slot; even (`sequence * 2`) when committed.
    seq: AtomicU64,
    kind_and_code: AtomicU64,
    hwnd: AtomicU64,
    elapsed_ms: AtomicU64,
}

impl DiagSlot {
    #[cfg(test)]
    const fn zeroed() -> Self {
        Self {
            seq: AtomicU64::new(0),
            kind_and_code: AtomicU64::new(0),
            hwnd: AtomicU64::new(0),
            elapsed_ms: AtomicU64::new(0),
        }
    }

    fn write(&self, sequence: u64, kind: DiagKind, hwnd: u64, code: u32, elapsed_ms: u64) {
        let writing = sequence.saturating_mul(2).saturating_sub(1);
        let committed = sequence.saturating_mul(2);
        self.seq.store(writing, SeqCst);
        self.kind_and_code
            .store((u64::from(kind as u32) << 32) | u64::from(code), SeqCst);
        self.hwnd.store(hwnd, SeqCst);
        self.elapsed_ms.store(elapsed_ms, SeqCst);
        self.seq.store(committed, SeqCst);
    }

    fn read(&self) -> Option<DiagEvent> {
        for _ in 0..4 {
            let before = self.seq.load(SeqCst);
            if before == 0 || before & 1 != 0 {
                return None;
            }
            let kind_and_code = self.kind_and_code.load(SeqCst);
            let hwnd = self.hwnd.load(SeqCst);
            let elapsed_ms = self.elapsed_ms.load(SeqCst);
            if before == self.seq.load(SeqCst) {
                let kind = DiagKind::from_u32((kind_and_code >> 32) as u32)?;
                return Some(DiagEvent {
                    sequence: before / 2,
                    kind,
                    hwnd,
                    code: kind_and_code as u32,
                    elapsed_ms,
                });
            }
        }
        None
    }
}

#[repr(C)]
struct Shared {
    magic: AtomicU64,
    sequence: AtomicU64,
    words: [AtomicU64; WORDS],
    started_unix_ms: AtomicU64,
    diag_total: AtomicU64,
    diag_counts: [AtomicU64; DIAG_KINDS],
    diag_ring: [DiagSlot; DIAG_CAPACITY],
}

impl Shared {
    fn publish(&self, state: Snapshot) -> u64 {
        self.sequence.fetch_add(1, SeqCst);
        for (word, value) in self.words.iter().zip(state.words()) {
            word.store(value, SeqCst);
        }
        let sequence = self.sequence.fetch_add(1, SeqCst) + 1;
        self.magic.store(MAGIC, SeqCst);
        sequence
    }

    fn set_started(&self, unix_ms: u64) {
        self.started_unix_ms.store(unix_ms, SeqCst);
    }

    fn record_diag(&self, kind: DiagKind, hwnd: u64, code: u32, elapsed_ms: u64) {
        let sequence = self.diag_total.fetch_add(1, SeqCst) + 1;
        self.diag_counts[kind as usize].fetch_add(1, SeqCst);
        let slot = &self.diag_ring[((sequence - 1) as usize) % DIAG_CAPACITY];
        slot.write(sequence, kind, hwnd, code, elapsed_ms);
    }

    fn read_diag(&self) -> Option<Diagnostics> {
        if self.magic.load(SeqCst) != MAGIC {
            return None;
        }
        let started_unix_ms = self.started_unix_ms.load(SeqCst);
        let total = self.diag_total.load(SeqCst);
        let counts = std::array::from_fn(|index| self.diag_counts[index].load(SeqCst));
        let mut recent: Vec<_> = self.diag_ring.iter().filter_map(DiagSlot::read).collect();
        recent.sort_by_key(|event| event.sequence);
        Some(Diagnostics {
            started_unix_ms,
            total,
            counts,
            recent,
        })
    }

    fn read(&self) -> Option<(u64, Snapshot)> {
        if self.magic.load(SeqCst) != MAGIC {
            return None;
        }
        // Never spin on a paused/dead writer. The next notification retries.
        for _ in 0..8 {
            let before = self.sequence.load(SeqCst);
            if before == 0 || before & 1 != 0 {
                continue;
            }
            let words = std::array::from_fn(|index| self.words[index].load(SeqCst));
            if before == self.sequence.load(SeqCst) {
                return Snapshot::decode(words).map(|state| (before, state));
            }
        }
        None
    }
}

struct Mapping {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl Mapping {
    fn name(pid: u32) -> Vec<u16> {
        format!("Local\\wm.bar.v2.{pid}")
            .encode_utf16()
            .chain(Some(0))
            .collect()
    }

    fn new(pid: u32, write: bool) -> Result<Self> {
        let name = Self::name(pid);
        let handle = unsafe {
            if write {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    0,
                    size_of::<Shared>() as u32,
                    PCWSTR(name.as_ptr()),
                )?
            } else {
                OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr()))?
            }
        };
        let view = unsafe {
            MapViewOfFile(
                handle,
                if write { FILE_MAP_WRITE } else { FILE_MAP_READ },
                0,
                0,
                size_of::<Shared>(),
            )
        };
        if view.Value.is_null() {
            let error = Error::from_thread();
            let _ = unsafe { CloseHandle(handle) };
            return Err(error);
        }
        // Page-file mappings start zeroed. Atomic integer representations,
        // including zero, are valid; views have page alignment.
        Ok(Self { handle, view })
    }

    fn shared(&self) -> &Shared {
        // Mapping outlives this reference; readers only perform atomic loads.
        unsafe { &*self.view.Value.cast::<Shared>() }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let _ = unsafe { UnmapViewOfFile(self.view) };
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

pub fn update_message() -> u32 {
    unsafe { RegisterWindowMessageW(w!("wm.bar.changed.v1")) }
}

pub fn find_host() -> Option<HWND> {
    unsafe { FindWindowW(HOST_CLASS, None) }.ok()
}

pub fn has_class(hwnd: HWND, class: PCWSTR) -> bool {
    let mut name = [0u16; 64];
    let length = unsafe { GetClassNameW(hwnd, &mut name) } as usize;
    length != 0 && &name[..length] == unsafe { class.as_wide() }
}

/// Register only a procedure which never calls an external app.
pub fn register_class(
    name: PCWSTR,
    procedure: windows::Win32::UI::WindowsAndMessaging::WNDPROC,
) -> Result<()> {
    let class = WNDCLASSW {
        lpfnWndProc: procedure,
        hInstance: HINSTANCE(unsafe { GetModuleHandleW(None)? }.0),
        lpszClassName: name,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0
        && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS
    {
        return Err(Error::from_thread());
    }
    Ok(())
}

unsafe extern "system" fn host_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Workspace(u32),
    Overview,
    Agent(usize),
}

/// A hidden window on the WM thread plus a shared atomic file mapping. Exactly one
/// notification can be outstanding; the bar ACKs the revision it actually read.
pub struct Host {
    pub window: HWND,
    mapping: Mapping,
    subscriber: Option<HWND>,
    pending: bool,
    message: u32,
    revision: u64,
    last: Option<Snapshot>,
    started: std::time::Instant,
}

impl Host {
    pub fn new() -> Result<Self> {
        if find_host().is_some() {
            return Err(Error::new(E_FAIL, "wm is already running in this session"));
        }
        Self::create(HOST_CLASS)
    }

    fn create(class: PCWSTR) -> Result<Self> {
        register_class(class, Some(host_proc))?;
        let mapping = Mapping::new(std::process::id(), true)?;
        let started_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        mapping.shared().set_started(started_unix_ms);
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("wm"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(HINSTANCE(GetModuleHandleW(None)?.0)),
                None,
            )?
        };
        Ok(Self {
            window,
            mapping,
            subscriber: None,
            pending: false,
            message: update_message(),
            revision: 0,
            last: None,
            started: std::time::Instant::now(),
        })
    }

    pub fn record_diag(&self, kind: DiagKind, hwnd: u64, code: u32) {
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        self.mapping
            .shared()
            .record_diag(kind, hwnd, code, elapsed_ms);
    }

    /// Separate from construction so tests can use hidden fixtures silently.
    pub fn announce(&self) {
        let _ = unsafe {
            PostMessageW(
                Some(HWND_BROADCAST),
                self.message,
                WPARAM(self.window.0 as usize),
                LPARAM(0),
            )
        };
    }

    pub fn publish(&mut self, state: Snapshot) {
        if self.last == Some(state) {
            return;
        }
        self.revision = self.mapping.shared().publish(state);
        self.last = Some(state);
        self.notify();
    }

    fn notify(&mut self) {
        if self.pending {
            return;
        }
        if let Some(bar) = self.subscriber {
            if has_class(bar, BAR_CLASS)
                && unsafe {
                    PostMessageW(
                        Some(bar),
                        self.message,
                        WPARAM(self.window.0 as usize),
                        LPARAM(0),
                    )
                }
                .is_ok()
            {
                self.pending = true;
            } else {
                self.subscriber = None;
            }
        }
    }

    /// Called only on queued messages to this endpoint. Sent messages run the
    /// inert window procedure, so another process cannot re-enter WM actions.
    pub fn handle(&mut self, message: &MSG) -> Option<Command> {
        if message.hwnd != self.window {
            return None;
        }
        match message.message {
            ATTACH => {
                let bar = HWND(message.wParam.0 as *mut _);
                if has_class(bar, BAR_CLASS) {
                    self.subscriber = Some(bar);
                    self.pending = false;
                    self.notify();
                }
            }
            ACK => {
                if self
                    .subscriber
                    .is_some_and(|bar| bar.0 as usize == message.wParam.0)
                {
                    self.pending = false;
                    if message.lParam.0 as u64 != self.revision {
                        self.notify();
                    }
                }
            }
            COMMAND if message.lParam.0 == -1 => return Some(Command::Agent(message.wParam.0)),
            COMMAND if message.lParam.0 as u64 == self.revision => {
                let code = message.wParam.0;
                if code == 0 {
                    return Some(Command::Overview);
                }
                if let Some(state) = self.last
                    && code - 1 < state.workspaces as usize
                {
                    return Some(Command::Workspace((code - 1) as u32));
                }
            }
            _ => {}
        }
        None
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(bar) = self.subscriber {
            let _ = unsafe { PostMessageW(Some(bar), self.message, WPARAM(0), LPARAM(0)) };
        }
        let _ = unsafe { DestroyWindow(self.window) };
    }
}

/// Post an action index without waiting for the WM to process it.
pub fn post_agent(host: HWND, action: usize) -> Result<()> {
    unsafe { PostMessageW(Some(host), COMMAND, WPARAM(action), LPARAM(-1)) }
}

/// Read the running WM's snapshot and diagnostic ring buffer without attaching
/// or waking the WM thread. Returns `None` when `wm` is not running or has not
/// published yet.
pub fn peek() -> Option<(u64, Snapshot, Diagnostics)> {
    let host = find_host()?;
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(host, Some(&mut pid)) };
    if pid == 0 || !has_class(host, HOST_CLASS) {
        return None;
    }
    let mapping = Mapping::new(pid, false).ok()?;
    let shared = mapping.shared();
    let (revision, state) = shared.read()?;
    let diag = shared.read_diag()?;
    Some((revision, state, diag))
}

pub struct Client {
    pub host: HWND,
    mapping: Mapping,
}

impl Client {
    pub fn connect(host: HWND, bar: HWND) -> Result<Self> {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(host, Some(&mut pid)) };
        if pid == 0 || !has_class(host, HOST_CLASS) {
            return Err(Error::new(E_FAIL, "wm is not running"));
        }
        let mapping = Mapping::new(pid, false)?;
        unsafe { PostMessageW(Some(host), ATTACH, WPARAM(bar.0 as usize), LPARAM(0))? };
        Ok(Self { host, mapping })
    }

    pub fn read(&self) -> Option<(u64, Snapshot)> {
        self.mapping.shared().read()
    }

    pub fn acknowledge(&self, bar: HWND, revision: u64) {
        let _ = unsafe {
            PostMessageW(
                Some(self.host),
                ACK,
                WPARAM(bar.0 as usize),
                LPARAM(revision as isize),
            )
        };
    }

    pub fn command(&self, command: Command, revision: u64) {
        let code = match command {
            Command::Workspace(index) => index as usize + 1,
            Command::Overview => 0,
            Command::Agent(_) => return,
        };
        let _ = unsafe {
            PostMessageW(
                Some(self.host),
                COMMAND,
                WPARAM(code),
                LPARAM(revision as isize),
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn block() -> Shared {
        Shared {
            magic: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
            words: std::array::from_fn(|_| AtomicU64::new(0)),
            started_unix_ms: AtomicU64::new(0),
            diag_total: AtomicU64::new(0),
            diag_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            diag_ring: std::array::from_fn(|_| DiagSlot::zeroed()),
        }
    }

    #[test]
    fn diagnostic_ring_buffer_wraps_and_preserves_counts() {
        let shared = block();
        shared.set_started(123_456);
        shared.publish(Snapshot {
            active: 1,
            workspaces: 3,
            paused: true,
            tiled: 4,
            floating: 1,
            scratchpad: 2,
            scratchpad_shown: true,
            mode: 5,
            ..Default::default()
        });
        let (_, state) = shared.read().unwrap();
        assert!(state.paused && state.scratchpad_shown);
        assert_eq!((state.tiled, state.floating, state.scratchpad, state.mode), (4, 1, 2, 5));

        for i in 0..(DIAG_CAPACITY as u64 + 5) {
            let kind = if i % 2 == 0 {
                DiagKind::PlaceFailed
            } else {
                DiagKind::StaleEvent
            };
            shared.record_diag(kind, 0x1000 + i, i as u32, i * 10);
        }
        let diag = shared.read_diag().unwrap();
        assert_eq!(diag.started_unix_ms, 123_456);
        assert_eq!(diag.total, DIAG_CAPACITY as u64 + 5);
        assert_eq!(diag.recent.len(), DIAG_CAPACITY);
        assert_eq!(diag.recent[0].sequence, 6);
        assert_eq!(diag.recent.last().unwrap().sequence, DIAG_CAPACITY as u64 + 5);
        assert_eq!(
            diag.counts[DiagKind::PlaceFailed as usize]
                + diag.counts[DiagKind::StaleEvent as usize],
            diag.total
        );
    }

    #[test]
    fn concurrent_snapshots_are_complete_and_dead_writers_do_not_spin() {
        let shared = Arc::new(block());
        assert_eq!(shared.read(), None);
        let writer = shared.clone();
        let thread = std::thread::spawn(move || {
            for n in 1..20_000 {
                writer.publish(Snapshot {
                    active: n - 1,
                    workspaces: n,
                    focused: u64::from(n),
                    ..Default::default()
                });
            }
        });
        for _ in 0..20_000 {
            if let Some((_, state)) = shared.read() {
                assert_eq!(state.active + 1, state.workspaces);
                assert_eq!(state.focused, u64::from(state.workspaces));
            }
        }
        thread.join().unwrap();
        shared.sequence.fetch_add(1, SeqCst);
        assert_eq!(shared.read(), None);
    }

    #[test]
    fn read_only_mapping_reads_publication_and_stale_commands_are_rejected() {
        let mut host = Host::create(w!("wm.bar.test.host")).unwrap();
        host.publish(Snapshot {
            workspaces: 3,
            ..Default::default()
        });
        let reader = Mapping::new(std::process::id(), false).unwrap();
        let (revision, state) = reader.shared().read().unwrap();
        assert_eq!(state.workspaces, 3);
        let mut message = MSG {
            hwnd: host.window,
            message: COMMAND,
            wParam: WPARAM(3),
            lParam: LPARAM(revision as isize),
            ..Default::default()
        };
        assert_eq!(host.handle(&message), Some(Command::Workspace(2)));
        host.publish(Snapshot {
            workspaces: 2,
            ..Default::default()
        });
        assert_eq!(
            host.handle(&message),
            None,
            "workspace indices may have shifted"
        );
        message.lParam = LPARAM(host.revision as isize);
        assert_eq!(host.handle(&message), None, "index is now out of range");
        message.wParam = WPARAM(0);
        assert_eq!(host.handle(&message), Some(Command::Overview));
        message.wParam = WPARAM(31);
        message.lParam = LPARAM(-1);
        assert_eq!(host.handle(&message), Some(Command::Agent(31)));

        // A stalled bar gets one notification, regardless of how often the
        // WM changes. ACKing an old revision delivers the latest state next.
        register_class(BAR_CLASS, Some(host_proc)).unwrap();
        let bar = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                BAR_CLASS,
                w!("fixture"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        host.handle(&MSG {
            hwnd: host.window,
            message: ATTACH,
            wParam: WPARAM(bar.0 as usize),
            ..Default::default()
        });
        for n in 1..100 {
            host.publish(Snapshot {
                focused: n,
                ..Default::default()
            });
        }
        let update = host.message;
        let drain = || {
            let mut count = 0;
            let mut message = MSG::default();
            while unsafe {
                windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                    &mut message,
                    Some(bar),
                    update,
                    update,
                    windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
                )
            }
            .as_bool()
            {
                count += 1;
            }
            count
        };
        assert_eq!(drain(), 1);
        host.handle(&MSG {
            hwnd: host.window,
            message: ACK,
            wParam: WPARAM(bar.0 as usize),
            lParam: LPARAM(revision as isize),
            ..Default::default()
        });
        assert_eq!(drain(), 1);
        host.handle(&MSG {
            hwnd: host.window,
            message: ACK,
            wParam: WPARAM(bar.0 as usize),
            lParam: LPARAM(host.revision as isize),
            ..Default::default()
        });
        assert_eq!(drain(), 0);
        unsafe { DestroyWindow(bar) }.unwrap();
    }
}

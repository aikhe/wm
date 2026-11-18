//! A sleeping Windows change-notification worker. It only reads/parses after
//! a save (or explicit reload); the desktop's message loop never waits on I/O.
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{
    CloseHandle, HANDLE, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Storage::FileSystem::{
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_APP};
use windows::core::PCWSTR;

use crate::config::{self, Config};

pub const MESSAGE: u32 = WM_APP + 1;
const SETTLE_MS: u32 = 150;
type Update = Result<Config, String>;

/// Kernel handles can be used across threads. Store their value as an integer;
/// the owning guard outlives the worker and closes it only after joining.
struct Signal(isize);

impl Signal {
    fn new(manual: bool) -> Result<Self, String> {
        unsafe { CreateEventW(None, manual, false, None) }
            .map(|handle| Self(handle.0 as isize))
            .map_err(|error| error.to_string())
    }
    fn handle(&self) -> HANDLE {
        HANDLE(self.0 as *mut c_void)
    }
    fn set(&self) {
        let _ = unsafe { SetEvent(self.handle()) };
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.handle()) };
    }
}

struct Changes(isize);
impl Changes {
    fn handle(&self) -> HANDLE {
        HANDLE(self.0 as *mut c_void)
    }
}
impl Drop for Changes {
    fn drop(&mut self) {
        let _ = unsafe { FindCloseChangeNotification(self.handle()) };
    }
}

pub struct Watcher {
    stop: Signal,
    reload: Signal,
    worker: Option<JoinHandle<()>>,
    updates: Receiver<Update>,
}

impl Watcher {
    /// Arm the directory before spawning; the worker's initial comparison
    /// catches a save between the caller's first read and this registration.
    pub fn new(
        path: PathBuf,
        initial: (String, String),
        thread: Option<u32>,
    ) -> Result<Self, String> {
        let parent = path
            .parent()
            .ok_or("configuration has no parent directory")?;
        let wide: Vec<_> = parent.as_os_str().encode_wide().chain(Some(0)).collect();
        let changes = Changes(
            unsafe {
                FindFirstChangeNotificationW(
                    PCWSTR(wide.as_ptr()),
                    false,
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_LAST_WRITE
                        | FILE_NOTIFY_CHANGE_SIZE,
                )
            }
            .map_err(|error| format!("couldn't watch {}: {error}", parent.display()))?
            .0 as isize,
        );
        let stop = Signal::new(true)?;
        let reload = Signal::new(false)?;
        let (stop_value, reload_value) = (stop.0, reload.0);
        let (sender, updates) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("wm config".into())
            .spawn(move || {
                let handles = [
                    HANDLE(stop_value as *mut c_void),
                    changes.handle(),
                    HANDLE(reload_value as *mut c_void),
                ];
                let send = |update| {
                    if sender.send(update).is_err() {
                        return false;
                    }
                    if let Some(thread) = thread {
                        let _ =
                            unsafe { PostThreadMessageW(thread, MESSAGE, WPARAM(0), LPARAM(0)) };
                    }
                    true
                };
                let mut last: Result<(String, String), String> = Ok(initial);
                let mut pending = true;
                let mut force = false;
                let mut retries = 0;
                loop {
                    let event = unsafe {
                        WaitForMultipleObjects(
                            &handles,
                            false,
                            if pending { SETTLE_MS } else { INFINITE },
                        )
                    };
                    if event == WAIT_OBJECT_0 {
                        break;
                    }
                    if event.0 == WAIT_OBJECT_0.0 + 1 {
                        if let Err(error) = unsafe { FindNextChangeNotification(changes.handle()) }
                        {
                            send(Err(format!("config watcher stopped: {error}")));
                            break;
                        }
                        pending = true;
                        retries = 0;
                        continue;
                    }
                    if event.0 == WAIT_OBJECT_0.0 + 2 {
                        pending = true;
                        force = true;
                        retries = 0;
                        continue;
                    }
                    if event != WAIT_TIMEOUT {
                        send(Err(format!(
                            "config watcher wait failed: {}",
                            windows::core::Error::from_thread()
                        )));
                        break;
                    }
                    let contents = config::read_all(&path);
                    // Some editors briefly lock/remove the file. Retry only after
                    // a notification, bounded to 750ms; no periodic idle wakeups.
                    if contents.is_err() && retries < 4 {
                        retries += 1;
                        continue;
                    }
                    pending = false;
                    retries = 0;
                    if contents == last && !force {
                        continue;
                    }
                    force = false;
                    let parsed =
                        contents
                            .as_ref()
                            .map_err(Clone::clone)
                            .and_then(|(text, saved)| {
                                Config::parse_with(text, saved)
                                    .map_err(|error| format!("{}: {error}", path.display()))
                            });
                    last = contents;
                    if !send(parsed) {
                        break;
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            stop,
            reload,
            worker: Some(worker),
            updates,
        })
    }

    pub fn reload(&self) {
        self.reload.set();
    }

    /// Coalesce queued saves; only the latest complete result is relevant.
    pub fn take(&self) -> Option<Update> {
        self.updates.try_iter().last()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use windows::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW};

    #[test]
    fn saves_replacements_errors_and_recovery_are_event_driven() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder =
            std::env::temp_dir().join(format!("wm-config-test-{}-{unique}", std::process::id()));
        fs::create_dir(&folder).unwrap();
        let path = folder.join("config.toml");
        fs::write(&path, "").unwrap();
        let watcher = Watcher::new(path.clone(), (String::new(), String::new()), None).unwrap();
        assert!(
            watcher
                .updates
                .recv_timeout(Duration::from_millis(350))
                .is_err(),
            "no reloads while idle"
        );

        fs::write(&path, "[layout]\ngap=12").unwrap();
        assert_eq!(
            watcher
                .updates
                .recv_timeout(Duration::from_secs(4))
                .unwrap()
                .unwrap()
                .layout
                .gap,
            12
        );
        fs::write(&path, "[layout]\ngap=-1").unwrap();
        assert!(
            watcher
                .updates
                .recv_timeout(Duration::from_secs(4))
                .unwrap()
                .is_err()
        );

        let replacement = folder.join("config.tmp");
        fs::write(&replacement, "[layout]\ngap=16").unwrap();
        let wide = |path: &std::path::Path| {
            path.as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        };
        unsafe {
            MoveFileExW(
                PCWSTR(wide(&replacement).as_ptr()),
                PCWSTR(wide(&path).as_ptr()),
                MOVEFILE_REPLACE_EXISTING,
            )
        }
        .unwrap();
        assert_eq!(
            watcher
                .updates
                .recv_timeout(Duration::from_secs(4))
                .unwrap()
                .unwrap()
                .layout
                .gap,
            16
        );
        // Another file in the directory is not a config edit.
        fs::write(folder.join("unrelated"), "changed").unwrap();
        assert!(
            watcher
                .updates
                .recv_timeout(Duration::from_millis(350))
                .is_err()
        );
        // A manual retry must work even if a previous apply was rejected.
        watcher.reload();
        assert_eq!(
            watcher
                .updates
                .recv_timeout(Duration::from_secs(4))
                .unwrap()
                .unwrap()
                .layout
                .gap,
            16
        );
        drop(watcher);
        fs::remove_file(path).unwrap();
        fs::remove_file(folder.join("unrelated")).unwrap();
        fs::remove_dir(folder).unwrap();
    }
}

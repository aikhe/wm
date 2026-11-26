//! Hides windows the way virtual desktops do: the app keeps running and
//! rendering, and the compositor just stops drawing it. Showing it again is
//! instant because nothing has to repaint.
//!
//! Windows only offers this for other apps' windows through an undocumented
//! shell interface. The interface layouts below come from Ciantic's
//! AltTabAccessor (MIT), by way of komorebi.

use std::ffi::c_void;

use windows::Win32::Foundation::{E_FAIL, HANDLE, HWND};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, IServiceProvider,
};
use windows::Win32::UI::WindowsAndMessaging::{GetPropW, RemovePropW, SetPropW};
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, PCWSTR, Result, w};
use windows_core::interface;

use crate::window::Window;

const IMMERSIVE_SHELL: GUID = GUID::from_u128(0xc2f03a33_21f5_47fa_b4bb_156362a2f239);

/// A window property left on every window we cloak. It lives in Windows rather
/// than in this process, so it survives a crash and the next run can tell which
/// windows to bring back.
const MARK: PCWSTR = w!("wm.cloaked");

/// A window property left on the focused window while it waits off-stage
/// beneath the screen during an animation, so startup can pull it back after
/// a crash.
const PARKED_MARK: PCWSTR = w!("wm.parked");

const CLOAK: i32 = 2;
const UNCLOAK: i32 = 0;

// Only `set_cloak` is called. The methods before it are declared because the
// call is made by position in this list.
#[interface("372E1D3B-38D3-42E4-A15B-8AB2B178F513")]
unsafe trait IApplicationView: IUnknown {
    unsafe fn get_iids(&self, count: *mut u32, iids: *mut *mut GUID) -> HRESULT;
    unsafe fn get_runtime_class_name(&self, name: *mut c_void) -> HRESULT;
    unsafe fn get_trust_level(&self, level: *mut c_void) -> HRESULT;
    unsafe fn set_focus(&self) -> HRESULT;
    unsafe fn switch_to(&self) -> HRESULT;
    unsafe fn try_invoke_back(&self, callback: *mut c_void) -> HRESULT;
    unsafe fn get_thumbnail_window(&self, hwnd: *mut HWND) -> HRESULT;
    unsafe fn get_monitor(&self, monitor: *mut *mut c_void) -> HRESULT;
    unsafe fn get_visibility(&self, visibility: *mut i32) -> HRESULT;
    unsafe fn set_cloak(&self, cloak_type: u32, flags: i32) -> HRESULT;
}

// Likewise, only `get_view_for_hwnd` is called.
#[interface("1841C6D7-4F9D-42C0-AF41-8747538F10E5")]
unsafe trait IApplicationViewCollection: IUnknown {
    unsafe fn get_views(&self, views: *mut *mut c_void) -> HRESULT;
    unsafe fn get_views_by_zorder(&self, views: *mut *mut c_void) -> HRESULT;
    unsafe fn get_views_by_app_user_model_id(&self, id: PCWSTR, views: *mut *mut c_void)
    -> HRESULT;
    unsafe fn get_view_for_hwnd(&self, hwnd: HWND, view: *mut Option<IApplicationView>) -> HRESULT;
}

/// Cloaks and uncloaks other apps' windows through Explorer.
pub struct Cloaker {
    /// Explorer's view of the open windows, connected on first use.
    views: Option<IApplicationViewCollection>,
}

impl Cloaker {
    pub fn new() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Cloaker { views: None })
    }

    /// Cloaks or uncloaks the window. Returns `false` if Explorer refused the
    /// request, in which case the window stays as it was.
    pub fn set(&mut self, window: Window, cloaked: bool) -> bool {
        // Explorer may have restarted since we connected, so a failure gets one
        // more try on a fresh connection.
        if self.try_set(window, cloaked).is_err() {
            self.views = None;
            if self.try_set(window, cloaked).is_err() {
                return false;
            }
        }

        let _ = unsafe {
            if cloaked {
                SetPropW(window.0, MARK, Some(HANDLE(std::ptr::dangling_mut())))
            } else {
                RemovePropW(window.0, MARK).map(drop)
            }
        };
        true
    }

    /// Uncloaks every window we cloaked, in this run or an earlier one that
    /// didn't get to clean up.
    pub fn uncloak_all(&mut self) {
        for window in Window::visible().unwrap_or_default() {
            if is_cloaked(window) {
                self.set(window, false);
            }
        }
    }

    fn try_set(&mut self, window: Window, cloaked: bool) -> Result<()> {
        let views = match &self.views {
            Some(views) => views,
            None => self.views.insert(connect()?),
        };

        let mut view = None;
        unsafe { views.get_view_for_hwnd(window.0, &mut view).ok()? };
        let view = view.ok_or(E_FAIL)?;
        unsafe {
            view.set_cloak(1, if cloaked { CLOAK } else { UNCLOAK })
                .ok()
        }
    }
}

/// Whether we cloaked this window, as opposed to Windows or another app.
pub fn is_cloaked(window: Window) -> bool {
    !unsafe { GetPropW(window.0, MARK) }.0.is_null()
}

/// Marks a window while it is parked off-stage beneath the screen during an
/// animation, so a crash can detect and pull it back on startup.
pub fn mark_parked(window: Window, parked: bool) {
    let _ = unsafe {
        if parked {
            SetPropW(
                window.0,
                PARKED_MARK,
                Some(HANDLE(std::ptr::dangling_mut())),
            )
        } else {
            RemovePropW(window.0, PARKED_MARK).map(drop)
        }
    };
}

/// Whether this window was left off-stage by an interrupted animation.
pub fn is_parked(window: Window) -> bool {
    !unsafe { GetPropW(window.0, PARKED_MARK) }.0.is_null()
}

fn connect() -> Result<IApplicationViewCollection> {
    unsafe {
        let shell: IServiceProvider = CoCreateInstance(&IMMERSIVE_SHELL, None, CLSCTX_ALL)?;
        shell.QueryService(&IApplicationViewCollection::IID)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a desktop session with an app window open, and blinks that window
    /// for a moment, so it doesn't run by default. Worth running after a
    /// Windows update, since the interfaces it goes through are undocumented.
    #[test]
    #[ignore]
    fn cloaks_and_uncloaks_another_apps_window() {
        let open = Window::visible().expect("windows can be listed");
        let window = open
            .into_iter()
            .find(|window| window.is_tileable())
            .expect("an app window");
        let mut cloaker = Cloaker::new().expect("COM starts");

        cloaker.set(window, true);
        let hidden = (window.info().cloaked, is_cloaked(window));
        cloaker.set(window, false);
        let shown = (window.info().cloaked, is_cloaked(window));

        assert_eq!(hidden, (true, true), "cloaked and marked");
        assert_eq!(shown, (false, false), "uncloaked and unmarked");
    }
}

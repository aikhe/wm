//! Precision Touchpad multi-finger swipe gestures over Win32 Raw Input HID.
//!
//! Registers for HID Digitizer Touch Pad reports (`0x0D` / `0x05`) with
//! `RIDEV_INPUTSINK` only when `[gestures] enabled = true`, so idle CPU is
//! zero and no reports arrive unless fingers are touching the pad.

use std::collections::HashMap;
use std::ffi::c_void;

use windows::Win32::Devices::HumanInterfaceDevice::{
    HIDP_CAPS, HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS, HidP_GetCaps, HidP_GetUsageValue,
    HidP_GetUsages, HidP_GetValueCaps, HidP_Input, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIDI_PREPARSEDDATA, RIM_TYPEHID,
    RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_MESSAGE, MSG, RegisterClassW,
    WINDOW_EX_STYLE, WM_INPUT, WNDCLASSW, WS_POPUP,
};
use windows::core::{Error, PCWSTR, Result, w};

use crate::config::Gestures;
use crate::layout::Direction;

const CLASS: PCWSTR = w!("wm.gestures");
const USAGE_PAGE_DIGITIZER: u16 = 0x0D;
const USAGE_TOUCHPAD: u16 = 0x05;
const USAGE_TIP_SWITCH: u16 = 0x42;
const USAGE_CONTACT_ID: u16 = 0x51;
const USAGE_CONTACT_COUNT: u16 = 0x54;
const USAGE_PAGE_GENERIC: u16 = 0x01;
const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;

/// Deadzone fraction of the touchpad span before locking onto an axis.
const DEADZONE_FRAC: f64 = 0.03;

/// Velocity (in viewport widths/heights per second) that commits a short flick.
const FLICK_VELOCITY: f64 = 1.4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SwipeEvent {
    Start(Axis),
    Update { axis: Axis, offset_px: i32 },
    Commit { direction: Direction },
    Cancel,
}

#[derive(Debug, Clone)]
pub struct TouchFrame {
    pub timestamp_ms: u64,
    pub contacts: Vec<(u32, i32, i32)>,
    pub span_x: i32,
    pub span_y: i32,
}

/// Pure multi-finger swipe recognizer. Tracks finger centroid, locks an axis,
/// emits live pixel offsets for `Stage` scrubbing, and commits or cancels on
/// lift based on displacement and flick velocity.
pub struct SwipeTracker {
    config: Gestures,
    active: bool,
    anchor: (f64, f64),
    axis: Option<Axis>,
    last_delta: f64,
    last_time_ms: u64,
    velocity: f64,
}

impl SwipeTracker {
    pub fn new(config: Gestures) -> Self {
        Self {
            config,
            active: false,
            anchor: (0.0, 0.0),
            axis: None,
            last_delta: 0.0,
            last_time_ms: 0,
            velocity: 0.0,
        }
    }

    pub fn configure(&mut self, config: Gestures) {
        self.config = config;
        self.reset();
    }

    pub fn reset(&mut self) {
        self.active = false;
        self.axis = None;
        self.last_delta = 0.0;
        self.velocity = 0.0;
    }

    /// Consumes one touchpad frame and produces any resulting gesture event.
    /// `viewport_w` and `viewport_h` scale normalized deltas into screen pixels.
    pub fn push(
        &mut self,
        frame: &TouchFrame,
        viewport_w: i32,
        viewport_h: i32,
    ) -> Option<SwipeEvent> {
        if !self.config.enabled {
            return None;
        }
        let wanted = self.config.fingers as usize;
        if frame.contacts.len() < wanted {
            return self.finish();
        }
        if frame.contacts.len() > wanted {
            return self.cancel();
        }
        let span_x = f64::from(frame.span_x.max(1));
        let span_y = f64::from(frame.span_y.max(1));
        let count = frame.contacts.len() as f64;
        let cx = frame.contacts.iter().map(|(_, x, _)| f64::from(*x)).sum::<f64>() / count;
        let cy = frame.contacts.iter().map(|(_, _, y)| f64::from(*y)).sum::<f64>() / count;

        if !self.active {
            self.active = true;
            self.anchor = (cx, cy);
            self.axis = None;
            self.last_delta = 0.0;
            self.last_time_ms = frame.timestamp_ms;
            self.velocity = 0.0;
            return None;
        }

        let dx = (cx - self.anchor.0) / span_x;
        let dy = (cy - self.anchor.1) / span_y;

        if self.axis.is_none() {
            if dx.abs() < DEADZONE_FRAC && dy.abs() < DEADZONE_FRAC {
                return None;
            }
            let axis = if dx.abs() >= dy.abs() {
                if !self.config.horizontal {
                    return None;
                }
                Axis::Horizontal
            } else {
                if !self.config.vertical {
                    return None;
                }
                Axis::Vertical
            };
            self.axis = Some(axis);
            self.last_delta = if axis == Axis::Horizontal { dx } else { dy };
            self.last_time_ms = frame.timestamp_ms;
            return Some(SwipeEvent::Start(axis));
        }

        let axis = self.axis?;
        let delta = if axis == Axis::Horizontal { dx } else { dy };
        let dt_ms = frame.timestamp_ms.saturating_sub(self.last_time_ms);
        if dt_ms > 0 {
            let instant_v = (delta - self.last_delta) / (dt_ms as f64 / 1000.0);
            self.velocity = 0.6 * instant_v + 0.4 * self.velocity;
        }
        self.last_delta = delta;
        self.last_time_ms = frame.timestamp_ms;

        let sensitivity = f64::from(self.config.sensitivity) / 100.0;
        let sign = if self.config.natural { 1.0 } else { -1.0 };
        let extent = f64::from(match axis {
            Axis::Horizontal => viewport_w,
            Axis::Vertical => viewport_h,
        });
        let offset_px = (delta * sign * sensitivity * extent).round() as i32;
        Some(SwipeEvent::Update { axis, offset_px })
    }

    fn finish(&mut self) -> Option<SwipeEvent> {
        let axis = self.axis.take();
        let delta = self.last_delta;
        let velocity = self.velocity;
        self.reset();
        let axis = axis?;
        let threshold = f64::from(self.config.threshold_percent) / 100.0;
        let flick = delta.abs() >= threshold / 3.0
            && velocity.abs() >= FLICK_VELOCITY
            && delta.signum() == velocity.signum();
        if delta.abs() < threshold && !flick {
            return Some(SwipeEvent::Cancel);
        }
        let positive = if self.config.natural {
            delta < 0.0
        } else {
            delta > 0.0
        };
        let direction = match (axis, positive) {
            (Axis::Horizontal, true) => Direction::Right,
            (Axis::Horizontal, false) => Direction::Left,
            (Axis::Vertical, true) => Direction::Down,
            (Axis::Vertical, false) => Direction::Up,
        };
        Some(SwipeEvent::Commit { direction })
    }

    fn cancel(&mut self) -> Option<SwipeEvent> {
        let had_axis = self.axis.is_some();
        self.reset();
        had_axis.then_some(SwipeEvent::Cancel)
    }
}

struct DeviceCaps {
    preparsed: Vec<u8>,
    count_collection: u16,
    contact_collections: Vec<u16>,
    min_x: i32,
    max_x: i32,
    min_y: i32,
    max_y: i32,
}

/// Win32 Raw Input listener for Precision Touchpads. Only registers with
/// Windows when `[gestures] enabled = true`.
pub struct Touchpad {
    window: HWND,
    registered: bool,
    devices: HashMap<isize, DeviceCaps>,
    tracker: SwipeTracker,
    epoch: std::time::Instant,
}

impl Touchpad {
    pub fn new(config: Gestures) -> Result<Self> {
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
        let window = unsafe {
            let class = WNDCLASSW {
                lpfnWndProc: Some(default_behaviour),
                hInstance: instance,
                lpszClassName: CLASS,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(Error::from_thread());
            }
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!("wm gestures"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                None,
            )?
        };
        let mut touchpad = Self {
            window,
            registered: false,
            devices: HashMap::new(),
            tracker: SwipeTracker::new(config.clone()),
            epoch: std::time::Instant::now(),
        };
        if config.enabled {
            let _ = touchpad.set_registered(true);
        }
        Ok(touchpad)
    }

    pub fn configure(&mut self, config: Gestures) {
        let want = config.enabled;
        self.tracker.configure(config);
        let _ = self.set_registered(want);
    }

    pub fn owns(&self, message: &MSG) -> bool {
        message.hwnd == self.window && message.message == WM_INPUT
    }

    pub fn cancel(&mut self) {
        self.tracker.reset();
    }

    /// Decodes a `WM_INPUT` message from the queue and advances the swipe tracker.
    pub fn handle(
        &mut self,
        message: &MSG,
        viewport_w: i32,
        viewport_h: i32,
    ) -> Option<SwipeEvent> {
        if !self.registered || !self.owns(message) {
            return None;
        }
        let frame = self.read_raw_input(HRAWINPUT(message.lParam.0 as *mut c_void))?;
        self.tracker.push(&frame, viewport_w, viewport_h)
    }

    fn set_registered(&mut self, enable: bool) -> Result<()> {
        if self.registered == enable {
            return Ok(());
        }
        let device = RAWINPUTDEVICE {
            usUsagePage: USAGE_PAGE_DIGITIZER,
            usUsage: USAGE_TOUCHPAD,
            dwFlags: if enable { RIDEV_INPUTSINK } else { RIDEV_REMOVE },
            hwndTarget: if enable { self.window } else { HWND::default() },
        };
        unsafe { RegisterRawInputDevices(&[device], size_of::<RAWINPUTDEVICE>() as u32)? };
        self.registered = enable;
        Ok(())
    }

    fn read_raw_input(&mut self, handle: HRAWINPUT) -> Option<TouchFrame> {
        let mut size = 0u32;
        let header_size = size_of::<RAWINPUTHEADER>() as u32;
        unsafe {
            if GetRawInputData(handle, RID_INPUT, None, &mut size, header_size) != 0 || size == 0 {
                return None;
            }
        }
        let mut buffer = vec![0u8; size as usize];
        let copied = unsafe {
            GetRawInputData(
                handle,
                RID_INPUT,
                Some(buffer.as_mut_ptr().cast()),
                &mut size,
                header_size,
            )
        };
        if copied == u32::MAX || (copied as usize) < size_of::<RAWINPUTHEADER>() {
            return None;
        }
        let raw = unsafe { &*(buffer.as_ptr().cast::<RAWINPUT>()) };
        if raw.header.dwType != RIM_TYPEHID.0 {
            return None;
        }
        let device_key = raw.header.hDevice.0 as isize;
        let hid_size = unsafe { raw.data.hid.dwSizeHid } as usize;
        let hid_count = unsafe { raw.data.hid.dwCount } as usize;
        if hid_size == 0 || hid_count == 0 {
            return None;
        }
        let data_offset = size_of::<RAWINPUTHEADER>() + 8;
        if data_offset + hid_size * hid_count > buffer.len() {
            return None;
        }
        let report = &mut buffer[data_offset..data_offset + hid_size];

        if !self.devices.contains_key(&device_key) {
            let caps = load_device_caps(raw.header.hDevice)?;
            self.devices.insert(device_key, caps);
        }
        let caps = self.devices.get(&device_key)?;
        parse_report(caps, report, self.epoch.elapsed().as_millis() as u64)
    }
}

impl Drop for Touchpad {
    fn drop(&mut self) {
        let _ = self.set_registered(false);
        let _ = unsafe { DestroyWindow(self.window) };
    }
}

fn load_device_caps(device: HANDLE) -> Option<DeviceCaps> {
    let mut size = 0u32;
    unsafe {
        if GetRawInputDeviceInfoW(Some(device), RIDI_PREPARSEDDATA, None, &mut size) != 0
            || size == 0
        {
            return None;
        }
    }
    let mut preparsed = vec![0u8; size as usize];
    let res = unsafe {
        GetRawInputDeviceInfoW(
            Some(device),
            RIDI_PREPARSEDDATA,
            Some(preparsed.as_mut_ptr().cast()),
            &mut size,
        )
    };
    if res == u32::MAX {
        return None;
    }
    let pp = PHIDP_PREPARSED_DATA(preparsed.as_mut_ptr() as isize);
    let mut hid_caps = HIDP_CAPS::default();
    if unsafe { HidP_GetCaps(pp, &mut hid_caps) } != HIDP_STATUS_SUCCESS {
        return None;
    }
    let mut val_len = hid_caps.NumberInputValueCaps;
    let mut values = vec![HIDP_VALUE_CAPS::default(); val_len as usize];
    if unsafe { HidP_GetValueCaps(HidP_Input, values.as_mut_ptr(), &mut val_len, pp) }
        != HIDP_STATUS_SUCCESS
    {
        return None;
    }
    values.truncate(val_len as usize);

    let mut count_collection = 0u16;
    let mut contact_collections = Vec::new();
    let mut min_x = 0i32;
    let mut max_x = 1000i32;
    let mut min_y = 0i32;
    let mut max_y = 1000i32;

    for cap in &values {
        let usage = unsafe { cap.Anonymous.NotRange.Usage };
        match (cap.UsagePage, usage) {
            (USAGE_PAGE_DIGITIZER, USAGE_CONTACT_COUNT) => {
                count_collection = cap.LinkCollection;
            }
            (USAGE_PAGE_DIGITIZER, USAGE_CONTACT_ID) => {
                if !contact_collections.contains(&cap.LinkCollection) {
                    contact_collections.push(cap.LinkCollection);
                }
            }
            (USAGE_PAGE_GENERIC, USAGE_X) => {
                min_x = cap.LogicalMin;
                max_x = cap.LogicalMax;
            }
            (USAGE_PAGE_GENERIC, USAGE_Y) => {
                min_y = cap.LogicalMin;
                max_y = cap.LogicalMax;
            }
            _ => {}
        }
    }
    if contact_collections.is_empty() {
        return None;
    }
    Some(DeviceCaps {
        preparsed,
        count_collection,
        contact_collections,
        min_x,
        max_x,
        min_y,
        max_y,
    })
}

fn parse_report(caps: &DeviceCaps, report: &mut [u8], timestamp_ms: u64) -> Option<TouchFrame> {
    let pp = PHIDP_PREPARSED_DATA(caps.preparsed.as_ptr() as isize);
    let mut contact_count = 0u32;
    let _ = unsafe {
        HidP_GetUsageValue(
            HidP_Input,
            USAGE_PAGE_DIGITIZER,
            Some(caps.count_collection),
            USAGE_CONTACT_COUNT,
            &mut contact_count,
            pp,
            report,
        )
    };
    let mut contacts = Vec::new();
    for &collection in &caps.contact_collections {
        let mut usages = [0u16; 8];
        let mut usage_len = usages.len() as u32;
        let tip_down = unsafe {
            HidP_GetUsages(
                HidP_Input,
                USAGE_PAGE_DIGITIZER,
                Some(collection),
                usages.as_mut_ptr(),
                &mut usage_len,
                pp,
                report,
            ) == HIDP_STATUS_SUCCESS
                && usages[..usage_len as usize].contains(&USAGE_TIP_SWITCH)
        };
        if !tip_down {
            continue;
        }
        let (mut id, mut x, mut y) = (0u32, 0u32, 0u32);
        let ok = unsafe {
            HidP_GetUsageValue(
                HidP_Input,
                USAGE_PAGE_DIGITIZER,
                Some(collection),
                USAGE_CONTACT_ID,
                &mut id,
                pp,
                report,
            ) == HIDP_STATUS_SUCCESS
                && HidP_GetUsageValue(
                    HidP_Input,
                    USAGE_PAGE_GENERIC,
                    Some(collection),
                    USAGE_X,
                    &mut x,
                    pp,
                    report,
                ) == HIDP_STATUS_SUCCESS
                && HidP_GetUsageValue(
                    HidP_Input,
                    USAGE_PAGE_GENERIC,
                    Some(collection),
                    USAGE_Y,
                    &mut y,
                    pp,
                    report,
                ) == HIDP_STATUS_SUCCESS
        };
        if ok {
            contacts.push((id, x as i32 - caps.min_x, y as i32 - caps.min_y));
        }
    }
    if contact_count > 0 && contacts.len() > contact_count as usize {
        contacts.truncate(contact_count as usize);
    }
    Some(TouchFrame {
        timestamp_ms,
        contacts,
        span_x: (caps.max_x - caps.min_x).max(1),
        span_y: (caps.max_y - caps.min_y).max(1),
    })
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

    fn enabled_config() -> Gestures {
        Gestures {
            enabled: true,
            ..Gestures::default()
        }
    }

    fn frame(t: u64, x: i32, y: i32, fingers: usize) -> TouchFrame {
        TouchFrame {
            timestamp_ms: t,
            contacts: (0..fingers).map(|i| (i as u32, x + i as i32 * 50, y)).collect(),
            span_x: 1000,
            span_y: 1000,
        }
    }

    #[test]
    fn three_finger_horizontal_swipe_tracks_and_commits_with_natural_direction() {
        let mut tracker = SwipeTracker::new(enabled_config());
        assert_eq!(tracker.push(&frame(0, 500, 500, 3), 1000, 800), None);
        // Tiny motion stays inside deadzone.
        assert_eq!(tracker.push(&frame(10, 510, 500, 3), 1000, 800), None);
        // Passing deadzone locks horizontal axis.
        assert_eq!(
            tracker.push(&frame(20, 450, 500, 3), 1000, 800),
            Some(SwipeEvent::Start(Axis::Horizontal))
        );
        // Live update reports pixel offset.
        assert_eq!(
            tracker.push(&frame(40, 300, 500, 3), 1000, 800),
            Some(SwipeEvent::Update {
                axis: Axis::Horizontal,
                offset_px: -200,
            })
        );
        // Lifting fingers past threshold commits Right (natural scroll: swipe left -> next right).
        assert_eq!(
            tracker.push(&frame(60, 300, 500, 0), 1000, 800),
            Some(SwipeEvent::Commit {
                direction: Direction::Right,
            })
        );
    }

    #[test]
    fn short_slow_swipe_cancels_and_vertical_flick_commits() {
        let mut tracker = SwipeTracker::new(enabled_config());
        tracker.push(&frame(0, 500, 500, 3), 1000, 800);
        assert_eq!(
            tracker.push(&frame(20, 540, 500, 3), 1000, 800),
            Some(SwipeEvent::Start(Axis::Horizontal))
        );
        tracker.push(&frame(200, 560, 500, 3), 1000, 800);
        assert_eq!(
            tracker.push(&frame(220, 560, 500, 0), 1000, 800),
            Some(SwipeEvent::Cancel)
        );

        // Quick vertical flick upward (natural -> workspace Down).
        tracker.push(&frame(300, 500, 500, 3), 1000, 800);
        assert_eq!(
            tracker.push(&frame(310, 500, 460, 3), 1000, 800),
            Some(SwipeEvent::Start(Axis::Vertical))
        );
        tracker.push(&frame(325, 500, 420, 3), 1000, 800);
        assert_eq!(
            tracker.push(&frame(330, 500, 420, 0), 1000, 800),
            Some(SwipeEvent::Commit {
                direction: Direction::Down,
            })
        );
    }

    #[test]
    fn non_natural_mode_reverses_swipe_direction() {
        let mut tracker = SwipeTracker::new(Gestures {
            enabled: true,
            natural: false,
            ..Gestures::default()
        });
        tracker.push(&frame(0, 500, 500, 3), 1000, 800);
        tracker.push(&frame(10, 450, 500, 3), 1000, 800);
        tracker.push(&frame(30, 300, 500, 3), 1000, 800);
        assert_eq!(
            tracker.push(&frame(40, 300, 500, 0), 1000, 800),
            Some(SwipeEvent::Commit {
                direction: Direction::Left,
            })
        );
    }
}

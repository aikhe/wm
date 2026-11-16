//! Owned live DWM pictures with stable, full-source visible-frame alignment.
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY,
    DWM_TNP_VISIBLE, DwmQueryThumbnailSourceSize, DwmRegisterThumbnail, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
use windows::core::Result;

use crate::layout::Rect;
use crate::window::Window;

pub struct Thumbnail {
    pub window: Window,
    handle: isize,
    // Capture the frame once, before the manager moves the source. USER and
    // DWM update geometry at different times; mixing their screen coordinates
    // on every animation frame makes the source crop jump around.
    insets: Insets,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Insets {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Insets {
    fn capture(size: (i32, i32), outer: Rect, frame: Rect) -> Self {
        // Some DWM sources already expose only the visible frame.
        if size == (frame.width, frame.height) {
            return Self::default();
        }
        let horizontal = size.0 - frame.width;
        let vertical = size.1 - frame.height;
        if !(0..=64).contains(&horizontal) || !(0..=64).contains(&vertical) {
            return Self::default();
        }
        let left = frame.x - outer.x;
        let top = frame.y - outer.y;
        let left = if (0..=horizontal).contains(&left) {
            left
        } else {
            horizontal / 2
        };
        let top = if (0..=vertical).contains(&top) {
            top
        } else {
            0
        };
        Self {
            left,
            top,
            right: horizontal - left,
            bottom: vertical - top,
        }
    }

    fn source(self, width: i32, height: i32) -> Rect {
        Rect {
            x: self.left,
            y: self.top,
            width: (width - self.left - self.right).max(0),
            height: (height - self.top - self.bottom).max(0),
        }
    }
}

impl Thumbnail {
    pub fn new(destination: HWND, window: Window) -> Result<Self> {
        let handle = unsafe { DwmRegisterThumbnail(destination, window.0)? };
        let mut thumbnail = Self {
            window,
            handle,
            insets: Insets::default(),
        };
        let size = unsafe { DwmQueryThumbnailSourceSize(handle)? };
        let mut outer = RECT::default();
        if unsafe { GetWindowRect(window.0, &mut outer) }.is_ok() {
            thumbnail.insets = Insets::capture((size.cx, size.cy), outer.into(), window.bounds());
        }
        Ok(thumbnail)
    }

    /// Shows the window's visible frame over `rect`, in the coordinates of
    /// the window the picture was made for, or hides it when none of `rect`
    /// is inside `clip`. A `rect` the size of the frame is drawn unscaled.
    pub fn draw(&self, rect: Rect, clip: Rect) -> Result<()> {
        let size = unsafe { DwmQueryThumbnailSourceSize(self.handle)? };
        let source = self.insets.source(size.cx, size.cy);
        let destination = destination(rect, clip, source, (size.cx, size.cy));
        let mut properties = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_VISIBLE | DWM_TNP_SOURCECLIENTAREAONLY,
            fVisible: destination.is_some().into(),
            fSourceClientAreaOnly: false.into(),
            ..Default::default()
        };
        if let Some(destination) = destination {
            properties.dwFlags |= DWM_TNP_RECTDESTINATION;
            properties.rcDestination = destination.into();
        }
        unsafe { DwmUpdateThumbnailProperties(self.handle, &properties) }
    }
}

impl Drop for Thumbnail {
    fn drop(&mut self) {
        let _ = unsafe { DwmUnregisterThumbnail(self.handle) };
    }
}

fn destination(rect: Rect, clip: Rect, source: Rect, size: (i32, i32)) -> Option<Rect> {
    if rect.width <= 0 || rect.height <= 0 || source.width <= 0 || source.height <= 0 {
        return None;
    }
    let left = rect.x.max(clip.x);
    let top = rect.y.max(clip.y);
    let right = (rect.x + rect.width).min(clip.x + clip.width);
    let bottom = (rect.y + rect.height).min(clip.y + clip.height);
    if right <= left || bottom <= top {
        return None;
    }
    // Map the COMPLETE DWM source, expanding the destination for invisible
    // frame margins. The destination HWND clips it naturally. Explicit source
    // cropping plus a clipped destination can introduce DWM letterboxing and
    // a second scale, leaving a visible strip beside the picture.
    let x = |value: i32| {
        (f64::from(value) * f64::from(rect.width) / f64::from(source.width)).round() as i32
    };
    let y = |value: i32| {
        (f64::from(value) * f64::from(rect.height) / f64::from(source.height)).round() as i32
    };
    let left = rect.x - x(source.x);
    let top = rect.y - y(source.y);
    Some(Rect {
        x: left,
        y: top,
        width: x(size.0),
        height: y(size.1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_crop_keeps_its_origin_when_the_source_moves_or_resizes() {
        let outer = Rect {
            x: 93,
            y: 100,
            width: 514,
            height: 407,
        };
        let frame = Rect {
            x: 100,
            y: 100,
            width: 500,
            height: 400,
        };
        let insets = Insets::capture((514, 407), outer, frame);
        assert_eq!(
            insets.source(514, 407),
            Rect {
                x: 7,
                y: 0,
                width: 500,
                height: 400
            }
        );
        assert_eq!(
            insets.source(614, 407),
            Rect {
                x: 7,
                y: 0,
                width: 600,
                height: 400
            }
        );
        // A DWM source already trimmed to its visible frame must not be cropped twice.
        assert_eq!(Insets::capture((500, 400), outer, frame), Insets::default());
    }

    #[test]
    fn full_source_mapping_preserves_border_offsets_and_partial_window_scale() {
        let clip = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 200,
        };
        let source = Rect {
            x: 8,
            y: 0,
            width: 1000,
            height: 2000,
        };
        let dest = destination(Rect { x: -50, ..clip }, clip, source, (1016, 2008)).unwrap();
        assert_eq!(dest.x, -51);
        assert_eq!(dest.width, 102);
        assert!(destination(Rect { x: 100, ..clip }, clip, source, (1016, 2008)).is_none());
        assert!(destination(Rect { width: 0, ..clip }, clip, source, (1016, 2008)).is_none());
        let visible = Rect {
            x: 0,
            y: 0,
            width: 506,
            height: 353,
        };
        let source = Rect {
            x: 7,
            y: 0,
            ..visible
        };
        assert_eq!(
            destination(visible, visible, source, (520, 360)).unwrap(),
            Rect {
                x: -7,
                y: 0,
                width: 520,
                height: 360
            }
        );
    }
}

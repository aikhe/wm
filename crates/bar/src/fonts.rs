//! Query the same GDI font families the bar can actually render, including
//! fonts installed for the current user. Registry display names can differ.
use std::collections::BTreeSet;

use windows::Win32::Foundation::LPARAM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DEFAULT_CHARSET, DeleteDC, EnumFontFamiliesExW, LOGFONTW, TEXTMETRICW,
    TRUETYPE_FONTTYPE,
};

pub fn installed() -> Result<Vec<String>, Box<dyn std::error::Error>> {
    unsafe extern "system" fn collect(
        font: *const LOGFONTW,
        _metrics: *const TEXTMETRICW,
        kind: u32,
        parameter: LPARAM,
    ) -> i32 {
        if kind & TRUETYPE_FONTTYPE == 0 {
            return 1;
        }
        let face = unsafe { &(*font).lfFaceName };
        let length = face
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(face.len());
        let name = String::from_utf16_lossy(&face[..length]);
        if !name.is_empty() && !name.starts_with('@') {
            let names = unsafe { &mut *(parameter.0 as *mut BTreeSet<String>) };
            names.insert(name);
        }
        1
    }

    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.is_invalid() {
        return Err("couldn't enumerate installed fonts".into());
    }
    let query = LOGFONTW {
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    let mut names = BTreeSet::new();
    unsafe {
        EnumFontFamiliesExW(
            dc,
            &raw const query,
            Some(collect),
            LPARAM(&raw mut names as isize),
            0,
        );
        let _ = DeleteDC(dc);
    }
    Ok(names.into_iter().collect())
}

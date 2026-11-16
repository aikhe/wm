mod cloak;
mod events;
mod layout;
mod manager;
mod window;

use std::process::ExitCode;

use window::Window;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::Result;

/// With no arguments, tiles the desktop. `wm list` prints what it would tile
/// and what it would skip, without touching anything.
fn main() -> Result<ExitCode> {
    // Without this, coordinates are scaled on monitors that aren't at 100%.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)? };

    match std::env::args().nth(1).as_deref() {
        None => manager::run()?,
        Some("list") => list()?,
        Some(other) => {
            eprintln!("unknown command `{other}`; use `wm` to tile or `wm list` to inspect");
            return Ok(ExitCode::FAILURE);
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn list() -> Result<()> {
    for window in Window::visible()? {
        let info = window.info();
        let exe = window.exe().unwrap_or_else(|| "?".into());
        match window.skip(&info) {
            None => println!("tile  {exe:<22} {info}"),
            Some(reason) => println!("skip  {exe:<22} {info}  [{reason:?}]"),
        }
    }

    Ok(())
}

mod window;

use window::Window;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::Result;

/// Lists the windows the layout would tile. Pass `--skipped` to also list the
/// visible windows it leaves alone, with the reason.
fn main() -> Result<()> {
    // Without this, coordinates are scaled on monitors that aren't at 100%.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)? };

    let show_skipped = std::env::args().any(|arg| arg == "--skipped");

    for window in Window::visible()? {
        let info = window.info();
        match info.skip() {
            None => println!("tile  {info}"),
            Some(reason) if show_skipped => println!("skip  {info}  [{reason:?}]"),
            Some(_) => {}
        }
    }

    Ok(())
}

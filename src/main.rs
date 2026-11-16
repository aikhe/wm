mod bindings;
mod cloak;
mod config;
mod config_watch;
mod events;
mod layout;
mod manager;
mod overview;
mod rules;
mod window;

use std::process::ExitCode;

use window::Window;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::Result;

/// With no arguments, tiles the desktop. `wm list` prints what it would tile
/// and what it would skip, without touching anything.
fn main() -> ExitCode {
    match execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn execute() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<_> = std::env::args().skip(1).collect();
    let mut chosen_path = None;
    while let Some(index) = args.iter().position(|arg| arg == "--config") {
        if chosen_path.is_some() || index + 1 >= args.len() {
            return Err("use --config PATH once".into());
        }
        chosen_path = Some(std::path::PathBuf::from(args.remove(index + 1)));
        args.remove(index);
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!(
            "wm [--config PATH]      Tile; config is created if missing and reloads on save.\nwm list                Inspect windows without rearranging them.\nwm config init         Create the commented defaults without overwriting a file.\nwm config check        Validate the file and monitor geometry without tiling.\nwm config path         Print its absolute path.\nwm config defaults     Print all commented defaults.\nAll config commands accept --config PATH."
        );
        return Ok(());
    }
    if args.as_slice() == ["config", "defaults"] {
        print!("{}", config::DEFAULT_CONFIG);
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "config") {
        let path = std::path::absolute(chosen_path.map_or_else(config::default_path, Ok)?)?;
        match args.get(1).map(String::as_str) {
            Some("path") if args.len() == 2 => println!("{}", path.display()),
            Some("init") if args.len() == 2 => {
                let created = config::init(&path)?;
                println!(
                    "{} {}",
                    if created {
                        "Created"
                    } else {
                        "Already exists:"
                    },
                    path.display()
                );
            }
            Some("check") if args.len() == 2 => {
                let parsed = config::Config::parse(&config::read(&path)?)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                unsafe {
                    SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?
                };
                parsed.viewport(manager::work_area()?)?;
                println!(
                    "Valid: {} ({} hotkeys; availability checked when applied)",
                    path.display(),
                    parsed.bindings.len()
                );
            }
            _ => return Err("use wm config init|check|path|defaults [--config PATH]".into()),
        }
        return Ok(());
    }
    // Without this, coordinates are scaled on monitors that aren't at 100%.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)? };

    match args.as_slice() {
        [] => manager::run(std::path::absolute(
            chosen_path.map_or_else(config::default_path, Ok)?,
        )?)?,
        [command] if command == "list" => list()?,
        _ => return Err("use wm --help for commands".into()),
    }

    Ok(())
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

mod animation;
#[cfg(test)]
mod animation_probe;
mod bindings;
mod cloak;
mod config;
mod config_watch;
mod events;
mod frame_clock;
mod gestures;
mod layout;
mod manager;
mod overview;
mod platform;
mod rules;
mod session;
mod setups;
mod stage;
mod thumbnail;
mod window;

use std::ffi::c_void;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use window::Window;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::Result;
use wm_bar_protocol::DiagKind;

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
        chosen_path = Some(PathBuf::from(args.remove(index + 1)));
        args.remove(index);
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!(
            "wm [--config PATH]      Tile; config is created if missing and reloads on save.\n\
             wm list                 Inspect windows without rearranging them.\n\
             wm status               Read live state from the running WM.\n\
             wm doctor [--config P]  Check config, recovery markers, and recent anomalies.\n\
             wm do ACTION            Post an allowed action to the running WM.\n\
             wm config init          Create the commented defaults without overwriting a file.\n\
             wm config check         Validate the file and monitor geometry without tiling.\n\
             wm config path          Print its absolute path.\n\
             wm config defaults      Print all commented defaults.\n\
             All config and doctor commands accept --config PATH."
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
                let (text, saved) = config::read_all(&path)?;
                let parsed = config::Config::parse_with(&text, &saved)
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
    if args.first().is_some_and(|arg| arg == "do") {
        if args.len() != 2 || chosen_path.is_some() {
            return Err("use wm do ACTION".into());
        }
        let index = bindings::action_index(&args[1]).ok_or_else(|| {
            let names = bindings::DEFAULTS
                .iter()
                .map(|(name, _, _)| *name)
                .collect::<Vec<_>>()
                .join(", ");
            format!("unknown action `{}`; valid actions: {names}", args[1])
        })?;
        let host = wm_bar_protocol::find_host().ok_or("wm is not running")?;
        wm_bar_protocol::post_agent(host, index)
            .map_err(|error| format!("could not post action to wm: {error}"))?;
        return Ok(());
    }
    if args.as_slice() == ["status"] {
        if chosen_path.is_some() {
            return Err("use wm status without --config".into());
        }
        return status();
    }
    if args.as_slice() == ["doctor"] {
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)? };
        let path = std::path::absolute(chosen_path.map_or_else(config::default_path, Ok)?)?;
        return doctor(&path);
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
            None if info.dialog_like() => println!("float {exe:<22} {info}"),
            None => println!("tile  {exe:<22} {info}"),
            Some(reason) => println!("skip  {exe:<22} {info}  [{reason:?}]"),
        }
    }

    Ok(())
}

fn status() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let Some((revision, state, diag)) = wm_bar_protocol::peek() else {
        return Err("wm is not running".into());
    };
    let phase = if state.paused {
        "paused"
    } else if state.overview {
        "overview"
    } else {
        "tiling"
    };
    let mode = match state.mode {
        1 => "tiled",
        2 => "floating",
        3 => "sticky",
        4 => "on top",
        5 => "scratchpad",
        _ => "empty",
    };
    let uptime = format_uptime(diag.started_unix_ms);
    println!("state       {phase} (rev {revision}, up {uptime})");
    println!("workspace   {}/{}", state.active + 1, state.workspaces);
    let scratch_state = if state.scratchpad == 0 {
        "0".to_owned()
    } else if state.scratchpad_shown {
        format!("{} (shown)", state.scratchpad)
    } else {
        format!("{} (hidden)", state.scratchpad)
    };
    println!(
        "windows     tiled {}  floating {}  scratchpad {scratch_state}",
        state.tiled, state.floating
    );
    if state.focused == 0 {
        println!("focused     none");
    } else {
        let window = Window(HWND(state.focused as *mut c_void));
        let exe = window.exe().unwrap_or_else(|| "?".into());
        println!("focused     {mode:<10} {exe}  (0x{:x})", state.focused);
    }
    Ok(())
}

fn doctor(config_path: &std::path::Path) -> std::result::Result<(), Box<dyn std::error::Error>> {
    println!("config      {}", config_path.display());
    match config::read_all(config_path)
        .and_then(|(text, saved)| config::Config::parse_with(&text, &saved))
        .and_then(|parsed| {
            let area = manager::work_area().map_err(|error| error.to_string())?;
            let viewport = parsed.viewport(area)?;
            Ok((parsed, viewport))
        }) {
        Ok((parsed, viewport)) => println!(
            "            ok ({} hotkeys, viewport {}x{} at {},{})",
            parsed.bindings.len(),
            viewport.width,
            viewport.height,
            viewport.x,
            viewport.y
        ),
        Err(error) => println!("            ERROR: {error}"),
    }

    let visible = Window::visible().unwrap_or_default();
    let marked_cloaked = visible.iter().filter(|w| cloak::is_cloaked(**w)).count();
    let marked_parked = visible.iter().filter(|w| cloak::is_parked(**w)).count();
    if let Some(session_path) = session::default_path() {
        let saved = session::load_from(&session_path);
        println!(
            "session     {} ({} saved entries)",
            session_path.display(),
            saved.entries.len()
        );
    }

    match wm_bar_protocol::peek() {
        None => {
            println!("runtime     not running");
            if marked_cloaked > 0 || marked_parked > 0 {
                println!(
                    "recovery    WARNING: {marked_cloaked} wm.cloaked and {marked_parked} wm.parked window(s) left from an earlier run (start wm to recover)"
                );
            } else {
                println!("recovery    clean (0 leftover wm.cloaked / wm.parked windows)");
            }
        }
        Some((revision, state, diag)) => {
            let phase = if state.paused {
                "paused"
            } else if state.overview {
                "overview"
            } else {
                "tiling"
            };
            println!(
                "runtime     {phase} (rev {revision}, up {}, cloaked={marked_cloaked}, parked={marked_parked})",
                format_uptime(diag.started_unix_ms)
            );
            println!("anomalies   {} total", diag.total);
            for idx in 0..wm_bar_protocol::DIAG_KINDS {
                let count = diag.counts[idx];
                if count > 0
                    && let Some(kind) = DiagKind::from_u32(idx as u32)
                {
                    println!("            {:<20} {count}", kind.label());
                }
            }
            if !diag.recent.is_empty() {
                println!("recent      (last {} events, titles omitted)", diag.recent.len());
                for event in diag.recent.iter().rev().take(10) {
                    println!(
                        "            #{:<4} +{:>6.1}s  {:<18} hwnd=0x{:x} code={}",
                        event.sequence,
                        event.elapsed_ms as f64 / 1000.0,
                        event.kind.label(),
                        event.hwnd,
                        event.code
                    );
                }
            }
        }
    }
    Ok(())
}

fn format_uptime(started_unix_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let secs = now_ms.saturating_sub(started_unix_ms) / 1000;
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rem = secs % 60;
    if hours > 0 {
        format!("{hours}h{mins:02}m{rem:02}s")
    } else if mins > 0 {
        format!("{mins}m{rem:02}s")
    } else {
        format!("{rem}s")
    }
}

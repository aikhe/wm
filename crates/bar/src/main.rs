mod config;
mod fonts;
mod layout;
mod stats;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn execute() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<_> = std::env::args().skip(1).collect();
    let mut chosen = None;
    while let Some(index) = args.iter().position(|arg| arg == "--config") {
        if chosen.is_some() || index + 1 >= args.len() {
            return Err("use --config PATH once".into());
        }
        chosen = Some(PathBuf::from(args.remove(index + 1)));
        args.remove(index);
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!(
            "wm-bar [--config PATH]   Run the separate native bar.\nwm-bar fonts              List installed scalable font families.\nwm-bar config init        Create the commented defaults without overwriting.\nwm-bar config check       Validate without showing a bar.\nwm-bar config path        Print the config path.\nwm-bar config defaults    Print all defaults.\nSave the config to reload. All commands accept --config PATH.\nSet wm's [layout] reserved_top to the bar's height (28 by default)."
        );
        return Ok(());
    }
    if args.as_slice() == ["config", "defaults"] {
        print!("{}", config::DEFAULT_CONFIG);
        return Ok(());
    }
    if args.as_slice() == ["fonts"] {
        for family in fonts::installed()? {
            println!("{family}");
        }
        return Ok(());
    }
    let path = std::path::absolute(chosen.map_or_else(config::default_path, Ok)?)?;
    if args.first().is_some_and(|arg| arg == "config") {
        match args.get(1).map(String::as_str) {
            Some("path") if args.len() == 2 => println!("{}", path.display()),
            Some("init") if args.len() == 2 => println!(
                "{} {}",
                if config::init(&path)? {
                    "Created"
                } else {
                    "Already exists:"
                },
                path.display()
            ),
            Some("check") if args.len() == 2 => {
                config::Config::parse(&std::fs::read_to_string(&path)?)?;
                println!("Valid: {}", path.display());
            }
            _ => return Err("use wm-bar config init|check|path|defaults [--config PATH]".into()),
        }
        return Ok(());
    }
    if !args.is_empty() {
        return Err("use wm-bar --help for commands".into());
    }
    config::init(&path)?;
    let settings = config::Config::parse(&std::fs::read_to_string(&path)?)?;
    ui::run(path, settings)
}

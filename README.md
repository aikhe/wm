# wm

A fast, lightweight scrollable-tiling window manager for Windows inspired by
[niri](https://github.com/YaLTeR/niri), written in pure Rust calling Win32
directly.

## Features

- **Scrollable tiling & overview:** Horizontal columns per vertical workspace
  (named or dynamic) with a live zoomed-out overview (`Alt+O`).
- **Instant input, zero idle CPU:** Single-thread Win32 loop with in-process
  hotkeys, non-blocking placement, and no idle polling.
- **Smooth DWM animations:** Presentation-only spring motion via DWM thumbnails
  paced by `DwmFlush`; focus never waits for animations.
- **Modes, rules & setups:** Floating, sticky, always-on-top, auto-floated
  dialogs, `[[window_rules]]`, learned widths/modes (`rules.txt`), and saved
  app layouts (`[setups.*]`).
- **Atomic hot-reload:** Saving `%APPDATA%\wm\config.toml` reloads live;
  invalid edits keep the current setup working.
- **Optional bar & CLI:** Separate Win32/GDI top bar (`wm-bar`) and allow-listed
  CLI actions (`wm do`).

## Quick start

Requires Windows 10/11 and Rust. Run from **PowerShell** (Git Bash sets
`__COMPAT_LAYER` and breaks DPI awareness). Stop other tiling managers or
hotkey daemons first.

```powershell
cargo build --workspace --release
target\release\wm.exe config init   # creates %APPDATA%\wm\config.toml
target\release\wm.exe               # Alt+Shift+E or Ctrl+C quits and restores windows
```

## Keybindings

Configurable in `config.toml` (supports multiple keys per action, e.g.
`["Alt+H", "Alt+A"]`). Full list in [wm.default.toml](wm.default.toml).

| Action | Default |
| --- | --- |
| Focus column / workspace | `Alt+H` `Alt+L` / `Alt+K` `Alt+J` |
| Move window / across workspaces | `Alt+Shift+H` `Alt+Shift+L` / `Alt+Shift+K` `Alt+Shift+J` |
| Switch / send to workspace 1–9 | `Alt+1..9` / `Alt+Shift+1..9` |
| Cycle width / resize | `Alt+R` / `Alt+-` `Alt+=` |
| Full width / float / sticky / on-top | `Alt+F` / `Alt+T` / `Alt+Shift+T` / `Ctrl+Alt+Shift+T` |
| Overview | `Alt+O` |
| Close / reload / quit | `Alt+Shift+W` / `Alt+Shift+R` / `Alt+Shift+E` |

## Config

`%APPDATA%\wm\config.toml` is created on first run and reloads on save. See
[wm.default.toml](wm.default.toml) for all options:

- **Workspaces:** `workspaces = ["web", "chat", "code"]` pins named workspaces
  at the top; unnamed ones below appear and vanish automatically.
- **Rules:** `[[window_rules]]` overrides learned modes/widths in `rules.txt`,
  which override dialog auto-floating and defaults. `wm` never edits `config.toml`.
- **Setups:** Define `[setups.<name>]` or bind `save_key` to snapshot layouts
  to `setups.toml`; set `[behavior] startup_setup` to load one at startup.
- **Animations & CLI:** Configure per-effect timing under `[animations]` and
  allowed `wm do` actions under `[agent] allow`.

## Commands

```powershell
wm [--config PATH]        # Start tiling
wm list                   # List eligible and skipped windows
wm do <action>            # Send an allowed action to the running WM
wm config init|check|path|defaults
```

## Bar (`wm-bar`)

Optional standalone Win32/GDI top bar with workspaces, focused mode/title
(click for overview), clock, CPU, and memory. Drops behind fullscreen apps and
uses no timers when `clock`/`cpu`/`memory` are disabled.

Reserve space in `%APPDATA%\wm\config.toml` matching `bar.height` (`28` by default):

```toml
[layout]
reserved_top = 28
```

Start `target\release\wm-bar.exe` (in either order with `wm`); stop with
`Stop-Process -Name wm-bar`. Configured via `%APPDATA%\wm\bar.toml`
([bar.default.toml](crates/bar/bar.default.toml), 18px theme:
[yasb.toml](crates/bar/themes/yasb.toml)).

```powershell
wm-bar fonts              # List installed scalable fonts
wm-bar config init|check|path|defaults [--config PATH]
```

## Status & testing

Currently targets the primary monitor and one window per column.

```powershell
cargo test --workspace
```

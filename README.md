# wm

A small scrollable tiling window manager for Windows, written in Rust.

Create your configuration without moving any windows:

```powershell
cargo run -- config init
notepad "$env:APPDATA\wm\config.toml"
```

The file contains every setting's default and comments explaining it: layout
gaps, width presets, resize steps, space for a bar, navigation, remembered app
behavior, overview appearance, and all keyboard shortcuts. See
[wm.default.toml](wm.default.toml) for the same template.

Run with `cargo run --release`. If an older version is running, quit it first
with Alt+Shift+E. Stop other tiling managers and conflicting hotkey daemons first.

Save `config.toml` to reload. Windows change notifications wake a worker only
when the directory changes; there is no polling timer while idle. Ordinary saves
and atomic file replacements both work. Save bursts settle for 150ms before
parsing. Invalid settings or a new hotkey that another application owns leave
the entire previous configuration working. Errors print to the console.

Layout, overview, navigation and hotkeys update live, preserving workspaces and
focus. Current column widths are retained, except when new limits require
clamping. Default widths and explicit app rules apply when windows next join
(startup, opening or restoring); they do not override current window modes.

For example, edit the existing `[layout]` section's `reserved_top` to leave room
for YASB, or edit `[hotkeys]` to rebind an action. Omitted settings retain built-in
defaults; `""` disables a hotkey except `quit`. The template lists accepted key
names and commented `[[window_rules]]` examples.

Learned modes/widths stay in `%APPDATA%\wm\rules.txt`, saved on graceful exit.
Explicit rule fields take precedence over remembered values; wm never overwrites
your config while recording learned behavior.

Other commands:

```powershell
cargo run -- config check       # Validate without tiling; does not reserve hotkeys
cargo run -- config path        # Print the config location
cargo run -- config defaults    # Print the full commented template
cargo run -- --config C:\path\config.toml  # Use a different file
cargo run -- list               # Inspect eligible/skipped windows
cargo test
```

Alt+Shift+R manually retries a reload (useful after resolving a hotkey conflict).
The config lists all other default keys; Alt+Shift+E or Ctrl+C quits and restores
your windows.

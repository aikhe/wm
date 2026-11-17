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
for a bar, or edit `[hotkeys]` to rebind an action. Omitted settings retain built-in
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
cargo run -- do focus_right     # Post an action to the running WM
cargo test
```

`wm do ACTION` posts a named action to the running WM and returns without
waiting for it to run. The `[agent] allow` list controls which actions the WM
accepts; it defaults to every action except `close` and `quit` and reloads on
save. Add those names explicitly to allow them. Invalid or repeated names reject
the configuration. Refused actions are logged. Hotkeys and bar commands are
unaffected. The list of action names is in `[hotkeys]` in the config template.

Alt+Shift+R manually retries a reload (useful after resolving a hotkey conflict).
The config lists all other default keys; Alt+Shift+E or Ctrl+C quits and restores
your windows.

Animations are on by default: columns scroll, workspaces slide past like
pages, and the overview zooms out of the desktop and back. Edit `[animations]`
and its `horizontal`, `workspaces` and `overview` tables in the config. Each
effect has `enabled` and `duration_ms`, the time a move takes to settle (0 to
2000, where 0 is instant; 300 by default). `enabled = false` under
`[animations]` turns them all off. Saving hot-reloads these settings.

They are presentation only. Keyboard focus changes at once and the real windows
go straight to where they end up; what moves is a live picture of each window
on a see-through surface above the desktop, which lets clicks through and
covers nothing else, so the wallpaper never flickers. The pictures appear
exactly over the real windows before those are hidden, and the real windows
are shown beneath the pictures before those go, so neither swap can be seen.

The motion is springs, as in niri. A move eases in and out, and a key pressed
mid-move bends it toward the new place at the speed it had instead of
restarting, so holding a key down scrolls smoothly. Frames are paced by the
compositor, one for each refresh of the screen, and only while something is
moving. Opening, closing, dragging or resizing a window ends any motion at once.

`cargo test the_stage_shows -- --ignored --nocapture`, from PowerShell, checks
the surface on the real desktop: it slides a picture of a test window of its
own and reads the screen back. It doesn't touch any other app's windows.

These effects cover WM-managed windows and workspaces. They do not control tabs
inside applications. This WM still manages one window per column.

## Native top bar

`wm-bar` is an optional, separate Rust process. It uses Win32/GDI with a cached
font and backbuffer. It shows clickable workspaces, the focused window's mode
and title, CPU usage, physical memory usage and a minute-aligned date/time.
Click the title or mode to toggle overview;
scroll over workspace buttons to switch up/down. Right-click anywhere to quit
the bar. The bar never registers the WM's hotkeys or starts a tiling manager.

Build both binaries:

```powershell
cargo build --workspace --release
```

In wm's existing `%APPDATA%\wm\config.toml`, set:

```toml
[layout]
reserved_top = 28
```

Edit the existing section rather than adding a second `[layout]`. Save to reload.
Start `target\release\wm.exe` and `target\release\wm-bar.exe` independently, in
either order. For development, use `cargo run -p wm-bar`. An older WM binary
must be restarted once to gain the bar integration.

The bar creates `%APPDATA%\wm\bar.toml` once. Edit it to reorder/hide modules,
change colors, font, height, padding, workspace button sizes and the clock
format. Saves hot-reload; invalid edits keep the previous settings. See
[bar.default.toml](crates/bar/bar.default.toml) for every setting and its default.
Keep `bar.height` and wm's `layout.reserved_top` equal. There is no automatic
height negotiation in this version.

[themes/yasb.toml](crates/bar/themes/yasb.toml) matches the existing personal
YASB styling: an 18px flat `#101010` strip, 11px Segoe UI regular text,
Segoe MDL2 icons, gray workspace highlights, centered date/time and CPU/RAM on
the right. Copy it to `%APPDATA%\wm\bar.toml` and set wm's `reserved_top = 18`.
The rendering remains native Rust/GDI; YASB is not required. Keep the WM running
when upgrading just the bar; close the bar with a right-click before replacing
its executable, then start the new bar.
Font selection requires scalable fonts and falls back to Segoe UI when a
configured family is unavailable, preventing legacy bitmap substitutions on
machines without Segoe UI Variable.

Choose any installed scalable font by its family name in `[theme]`:

```toml
[theme]
font = "Segoe UI"
font_size = 11
font_weight = 400
```

`wm-bar fonts` lists the family names GDI sees, including per-user fonts.
An omitted or blank `font` defaults to Segoe UI; omitted size/weight default
to 11px regular (400). Semibold (600) and bold (700) are wider/heavier at small
sizes. Glyphs retain the font's natural proportions; widget widths only affect
clipping and ellipsis. Save `bar.toml` to change the font without restarting.

`cpu` samples busy processor time every 2000ms by default; `memory` samples
physical RAM (total minus available) every 5000ms. Both intervals are configurable
from 500 to 60000ms, and their timers stop when the module is removed. The first
CPU percentage appears after one sampling interval; `--` means no valid sample.
Memory can show `{percent}`, `{used_gib}` and `{total_gib}`; GiB uses 1024³ bytes.
The clock supports `{weekday}` and `{month_name}` as English abbreviations in
addition to numeric date/time fields.

```powershell
cargo run -p wm-bar -- config init
cargo run -p wm-bar -- config check
cargo run -p wm-bar -- config path
cargo run -p wm-bar -- config defaults
cargo run -p wm-bar -- --config C:\path\bar.toml
cargo test --workspace
```

This version targets the primary monitor and a single bar. It uses manual
reserved space rather than a Windows AppBar: maximized floating/unmanaged
windows can extend behind it, and reserved space remains if the bar closes.
Large workspace lists show a page around the active workspace. A disconnected
bar shows `Offline` and reconnects when a WM with bar support starts again.

The WM publishes a 56-byte atomic shared-memory snapshot after logical state
changes and posts one outstanding notification. Commands are posted back to a
hidden WM window and use the same actions as hotkeys. Stale workspace clicks
are rejected if the state changed before delivery. There are no pipe workers,
JSON serialization, external processes or bar waits in the hotkey path. The
bar tracks title changes only on the focused window's thread. Its message loop
waits for input and directory notifications; the clock wakes once per minute.
Remove `cpu`, `memory` and `clock` from the module lists to disable periodic
timers entirely. Sampling is confined to the bar; the WM's input path is unchanged.
The private protocol is versioned; build both binaries from the same checkout.

# wm

A scrollable-tiling window manager for Windows, built for Ike's own use: niri's
layout model on Windows, in pure Rust calling the Win32 API directly.

## Goals

- Lightweight and fast, with zero CPU use when idle.
- Switching stays instant when apps or the whole machine are lagging, the way
  Alt+Tab does.

## Design rules

- Hotkeys are handled in-process: `RegisterHotKey` straight into a function
  call. No CLI, IPC or process spawn between a keypress and its action.
- Never wait on another app. Use `SWP_ASYNCWINDOWPOS`, `ShowWindowAsync` and
  `SendMessageTimeout`; never plain `SendMessage`.
- Off-screen columns are cloaked, not hidden or minimized.
- One desktop thread, one `GetMessage` loop, window events from `SetWinEventHook`
  with `WINEVENT_OUTOFCONTEXT`. No async runtime and no polling. A config worker
  blocks on Windows directory notifications and parses saves off the input thread.
- Layout is a pure function from state to window rectangles, and only the
  differences get applied. This is the part worth unit testing.
- Window events arrive late, after hotkeys that came in behind them. Check the
  window's state as it is now before acting on one, never the event alone.
- Floating windows are tracked but never positioned. They stay above tiled
  ones by being raised again when a tiled window takes focus, not by topmost.
- The overview is a view of the layout, with no state of its own: the
  highlight is the layout's focused window, and it redraws after every
  `apply`. Its pictures are DWM thumbnails, never `PrintWindow`, which asks
  the app to draw. Its window's messages are handled in the one loop, not in
  a window procedure.
- Scrolling moves windows and never resizes them.
- No animations unless Ike asks for them.
- Add crates reluctantly. `windows`/`windows-core` provide Win32; `serde` and
  `toml` parse configuration only at startup and on reload, never on a keypress.

## References

`references/` is gitignored and holds clones to read, not to copy from:

- `komorebi`: how to deal with Win32 (window filtering, cloaking, focus,
  borders). Its scrolling layout lives in `komorebi-scrolling/`.
- `niri`: the layout model to match, mainly `src/layout/scrolling.rs`.
- `LeopardWM`: another scrolling manager for Windows in Rust (GPL-3), useful
  for how it handles floating, sticky and z-order in
  `crates/platform_win32/`.

Don't use GlazeWM as a reference; Ike finds it too slow.

## Build order

1. Enumerate and filter windows.
2. One monitor, one workspace, a column strip, focus left/right by hotkey.
3. Cloak off-screen columns; move column; width presets.
4. Vertical workspaces, then multi-monitor, then a config file.

## Running

- `%APPDATA%\wm\config.toml` holds user settings and hotkeys. It is generated
  automatically without overwriting existing files; `wm.default.toml` is the
  embedded, fully commented list of defaults. Omitted values use those defaults.
- Saving hot-reloads settings with Windows directory-change notifications,
  including editor saves that replace the file. The worker debounces save
  bursts by 150ms, then waits indefinitely while idle. Parsing, geometry errors,
  unknown settings, duplicate keys and unavailable new hotkeys reject the whole
  update; previous settings and hotkeys keep working. Alt+Shift+R retries manually.
- `cargo run -- config init` creates the config without tiling. `config check`
  validates its syntax, settings and current primary work-area geometry without
  registering keys or moving windows; key availability is checked on apply.
  `config path` prints the path; `config defaults` prints the commented template.
  `--config PATH` selects another file for either commands or the manager.
- Layout geometry, navigation, hotkeys and overview appearance update live.
  Existing workspaces/focus/modes and widths survive reloads; widths are only
  clamped if new layout limits require it. New default widths and explicit
  `[[window_rules]]` apply when a window joins (startup/open/restore), not to
  already managed windows. First matching rule wins; its supplied fields override
  learned state, then omitted values fall back to built-in defaults.
- Learned state stays in `rules.txt`; wm never writes user changes back to
  `config.toml`. Explicit rules can match exe, class and title substring, and set
  tiled/float/sticky/top/ignore, width and full width. All present match conditions
  must match; executable names are case insensitive.

- `cargo run` tiles the desktop. Alt+H/L moves focus between columns and
  Alt+K/J between workspaces; add Shift to move the focused column instead.
  Alt+1..9 jumps to a workspace by number and Alt+Shift+1..9 takes the
  focused window there. Floating windows move between workspaces too. Focus
  and move wrap round the ends.
- Alt+R cycles the focused column's width through the presets, Alt+- / Alt+=
  step it by a tenth of the screen, and Alt+F makes it fill the screen or
  puts it back.
- Alt+T floats the focused window at the spot it had before it was tiled, or
  tiles it again. Alt+Shift+T pins it to every workspace, and
  Ctrl+Alt+Shift+T also keeps it above every other window; pressing either
  again leaves it floating.
- Alt+O opens the overview: every workspace as a row of live window
  pictures, the focused window highlighted. All the hotkeys keep working on
  it, and while it has the keyboard they work without Alt too (except quit),
  with the arrow keys standing in for H/J/K/L. Enter, Escape or Alt+O closes
  it on the highlighted window; a click goes to that window or workspace.
- Alt+Shift+W closes the window. Alt+Shift+E or Ctrl+C quits and restores
  every window.
- The mode a window is put in and the width its column is given become the
  rule for its kind (program file name plus window class), so the next one
  opens the same way. Rules are written to `%APPDATA%\wm\rules.txt` on quit,
  never on a key press: one tab-separated `mode exe class [width [full]]` per
  line, with `tiled`, `float`, `sticky` or `top`. It can be edited by hand
  while the manager isn't running.
- Every hotkey and window event that changes something prints a line: what
  happened, the workspace on screen, then the focused window and how it's
  kept.
- Workspaces are never made by hand: the bottom one is always empty, and
  others disappear once emptied and left.
- `cargo run -- list` prints what would be tiled and what is skipped, with the
  reason, without touching anything.
- `cargo test -- --ignored` cloaks and uncloaks one real window to check the
  undocumented shell interface still works, which blinks that window, and
  checks that Windows gives the hidden overview a thumbnail, which shows
  nothing.

Cloaked windows carry a `wm.cloaked` window property, so a run that was killed
can be cleaned up by starting and quitting the manager again.

Stop komorebi and whkd before tiling (`komorebic stop --whkd`): two managers
fight over the same windows, and whkd swallows the same hotkeys.

Agents must not run the tiling mode unasked. It rearranges Ike's real desktop;
use `list` and the tests to check work instead.

Run it from PowerShell. The agent Git Bash shell sets `__COMPAT_LAYER` to
force DPI-unaware, so the binary exits there with "Access is denied".

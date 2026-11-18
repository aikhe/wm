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
  Only while something is animating, the loop instead makes one frame per
  `DwmFlush` and takes input between frames.
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
  a window procedure. Its window is only the backdrop; the pictures, and the
  highlight round the focused one, are on the stage above it.
- Scrolling moves windows and never resizes them.
- No animations unless Ike asks for them.
- Ike asked for configurable animations, to feel like niri's: column
  scrolls, workspace slides and the overview zoom. They are presentation only.
  Focus changes at once, and nothing waits for an animation to end.
  - Windows are never animated themselves: no per-frame `SetWindowPos`, no
    waiting on an app's repaint. The stage (`stage.rs`), a see-through,
    click-through surface, moves a DWM thumbnail of each window instead.
  - Nothing may flash. Pictures first appear exactly over the real windows;
    one composed frame later the real ones are cloaked and placed where they
    end up; when the pictures come to rest the real ones are uncloaked beneath
    them before the pictures go. Two copies in one spot look like one, a frame
    with neither doesn't. The stage paints nothing of its own, so the
    wallpaper is never covered.
  - Windows uncloaks a window that takes the keyboard, by itself and a
    moment later, and takes the keyboard from one that's cloaked. So the
    focused window is never cloaked under its picture: it waits uncloaked a
    screen's height below its place, and takes the keyboard only once it's
    there. Check `Window::cloaked` as well as our mark, which can outlive
    the cloak.
  - Pictures on the desktop are drawn at the window's real size, never scaled.
    Only the overview scales them.
  - Motion is critically damped springs (`animation.rs`), stepped exactly, one
    per edge of each picture. A key pressed mid-move retargets them from where
    they are, keeping their speed. Never restart a move or queue one.
  - Frames are paced by `DwmFlush` and advance by whole refreshes, not by a
    timer, which drifts against the screen and judders.
  - Width and mode changes apply instantly, with everything at rest.
  - `[animations]` hot-reloads with the rest of the config.
- Add crates reluctantly. `windows`/`windows-core` provide Win32; `serde` and
  `toml` parse configuration only at startup and on reload, never on a keypress.
- `wm-bar` is a separate optional Rust/GDI process in `crates/bar`. The private
  `crates/bar-protocol` crate publishes a small atomic mapping and coalesces
  posted notifications with ACKs; no IPC threads or serialization in the WM.
  Bar commands use the existing actions; hotkeys still call them directly.
  The bar owns scoped title hooks and its minute clock. `bar.toml` hot-reloads.
  It registers as a shell appbar only to hear `ABN_FULLSCREENAPP`, never to
  reserve space, and drops to the bottom while a fullscreen app is open.
  CPU and physical-memory widgets use native GetSystemTimes/GlobalMemoryStatusEx
  on the bar thread, with timers only while enabled (2s/5s defaults). The
  `themes/yasb.toml` profile matches Ike's compact 18px personal YASB styling.
  Use manual `layout.reserved_top` matching `bar.height`, primary monitor only.
  Build/test both with `cargo build --workspace` / `cargo test --workspace`.
  Never launch the visible bar for agent verification; its renderer tests use
  a private bitmap and the protocol tests use hidden windows only.

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
- `wm do ACTION` posts a named hotkey action to the running WM without waiting
  for it to run. `[agent] allow` lists permitted names and reloads on save;
  all actions except `close` and `quit` are allowed by default. Refusals are
  logged by the WM. Hotkeys and bar commands do not use this list.
- Layout geometry, navigation, hotkeys and overview appearance update live.
  Existing workspaces/focus/modes and widths survive reloads; widths are only
  clamped if new layout limits require it. New default widths and explicit
  `[[window_rules]]` apply when a window joins (startup/open/restore), not to
  already managed windows. First matching rule wins; its supplied fields override
  learned state, then omitted values fall back to built-in defaults.
- Learned state stays in `rules.txt`; wm never writes user changes back to
  `config.toml`. Explicit rules can match exe, class and title substring, and set
  tiled/float/sticky/top/ignore, width, full width, and the workspace a window
  opens on (`focus = false` places it there without following). All present
  match conditions must match; executable names are case insensitive.
- Top-level `workspaces = [...]` names workspaces, which sit at the top in that
  order and stay when empty, as in niri. Rules and setups target them by name,
  or any workspace by number from the top.
- `[setups.NAME]` lists apps with their workspace, mode and width; its `key`
  moves the open windows of those apps into place (left to right in list
  order), launches the rest with `ShellExecute` on a thread, and places their
  windows as they appear for two minutes, without focus. A setup without
  `apps` keeps them in `setups.toml` beside the config, which its `save_key`
  rewrites from the layout on a thread; it's parsed with the config and
  hot-reloads with it. `behavior.startup_setup` loads one when wm starts.

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
- Windows that look like dialogs open floating, as in niri: ones with no
  resize border or the `#32770` dialog class. `behavior.float_dialogs`
  turns this off; explicit and learned rules win over it.
- The mode a window is put in and the width its column is given become the
  rule for its kind (program file name plus window class), so the next one
  opens the same way. Rules are written to `%APPDATA%\wm\rules.txt` on quit,
  never on a key press: one tab-separated `mode exe class [width [full]]` per
  line, with `tiled`, `float`, `sticky` or `top`. It can be edited by hand
  while the manager isn't running.
- Every hotkey and window event that changes something prints a line: what
  happened, the workspace on screen, then the focused window and how it's
  kept.
- Workspaces are never made by hand: named ones come from the config and
  always stay, the bottom one is always empty, and others disappear once
  emptied and left.
- `cargo run -- list` prints what would be tiled and what is skipped, with the
  reason, without touching anything.
- `cargo test -- --ignored` cloaks and uncloaks one real window to check the
  undocumented shell interface still works, which blinks that window, checks
  that Windows gives the hidden stage a thumbnail, which shows nothing, and
  slides a picture of a test window of its own across the stage for a second,
  reading the screen back to check that it's exact, see-through and
  click-through. Run that last one from PowerShell.

Cloaked windows carry a `wm.cloaked` window property, so a run that was killed
can be cleaned up by starting and quitting the manager again.

Stop komorebi and whkd before tiling (`komorebic stop --whkd`): two managers
fight over the same windows, and whkd swallows the same hotkeys.

Agents must not run the tiling mode unasked. It rearranges Ike's real desktop;
use `list` and the tests to check work instead.

Run it from PowerShell. If the binary exits with "Access is denied", the shell
has set `__COMPAT_LAYER` to force DPI-unaware, as some agent Git Bash shells do.

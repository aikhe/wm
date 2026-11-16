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
- One thread, one `GetMessage` loop, window events from `SetWinEventHook` with
  `WINEVENT_OUTOFCONTEXT`. No async runtime and no polling.
- Layout is a pure function from state to window rectangles, and only the
  differences get applied. This is the part worth unit testing.
- Scrolling moves windows and never resizes them.
- No animations unless Ike asks for them.
- `windows` is the only dependency. Add crates reluctantly.

## References

`references/` is gitignored and holds clones to read, not to copy from:

- `komorebi`: how to deal with Win32 (window filtering, cloaking, focus,
  borders). Its scrolling layout lives in `komorebi-scrolling/`.
- `niri`: the layout model to match, mainly `src/layout/scrolling.rs`.

Don't use GlazeWM as a reference; Ike finds it too slow.

## Build order

1. Enumerate and filter windows.
2. One monitor, one workspace, a column strip, focus left/right by hotkey.
3. Cloak off-screen columns; move column; width presets.
4. Vertical workspaces, then multi-monitor, then a config file.

## Running

- `cargo run` tiles the desktop. Alt+H/L moves focus between columns and
  Alt+K/J between workspaces; add Shift to move the focused column instead.
  Alt+R cycles its width, Alt+Shift+W closes the window, Alt+Shift+Q or Ctrl+C
  quits and restores every window. Focus and move wrap round the ends.
- Workspaces are never made by hand: the bottom one is always empty, and
  others disappear once emptied and left.
- `cargo run -- list` prints what would be tiled and what is skipped, with the
  reason, without touching anything.
- `cargo test -- --ignored` cloaks and uncloaks one real window to check the
  undocumented shell interface still works. It blinks that window.

Cloaked windows carry a `wm.cloaked` window property, so a run that was killed
can be cleaned up by starting and quitting the manager again.

Stop komorebi and whkd before tiling (`komorebic stop --whkd`): two managers
fight over the same windows, and whkd swallows the same hotkeys.

Agents must not run the tiling mode unasked. It rearranges Ike's real desktop;
use `list` and the tests to check work instead.

Run it from PowerShell. The agent Git Bash shell sets `__COMPAT_LAYER` to
force DPI-unaware, so the binary exits there with "Access is denied".

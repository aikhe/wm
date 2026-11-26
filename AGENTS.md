# wm

Scrollable-tiling window manager for Windows, modeled after niri in pure Rust
calling Win32 directly.

## Principles

- **Instant under load:** Switching must feel like Alt+Tab even when apps or
  the machine lag. Nothing sits between a hotkey and its action, and `wm` never
  waits on another app.
- **Zero idle CPU:** One thread blocked on `GetMessageW`, out-of-context
  WinEvents, and a directory-notification config watcher. No async runtime, no
  polling, no idle timers.
- **Small and obvious:** Prefer the simplest design that works. Don't add
  abstractions or keep complexity just because it exists.

## Safety

- **Never launch tiling mode or a visible bar unasked:** `cargo run` rearranges
  the live desktop and `cargo run -p wm-bar` puts a bar on screen. Verify with
  `cargo test --workspace`, `cargo run -- list`, and `cargo run -- config check`.
  (Stop komorebi/whkd before tiling so they don't fight over windows/hotkeys.)
- **Never write to `%APPDATA%\wm\`:** That holds live `config.toml`, `bar.toml`,
  `setups.toml`, `rules.txt`, and `session.txt`. Use `--config <temp-path>` for
  CLI checks.
- **Always use PowerShell:** Shells that set `__COMPAT_LAYER` (like Git Bash)
  force DPI-unaware and make `SetProcessDpiAwarenessContext` fail with
  `Access is denied`.
- **Never block on other apps or poll while idle:** Avoid plain `SendMessage`,
  `PrintWindow`, and idle timers. Use `SWP_ASYNCWINDOWPOS`, `ShowWindowAsync`,
  `SendMessageTimeout`, and DWM thumbnails.

## Codebase

- `src/layout.rs`, `src/layout/workspaces.rs` — Pure layout state → rects and
  diffs (main unit-test surface).
- `src/manager.rs`, `src/events.rs`, `src/bindings.rs`, `src/window.rs`,
  `src/cloak.rs` — Message loop, `RegisterHotKey`, WinEvents, window snapshots,
  and cloaking (`wm.cloaked`).
- `src/stage.rs`, `src/animation.rs`, `src/frame_clock.rs`, `src/overview.rs`,
  `src/thumbnail.rs` — DWM thumbnail stage, springs, `DwmFlush` pacing, and
  overview backdrop.
- `src/config.rs`, `src/config_watch.rs`, `src/rules.rs`, `src/setups.rs`,
  `src/session.rs` — TOML config, hot-reload watcher, learned `rules.txt`,
  setups, and `session.txt` restored on restart. Defaults:
  [wm.default.toml](wm.default.toml).
- `crates/bar-protocol` — Private versioned shared-memory (`AtomicU64`) and
  window-message protocol for `wm`, `wm do`, and `wm-bar`.
- `crates/bar` — Optional Win32/GDI top bar (`wm-bar`). Defaults:
  [crates/bar/bar.default.toml](crates/bar/bar.default.toml).
- `.agents/references/` — Gitignored, read-only references when present (`niri`
  for `src/layout/scrolling.rs`, `komorebi` for Win32 quirks, `LeopardWM` for
  floating/sticky/z-order in `crates/platform_win32/`). Never edit them, copy
  from them, or use GlazeWM.

## Architecture

### Win32 & events

- **In-process hotkeys:** `RegisterHotKey` dispatches directly to a function
  call—no CLI, IPC, or process spawn.
- **Cloak off-screen windows:** Cloak off-screen columns and mark them with
  `wm.cloaked` so startup/exit cleans up after crashes. Never hide or minimize
  managed windows.
- **Re-check late WinEvents:** `SetWinEventHook` (`WINEVENT_OUTOFCONTEXT`) can
  arrive after newer hotkeys. Always re-check live window state before acting.
- **Floating z-order:** Track floating windows per workspace without
  positioning them. Keep them above tiled windows by re-raising on tiled focus;
  only `top` mode uses topmost.
- **Unwind on panic:** Release builds intentionally unwind (no `panic = "abort"`)
  so `Drop` still uncloaks and restores windows.

### Layout & config

- **Pure layout:** State maps purely to rects; apply diffs only. Scrolling
  moves windows without resizing them.
- **Workspaces:** Named workspaces stay at the top even when empty; the bottom
  workspace is always empty; unnamed workspaces vanish once emptied and left.
- **Atomic hot-reload:** Parse off the input thread. Invalid syntax, geometry,
  unknown keys, duplicates, or unavailable hotkeys reject the whole update and
  keep existing workspaces, focus, modes, and hotkeys.
- **Never mutate `config.toml`:** Learned state goes to `rules.txt` and saved
  setups to `setups.toml`. Precedence: `[[window_rules]]` > learned state >
  dialog floating > defaults.

### Animations & overview

- **Presentation only:** Focus changes immediately. Never move real windows per
  frame—`stage.rs` (see-through, click-through, paints nothing so wallpaper stays
  visible) animates DWM thumbnails instead.
- **No-flash swap:** Show thumbnails over real windows → one composed frame
  later cloak and move real windows → once settled, uncloak real windows under
  the thumbnails before clearing them.
- **Focused window cloak trap:** Windows uncloaks a window that takes focus and
  strips focus from a cloaked one. Never cloak the focused window under its
  thumbnail—park it uncloaked one screen height below its slot and focus it
  once placed. Check both `Window::cloaked` and `wm.cloaked`.
- **Springs & pacing:** Critically damped springs (`animation.rs`) per edge,
  paced by `DwmFlush` only while moving. Mid-move input retargets keeping
  velocity. Desktop thumbnails stay 1:1 (only overview scales); width and mode
  changes apply instantly at rest.
- **Stateless overview:** `overview.rs` stores no layout state, redraws after
  every `apply`, runs in the main loop, and draws only the backdrop behind
  `stage.rs`.

### Bar & `wm do`

- **Lock-free publish:** `wm` writes an atomic shared-memory snapshot and
  coalesces one notification per `ACK`. No IPC threads or serialization in `wm`;
  build `wm` and `wm-bar` together.
- **Action gating:** `wm do` posts to the same host window and checks
  `[agent] allow`. Hotkeys and bar commands bypass `[agent] allow`.
- **Bar scope:** `wm-bar` targets the primary monitor using `layout.reserved_top`.
  It registers as an appbar only for `ABN_FULLSCREENAPP` (drops behind
  fullscreen apps), never to reserve space. Focused-thread title hooks and
  optional `clock`/`cpu`/`memory` timers (`GetSystemTimes`/`GlobalMemoryStatusEx`)
  live in `wm-bar` and stop when disabled.

## Checklist

Before finishing a change, check what applies:

- **Layout:** pure logic and unit tests (`src/layout.rs`, `src/layout/workspaces.rs`).
- **Presentation:** `stage.rs` animations and `overview.rs` clicks/highlights.
- **Config:** `wm.default.toml` / `crates/bar/bar.default.toml`, atomic reload, `config check`.
- **Persistence:** `[[window_rules]]`, `rules.txt`, `setups.toml`, and `session.txt`.
- **External paths:** `wm do` / `[agent] allow` and `bar-protocol` / `wm-bar`.
- **Docs:** update [README.md](README.md) only when user-facing behavior changes.

## Checks

Run from PowerShell:

```powershell
cargo test --workspace
cargo build --workspace
cargo run -- config check
cargo run -- list
```

- Bar tests use offscreen bitmaps and hidden windows only.
- `cargo test -- --ignored` blinks a real window and slides a test thumbnail
  while reading the screen back—run only from PowerShell when testing cloaking
  or `stage.rs`.

## Conventions

- **Dependencies:** Add crates reluctantly (`windows`/`windows-core` for Win32;
  `serde`/`toml` only on load/reload).
- **Comments:** Default to none. Comment only Win32 quirks or non-obvious
  invariants.
- **Docs:** Don't duplicate [README.md](README.md) or `*.default.toml` here;
  rewrite stale text instead of appending.
- **Issues & PRs:** Use `.agents/skills/issue/SKILL.md` and
  `.agents/skills/pr/SKILL.md` when present. Don't open a PR unless asked.
- **Push back:** Call out any rule here that conflicts with the task before
  breaking it.

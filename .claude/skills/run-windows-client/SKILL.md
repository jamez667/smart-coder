---
name: run-windows-client
description: Launch the smart-coder Windows desktop client (sc-win, an iced GUI). Use when asked to run, start, open, or screenshot the Windows client / the GUI / the desktop app.
---

# Run the smart-coder Windows client (`sc-win`)

`sc-win` is a native **iced 0.14** desktop GUI — the "vibe coding" Windows client
(spec 12 / M9). It opens a window on launch; it needs **no CLI args** and **no
running model backend just to start** (the backend is only needed once you drive
an actual coding task from inside the app).

## Build

```bash
rtk cargo build -p sc-win
```

Binary lands at `target/debug/sc-win.exe` (or `target/release/sc-win.exe` with
`--release`).

### Debug vs release: the console window

`main.rs` sets `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`:

- **Release** (`--release`) → pure GUI, **no console window**. Use this for a
  clean single-window run / demo / screenshot.
- **Debug** → keeps a console window so panics and `println!` diagnostics are
  visible. The stray blank black terminal next to the GUI is expected here, not a
  bug. Use debug when you want that diagnostic output.

## Launch (background, non-blocking)

A GUI blocks its shell, so launch detached and confirm the window opened:

```powershell
# Use target\release\sc-win.exe for a console-free run; debug keeps the terminal.
$p = Start-Process -FilePath "target\release\sc-win.exe" -PassThru
Start-Sleep -Seconds 3
$proc = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
if ($proc) {
    "RUNNING pid=$($p.Id) title='$($proc.MainWindowTitle)'"
} else {
    "EXITED early — crashed on launch"
}
```

Expected: `RUNNING` with window title **`smart-coder — vibe coding`**. If it
exited early, re-run in the foreground to see the panic:
`.\target\debug\sc-win.exe`.

## Console windows on Windows (two separate gotchas)

1. **A stationary blank terminal next to the GUI** = the binary built as a console
   subsystem app. `main.rs` sets `windows_subsystem = "windows"` for release, so
   this only appears in **debug** (intentional — keeps diagnostics visible).
2. **Hundreds of terminals flashing open/closed** = the GUI shelling out to `git`
   (status/diff polling on refresh) with each console child popping a `conhost`
   window. Fixed: all git/subprocess spawns go through [`crate::proc`](../../../crates/sc-win/src/proc.rs)
   (`proc::git()` / `proc::command()`), which set `CREATE_NO_WINDOW` on Windows.
   **If new flashing appears, a new spawn site bypassed `proc::` — route it through
   the helper** rather than calling `std::process::Command::new` directly.
   (Note: `sc-verify`/swarm spawns during an actual task are not yet routed through
   `proc::` — that crate is shared with the headless CLI where a console is wanted.)

## Drive it — you CANNOT synthesize input

**Hard rule for this repo:** never move the mouse or send keystrokes to drive the
GUI (no `SetCursorPos`, `mouse_event`, `SendKeys`, or any synthetic input). To
verify the window actually rendered (not a blank frame), **ask the user for a
screenshot** — that is the only permitted way to observe the UI. A launched
process with a live `MainWindowTitle` proves the entrypoint resolved, but only a
screenshot proves it drew.

## Config & backend (for real tasks, not just launch)

- The app reads its backend endpoint/model from `%APPDATA%\smart-coder\config.json`,
  overridden by env vars **`SC_BASE_URL`** / **`SC_MODEL`** (defaults to
  `http://localhost:11435/v1` + `qwen3-coder-30b` in the driver examples).
- The model backends themselves (llama.cpp launchers) live in the separate
  **smart-coder-ops** repo, not here. The GUI launches fine without them; coding
  tasks inside it will fail to reach a model until a backend is up.

## Stop it

```powershell
Get-Process sc-win -ErrorAction SilentlyContinue | Stop-Process -Force
```

## Note: rust-analyzer file locks

Renaming/moving files under `crates/` can fail with "Permission denied" on Windows
because VS Code's rust-analyzer holds handles. If a `git mv`/build fails that way,
stop rust-analyzer (`Get-Process rust-analyzer | Stop-Process -Force`) — VS Code
respawns it automatically.

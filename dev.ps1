# Rebuilds wm and swaps it in for the running one. The old one quits cleanly,
# saving session.txt, so the new one puts every window back where it was.
# Needs "quit" in [agent] allow. Arguments go to the new wm, e.g. --config PATH.

$ErrorActionPreference = 'Stop'
# Git Bash sets this, and it makes wm DPI-unaware.
Remove-Item Env:__COMPAT_LAYER -ErrorAction SilentlyContinue

# Its own target folder, since Windows locks the running wm.exe.
$target = Join-Path $PSScriptRoot 'target\dev'
cargo build --workspace --target-dir $target
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$exe = Join-Path $target 'debug\wm.exe'

$running = @(Get-Process -Name wm -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
    & $exe do quit
    try {
        $running | Wait-Process -Timeout 10 -ErrorAction Stop
    } catch {
        # Killing it would skip restoring the windows and saving the session.
        Write-Error ('wm did not quit. Check its console for "Agent refused quit" or ' +
            'a rejected reload, or quit it with its hotkey and run this again.')
    }
}

& $exe @args

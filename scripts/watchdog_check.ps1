#Requires -Version 5.1
# watchdog_check.ps1 - Windows equivalent of watchdog_check.sh.
#
# Verify the parent-death watchdog terminates rust-watcher when its Win32
# parent dies without closing stdin.
#
# Strategy:
#   1. Spawn `cmd /c "<keeper> | rust-watcher.exe"`. cmd.exe creates an
#      anonymous pipe, spawns the keeper (a long Start-Sleep) with stdout =
#      pipe write-end, and spawns rust-watcher.exe with stdin = pipe read-end.
#      cmd.exe is the Win32 parent of both children.
#   2. TerminateProcess the wrapper cmd.exe. Windows does NOT propagate this
#      to child processes, so the keeper survives and continues to hold the
#      pipe write-end - rust-watcher's stdin therefore stays open. The only
#      thing that can terminate rust-watcher is the parent-death watchdog
#      thread (WaitForSingleObject on the parent process handle).
#   3. Verify rust-watcher exits within a deadline. Clean up the orphaned
#      keeper afterwards.

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$rustBin = Join-Path $repoRoot 'target\release\rust-watcher.exe'

if (-not (Test-Path -LiteralPath $rustBin)) {
    Write-Host "watchdog_check: $rustBin not found - build the release binary first"
    exit 1
}

$keeperCmd = 'powershell.exe -NoProfile -Command "Start-Sleep -Seconds 60"'
$pipeline  = "$keeperCmd | `"$rustBin`""

$wrapper = Start-Process -FilePath 'cmd.exe' -ArgumentList '/c', $pipeline `
    -PassThru -WindowStyle Hidden

# Wait for rust-watcher.exe to appear as a child of the wrapper.
$rustPid = 0
for ($i = 0; $i -lt 50; $i++) {
    Start-Sleep -Milliseconds 100
    $proc = Get-CimInstance Win32_Process -Filter "Name = 'rust-watcher.exe' AND ParentProcessId = $($wrapper.Id)"
    if ($proc) {
        $rustPid = [int]$proc.ProcessId
        break
    }
}
if ($rustPid -eq 0) {
    Write-Host "watchdog_check: FAIL - rust-watcher did not start"
    Stop-Process -Id $wrapper.Id -Force -ErrorAction SilentlyContinue
    exit 1
}

# Capture the keeper PID for cleanup before we kill the wrapper.
$keeperProc = Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe' AND ParentProcessId = $($wrapper.Id)"
$keeperPid = if ($keeperProc) { [int]$keeperProc.ProcessId } else { 0 }

# Orphan rust-watcher.
Stop-Process -Id $wrapper.Id -Force

# Watchdog must terminate rust-watcher within the deadline.
$deadlineSec = 5
$start = Get-Date
$exited = $false
while (((Get-Date) - $start).TotalSeconds -lt $deadlineSec) {
    if (-not (Get-Process -Id $rustPid -ErrorAction SilentlyContinue)) {
        $exited = $true
        break
    }
    Start-Sleep -Milliseconds 100
}

if ($keeperPid -ne 0) {
    Stop-Process -Id $keeperPid -Force -ErrorAction SilentlyContinue
}

if ($exited) {
    $elapsed = [int]((Get-Date) - $start).TotalSeconds
    Write-Host "watchdog_check: OK - rust-watcher (pid $rustPid) exited ${elapsed}s after parent death"
    exit 0
}

Write-Host "watchdog_check: FAIL - rust-watcher (pid $rustPid) still alive ${deadlineSec}s after parent death"
Stop-Process -Id $rustPid -Force -ErrorAction SilentlyContinue
exit 1

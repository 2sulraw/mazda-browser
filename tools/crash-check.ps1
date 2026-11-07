# Supervisor crash-recovery check (P1).
#
# Starts the real shell, waits for the first page, kills the renderer process
# (not the shell), and asserts that the supervisor restarts it, replays the
# tab, and the page comes back. The shell's crash budget is 5 restarts.
#
# Usage:
#   .\tools\crash-check.ps1
#   .\tools\crash-check.ps1 -Url https://github.com

param(
    [string]$ShellExe = (Join-Path $PSScriptRoot '..\target\release\shell.exe'),
    [string]$Url = 'https://example.com',
    [int]$TimeoutSeconds = 60
)

$ErrorActionPreference = 'Stop'

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ShellExe
$psi.Arguments = $Url
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$p = [System.Diagnostics.Process]::Start($psi)

$lines = New-Object System.Collections.ArrayList
$script:pending = $null
function Read-Line([int]$timeoutMs = 2000) {
    if ($null -eq $script:pending) { $script:pending = $p.StandardOutput.ReadLineAsync() }
    if (-not $script:pending.Wait($timeoutMs)) { return $null }
    $line = $script:pending.Result
    $script:pending = $null
    return $line
}
function Pump-Until([string]$pattern, [int]$timeoutMs = 0) {
    if ($timeoutMs -le 0) { $timeoutMs = $TimeoutSeconds * 1000 }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $timeoutMs) {
        $line = Read-Line 500
        if ($null -ne $line) {
            [void]$lines.Add($line)
            if ($line -match $pattern) { return $true }
        }
    }
    return $false
}
function Renderer-Child {
    @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($p.Id)" |
        Where-Object { $_.Name -eq 'renderer.exe' })
}

try {
    if (-not (Pump-Until '^\[page\]' )) { throw "no first page within $TimeoutSeconds s" }
    $kids = @(Renderer-Child)
    if ($kids.Count -ne 1) { throw "expected one renderer, found $($kids.Count)" }
    $firstPid = $kids[0].ProcessId
    "first page: $(@($lines | Where-Object { $_ -match '^\[page\]' })[0])"
    "killing renderer pid $firstPid"

    Stop-Process -Id $firstPid -Force
    $restartSeen = Pump-Until '^\[supervisor\] restart 1'
    $pageSeen = Pump-Until '^\[page\]'
    $kids2 = @(Renderer-Child)
    $newPid = if ($kids2.Count -eq 1) { $kids2[0].ProcessId } else { 0 }
} finally {
    try { $p.StandardInput.WriteLine('quit'); $p.StandardInput.Flush() } catch {}
    $null = $p.WaitForExit(5000)
    if (-not $p.HasExited) { try { $p.Kill() } catch {} }
}

"--- shell output"
$lines | ForEach-Object { $_ }
$pass = $restartSeen -and $pageSeen -and ($kids2.Count -eq 1) -and ($newPid -ne $firstPid)
"gate: restart=$restartSeen restored-page=$pageSeen new-renderer=$($newPid -ne $firstPid) -> $(if ($pass) { 'PASS' } else { 'FAIL' })"
if (-not $pass) { exit 1 }

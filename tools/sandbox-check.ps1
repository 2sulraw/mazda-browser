# Renderer sandbox check (P2).
#
# Runs the sandbox probe, which spawns itself through the same sandbox the
# shell uses and verifies from the inside:
#   * the user's home file cannot be read
#   * no child process can be created
#   * the network still works
#   * the sandbox temp directory is writable (frame file path)
#
# There is no memory cap by design: the renderer is kept lean by what it
# allocates, not by being killed at a threshold.
#
# Usage:
#   .\tools\sandbox-check.ps1

param(
    [string]$ProbeExe = (Join-Path $PSScriptRoot '..\target\release\sandbox-probe.exe')
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $ProbeExe)) {
    throw "missing $ProbeExe - run: cargo build --release --workspace"
}

$canary = Join-Path $env:USERPROFILE "mazda-sandbox-canary-$PID.txt"
Set-Content -Path $canary -Value 'this must stay unreadable' -Encoding ascii
try {
    & $ProbeExe --canary $canary
    $code = $LASTEXITCODE
} finally {
    Remove-Item $canary -Force -ErrorAction SilentlyContinue
}

if ($code -ne 0) { exit $code }
"gate: home read denied, no child process, network ok -> PASS"

# Paint timeline: how much of the page is on screen, second by second.
#
# A load is not one paint. The markup + stylesheet pass publishes a frame
# while the page's scripts are still running, and the finished page replaces
# it later, so "blank until it loads" and "painted at 4.5 s" can both be true
# of different moments. This tool drives the renderer the way the shell does
# (`resize`, `load`, frames on stdout) and samples the published frame on a
# fixed interval, so every second of the load has a number: how much ink is on
# screen and how many colours it is made of. A blank window reads 0 % ink and
# 1 colour; a text-only first paint reads a couple of percent; the finished
# page reads two digits. No window, no screenshot.
#
#   powershell -File tools\paint-timeline.ps1 -Url https://github.com/
#   powershell -File tools\paint-timeline.ps1 -Url https://example.com -Seconds 20
#
# The frame buffer is `%TEMP%\mazda-frame-<pid>-<w>x<h>.bin` (ipc::frame_path):
# a 32-byte header (magic, version, w, h, seq, slot) and two BGRA planes, the
# header naming the plane that holds the newest frame. Only every 4th row and
# every 4th pixel is read, so a sample costs about a sixteenth of a frame and
# does not itself slow the load down. The file is opened shared: the renderer
# keeps it mapped for writing.
param(
    [Parameter(Mandatory = $true)][string]$Url,
    [int]$Width = 1000,
    [int]$Height = 670,
    [int]$Seconds = 40,
    # seconds between printed rows; a new frame is always printed
    [int]$Every = 1,
    [int]$IntervalMs = 200,
    [string]$RendererExe = '',
    # fresh profile by default: a warm cache measures the cache, not the load
    [string]$Profile = "$env:TEMP\mazda-paint-profile",
    [string]$KeepProfile = '',
    # optional: write every new frame as a PNG (the renderer's own pixels, not
    # a screen capture) so a bad-looking frame can be looked at
    [string]$PngDir = ''
)

$ErrorActionPreference = 'Stop'

# `$PSScriptRoot` is empty inside a param default under `powershell -File`
if (-not $RendererExe) {
    $RendererExe = Join-Path $PSScriptRoot '..\target\release\renderer.exe'
}
if (-not (Test-Path $RendererExe)) { throw "renderer not found: $RendererExe" }

Add-Type -TypeDefinition @"
using System;
using System.IO;
using System.Collections.Generic;

public static class FrameBin {
    // Returns "seq w h ink colours bg" or "" when there is no readable frame.
    public static string Sample(string path, int rowStep, int colStep) {
        try {
            using (var fs = new FileStream(path, FileMode.Open, FileAccess.Read,
                                          FileShare.ReadWrite)) {
                var hb = new byte[32];
                int got = 0;
                while (got < 32) {
                    int k = fs.Read(hb, got, 32 - got);
                    if (k <= 0) return "";
                    got += k;
                }
                if (BitConverter.ToUInt32(hb, 0) != 0x4D5A4442u) return "";
                int w = BitConverter.ToInt32(hb, 8);
                int h = BitConverter.ToInt32(hb, 12);
                uint seq = BitConverter.ToUInt32(hb, 16);
                int slot = BitConverter.ToInt32(hb, 20);
                if (w <= 0 || h <= 0 || slot < 0 || slot > 1) return "";
                long plane = (long)w * h * 4;
                long off = 32 + (long)slot * plane;
                if (fs.Length < off + plane) return "";
                var row = new byte[w * 4];
                var hist = new Dictionary<int, int>();
                int total = 0;
                for (int y = 0; y < h; y += rowStep) {
                    fs.Position = off + (long)y * w * 4;
                    got = 0;
                    while (got < row.Length) {
                        int k = fs.Read(row, got, row.Length - got);
                        if (k <= 0) break;
                        got += k;
                    }
                    if (got < row.Length) break;
                    for (int x = 0; x < w; x += colStep) {
                        int o = x * 4;
                        int c = row[o] | (row[o + 1] << 8) | (row[o + 2] << 16);
                        int v;
                        hist.TryGetValue(c, out v);
                        hist[c] = v + 1;
                        total++;
                    }
                }
                if (total == 0) return "";
                int bg = 0, bgc = 0;
                foreach (var kv in hist) {
                    if (kv.Value > bg) { bg = kv.Value; bgc = kv.Key; }
                }
                double ink = 100.0 * (total - bg) / total;
                return string.Format("{0} {1} {2} {3:F2} {4} {5:X6}",
                                     seq, w, h, ink, hist.Count, bgc);
            }
        } catch (IOException) {
            return "";   // mid-write, or the renderer just remapped it
        } catch (UnauthorizedAccessException) {
            return "";
        }
    }
}
"@

# The renderer's own pixels for one frame, saved as a PNG. This is not a
# screen capture: it reads the frame buffer the shell blits, so what it shows
# is exactly what the window shows, even with nothing on screen.
function Save-FramePng([string]$path, [string]$png) {
    Add-Type -AssemblyName System.Drawing -ErrorAction SilentlyContinue
    $fs = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try {
        $hb = New-Object byte[] 32
        $got = 0
        while ($got -lt 32) {
            $k = $fs.Read($hb, $got, 32 - $got)
            if ($k -le 0) { return }
            $got += $k
        }
        $w = [BitConverter]::ToInt32($hb, 8)
        $h = [BitConverter]::ToInt32($hb, 12)
        $slot = [BitConverter]::ToInt32($hb, 20)
        $plane = $w * $h * 4
        $fs.Position = 32 + $slot * $plane
        $buf = New-Object byte[] $plane
        $got = 0
        while ($got -lt $plane) {
            $k = $fs.Read($buf, $got, $plane - $got)
            if ($k -le 0) { break }
            $got += $k
        }
        if ($got -lt $plane) { return }
    } finally {
        $fs.Dispose()
    }
    $bmp = New-Object Drawing.Bitmap($w, $h, [Drawing.Imaging.PixelFormat]::Format32bppRgb)
    $rect = New-Object Drawing.Rectangle(0, 0, $w, $h)
    $data = $bmp.LockBits($rect, [Drawing.Imaging.ImageLockMode]::WriteOnly, $bmp.PixelFormat)
    [Runtime.InteropServices.Marshal]::Copy($buf, 0, $data.Scan0, $plane)
    $bmp.UnlockBits($data)
    $bmp.Save($png, [Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}

if ($PngDir) { New-Item -ItemType Directory -Force -Path $PngDir | Out-Null }

if ($KeepProfile) { $Profile = $KeepProfile }
if (-not $KeepProfile) { Remove-Item -Recurse -Force $Profile -ErrorAction SilentlyContinue }
$env:LOCALAPPDATA = $Profile
# stage timings land on stderr and are quoted in the summary
$env:MAZDA_TIME = '1'

$psi = New-Object Diagnostics.ProcessStartInfo
$psi.FileName = $RendererExe
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$p = [Diagnostics.Process]::Start($psi)
# stderr is loud under MAZDA_TIME: drain it on a background read
$err = $p.StandardError.ReadToEndAsync()

$framePath = Join-Path $env:TEMP ("mazda-frame-{0}-{1}x{2}.bin" -f $p.Id, $Width, $Height)

$p.StandardInput.WriteLine("resize $Width $Height")
Start-Sleep -Milliseconds 250
$sw = [Diagnostics.Stopwatch]::StartNew()
$p.StandardInput.WriteLine("load $Url")
$p.StandardInput.Flush()

$rows = @()
$frames = @()
$lastSeq = -1
$nextTick = 0
$blankSeconds = 0.0
while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
    $t = $sw.Elapsed.TotalSeconds
    $line = [FrameBin]::Sample($framePath, 4, 4)
    if ($line) {
        $f = $line -split ' '
        $seq = [int]$f[0]
        $ink = [double]$f[3]
        $colours = [int]$f[4]
        $isNew = $seq -ne $lastSeq
        $due = $t -ge $nextTick
        if ($isNew) {
            $lastSeq = $seq
            $frames += [pscustomobject]@{ Seq = $seq; At = [math]::Round($t, 2); Ink = $ink; Colours = $colours }
            if ($PngDir) {
                Save-FramePng $framePath (Join-Path $PngDir ("frame{0:D2}-seq{1}.png" -f $frames.Count, $seq))
            }
        }
        if ($isNew -or $due) {
            $nextTick = ([math]::Floor($t / $Every) + 1) * $Every
            $rows += [pscustomobject]@{
                Time    = [math]::Round($t, 1)
                Frame   = $seq
                Ink     = $ink
                Colours = $colours
                Note    = if ($isNew) { 'new frame' } else { '' }
            }
            if ($rows.Count -eq 1 -and $rows[0].Frame -eq 0) { $blankSeconds = $t }
        }
    } elseif ($t -ge $nextTick) {
        # no frame at all yet: the window is whatever the shell painted before
        $rows += [pscustomobject]@{
            Time = [math]::Round($t, 1); Frame = -1; Ink = 0.0; Colours = 0; Note = 'no frame yet'
        }
        $nextTick = ([math]::Floor($t / $Every) + 1) * $Every
    }
    $n = [math]::Max(20, $IntervalMs - [int]((($sw.Elapsed.TotalSeconds - $t) * 1000)))
    Start-Sleep -Milliseconds $n
}

$p.StandardInput.WriteLine('quit')
$p.StandardInput.Flush()
if (-not $p.WaitForExit(4000)) { $p.Kill() }

Write-Output ""
Write-Output "== $Url  (${Width}x${Height})  $(Get-Date -Format 'HH:mm:ss')"
$rows | Format-Table -AutoSize | Out-String | Write-Output

$first = $frames | Where-Object { $_.Seq -ge 1 } | Select-Object -First 1
$final = $frames | Select-Object -Last 1
if ($first) {
    Write-Output ("first frame: {0,6:N1} s  ink {1,5:N2} %  {2} colours" -f $first.At, $first.Ink, $first.Colours)
}
if ($final) {
    Write-Output ("last frame:  {0,6:N1} s  ink {1,5:N2} %  {2} colours  (#{3})" -f $final.At, $final.Ink, $final.Colours, $final.Seq)
}
if ($frames.Count -gt 1) {
    Write-Output ("frames:      {0}  at {1}" -f $frames.Count, (($frames | ForEach-Object { $_.At }) -join ', '))
}

$log = $err.Result
$stages = $log -split "`r?`n" | Where-Object {
    $_ -match 'first paint|painted at|scripts:|image probe|media probe|cascade \+ box tree|css parse'
}
if ($stages) {
    Write-Output ""
    Write-Output "engine stages (stderr):"
    $stages | ForEach-Object { "  $_" }
}

if (-not $KeepProfile) { Remove-Item -Recurse -Force $Profile -ErrorAction SilentlyContinue }

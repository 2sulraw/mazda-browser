param(
    [string]$Url = 'https://example.com',
    [string]$Out = 'C:\Users\werlos\AppData\Local\Temp\opencode\ui.png',
    [int]$Wait = 8,
    [int]$Width = 0,
    [int]$Height = 0
)
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinApi {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  public static void Focus(IntPtr h) {
    uint pid;
    uint fg = GetWindowThreadProcessId(GetForegroundWindow(), out pid);
    uint me = GetCurrentThreadId();
    AttachThreadInput(me, fg, true);
    ShowWindow(h, 9); // SW_RESTORE
    BringWindowToTop(h);
    SetForegroundWindow(h);
    AttachThreadInput(me, fg, false);
  }
}
"@
$exe = Join-Path $PSScriptRoot '..\target\release\shell.exe'
$argsList = @($Url)
$p = Start-Process -FilePath $exe -ArgumentList $argsList -PassThru
Start-Sleep -Seconds $Wait
if ($Width -gt 0 -and $Height -gt 0) {
    # SWP_NOMOVE | SWP_NOZORDER
    [WinApi]::SetWindowPos($p.MainWindowHandle, [IntPtr]::Zero, 0, 0, $Width, $Height, 0x0002 -bor 0x0004) | Out-Null
    Start-Sleep -Seconds 1
}
[WinApi]::Focus($p.MainWindowHandle)
Start-Sleep -Seconds 1
$r = New-Object WinApi+RECT
[WinApi]::GetWindowRect($p.MainWindowHandle, [ref]$r) | Out-Null
Add-Type -AssemblyName System.Drawing
$w = $r.Right - $r.Left
$h = $r.Bottom - $r.Top
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size $w, $h))
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
"window $w x $h at $($r.Left),$($r.Top) -> $Out"
$p.CloseMainWindow() | Out-Null
Start-Sleep -Milliseconds 400
if (-not $p.HasExited) { $p | Stop-Process -Force }

# Capture the OMRC client window rectangle (ASCII only on purpose:
# Windows PowerShell 5.1 misreads UTF-8 scripts without BOM).
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32 {
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr h);
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
"@

$target = [IntPtr]::Zero
foreach ($p in Get-Process) {
    if ($p.MainWindowTitle -like "OMRC*") { $target = $p.MainWindowHandle }
}

if ($target -eq [IntPtr]::Zero) {
    Write-Output "client window not found"
    exit 1
}

[void][Win32]::SetForegroundWindow($target)
Start-Sleep -Milliseconds 400

$r = New-Object Win32+RECT
[void][Win32]::GetWindowRect($target, [ref]$r)
$w = $r.Right - $r.Left
$h = $r.Bottom - $r.Top
if ($w -le 0 -or $h -le 0) { Write-Output "bad rect"; exit 1 }

$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size($w, $h)))
$bmp.Save("D:\project\software\OMRC\client_shot.png", [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bmp.Dispose()
Write-Output ("saved " + $w + "x" + $h)

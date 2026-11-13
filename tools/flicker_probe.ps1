# Sample the client window's average brightness a few times to detect black flashing.
# ASCII only (Windows PowerShell 5.1 misreads UTF-8 scripts without BOM).
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32b {
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr h, out RECT r);
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
"@

$target = [IntPtr]::Zero
foreach ($p in Get-Process) {
    if ($p.MainWindowTitle -like "OMRC*") { $target = $p.MainWindowHandle }
}
if ($target -eq [IntPtr]::Zero) { Write-Output "client window not found"; exit 1 }

$r = New-Object Win32b+RECT
[void][Win32b]::GetWindowRect($target, [ref]$r)

for ($i = 0; $i -lt 8; $i++) {
    $bmp = New-Object System.Drawing.Bitmap(160, 90)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.Left + 60, $r.Top + 60, 0, 0, (New-Object System.Drawing.Size(160, 90)))
    $sum = 0
    for ($y = 0; $y -lt 90; $y += 6) {
        for ($x = 0; $x -lt 160; $x += 6) {
            $c = $bmp.GetPixel($x, $y)
            $sum += ($c.R + $c.G + $c.B) / 3
        }
    }
    $n = [math]::Floor(160 / 6) * [math]::Floor(90 / 6)
    Write-Output ("sample {0}: mean {1:N1}" -f $i, ($sum / $n))
    $g.Dispose(); $bmp.Dispose()
    Start-Sleep -Milliseconds 220
}

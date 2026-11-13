# Keep omrc-host alive: if it exits (watchdog suicide on a stalled DXGI call),
# restart it after a short pause. Ctrl+C / stopping this process kills the loop.
# ASCII only (Windows PowerShell 5.1 misreads UTF-8 scripts without BOM).
param(
    [string]$Peer = "127.0.0.1:9100",
    [string]$Fps = "60",
    [string]$Bitrate = "30000000",
    [string]$Encoder = "hw",
    [string]$LogDir = "D:\project\software\OMRC"
)

$exe = Join-Path $LogDir "target\release\omrc-host.exe"
$out = Join-Path $LogDir "host_out.log"
$err = Join-Path $LogDir "host_err.log"

$n = 0
while ($true) {
    $n++
    "[supervisor] start #$n $(Get-Date -Format o)" | Out-File $err -Append -Encoding utf8
    $p = Start-Process -FilePath $exe -ArgumentList @(
        "play", "--peer", $Peer, "--fps", $Fps, "--bitrate", $Bitrate, "--encoder", $Encoder
    ) -RedirectStandardOutput $out -RedirectStandardError $err -NoNewWindow -PassThru -Wait
    "[supervisor] exited code=$($p.ExitCode) $(Get-Date -Format o)" | Out-File $err -Append -Encoding utf8
    Start-Sleep -Milliseconds 700
}

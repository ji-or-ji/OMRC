# 提权命令托管宿主。
#
# 由季手动以管理员身份启动（UAC 授权一次），之后本会话的管理员命令
# 都通过往 inbox 放 .ps1 来执行。
#
# 设计约束：
#   * 不监听任何端口，不接受远程输入
#   * 只执行 inbox 里的 .ps1，跑完改名成 .done 留档
#   * stop 文件或 45 分钟空闲自动退出

$ErrorActionPreference = 'Stop'

$root   = 'D:\project\software\OMRC\.admin'
$inbox  = Join-Path $root 'inbox'
$outbox = Join-Path $root 'outbox'
$log    = Join-Path $root 'host.log'

New-Item -ItemType Directory -Force -Path $inbox, $outbox | Out-Null

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = ([Security.Principal.WindowsPrincipal]$identity).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

"$(Get-Date -Format o) host start user=$($identity.Name) admin=$isAdmin pid=$PID" |
    Out-File $log -Append -Encoding utf8

if (-not $isAdmin) {
    "$(Get-Date -Format o) 警告：没有拿到管理员令牌，任务会以普通权限执行" |
        Out-File $log -Append -Encoding utf8
}

$idleSince = Get-Date

while ($true) {
    if (Test-Path (Join-Path $root 'stop')) {
        Remove-Item (Join-Path $root 'stop') -Force -ErrorAction SilentlyContinue
        "$(Get-Date -Format o) stop 文件出现，退出" | Out-File $log -Append -Encoding utf8
        break
    }

    $jobs = @(Get-ChildItem $inbox -Filter *.ps1 -File -ErrorAction SilentlyContinue |
        Sort-Object Name)

    if ($jobs.Count -gt 0) {
        $idleSince = Get-Date
        foreach ($job in $jobs) {
            $base = $job.BaseName
            $started = Get-Date
            "$(Get-Date -Format o) run $base" | Out-File $log -Append -Encoding utf8

            try {
                $out = & pwsh -NoProfile -NonInteractive -ExecutionPolicy Bypass `
                    -File $job.FullName 2>&1 | Out-String
                $code = $LASTEXITCODE
            } catch {
                $out = ($_ | Out-String)
                $code = 1
            }

            $out | Out-File (Join-Path $outbox "$base.out") -Encoding utf8
            "exit=$code started=$($started.ToString('o')) ended=$((Get-Date).ToString('o'))" |
                Out-File (Join-Path $outbox "$base.code") -Encoding utf8
            Move-Item $job.FullName (Join-Path $outbox "$base.done") -Force `
                -ErrorAction SilentlyContinue

            "$(Get-Date -Format o) done $base exit=$code" | Out-File $log -Append -Encoding utf8
        }
    } elseif (((Get-Date) - $idleSince).TotalMinutes -gt 45) {
        "$(Get-Date -Format o) 空闲超时，退出" | Out-File $log -Append -Encoding utf8
        break
    }

    Start-Sleep -Milliseconds 300
}

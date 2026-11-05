# T01 基线采集驱动:启动隔离 mica 实例,按 scenarios.md 的 S1-S4 采样。
# 只终止自己创建的进程树(taskkill /T),绝不按进程名全局清理。
#
# 用法: powershell -File scripts\run-baseline.ps1 -Exe <mica.exe> -OutDir <dir>

param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [int]$SampleSeconds = 60
)

$ErrorActionPreference = 'Stop'

# IPC 身份守卫:管道名全局固定(\\.\pipe\mica)。若已有别的 mica 实例,
# 我们的 new-tab/send 会打到**那个**实例而采样目标实例——数据张冠李戴。
# 要求:启动前系统里没有任何 mica.exe(运行中被第三方实例抢占属残余风险)。
$existing = Get-Process mica -ErrorAction SilentlyContinue
if ($existing) {
    throw "已有 mica 实例在运行(pid: $($existing.Id -join ','));固定管道会让它接收采样指令。请关闭后重试。"
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# 配置隔离:APPDATA 指向空目录 = 全默认设置(字体/主题/背景图不受用户
# 配置影响,基线可复现)。LOCALAPPDATA 不动(pty 注册表是 T02 机制本体)。
$isoAppData = Join-Path $OutDir "iso-appdata"
New-Item -ItemType Directory -Force -Path $isoAppData | Out-Null

Write-Host "== 启动 mica(隔离 APPDATA)=="
$prevAppData = $env:APPDATA
$env:APPDATA = $isoAppData
$app = Start-Process -FilePath $Exe -ArgumentList "--multi" -PassThru
$env:APPDATA = $prevAppData

try {
    # 等管道就绪(实例可受理 IPC)
    $deadline = (Get-Date).AddSeconds(20)
    do {
        Start-Sleep -Milliseconds 500
        $pipeUp = Test-Path ("\\.\pipe\mica-" + $env:USERNAME)
    } while (-not $pipeUp -and (Get-Date) -lt $deadline)
    if (-not $pipeUp) { throw "20s 内 IPC 管道未就绪" }
    Write-Host "== 管道就绪,pid=$($app.Id);等 shell 稳定 5s =="
    Start-Sleep -Seconds 5

    $samp = Join-Path $PSScriptRoot "measure-resources.ps1"

    Write-Host "== S1 空闲单标签 =="
    & powershell -NoProfile -File $samp -TargetPid $app.Id -Seconds $SampleSeconds -IntervalMs 1000 `
        -OutCsv (Join-Path $OutDir "s1-idle-1tab.csv") -Label idle-1tab

    Write-Host "== S2 5 标签 =="
    1..4 | ForEach-Object { & $Exe new-tab; Start-Sleep -Milliseconds 800 }
    Start-Sleep -Seconds 5
    & powershell -NoProfile -File $samp -TargetPid $app.Id -Seconds $SampleSeconds -IntervalMs 1000 `
        -OutCsv (Join-Path $OutDir "s2-tabs-5.csv") -Label tabs-5

    Write-Host "== S3 10 标签 =="
    1..5 | ForEach-Object { & $Exe new-tab; Start-Sleep -Milliseconds 800 }
    Start-Sleep -Seconds 5
    & powershell -NoProfile -File $samp -TargetPid $app.Id -Seconds $SampleSeconds -IntervalMs 1000 `
        -OutCsv (Join-Path $OutDir "s3-tabs-10.csv") -Label tabs-10

    Write-Host "== S4 持续输出(有界循环,自停)=="
    & $Exe send --literal '1..200000 | ForEach-Object { Get-Date -Format o }' | Out-Null
    & $Exe send '\r' | Out-Null
    Start-Sleep -Seconds 5
    & powershell -NoProfile -File $samp -TargetPid $app.Id -Seconds $SampleSeconds -IntervalMs 1000 `
        -OutCsv (Join-Path $OutDir "s4-sustained-output.csv") -Label sustained-output

    Write-Host "== 完成,关闭实例(树杀,只动自己创建的进程)=="
} finally {
    if (-not $app.HasExited) {
        taskkill /PID $app.Id /T /F | Out-Null
    }
    Write-Host "== 环境信息 =="
    Get-ComputerInfo -Property CsProcessors, OsName, OsVersion, CsTotalPhysicalMemory |
        Format-List | Out-String | Write-Host
    & $Exe --version 2>$null | Write-Host
    (Get-Item $Exe).VersionInfo | Format-List FileVersion, ProductVersion | Out-String | Write-Host
}

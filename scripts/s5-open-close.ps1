# S5 开关标签循环(T01 基线/T03 泄漏对照):每轮 new-tab → Ctrl+W 关闭,
# 循环期间并行采样。SendKeys 需窗口前台——best-effort,人工不得动键鼠。
# 只终止自己创建的进程树。

param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [int]$Rounds = 60
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
$isoAppData = Join-Path $OutDir "iso-appdata"
New-Item -ItemType Directory -Force -Path $isoAppData | Out-Null

$prevAppData = $env:APPDATA
$env:APPDATA = $isoAppData
$app = Start-Process -FilePath $Exe -ArgumentList "--multi" -PassThru
$env:APPDATA = $prevAppData

try {
    $deadline = (Get-Date).AddSeconds(20)
    do {
        Start-Sleep -Milliseconds 500
        $pipeUp = Test-Path ("\\.\pipe\mica-" + $env:USERNAME)
    } while (-not $pipeUp -and (Get-Date) -lt $deadline)
    if (-not $pipeUp) { throw "IPC 管道未就绪" }
    Start-Sleep -Seconds 5

    # 采样器并行跑全程(2s 间隔,覆盖循环 + 结尾静置)
    $samp = Join-Path $PSScriptRoot "measure-resources.ps1"
    $csv = Join-Path $OutDir "s5-open-close.csv"
    $estSec = ($Rounds * 2) + 40
    $sampler = Start-Process powershell -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$samp`" -TargetPid $($app.Id) -Seconds $estSec -IntervalMs 2000 -OutCsv `"$csv`" -Label open-close-$Rounds" -PassThru -WindowStyle Hidden

    $ws = New-Object -ComObject WScript.Shell
    $activated = $ws.AppActivate($app.Id)
    if (-not $activated) { $activated = $ws.AppActivate("Mica") }
    Write-Host "AppActivate=$activated(best-effort)"

    for ($i = 1; $i -le $Rounds; $i++) {
        & $Exe new-tab | Out-Null
        Start-Sleep -Milliseconds 400
        if (-not $ws.AppActivate($app.Id)) { $ws.AppActivate("Mica") | Out-Null }
        # 关 pane 键位是 Ctrl+Shift+W(WT 语义,末 pane 升级关标签);
        # 首轮曾误发 ^w(未绑定,透传 shell)——那两轮数据是标签累积不是泄漏
        $ws.SendKeys("^+w")
        Start-Sleep -Milliseconds 800
        if ($i % 10 -eq 0) { Write-Host "round $i / $Rounds" }
    }
    Write-Host "循环完成,静置采样 30s..."
    Start-Sleep -Seconds 30
    Wait-Process -Id $sampler.Id -Timeout ($estSec + 30) -ErrorAction SilentlyContinue
    Write-Host "CSV: $csv"
} finally {
    if (-not $app.HasExited) { taskkill /PID $app.Id /T /F | Out-Null }
}

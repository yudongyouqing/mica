# T05 真机分屏验证(手动运行):左右 pane 各自输出不同字符集(左 ASCII,
# 右 CJK),截图存 PNG 供目检。**需要机器空闲**(SendKeys 依赖前台焦点,
# 用户操作会抢焦点;2026-11-02 自动运行两次均被前台应用干扰,如实记
# 未完成)。用法见 scenarios.md S6 注记。
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$OutDir
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$iso = Join-Path $OutDir "iso-appdata"
New-Item -ItemType Directory -Force -Path (Join-Path $iso "mica") | Out-Null
# 显式绑定分屏键(wt_default 未含 split,D15 声明与实现的偏差,另行核)
Set-Content -Path (Join-Path $iso "mica\config") -Value "keybind = ctrl+shift+d = split_right" -Encoding UTF8

$existing = Get-Process mica -ErrorAction SilentlyContinue
if ($existing) { throw "已有 mica 实例" }

$prev = $env:APPDATA; $env:APPDATA = $iso
$app = Start-Process -FilePath $Exe -ArgumentList "--multi" -PassThru
$env:APPDATA = $prev

try {
    $deadline = (Get-Date).AddSeconds(20)
    do { Start-Sleep -Milliseconds 400; $up = Test-Path "\\.\pipe\mica" } while (-not $up -and (Get-Date) -lt $deadline)
    if (-not $up) { throw "管道未就绪" }
    Start-Sleep -Seconds 3

    $ws = New-Object -ComObject WScript.Shell
    [void]$ws.AppActivate($app.Id)

    # 左 pane(初始):ASCII 独占字符集
    & $Exe send --literal 'Write-Output ("A#" * 20); Write-Output ("@$%&*" * 8)' | Out-Null
    & $Exe send '\r' | Out-Null
    Start-Sleep -Seconds 2
    # 分屏:新 pane 在右,焦点在新 pane
    [void]$ws.AppActivate($app.Id)
    $ws.SendKeys("^+d")
    Start-Sleep -Seconds 3
    # 右 pane(焦点):CJK + emoji 独占字符集(图集插入集与左完全不同)
    & $Exe send --literal 'Write-Output ("你好世界汉字分屏图集" * 4); Write-Output ([char]::ConvertFromUtf32(0x1F600) * 12)' | Out-Null
    & $Exe send '\r' | Out-Null
    Start-Sleep -Seconds 3

    # 全屏截图(窗口前台;逐窗口裁剪的 pinvoke 在 PS 里不稳,整屏最可靠)
    Add-Type -AssemblyName System.Drawing
    Add-Type -AssemblyName System.Windows.Forms
    $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $bmp = New-Object System.Drawing.Bitmap($vs.Width, $vs.Height)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($vs.X, $vs.Y, 0, 0, $bmp.Size)
    $png = Join-Path $OutDir "split-atlas-full.png"
    $bmp.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    Write-Host "截图: $png (pid=$($app.Id))"
} finally {
    if (-not $app.HasExited) { taskkill /PID $app.Id /T /F | Out-Null }
}

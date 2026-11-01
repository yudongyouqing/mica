# Mica 资源采样器(T01,2026-11-01):按 PID 绑定采样目标进程与其子进程
# (shell/conhost),输出 CSV。默认只读,不创建/终止任何进程。
#
# 用法:
#   powershell -File scripts/measure-resources.ps1 -TargetPid <pid> `
#       -Seconds 60 -IntervalMs 1000 -OutCsv out.csv [-Label idle-1tab]
#
# 指标口径:
#   cpu_s / cpu_pct      目标进程累计 CPU 秒 / 区间增量(1 核满载=100%)
#   ws_kb / private_kb   Working Set / Private Bytes
#   handles / threads    句柄数 / 线程数
#   child_*              直接+间接子进程聚合(按 ParentProcessId 逐级下钻,
#                        与目标分开统计,不重复相加)
#   gpu_kb               GPU Process Memory 计数器(不可用则空列)
#
# 退出码:0 正常;2 目标进程不存在。

param(
    [Parameter(Mandatory = $true)][int]$TargetPid,
    [int]$Seconds = 60,
    [int]$IntervalMs = 1000,
    [string]$OutCsv = "",
    [string]$Label = ""
)

$ErrorActionPreference = 'Stop'

function Get-ProcOrNull([int]$procId) {
    try { Get-Process -Id $procId -ErrorAction Stop } catch { $null }
}

# 子进程树(逐层下钻,避免把父辈兄弟误计入);快照竞态下按能查到的算
function Get-ChildTree([int]$rootPid) {
    $all = Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId |
        Select-Object ProcessId, ParentProcessId
    $frontier = @($rootPid)
    $seen = New-Object System.Collections.Generic.HashSet[uint32]
    [void]$seen.Add($rootPid)
    $rows = @()
    while ($frontier.Count -gt 0) {
        $next = @()
        foreach ($p in $all) {
            $ppid = [uint32]$p.ParentProcessId
            if ($frontier -contains $ppid -and -not $seen.Contains([uint32]$p.ProcessId)) {
                [void]$seen.Add([uint32]$p.ProcessId)
                $next += [uint32]$p.ProcessId
            }
        }
        $frontier = $next
    }
    foreach ($pid2 in ($seen | Where-Object { $_ -ne $rootPid })) {
        $g = Get-ProcOrNull ([int]$pid2)
        if ($g) { $rows += $g }
    }
    $rows
}

# GPU 计数器实例名形如 pid_1234_型态;不可用(无 GPU 计数器权限/驱动)→ null
$gpuCounter = $null
try {
    $gpuCounter = (Get-Counter '\GPU Process Memory(*)\Total Committed' -ErrorAction Stop).CounterSamples
} catch {
    Write-Warning "GPU 计数器不可用, gpu_kb 列将为空: $($_.Exception.Message)"
}

$target0 = Get-ProcOrNull $TargetPid
if (-not $target0) {
    Write-Error "目标进程 $TargetPid 不存在"
    exit 2
}

if (-not $OutCsv) { $OutCsv = Join-Path $PWD ("mica-measure-{0}-{1}.csv" -f $TargetPid, (Get-Date -Format 'yyyyMMdd-HHmmss')) }

$sw = [System.Diagnostics.Stopwatch]::StartNew()
$rows = New-Object System.Collections.Generic.List[string]
$rows.Add("label,elapsed_s,cpu_s,cpu_pct,ws_kb,private_kb,handles,threads,child_count,child_cpu_s,child_cpu_pct,child_ws_kb,child_private_kb,child_handles,gpu_kb")

$prevCpu = $target0.CPU
$prevChildCpu = $null
while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
    Start-Sleep -Milliseconds $IntervalMs
    $t = Get-ProcOrNull $TargetPid
    if (-not $t) { break }   # 目标退出:停止采样
    $kids = Get-ChildTree $TargetPid
    $kidCpu = ($kids | Measure-Object CPU -Sum).Sum
    if ($null -eq $kidCpu) { $kidCpu = 0 }

    $gpuKb = ""
    if ($gpuCounter) {
        try {
            $gpuKb = [math]::Round((($gpuCounter | Where-Object { $_.InstanceName -match "pid_$TargetPid" } |
                Measure-Object CookedValue -Sum).Sum) / 1KB, 0)
        } catch { $gpuKb = "" }
    }

    $cpuPct = if ($t.CPU -ne $null -and $prevCpu -ne $null) {
        [math]::Round((($t.CPU - $prevCpu) / ($IntervalMs / 1000)) * 100, 2)
    } else { "" }
    $kidPct = if ($prevChildCpu -ne $null) {
        [math]::Round((($kidCpu - $prevChildCpu) / ($IntervalMs / 1000)) * 100, 2)
    } else { "" }

    $rows.Add(("{0},{1},{2},{3},{4},{5},{6},{7},{8},{9},{10},{11},{12},{13},{14}" -f `
        $Label, [math]::Round($sw.Elapsed.TotalSeconds, 1),
        [math]::Round($t.CPU, 2), $cpuPct,
        [math]::Round($t.WorkingSet64 / 1KB), [math]::Round($t.PrivateMemorySize64 / 1KB),
        $t.HandleCount, $t.Threads.Count,
        $kids.Count, [math]::Round($kidCpu, 2), $kidPct,
        [math]::Round((($kids | Measure-Object WorkingSet64 -Sum).Sum) / 1KB),
        [math]::Round((($kids | Measure-Object PrivateMemorySize64 -Sum).Sum) / 1KB),
        (($kids | Measure-Object HandleCount -Sum).Sum),
        $gpuKb))

    $prevCpu = $t.CPU
    $prevChildCpu = $kidCpu
}

$rows | Set-Content -Path $OutCsv -Encoding UTF8
Write-Host "采样完成: $OutCsv ($($rows.Count - 1) 行)"

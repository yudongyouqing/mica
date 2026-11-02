# Mica 资源采样器(T01,2026-11-01;同日按复核修正口径):按 PID 绑定
# 采样目标进程与其子进程(shell/conhost),输出 CSV。默认只读。
#
# 用法:
#   powershell -File scripts/measure-resources.ps1 -TargetPid <pid> `
#       -Seconds 60 -IntervalMs 1000 -OutCsv out.csv [-Label idle-1tab]
#
# 指标口径(2026-11-02 修正):
#   cpu_pct        CPU 秒增量 / **实际采样间隔 dt**(不是名义 IntervalMs;
#                  采样自身开销会让节拍漂移,固定分母会把 CPU 算小)
#   child_cpu_pct  子进程 CPU 合计增量 / dt;按 (pid, 进程启动时间) 键
#                  跟踪上一拍——进程退出/新建不串拍:新 pid 首拍不计增量
#                  (没有上一拍基线),退出 pid 的末拍自然失效
#   gpu_kb         每拍**重新查询** GPU Process Memory 计数器(旧版只在
#                  循环外取一次,后续列全是旧值);不可用则空列并告警
#   child_*        直接+间接子进程聚合(逐级下钻),与目标分开统计
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

# 子进程树(逐层下钻);快照竞态下按能查到的算。
# 附带 StartTime 用作键:PID 复用后 (pid, StartTime) 才是同一进程。
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

$target0 = Get-ProcOrNull $TargetPid
if (-not $target0) {
    Write-Error "目标进程 $TargetPid 不存在"
    exit 2
}

if (-not $OutCsv) { $OutCsv = Join-Path $PWD ("mica-measure-{0}-{1}.csv" -f $TargetPid, (Get-Date -Format 'yyyyMMdd-HHmmss')) }

$sw = [System.Diagnostics.Stopwatch]::StartNew()
$rows = New-Object System.Collections.Generic.List[string]
$rows.Add("label,elapsed_s,dt_s,cpu_s,cpu_pct,ws_kb,private_kb,handles,threads,child_count,child_cpu_pct,child_ws_kb,child_private_kb,child_handles,gpu_kb")

$prevCpu = $target0.CPU
$prevWall = $sw.Elapsed.TotalSeconds
# 上一拍子进程 CPU 基线:键 "pid|StartTimeTicks"(StartTime 防串拍)
$prevKids = @{}
while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
    Start-Sleep -Milliseconds $IntervalMs
    $t = Get-ProcOrNull $TargetPid
    if (-not $t) { break }   # 目标退出:停止采样
    $wall = $sw.Elapsed.TotalSeconds
    $dt = [math]::Max($wall - $prevWall, 0.001)

    $kids = Get-ChildTree $TargetPid
    $kidCpuNow = @{}
    $kidCpuSum = 0.0
    foreach ($k in $kids) {
        if ($null -ne $k.CPU) {
            $key = "{0}|{1}" -f $k.Id, $k.StartTime.Ticks
            $kidCpuNow[$key] = [double]$k.CPU
            $kidCpuSum += [double]$k.CPU
        }
    }
    # 增量只对两拍都在且键相同(同 pid 且未复用)的子进程累计;
    # 新 pid 首拍无基线不计(避免把历史 CPU 算成本拍增量)
    $kidDelta = 0.0
    foreach ($kv in $kidCpuNow.GetEnumerator()) {
        if ($prevKids.ContainsKey($kv.Key)) {
            $d = $kv.Value - $prevKids[$kv.Key]
            if ($d -gt 0) { $kidDelta += $d }
        }
    }

    # GPU 计数器每拍重查(一次性查询会静默重复旧值)
    $gpuKb = ""
    try {
        $gpuKb = [math]::Round((
            (Get-Counter '\GPU Process Memory(*)\Total Committed' -ErrorAction Stop).CounterSamples |
                Where-Object { $_.InstanceName -match "pid_$TargetPid" } |
                Measure-Object CookedValue -Sum).Sum / 1KB, 0)
    } catch { $gpuKb = "" }

    $cpuPct = if ($null -ne $t.CPU -and $null -ne $prevCpu) {
        [math]::Round((($t.CPU - $prevCpu) / $dt) * 100, 2)
    } else { "" }

    $rows.Add(("{0},{1},{2},{3},{4},{5},{6},{7},{8},{9},{10},{11},{12},{13},{14}" -f `
        $Label, [math]::Round($wall, 1), [math]::Round($dt, 2),
        [math]::Round($t.CPU, 2), $cpuPct,
        [math]::Round($t.WorkingSet64 / 1KB), [math]::Round($t.PrivateMemorySize64 / 1KB),
        $t.HandleCount, $t.Threads.Count,
        $kids.Count, [math]::Round(($kidDelta / $dt) * 100, 2),
        [math]::Round((($kids | Measure-Object WorkingSet64 -Sum).Sum) / 1KB),
        [math]::Round((($kids | Measure-Object PrivateMemorySize64 -Sum).Sum) / 1KB),
        (($kids | Measure-Object HandleCount -Sum).Sum),
        $gpuKb))

    $prevCpu = $t.CPU
    $prevWall = $wall
    $prevKids = $kidCpuNow
}

$rows | Set-Content -Path $OutCsv -Encoding UTF8
Write-Host "采样完成: $OutCsv ($($rows.Count - 1) 行)"

# Launch benchmark for Platter.
#   powershell -File bench\launch.ps1 -Runs 5 [-Exe path] [-Out results.json] [-NoCache] [-Window 15]
# Each run: kill Platter (and its WebView2 processes), optionally clear the cache, start it with
# PLATTER_PERF set, then watch a fixed window (default 15 s) and report the timeline marks plus the
# CPU time and memory used by the whole process tree (Platter + WebView2) in that window.
param([int]$Runs = 5, [string]$Exe = "", [string]$Out = "", [switch]$NoCache, [int]$Window = 15)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $Exe) {
  # The build goes to .cargo/config.toml's target-dir when set (D:\platter-target), else .\target.
  $Exe = if ($env:CARGO_TARGET_DIR) { Join-Path $env:CARGO_TARGET_DIR "release\platter.exe" } else { "D:\platter-target\release\platter.exe" }
  if (-not (Test-Path $Exe)) { $Exe = Join-Path $root "target\release\platter.exe" }
}
$cacheDir = Join-Path $env:LOCALAPPDATA "app.platter.disks\cache"

function Get-Tree([int]$rootPid) {
  $all = Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId, Name, WorkingSetSize, CommandLine, KernelModeTime, UserModeTime
  $ids = @($rootPid); $frontier = @($rootPid)
  while ($frontier.Count) {
    $kids = @($all | Where-Object { $frontier -contains $_.ParentProcessId } | ForEach-Object { [int]$_.ProcessId })
    $ids += $kids; $frontier = $kids
  }
  # WebView2 processes can outlive a killed parent and be reused, so also match them by data folder.
  $all | Where-Object { ($ids -contains $_.ProcessId) -or ($_.Name -eq "msedgewebview2.exe" -and $_.CommandLine -match "app.platter.disks") }
}
function Stop-Platter {
  $name = [IO.Path]::GetFileNameWithoutExtension($Exe)
  Get-Process platter, $name -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" | Where-Object { $_.CommandLine -match "app.platter.disks" } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
  Start-Sleep -Milliseconds 300
  # An elevated Platter (the usual one: it asks for admin rights at startup) can't be ended from an
  # unelevated shell, and a new launch would just bring it forward instead of being measured.
  if (Get-Process platter, $name -ErrorAction SilentlyContinue) {
    throw "Platter is still running (probably as administrator). Close it, or run this script from an elevated PowerShell."
  }
}

$results = @()
for ($i = 1; $i -le $Runs; $i++) {
  Stop-Platter
  Start-Sleep -Milliseconds 2000
  if ($NoCache -and (Test-Path $cacheDir)) {
    Get-ChildItem $cacheDir -Filter *.json | Where-Object { $_.Name -notin @("window.json", "theme.json", "askAdmin.json") } | Remove-Item -Force
  }
  $log = Join-Path $env:TEMP "platter-perf-$i.log"
  Remove-Item $log -ErrorAction SilentlyContinue
  $env:PLATTER_PERF = $log
  # --no-elevate: no UAC prompt at startup (the measured launch is the standard one).
  $p = Start-Process $Exe -ArgumentList "--no-elevate" -PassThru
  Start-Sleep -Seconds $Window
  $tree = @(Get-Tree $p.Id)
  $wsMB = [math]::Round((($tree | Measure-Object WorkingSetSize -Sum).Sum) / 1MB, 1)
  # CIM times are in 100 ns units.
  $cpuMs = [math]::Round((($tree | ForEach-Object { [double]$_.KernelModeTime + [double]$_.UserModeTime } | Measure-Object -Sum).Sum) / 10000)
  $p.Refresh()
  $platterCpu = [math]::Round($p.TotalProcessorTime.TotalMilliseconds)
  $marks = @{}
  if (Test-Path $log) { Get-Content $log | ForEach-Object { $t, $n = $_ -split "`t"; if ($n -and -not $marks.ContainsKey($n)) { $marks[$n] = [double]$t } } }
  $row = [ordered]@{ run = $i; workingSetMB = $wsMB; processes = $tree.Count; treeCpuMs = $cpuMs; platterCpuMs = $platterCpu }
  foreach ($k in $marks.Keys | Sort-Object { $marks[$_] }) { $row[$k] = $marks[$k] }
  $results += [pscustomobject]$row
  $shown = if ($marks.ContainsKey("rust:window-shown")) { $marks["rust:window-shown"] } else { "(at build) " + $marks["rust:window-built"] }
  Write-Host ("run {0}: shown {1} | first-paint {2} | disks {3} | history {4} | cleanup {5} | tree cpu {6} ms (platter {7}) | {8} MB / {9} procs" -f `
    $i, $shown, $marks["js:first-paint"], $marks["js:disks"], $marks["js:history"],
    ($(if ($marks["js:cleanup-done"]) { "measured " + $marks["js:cleanup-done"] } elseif ($marks["js:cleanup-cached"]) { "cached " + $marks["js:cleanup-cached"] } else { "-" })),
    $cpuMs, $platterCpu, $wsMB, $tree.Count)
}
Stop-Platter
Remove-Item Env:\PLATTER_PERF
$keys = @("rust:window-built", "js:script-start", "js:fonts-ready", "js:volumes", "js:first-render", "js:first-paint", "js:app-ready", "rust:window-shown",
  "js:cleanup-cached", "js:disks", "js:history", "js:cleanup-done", "treeCpuMs", "platterCpuMs", "workingSetMB")
Write-Host "`nmedian over $Runs runs (window $Window s, $(if ($NoCache) { 'empty cache' } else { 'cache as-is' })):"
$summary = [ordered]@{}
foreach ($k in $keys) {
  $vals = @($results | ForEach-Object { $_.$k } | Where-Object { $_ -ne $null } | Sort-Object)
  if ($vals.Count) { $m = $vals[[math]::Floor($vals.Count / 2)]; $summary[$k] = $m; Write-Host ("  {0,-20} {1}" -f $k, $m) }
}
if ($Out) { @{ runs = $results; median = $summary; noCache = [bool]$NoCache; window = $Window } | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 $Out }


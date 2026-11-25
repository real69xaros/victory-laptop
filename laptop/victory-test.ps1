# One timed, unattended test run from a save (diagnostics build: scripted keys and frame captures),
# with the process's GPU memory sampled from Windows' counters every 5 s.
#   victory-test.ps1 -Name G10t -Game '<folder>\game_scarlett_master.exe' -Out D:\runs -Env @{ VICTORY_RELAY = '1' } -Extra '--console','series-s'
# Writes <Out>\vt-<Name>\{log.txt, memory.csv, env.txt, frames\}. The run is stopped after -Seconds.
param(
  [Parameter(Mandatory)][string]$Name,
  [Parameter(Mandatory)][string]$Game,
  [Parameter(Mandatory)][string]$Out,
  [int]$Seconds = 720,
  [string]$Keys = '15:Enter,25:Enter,35:Enter,45:Enter,55:Enter,70:Enter,90:Enter,110:Enter',
  [string]$Frames = '1200,2400',
  [hashtable]$Env = @{},
  [string]$Exe = "$PSScriptRoot\..\target\diag\victory.exe",
  [string[]]$Extra = @()
)
$dir = Join-Path $Out "vt-$Name"
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$dir\frames" | Out-Null
$env:VICTORY_DISPLAY_MODE = '1280x720'
$env:VICTORY_KEYS = $Keys
$env:VICTORY_CAPTURE = "$dir\frames"
$env:VICTORY_CAPTURE_FRAMES = $Frames
$env:NO_COLOR = '1'
foreach ($k in $Env.Keys) { Set-Item "Env:$k" $Env[$k] }
($Env.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ' ' | Out-File -Encoding utf8 "$dir\env.txt"
$p = Start-Process -FilePath $Exe -ArgumentList (@('run', "`"$Game`"", '--execute') + $Extra) -WorkingDirectory "$PSScriptRoot\.." -RedirectStandardOutput "$dir\stdout.txt" -RedirectStandardError "$dir\log.txt" -PassThru
$t0 = Get-Date
"t,private_mb,dedicated_mb,shared_mb" | Out-File -Encoding utf8 "$dir\memory.csv"
while (-not $p.HasExited -and ((Get-Date) - $t0).TotalSeconds -lt $Seconds) {
  Start-Sleep -Seconds 5
  try {
    $pr = (Get-Process -Id $p.Id).PrivateMemorySize64 / 1MB
    $c = (Get-Counter "\GPU Process Memory(pid_$($p.Id)*)\Dedicated Usage", "\GPU Process Memory(pid_$($p.Id)*)\Shared Usage" -ErrorAction Stop).CounterSamples
    $d = ($c | Where-Object Path -like '*dedicated*' | Measure-Object CookedValue -Sum).Sum / 1MB
    $s = ($c | Where-Object Path -like '*shared*' | Measure-Object CookedValue -Sum).Sum / 1MB
    "{0:F0},{1:F0},{2:F0},{3:F0}" -f ((Get-Date) - $t0).TotalSeconds, $pr, $d, $s | Out-File -Append -Encoding utf8 "$dir\memory.csv"
  } catch {}
}
$ended = if ($p.HasExited) { "exited $($p.ExitCode)" } else { Stop-Process -Id $p.Id -Force; 'stopped by the script' }
"end: $ended after $([int]((Get-Date) - $t0).TotalSeconds) s" | Out-File -Append -Encoding utf8 "$dir\memory.csv"
# the average frame rate over the last 40 frame-rate lines
$l = Get-Content "$dir\log.txt" | Select-String ' fps, ' | Select-Object -Last 40
$avg = ($l | ForEach-Object { [double]([regex]::Match($_.Line, ' ([0-9.]+) fps, ').Groups[1].Value) } | Measure-Object -Average).Average
"$Name average fps (last 40 lines): $avg"

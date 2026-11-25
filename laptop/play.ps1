# Starts the Xbox Series build of GTA VI with this branch and the settings measured on an RTX 3050
# Laptop 4 GB (see laptop\README.md). Offline by design.
#   powershell -ExecutionPolicy Bypass -File laptop\play.ps1 -Game '<folder>\game_scarlett_master.exe' -Cache 'D:\victory-cache'
#   -SeriesX   the Series X profile (4K internal: sharper, much slower, more memory)
#   -Data      where saves go (default: next to the cache)
param(
  [Parameter(Mandatory)][string]$Game,
  [Parameter(Mandatory)][string]$Cache,
  [string]$Data = '',
  [string]$Exe = "$PSScriptRoot\..\target\release\victory.exe",
  [switch]$SeriesX
)
if (-not (Test-Path $Game -PathType Leaf)) { Write-Host "Not found: $Game" -ForegroundColor Red; exit 1 }
if (-not $Data) { $Data = Join-Path (Split-Path $Cache -Parent) 'victory-data' }
New-Item -ItemType Directory -Force $Cache, $Data | Out-Null

$env:VICTORY_CACHE_DIR = $Cache
$env:VICTORY_DATA_DIR = $Data
$env:VICTORY_DISPLAY_MODE = '1280x720'
$env:VICTORY_TEXTURE_BUDGET_MB = '2048'   # textures stay loaded; Series S leaves room (8.7 GB peak)
$env:VICTORY_LATE_WAIT = '3'
$env:VICTORY_TEXTURE_NO_WAIT = '1'
$env:VICTORY_PIPELINE_WAIT_MS = '24'
$env:VICTORY_LEAN_WINDOWS = '1'           # title memory windows on the card, no CPU copy (fxceless #265)
$env:VICTORY_WAVE_WIDE_ONLY = '369cbe906ea72764,2cc4c197ee29e291,5dc3b25986622949,6f225b641c5b0708'
$env:VICTORY_LDS_EXACT = '1'
$env:VICTORY_JOIN_COPIES = '1'
$env:VICTORY_TEXTURE_HOT = '1'
$env:VICTORY_WIDE_REFRESH = '1'           # copies brought up to date on several threads
$env:VICTORY_WIDE_MERGE = '1'
$env:VICTORY_WIDE_BACK = '1'
$env:VICTORY_CARD_REHASH_RUNS = '1'
$env:VICTORY_NARROW_WAITS = '1'           # ztrx #139
$env:VICTORY_RELAY = '1'                  # copy-back overlaps the next frame's GPU work
$env:VICTORY_RT_MISS = '1'                # rays miss: lit faces, no speckle (no structures are built)
$env:VICTORY_HOLD = '71B6330:1'           # the title's hang watchdog paused (frames take seconds)
# two of the title's object pools enlarged in memory (it stopped after 7-8 minutes in cutscenes);
# each applies only if the code there is exactly the expected bytes (title build 1.0.2.1)
$env:VICTORY_PATCH = '5AD8E60:E8DB605FFF:B800000100,5ABB240:E8FB3C61FF:B800000100'
$env:NO_COLOR = '1'

$console = if ($SeriesX) { 'series-x' } else { 'series-s' }
$log = Join-Path $Data "last-run-$(Get-Date -Format 'yyyyMMdd-HHmmss').log"
Write-Host "Starting ($console). Log: $log"
Start-Process -FilePath $Exe -ArgumentList @('run', "`"$Game`"", '--execute', '--console', $console) -NoNewWindow -Wait -RedirectStandardError $log

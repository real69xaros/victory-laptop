# VIctory on a 4 GB laptop

This branch is [fxceless/victory-fork](https://github.com/fxceless/victory-fork) (itself a fork of
[Marvisak/VIctory](https://github.com/Marvisak/VIctory)) plus fixes picked from other forks and pull
requests, tuned and measured on a low-end NVIDIA laptop that none of those projects test on.

It contains **no game files, keys, shaders or executables from the game**, and nothing that decrypts
or works around any protection. You need your own copy of the Xbox Series game files, already
readable on disk. Everything a run creates (shader cache, saves, logs) stays on your PC.

## The machine

RTX 3050 Laptop GPU 4 GB (32-lane waves), i5-12450H (8 cores, 12 threads), 16 GB RAM, Windows 11,
NVIDIA driver of 25 Nov 2026. Game build 1.0.2.1, Series S profile (`--console series-s`).

## Results

Each line is a 12-minute run from the same save (the prologue's prison yard), the average of the last
40 frame-rate lines of the log. No run below lost the device.

| Build / setting | fps in the world |
|---|---|
| First run that reached the world (Series X profile) | 0.6 |
| + title memory windows on the card (fxceless #265) | 1.5 |
| + lighting and water passes NVIDIA refused, in the wave-wide form | 1.65 |
| Series S profile (fxceless #188), control run | 2.27 |
| This branch, `VICTORY_WIDE_REFRESH/MERGE/BACK=1` (copies on several threads) | 2.49 |
| + `VICTORY_RELAY=1` (copy-back overlaps the next frame's GPU work) | 2.78 |
| + `VICTORY_TEXTURE_BUDGET_MB=2048` (textures stay loaded) | **2.80** |

Where a frame went on this machine (profiler and per-command GPU timestamps): about 200 ms of GPU
work, most of it in ~4,000 indirect mesh draws a frame, and about 145 ms of CPU preparing
(command translation, ~120 MB a frame of title memory copied to the card).

Shaders: of the title's ~26,000 containers, all but 10 translate on this branch (the rest of the
refusals are containers without a vertex, pixel, compute or mesh program). The ~290 draws a frame
still dropped are the ray-tracing structure builder's stages, which are off.

## What this branch adds over fxceless/victory-fork

Picked from other forks and pull requests (their authors are kept on each commit):

- fxceless #188: Series S plane composition and a plain stretch for the console upscaler
- fxceless #854: wave64 programs in a paired form on 32-lane GPUs; large mesh programs linked with their
  pixel program (about 230 more programs translate on NVIDIA), merged by hand onto this tree
- fxceless #911 (constant buffer views at most 64 KiB, an NVIDIA device removal), #936, #916, #823
- Zettrox/ztrx #135 (static samplers chosen across control flow), #139 (`VICTORY_NARROW_WAITS`), #144
- Belobug/VIctory-Belobug-AMD: ambient light where the ray-traced indirect light does not run, shaded
  by depth (ported by hand); #96 thin LTO
- the VIctory-GTA6-build-v6 patch (on jmehru/victory-amd-rx9070): `VICTORY_RT_MISS`, `VICTORY_HOLD`,
  `VICTORY_PATCH`
- FQY55kai (bind cache), Geckomacabre/vx-layer (`VICTORY_WAVE_WIDE_ONLY`, `VICTORY_TEXTURE_HOT`),
  upstream mesh index mask, and the earlier picks listed in the history

Ported or fixed here:

- the ambient light and `VICTORY_RT_MISS` onto this tree's draw path and translator
- comparison and non-comparison uses of one sampler kept as two static samplers when #135's
  `register_sampler` merges them
- the conflict resolutions for #854, #135, #911 and the profiler on this older base
- fxceless's sampling profiler (`VICTORY_PROFILE=1`, diagnostics builds) on this tree
- `laptop/play.ps1` and `laptop/victory-test.ps1`

## Build and run

```powershell
cargo build --release -p victory
# once per driver update or translator change, ~45 min: every shader and pipeline into the cache
$env:VICTORY_CACHE_DIR = 'D:\victory-cache'
$env:VICTORY_RT_MISS = '1'; $env:VICTORY_LDS_EXACT = '1'
$env:VICTORY_WAVE_WIDE_ONLY = '369cbe906ea72764,2cc4c197ee29e291,5dc3b25986622949,6f225b641c5b0708'
target\release\victory.exe translate --cache --pipelines
# then play with the measured settings
powershell -ExecutionPolicy Bypass -File laptop\play.ps1 -Game '<your game folder>\game_scarlett_master.exe' -Cache 'D:\victory-cache'
```

The pre-build matters: without it the first minutes of a run are a black or white screen while about
2,500 pipelines are still being built. The cache's key includes `VICTORY_RT_MISS`,
`VICTORY_WAVE_WIDE_ONLY` and `VICTORY_LDS_EXACT`, so pre-build and play with the same values.

## Known problems

- About 2.8 fps: playable for walking through the prologue, not more.
- No ray-traced light; shaded areas use a constant fill light; hair and edges can glow.
- The 4 GB card is full; textures over the budget spill to system memory.

## Credits and AI use

All the real work belongs to the VIctory, fxceless and fork authors named above. This branch was
assembled and measured by real69xaros (Discord: real666xaros), with Claude Code (Claude Opus 5.5),
which picked and merged the commits, wrote the ports and scripts, and ran the tests on the machine
above.

GPL-3.0-or-later, the license of VIctory.

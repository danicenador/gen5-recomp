# Testing and debugging

## Headless runs

`boot` runs headless for a number of frames and ends with a report; with a
directory it also saves both screens as PNG every 30 frames. It exits with 0
when it reaches the frame count (or finds `GEN5_STOP_FILE`), and with 3 when
the game stops before that: an unsupported operation, or the watchdog below.
`GEN5_INPUT=first-last:button+button,...` holds buttons over frame ranges,
for example `GEN5_INPUT=1300-1305:start,1500-1506:start`.

## Isolated saves

For isolated playthroughs, set `GEN5_STATE_DIR` to a separate directory; both
`save.bin` and `firmware.bin` will be read and written there, while imported
code and assets stay in the original cache. `GEN5_BOOT_SECONDS` changes the
headless watchdog (default 20 seconds; 0 disables it).

## Debug sessions and checkpoints

For debug playthroughs with automatic save history, use `debug run <cache> <rom>`
(or `debug boot <cache> <rom> [frames [png-directory]]`). Each launch creates an
isolated working save under `debug/runs/`, ignored by Git. Add
`--checkpoint=debug/checkpoints/<game>/<scenario>` to start from a copy of a named
checkpoint. `checkpoint <source> <destination> "description" "reproduction steps"`
promotes a selected snapshot for committing. See
[debug/checkpoints/README.md](../debug/checkpoints/README.md) for the workflow and
the limits of automatic snapshot detection.

## Replays

From PowerShell, `./scripts/white2/replay.ps1` replays the fresh-game Juniper
baseline to frame 2300. Each invocation creates a separate state directory,
screenshots and report under `.gen5-cache/checkpoints/`. Reaching the requested
frame is checked automatically; gameplay checkpoints require visual inspection.

`./scripts/white2/replay.ps1 -Inputs scripts/white2/introduction.inputs -Frames 9600`
continues through character creation into the Aspertia opening scene. It selects
the boy and uses `AAAAAAA` for the player and `HughAAA` for the rival. `-ScreenshotEvery`
sets the screenshot interval (120 frames by default; 0 saves only the last
frame). `-Touches path` accepts a second input file with `first-last:x:y`
entries for bottom-screen taps. The equivalent CLI environment variable is
`GEN5_TOUCH`; `GEN5_SCREEN_EVERY` sets its screenshot interval (default 30).

Headless input runs every frame independently of screenshot rendering; display
capture also continues every frame. `GEN5_INPUT_FILE` selects a button script
with one entry per line and `#` comments, replacing `GEN5_INPUT`. It reloads
every 60 frames so a diagnostic run can be extended in progress. The replay
helper copies its input file into the run directory before starting.
`GEN5_BOOT_FPS` optionally caps headless frame progression for live diagnosis
(0, the default, runs as fast as possible); windowed play keeps its normal pacing
(see fast-forward above for `GEN5_SPEED`).
`GEN5_INTERP_SHOTS=1` adds, for each screenshot that shows a new 3D list, a
`frame-NNNNN-half.png` of the frame before it drawn half way to that list,
exactly as 3D interpolation draws the in-between frames in the window. With
`GEN5_SCREEN_RANGE=first-last:1` it shows polygons that tear or stay behind.

`./scripts/white2/replay.ps1 -Inputs scripts/white2/field.inputs -Frames 52000`
replays the first controllable-field checkpoint from a fresh save: finish the
introduction, move out of the bedroom, complete Mom's conversation, leave home,
walk down and sideways in Aspertia, and re-enter home. Inspect these screenshots
in the printed run directory (the helper's default 120-frame interval):

| Frame | Expected visual checkpoint |
|---|---|
| 13680 | Player in the bedroom |
| 21000 | Player has moved toward the living room |
| 22320 | Mom's opening conversation |
| 24960 | Conversation finished; Mom moves away |
| 40560 | Player outside the house in Aspertia |
| 45120 | Player has walked down toward the fence |
| 45360 | Player has walked left along the street |
| 49320 | Player has re-entered the house |
| 52000 | Interior after returning home |

`./scripts/white2/replay.ps1 -Inputs scripts/white2/battle.inputs -Frames 83000`
carries on from there to the first battle: through Aspertia to the lookout,
Tepig from Bianca, the X menu, party, summary and bag, and the battle
against Hugh, choosing Tackle every turn. The battle's turns and outcome
vary between runs (the game's random numbers seem to follow the clock), so
its frames shift by a few hundred; winning or losing both carry the story
on to the Pokémon Center.

| Frame | Expected visual checkpoint |
|---|---|
| 67800 | Bianca holds out the starters' case (bottom screen) |
| 70600 | X menu |
| 70900 | Party |
| 71200 | Tepig's summary |
| 72200 | Bag ("It's empty.") |
| 77900 | Battle command menu |
| 78000 | Move list |
| 83000 | Walking into the Pokémon Center |

These scripts use the locally imported English White 2 ROM with CRC `95AB5437`
and its generated game library. They preserve the normal in-game story and map
transitions; no memory edits or event skips are used. Some geometry/shadow
rendering artifacts remain.

## Diagnostic variables

`GEN5_AUDIO_WAV=path` records a headless run's sound as a 16-bit stereo WAV
at the DS mixer rate. `GEN5_SOUND=off` acknowledges sound commands without
playing them, to rule sound in or out when diagnosing. `GEN5_STOP_FILE=path`
ends a run with the full report as soon as that file exists; the report's 3D
line includes the largest polygon list kept and any lists that overflowed
polygon or vertex RAM. `GEN5_SCREEN_RANGE=first-last:every` adds denser
screenshots in one stretch of a run. `GEN5_3D_TRACE=frame:path` writes every
polygon of the 3D list that follows that frame to `path`: whether it was kept
or culled (and why), its attributes and screen coordinates, its occurrence
number and model-space vertices with texture coordinates, plus box tests;
`first-last:path` writes one `path.frame` file per list in that range.
The report also says where host time went per frame (game code, 3D, 2D,
capture, sound) and how many frames took longer than a DS frame.
`GEN5_PROFILE=path` renders every frame as the window does, even headless,
and writes those times for each frame to `path` as CSV, with the size of the
polygon list shown.

## Smoke checks

The 3D path has 16 focused regression tests covering command decoding, matrix
stacks, vblank swaps, lighting, texture sampling, perspective interpolation,
clipping, strip winding, depth, blending and display capture. A White 2 headless
run with `GEN5_INPUT=1300-1305:start` renders Kyurem at frame 1600. Adding
`1500-1506:start,1650-1656:a,1900-1906:a,2100-2106:a` reaches Juniper's dialogue
at frame 2300. These are visual smoke checks, not full-game compatibility tests.

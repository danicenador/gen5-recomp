# Gen5 recomp

A PC port of Pokémon Generation V by static recompilation: the games' ARM9 code
is translated to C and runs on a Rust Nintendo DS runtime. Black 2 / White 2 are
the first game family.

An earlier prototype re-implemented the game by hand (map preview, field
movement, an NPC script VM). It was removed in favour of recompilation and is
kept at the `native-port-draft` tag.

## ROM explorer and importer

The CLI validates the NDS header checksums, checks the ROM section ranges, parses
the NitroFS filename and allocation tables, validates all file ranges, and
identifies Black, White, Black 2, and White 2 by game code. It does not emulate
the Nintendo DS.

```text
cargo run -- inspect "roms/Pokemon White Version 2.nds"
cargo run -- list "roms/Pokemon White Version 2.nds" a
cargo run -- import "roms/Pokemon White Version 2.nds"
```

`import` copies files referenced by the ROM's FNT/FAT into a versioned cache
(default `.gen5-cache/<game>/<rom-crc32>/romfs`) and writes a JSON manifest with
per-file CRC-32 values. Pass a second path argument to select a different cache
root. The `.gen5-cache` directory is ignored by Git.

The importer keeps ROM files byte-for-byte in a local cache.

## Code export for static recompilation

Game logic will run as statically recompiled ARM code, the way gen1recomp
runs Game Boy code, instead of being rewritten by hand. The first step is
getting the code out of the ROM in the form it takes in RAM:

```text
cargo run -- code-info "roms/Pokemon White Version 2.nds"
```

`code-info` prints the ARM9/ARM7 load and entry addresses, the ARM9 autoload
blocks (ITCM, DTCM and others) and every overlay. `import` also writes a
`code/` directory into the cache (and adds it to older caches that lack one):

- `arm9.bin`: the ARM9 static binary after BLZ decompression, with the module
  params' compressed-end field cleared as the boot code would.
- `arm7.bin`: the ARM7 static binary.
- `overlay9/overlay_NNNN.bin`: each ARM9 overlay, decompressed and checked
  against its table size.
- `code.json`: addresses, module params, autoload blocks (with their offsets in
  `arm9.bin`), the overlay tables and a CRC-32 per file.

All four games extract cleanly. Black and White have 237 ARM9 overlays, and
Black 2 and White 2 have 344. None of them has ARM7 overlays. These titles are
DSi-enhanced, so the autoload records use the TWL SDK's 16-byte layout. The
DSi-only ARM9i/ARM7i binaries are not exported because the target is DS mode.

## Disassembly and function discovery

The second recompilation step decodes the exported code and finds every
function in it:

```text
cargo run --release -- analyze ".gen5-cache/pokemon-white-2/95AB5437"
cargo run --release -- disasm ".gen5-cache/pokemon-white-2/95AB5437" main 0x02004800
cargo run --release -- disasm ".gen5-cache/pokemon-white-2/95AB5437" ov011 0x0218B360
```

- `recomp::arm` decodes ARMv5TE ARM and Thumb code into one structured form,
  which the code generator will also use.
- `recomp::program` loads `main`, the autoload blocks (`autoloadN`) and each
  overlay (`ovNNN`) as separate modules. A function is identified by module
  and address, because many overlays load at the same address.
- `recomp::analysis` finds functions by recursive traversal, starting from the
  entry point and the overlays' static initializers. It follows calls and tail
  calls, and accepts code pointers from literal pools and data once they decode
  cleanly. It recognizes the compiler's switch tables, `ldr`/`bx` veneers,
  Thumb `bx pc` state switches and interworking returns.

`analyze` writes `analysis/functions.json` into the cache. Results:

| | Functions | Main code coverage | Overlay code coverage | Unresolved indirect jumps |
|---|---|---|---|---|
| Black 2 / White 2 | about 39,950 | 82.6% | 79.2% | about 33 |
| Black / White | about 34,680 | 86.4% | 82.1% | 28 |

Coverage counts bytes decoded as code. The rest is mostly data and literal
pools.

About 9,600 BW2 calls cross from one overlay into an address that several
non-overlapping overlays can occupy. Which one is loaded is only known at run
time, so those calls stay `ambiguous` and the runtime will dispatch them. Calls
into `0x027xxxxx` (about 120) fall outside all loaded code. These are probably
DSi-mode paths, which are not targeted. A handful of functions still report
problems, mostly targets reached only from inline data.

## C code generation

The third recompilation step translates every discovered function to C:

```text
cargo run --release -- codegen ".gen5-cache/pokemon-white-2/95AB5437"
```

This writes `recomp/` into the cache:

- `gen5rt.h`: the ABI between generated code and the runtime. It defines the
  CPU state, inline memory access through 4 KiB page tables (with a runtime
  fallback for I/O), shifter and flag helpers, and the `rt_*` functions the
  runtime implements.
- `recomp.h`: prototypes for every function.
- One file per module (`main.c`, `autoload0.c`, `ov000.c`, ...) with
  `void f_<module>_<address>(Cpu *cpu)` per function.
- `functions.c`: a table of every function by module and address, for
  indirect calls.

Each instruction becomes a block of C, with labels at branch targets:

- Direct calls set `lr` and call the callee's C function, and a return is a C
  `return`.
- Switch tables become C `switch` statements.
- Literal-pool loads are folded into constants.
- Indirect calls, ambiguous cross-overlay calls, BIOS calls (`swi`) and
  CP15/PSR access go through the runtime.

Unit tests compile generated code with gcc and run it to check ARM semantics:
flags, shifter carry, unaligned and signed loads, writeback, push and pop, long
multiplies, switch tables and runtime calls. The tests are skipped if gcc is
not installed.

For White 2, the generator emits 39,948 functions (209 MB of C in 260 files) in
about 7 seconds. All files compile with `gcc -O1 -Wall` without warnings. The
build takes about 200 seconds on 14 threads. Linked against a stub runtime, it
is a 52 MB executable.

## Runtime

The fourth step builds the generated C into a game library and runs it on a
Rust model of the DS hardware:

```text
cargo run --release -- build ".gen5-cache/pokemon-white-2/95AB5437"
cargo run --release -- run ".gen5-cache/pokemon-white-2/95AB5437" "roms/Pokemon White Version 2.nds"
cargo run --release -- boot ".gen5-cache/pokemon-white-2/95AB5437" "roms/Pokemon White Version 2.nds" 600 frames
```

`run` opens a window with both screens stacked, paced to the DS's 59.83
frames per second. Keys: arrows for the D-pad, X = A, Z = B, S = X, A = Y,
Q = L, W = R, Enter = Start, Right Shift or Backspace = Select, and the
mouse on the lower screen as the stylus. Escape asks whether to quit.

Game controllers work too. The face buttons map by position, as on a DS
(bottom = B, right = A, top = X, left = Y, whatever the controller prints
on them); the shoulder buttons are L and R, Start and Back/View/Select are
Start and Select, and the D-pad or left stick move. The right trigger
turns fast-forward on and off, like Tab.

Add `--split-screens` to `run` to put the top and bottom DS screens in
separate, independently resizable windows. Keyboard controls work in either
window; the mouse acts as the stylus only in the bottom-screen window.
Closing either window quits. Without the flag, both screens remain stacked.

`run` takes an optional resolution multiple after the ROM, `run <cache>
<rom> 4` (or `GEN5_SCALE=4`), and keys 1 to 8 change it while playing. At
1 the software renderer draws the DS's own 256x192 per screen. Above 1 the
GPU draws the 3D layer at that multiple, so models and the field gain
detail, while 2D backgrounds and sprites are drawn at the DS resolution and
repeated, so they stay sharp. A window smaller than the result shows it
filtered down. 3D the game shows through display capture (the title
screen's Kyurem) stays at the DS resolution, since the game reads captured
pixels back.
`boot` saves screenshots at the same multiple with `GEN5_SCALE`, and
`GEN5_GPU=1` uses the GPU renderer at 1, to compare it with the software one.

Tab turns fast-forward on and off, and + and - (on any keyboard layout, or
the numpad) set its speed (2x to 8x, 3x to begin with; the window title
shows it). Fast-forward runs the game that
many frames per DS frame of real time, so walking, battles, text and
animations speed up, but it leaves two things at normal speed:

- Music and sound effects. The sound driver and mixer follow real time
  instead of the game's frames, so music keeps its tempo and pitch. Fades
  and jingles the game times itself still happen sooner.
- Menu key repeat. Holding a direction in a menu moves the cursor as often
  as at normal speed, and a short tap does not repeat. The runtime finds the
  repeat countdown of GameFreak's key update by its instructions (the same
  in Black 2 and White 2) and holds it back on the extra frames. Keys the
  game reads directly while held, such as walking, are not slowed down.

The window shows about one frame per DS frame of real time while
fast-forwarding, which leaves more time for the game; how fast it can go
depends on the host. `GEN5_SPEED=N` starts `run` fast-forwarding at N, and
makes `boot` run as if fast-forwarding at N, counting each frame as 1/N of a
DS frame of real time for sound and key repeat, so `GEN5_AUDIO_WAV` records
the music at normal speed.

`boot` runs headless for a number of frames and ends with a report; with a
directory it also saves both screens as PNG every 30 frames.
`GEN5_INPUT=first-last:button+button,...` holds buttons over frame ranges,
for example `GEN5_INPUT=1300-1305:start,1500-1506:start`.

For isolated playthroughs, set `GEN5_STATE_DIR` to a separate directory; both
`save.bin` and `firmware.bin` will be read and written there, while imported
code and assets stay in the original cache. `GEN5_BOOT_SECONDS` changes the
headless watchdog (default 20 seconds; 0 disables it).

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
`GEN5_AUDIO_WAV=path` records a headless run's sound as a 16-bit stereo WAV
at the DS mixer rate. `GEN5_SOUND=off` acknowledges sound commands without
playing them, to rule sound in or out when diagnosing. `GEN5_STOP_FILE=path`
ends a run with the full report as soon as that file exists; the report's 3D
line includes the largest polygon list kept and any lists that overflowed
polygon or vertex RAM. `GEN5_SCREEN_RANGE=first-last:every` adds denser
screenshots in one stretch of a run. `GEN5_3D_TRACE=frame:path` writes every
polygon of the 3D list that follows that frame to `path`: whether it was kept
or culled (and why), its attributes and screen coordinates, plus box tests;
`first-last:path` writes one `path.frame` file per list in that range.
The report also says where host time went per frame (game code, 3D, 2D,
capture, sound) and how many frames took longer than a DS frame.
`GEN5_PROFILE=path` renders every frame as the window does, even headless,
and writes those times for each frame to `path` as CSV, with the size of the
polygon list shown.

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

These scripts use the locally imported English White 2 ROM with CRC `95AB5437`
and its generated game library. They preserve the normal in-game story and map
transitions; no memory edits or event skips are used. Some geometry/shadow
rendering artifacts remain.

`build` compiles `recomp/*.c` with gcc in parallel into
`recomp/build/game.dll`, reusing objects whose sources have not changed.
`gen5host.c` forwards every `rt_*` hook to callbacks the runtime registers,
so the library builds once and the Rust runtime loads it with `libloading`.

The `nds` module provides:

- `memory`: the ARM9 address space as 4 KiB page tables over host buffers.
  It covers main RAM and its mirrors, ITCM and DTCM (placed through CP15),
  shared WRAM, palette, VRAM and OAM.
- `vram`: the nine VRAM banks, mapped by VRAMCNT into the BG, OBJ and LCDC
  regions, extended palettes and texture slots.
- `gpu2d`: both 2D engines, rendered once per frame at vblank: text,
  affine, extended and bitmap backgrounds, sprites, windows, blending and
  master brightness. Engine A's BG0 can composite the 3D layer, including
  its alpha, scrolling, window masks and priority. Lines are drawn in
  parallel, and display capture shares engine A's lines and the 3D layer
  with the screen of the same frame.
- `gpu3d`: a software geometry engine and rasterizer. Packed FIFO commands,
  direct command ports and geometry FIFO DMA feed the matrix stacks,
  lighting, vertex lists and frame swaps. The rasterizer clips polygons,
  interpolates colors and texture coordinates with perspective correction,
  samples all seven texture formats from mapped VRAM (including compressed
  4x4 textures), and applies depth tests and alpha blending. Like the
  hardware, polygon RAM (2048 polygons, 6144 vertices) only keeps polygons
  that survive frustum, far-plane and facing tests, so wide scenes such as
  the opening flyover keep their distant map blocks; polygons are drawn in
  the hardware's Y order. At the DS resolution everything runs on the CPU,
  with bands of rows rasterized in parallel on the host's cores; at higher
  resolutions the same culled, clipped and sorted triangles go to
  `runtime::scaled`, which rasterizes them with wgpu (textures decoded in
  the shader, translucent polygon IDs in the stencil buffer). Matrix and
  geometry-test results are readable by the game. Commands run synchronously
  except buffer swaps, which wait for vblank; FIFO timing is approximate.
  Fog, edge marking, antialiasing, wireframe, shadow polygons and rear-plane
  bitmaps remain unsupported. Lighting and depth are functional approximations rather than
  hardware-exact arithmetic.
- `gpu2d::capture`: frame-based display capture into LCDC VRAM, from engine A,
  the 3D output, another VRAM bank, or a blend of both sources. Captures wrap
  within the destination bank and finish without a window or screenshot sink.
  Main-memory display FIFO capture is not implemented.
- `input`: buttons and touch screen, shared between the window and the
  machine.
- `speed`: frame pacing and fast-forward, with real time kept for the sound
  driver and for the key update's repeat countdown.
- `io`: I/O registers. Display timing advances when the game polls it, and
  there are vblank and interrupt flags, immediate DMA, the divider and
  square-root units, and IPCSYNC and the IPC FIFO.
- `card`: game card reads (`0xB7` data blocks and `0xB8` chip ID) from the
  ROM file, including card-triggered DMA and the transfer-complete
  interrupt. Like a retail card, addresses wrap at the chip size from the
  header and reads below 0x8000 return data from 0x8000; Black 2 and
  White 2's anti-piracy code checks both.
- `cpu`: banked registers, CPSR and SPSR, and exception entry and return.
- `machine`: boot as the firmware leaves memory, CP15, BIOS calls (waits,
  division, copies, CRC, LZ77 and RLE), and interrupt delivery through the
  handler at the end of DTCM. Overlay-aware dispatch picks the overlay whose
  code is currently in RAM.
- `interpreter`: runs code with no recompiled function, such as routines the
  game copies to ITCM before running them, or jumps into a function's middle.
  Calls from it go back to native code. Black 2 and White 2 keep some overlay
  functions encrypted and decrypt them in RAM before calling them; calls to
  callees that do not decode cleanly go through the runtime, which sees the
  decrypted code in RAM and interprets it.

The ARM7 is not run. A stand-in answers the shared-RAM boot handshakes and
the PXI start-up handshake, and serves the ARM7 services the game asks for
over the IPC FIFO, as decoded from the ARM9's callbacks:

- PM (power management) and the touch panel: framed command packets
  answered with a result. Touch samples come from the window's stylus.
- RTC: the host's date and time, as BCD in shared RAM.
- NVRAM (firmware flash): a 256 KiB image of a freshly set up DS (English,
  three empty Wi-Fi Connection slots), kept in `firmware.bin` in the cache.
  The user settings and the console's MAC address are also copied to shared
  RAM at boot. DWC writes its user ID there. The MAC is in Nintendo's range;
  Black 2 and White 2 blank the screen on a console whose MAC is all zeros or
  a common emulator default, so images with that default are updated.
- Card backup (save memory): identify, read, write, program, verify and
  erase on 512 KiB of FLASH, kept in `save.bin` in the cache.
- Sound (`nds::sound`): a port of the NitroSDK's ARM7 sound driver runs the
  ARM9's command lists: sequences with their instrument banks and wave
  archives, envelopes, sweeps, LFOs, channel allocation and stealing,
  locked channels for direct PCM (cries), alarms, the shared work area's
  player and channel status, and `SND_ReadDriverInfo`. Its 5.2 ms interval
  and the mixer follow display time. The mixer models the 16 hardware
  channels (PCM8, PCM16, IMA-ADPCM, square waves and noise) at 32.73 kHz;
  windowed play sends it to the default output device through `cpal`.
  Sound capture (reverb) is not emulated.
- The vblank counter, and the X and Y buttons in shared RAM.

NitroSDK threads run as host threads that take turns. The SDK's
`OS_SaveContext` and `OS_LoadContext`, and the thread switch at the end of
its IRQ handler, are recognised by instruction pattern (`recomp::hle`) and
implemented by the runtime (`nds::threads`), because a context switch jumps
to a saved `pc` that generated C cannot reach. Hardware timers count with
display time and raise interrupts. Generated loops call `rt_tick` after a
budget of backward branches, so a busy-wait still lets time pass and takes
interrupts. The report covers the IPC traffic, the stand-in's replies, the
running thread's call chain and every parked thread. `GEN5_DUMP` adds memory
dumps. Every generated function checks on return that it left `sp` as it
found it; `GEN5_CHECK_SP=1` stops on the first one that did not.

White 2 boots, plays its opening, shows the title screen, opens the main
menu (New Game, Mystery Gift, ...) and completes Professor Juniper's New Game
introduction and character creation. The original game reaches the player's
bedroom, accepts field movement, runs Mom's opening conversation, and supports
leaving home, moving through Aspertia and re-entering the house. The title screen
renders Kyurem on its lower screen through
the 3D engine and display capture. Music and sound effects play.

The 3D path has 16 focused regression tests covering command decoding, matrix
stacks, vblank swaps, lighting, texture sampling, perspective interpolation,
clipping, strip winding, depth, blending and display capture. A White 2 headless
run with `GEN5_INPUT=1300-1305:start` renders Kyurem at frame 1600. Adding
`1500-1506:start,1650-1656:a,1900-1906:a,2100-2106:a` reaches Juniper's dialogue
at frame 2300. These are visual smoke checks, not full-game compatibility tests.

## Development sequence

Done: code export, disassembly and function discovery, the C code generator, a
first runtime with the ARM7 services used at startup, 2D display output with a
playable window, the 3D renderer and display capture, and sound through a port
of the ARM7 sound driver. White 2 runs New Game through the opening field and
house transitions.

Next:

1. Obtain the starter and complete the first rival battle.
2. Validate save and reload.
3. Fill the remaining graphics and sound gaps.
4. Complete White 2, then Black 2 and Black / White.

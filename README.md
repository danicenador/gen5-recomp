# Gen5 native engine

An early native Rust foundation for a PC engine for Pokémon Generation V. The
first vertical slice is a Nintendo DS ROM explorer and local importer, with
Black 2 / White 2 identified as the initial game family.

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

The importer keeps ROM files byte-for-byte in a local cache. The map preview
reads the needed NARC entries from that cache; it currently exports model
geometry to SVG rather than converting every archive and asset type.

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

## BW2 map preview

The second vertical slice adds a bounds-checked NARC reader, BW2 zone and map
matrix decoding, and a native Nitro model geometry reader. It can turn a map's
original model geometry and available vertex/material colors into an SVG
isometric preview, including multi-cell matrices.

```text
cargo run -- map-preview ".gen5-cache/pokemon-white-2/95AB5437"
cargo run -- map-preview ".gen5-cache/pokemon-white-2/95AB5437" 427 "aspertia-city.svg"
```

The default zone is White 2's Aspertia City (zone 427). Zone 428 is an indoor
house. The command writes to `<import-cache-directory>/previews/zone-427.svg`
unless an output path is given.

## BW2 field movement

The third vertical slice reads the map's original height-terrain grid and adds
a turn-by-turn field session. The `play` command opens the native WGPU window;
use W/A/S/D or the arrow keys to move, Enter or Space to talk/continue, and
Escape to exit. On Aspertia City, the default spawn is beside the outlook NPC.
Press Enter to run that NPC's original White 2 field script and show its
original dialogue. NPCs and the player currently use simple native markers. A
terminal control mode remains available as `walk` for quick map and collision
inspection.

```text
cargo run -- play ".gen5-cache/pokemon-white-2/95AB5437"
cargo run -- play ".gen5-cache/pokemon-white-2/95AB5437" 427
cargo run -- walk ".gen5-cache/pokemon-white-2/95AB5437"
```

Controllers use the D-pad to move and the South/A button to talk or continue.
The runtime initializes Kira's audio backend, but original ROM sound playback
awaits SDAT conversion and sample loading. The desktop renderer draws
multi-cell geometry, samples decoded BTX0 textures, overlays nearby movement
tiles on the 3D map, and displays dialogue. `wgpu` stays in the desktop
renderer; `FieldSession` owns movement, collision, and script state without a
rendering dependency. The field VM handles the initial stack, event-variable,
flag, branch, message, and wait commands used by Aspertia NPC scripts;
unsupported opcodes report their original command and byte offset. The runtime
stack uses `winit`, `wgpu`, `glam`, `egui`, `gilrs`, `kira`, `serde`, and
`tracing`.

The terminal command uses W/A/S/D followed by Enter to move, and a blank line
or E to talk/continue; Q quits.
On zones without the Aspertia outlook event, the default spawn is the nearest
recognized walkable tile to the terrain-grid center. Supply a zone and
optional grid coordinates to choose another start:

Movement currently uses the known ordinary-foot passable terrain codes and
treats other codes as solid. It reads one height-terrain layer. Map transitions,
the full Gen V script command set, and original character sprites are still
future work.

## Planned layers

- `formats::nds`: NDS header and NitroFS parsing.
- `formats::narc`: bounded Nitro Archive member access.
- `importer`: ROM validation and cache creation.
- `game::gen5`: title identification and game-specific behavior boundaries.
- `game::bw2`: Black 2 / White 2 behavior boundary, zone/matrix/event decoding,
  and White 2 field behavior.
- `render`: Nitro model geometry, BTX0 texture decoding, atlas sampling, and SVG
  map preview.
- `game::bw2::terrain` and `game::bw2::field`: original map terrain decoding,
  conservative tile collision, player state, and terminal input loop.
- `formats::gen5_script` and `formats::gen5_text`: original script pointers and
  message-bank decoding.
- `game::gen5::script_vm`: resumable field-script execution for the commands
  needed by the current NPC slice.
- `runtime`: PC rendering, audio initialization, and keyboard/controller input.

## Development sequence

The steps below come from the hand-written field prototype. The plan from here
is recompilation: export the code (done), disassembly and function discovery
(done), then the code generator, then the hardware runtime, aiming first to
boot White 2 to its title screen.

1. ROM explorer and validated import cache.
2. Decode and render a multi-cell BW2 map with original model textures.
3. Add player movement and map collision in the 3D field view.
4. Execute one original NPC script and show its dialogue (implemented).
5. Add map transitions, then menus and battles.
6. Complete White 2, then Black 2 and BW1.

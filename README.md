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
rendering dependency. This slice's script VM supports the commands used by the
Aspertia outlook NPC; unsupported opcodes report their original command and
byte offset. The runtime stack uses `winit`, `wgpu`, `glam`, `egui`, `gilrs`,
`kira`, `serde`, and `tracing`.

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

1. ROM explorer and validated import cache.
2. Decode and render a multi-cell BW2 map with original model textures.
3. Add player movement and map collision in the 3D field view.
4. Execute one original NPC script and show its dialogue (implemented).
5. Add map transitions, then menus and battles.
6. Complete White 2, then Black 2 and BW1.

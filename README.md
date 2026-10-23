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
isometric preview. The first renderer supports single-cell matrices; external
map textures are not decoded yet.

```text
cargo run -- map-preview ".gen5-cache/pokemon-white-2/95AB5437"
cargo run -- map-preview ".gen5-cache/pokemon-white-2/95AB5437" 428 "aspertia-city.svg"
```

The default zone is White 2's Aspertia City (zone 428). The command writes to
`<import-cache-directory>/previews/zone-428.svg` unless an output path is given.

## BW2 field movement

The third vertical slice reads the map's original height-terrain grid and adds
a turn-by-turn field session. The `play` command opens the native WGPU window;
use W/A/S/D or the arrow keys to move, and Escape to exit. The original model
geometry is drawn using its available colors, with a simple native player
marker and an egui collision-grid/debug panel. A terminal control mode remains
available as `walk` for quick map and collision inspection.

```text
cargo run -- play ".gen5-cache/pokemon-white-2/95AB5437"
cargo run -- play ".gen5-cache/pokemon-white-2/95AB5437" 428 16 16
cargo run -- walk ".gen5-cache/pokemon-white-2/95AB5437"
```

Controllers use the D-pad when available. The runtime initializes Kira's audio
backend, but original ROM sound playback awaits SDAT conversion and sample
loading. `wgpu` stays in the desktop renderer; `FieldSession` owns movement
state and collision and has no rendering dependency. The runtime stack uses
`winit`, `wgpu`, `glam`, `egui`, `gilrs`, `kira`, `serde`, and `tracing`.

The terminal command uses W/A/S/D followed by Enter; Q quits.
The default spawn is the nearest recognized walkable tile to the terrain-grid
center. Supply a zone and optional grid coordinates to choose another start:

For this first map, only Aspertia's known common ground terrain code is marked
walkable. Other terrain codes are treated as solid until their movement
permissions are decoded. Movement currently covers a single height-terrain
layer and one-cell map; it does not yet resolve the original slope/height tables,
events, map transitions, or render sprites in a native window.

## Planned layers

- `formats::nds`: NDS header and NitroFS parsing.
- `formats::narc`: bounded Nitro Archive member access.
- `importer`: ROM validation and cache creation.
- `game::gen5`: title identification and game-specific behavior boundaries.
- `game::bw2`: Black 2 / White 2 behavior boundary and zone/matrix decoding;
  White 2 is the first runtime target.
- `render`: first Nitro model geometry path and SVG map preview.
- `game::bw2::terrain` and `game::bw2::field`: original map terrain decoding,
  conservative tile collision, player state, and terminal input loop.
- Future format modules: Nitro graphics and textures, text, sound, and script data.
- Future runtime layers: original Gen V script VM, field/map simulation, Pokémon
  and battle rules, and PC rendering, audio, and input.

## Development sequence

1. ROM explorer and validated import cache.
2. Decode one BW2 map and render its original model geometry (initial preview;
   texture decoding remains in this slice's follow-up work).
3. Add player movement and map collision.
4. Execute one original NPC script and show its dialogue.
5. Add map transitions, then menus and battles.
6. Complete White 2, then Black 2 and BW1.

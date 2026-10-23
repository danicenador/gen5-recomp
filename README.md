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

This first cache is a byte-preserving import. It gives later format decoders a
stable input layer; it does not yet unpack NARC archives or convert graphics,
maps, text, scripts, models, textures, or audio into engine-native assets.

## Planned layers

- `formats::nds`: NDS header and NitroFS parsing.
- `importer`: ROM validation and cache creation.
- `game::gen5`: title identification and game-specific behavior boundaries.
- `game::bw2`: Black 2 / White 2 behavior boundary; White 2 is the first runtime target.
- Future format modules: NARC, Nitro graphics, map, text, sound, and script data.
- Future runtime layers: original Gen V script VM, field/map simulation, Pokémon
  and battle rules, and PC rendering, audio, and input.

## Development sequence

1. ROM explorer and validated import cache.
2. Decode one BW2 map and render its original tiles/objects.
3. Add player movement and map collision.
4. Execute one original NPC script and show its dialogue.
5. Add map transitions, then menus and battles.
6. Complete White 2, then Black 2 and BW1.

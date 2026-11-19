# Gen5 recomp

A PC port of Pokémon Generation V by static recompilation: the games' ARM9 code
is translated to C and runs on a Rust Nintendo DS runtime. Black 2 / White 2 are
the first game family.

An earlier prototype re-implemented the game by hand (map preview, field
movement, an NPC script VM). It was removed in favour of recompilation and is
kept at the `native-port-draft` tag.

> **This is an unofficial fan project.** It is not affiliated with, endorsed
> by or sponsored by Nintendo, Game Freak, Creatures or The Pokémon Company.
> No ROMs, game code or game assets are included: you need a dump of a
> cartridge you own. See [Legal](#legal).

## Status

White 2 plays from its opening and New Game into the story: the save
checkpoints in `debug/checkpoints/white2/` go through the first gym, Virbank
Complex and Route 20 to Castelia City, and the game saves and reloads. Music
and sound effects play, and the 3D can be drawn above the DS resolution, in
widescreen and at the monitor's refresh rate. Some 3D effects are still
missing (edge marking, shadow polygons), and some geometry and shadows
show artifacts. Black 2 shares White 2's code but is less tested; Black and
White are analyzed but untested. Windows is the only platform for now.

## Quick start

You need a C compiler on `PATH` (gcc; [MinGW-w64](https://www.mingw-w64.org/)
on Windows) and a dump of the game. Take `gen5` from a release, or build it
with `cargo build --release` (it is then `target/release/gen5`). Install the
game once, then play:

```text
gen5 install "roms/Pokemon White Version 2.nds"
gen5 run .gen5-cache/pokemon-white-2/95AB5437
```

`install` imports the ROM, translates its code to C and builds the game
library, which takes a few minutes. It prints the directory to play from,
`.gen5-cache/<game>/<rom-crc32>`. It keeps a copy of the ROM there, so the
original can be moved afterwards. After updating gen5, `gen5 status <install>`
says whether the install needs `gen5 update <install>`.

`gen5 help` lists every command, and `gen5 help <command>` or
`gen5 <command> --help` shows a command's arguments, options and, for `run`,
the keys.

## Documentation

- [Playing](docs/playing.md): keys, controllers, the PC settings menu,
  layouts, resolution, widescreen, fast-forward and where saves are kept.
- [How the recompiler works](docs/recompilation.md): import, code export,
  function discovery, C code generation and the build.
- [The Nintendo DS runtime](docs/runtime.md): the hardware model, the ARM7
  stand-in, threads and 3D interpolation.
- [Testing and debugging](docs/debugging.md): headless runs, debug sessions
  and checkpoints, replays and diagnostic variables.
- [Launcher interface](docs/launcher.md): the JSON commands a launcher uses
  to install, update and run games.

## Roadmap

Done: code export, disassembly and function discovery, the C code generator,
a runtime with the ARM7 services the game uses, 2D and 3D graphics with
display capture, sound through a port of the ARM7 sound driver, and a
playable White 2 from New Game past the first gym.

Next:

1. Fill the remaining graphics and sound gaps.
2. Complete White 2, then Black 2 and Black / White.
3. A launcher that downloads gen5, installs games and starts them.

## Legal

Gen5 recomp is an independent, non-commercial open-source project. It is not
affiliated with, endorsed by or sponsored by Nintendo, Game Freak, Creatures
or The Pokémon Company. Pokémon and the names of its games and characters are
trademarks of their respective owners, used here only to say which games the
tools work with.

This repository contains only original source code. It does not contain or
distribute ROMs, BIOS or firmware images, game code, or game assets, and it
does not explain how to obtain them: dump a cartridge you own. Everything the
tools produce from a ROM (the `.gen5-cache` directory: the extracted files, the
generated C code and the compiled game library) is derived from the game and
must not be shared. The project accepts no donations and has no paid builds.

## License

The source code in this repository is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. The license covers this project's own code only, not the games
it runs or anything generated from them.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.

# Launcher interface

A launcher (a separate program that downloads gen5, installs games and
starts them) drives the `gen5` executable through the commands below. They
print one JSON object per line on stdout when given `--json`; logs go to
stderr. Everything else in gen5 may change between versions, but these
commands and fields are kept compatible within a major version, and new
fields may be added.

The launcher must never ship anything a ROM produces. `install` makes it on
the player's machine from their own dump.

## Version

```text
gen5 version --json
{"version":"0.1.0+41f1273921"}
```

The version is the crate version, plus the commit gen5 was built from when
it was built from a Git checkout. `gen5 --version` prints the same for
people.

## Identifying a ROM

```text
gen5 inspect <rom.nds> --json
{"path":"roms/Pokemon White Version 2.nds","title":"POKEMON W2","game_code":"IRDO",
 "game":"pokemon-white-2","game_name":"Pokémon White Version 2","revision":0,
 "size":536870912,"rom_crc32":"95AB5437","support":"supported"}
```

`support` is `supported` (English Black 2 or White 2, what the project is
tested with), `experimental` (another Gen V game or region: it installs, but
is untested) or `unsupported` (not a Gen V game; `game` and `game_name` are
null). A ROM that is not a valid Nintendo DS image fails with exit code 1
and the reason on stderr. Reading the whole ROM for its CRC-32 takes about a
second.

## Installing

```text
gen5 install <rom.nds> --data-dir=<directory> [--cc=<c-compiler>] --json
```

This imports the ROM into `<directory>/<game>/<rom-crc32>/` (the *install*),
keeps a copy of it there as `rom.nds`, translates the code to C and builds
the game library. The C compiler defaults to `gcc` on `PATH`; pass `--cc`
with the full path of a bundled one. The build runs one compiler per CPU
thread; a full build takes about 200 seconds on 14 threads.

Events, in order:

```text
{"event":"step","step":"import","number":1,"steps":4,"title":"Copying the ROM's files and code"}
{"event":"step","step":"analyze","number":2,"steps":4,"title":"Finding the game's functions"}
{"event":"step","step":"codegen","number":3,"steps":4,"title":"Translating the code to C"}
{"event":"step","step":"build","number":4,"steps":4,"title":"Compiling the game library"}
{"event":"progress","step":"build","done":0,"total":261}
{"event":"progress","step":"build","done":1,"total":261}
...
{"event":"finished","cache":"<install>","state":"ready",...}
```

- `step`: a step starts. `number` counts from 1 to `steps`.
- `progress`: files compiled or reused so far in the build step, which is
  most of the time. Other steps report no progress; they take seconds.
- `finished`: the last line on success, with the fields of
  [`status`](#status). Exit code 0.
- `failed`: `{"event":"failed","message":"..."}`, the last line on failure,
  for example a compiler error. Exit code 1.
- `cancelled`: `{"event":"cancelled"}`, the last line after a cancel. Exit
  code 2.

To cancel, write `cancel` and a newline to gen5's stdin. Running compilers
are stopped, and nothing half-written is left looking finished. Running
`install` again resumes, reusing every object already compiled. Killing the
process works too, but on Windows it can leave compilers running unless the
launcher kills the whole process tree.

Running `install` on a ROM that is already installed brings that install up
to date, like `update`.

## Status

```text
gen5 status <install> --json
{"cache":"<install>","state":"ready","game":"pokemon-white-2",
 "game_name":"Pokémon White Version 2","game_code":"IRDO","rom_crc32":"95AB5437",
 "has_rom":true,"built_with":"0.1.0+41f1273921","current":"0.1.0+41f1273921"}
```

`state` is one of:

| State | Meaning | What to do |
|---|---|---|
| `missing` | No imported game in the directory | `install` |
| `not-built` | Imported, never built | `update` |
| `outdated` | Built by another gen5 (`built_with`), or by one that left no stamp | `update` |
| `ready` | Built by this gen5 | `run` |

`has_rom` says whether the install keeps its own copy of the ROM. Installs
made by the separate `import` command do not, and need the ROM's path to
run.

## Updating

```text
gen5 update <install> [--cc=<c-compiler>] --json
```

After a new gen5 is selected, `update` rebuilds an install with it. It
reports the same events as `install`; when the install has no copy of the
ROM it skips the import step and reports three steps. Unchanged generated
files keep their objects, so an update usually compiles only part of the
game.

## Playing

```text
gen5 run <install> [--continue] [--single-screen | --split-screens] [--fps=...]
```

`run` opens the game window and returns when the player quits; the DS soft
reset restarts the game inside the same `run`. Everything the player can
change while playing (resolution, layout, controls, **Start in saved game**)
is set in the in-game PC Settings menu and saved in `pc-settings.json`, so a
launcher only needs options it wants to force for one launch. `--continue`
starts in the saved game.

The game's save (`save.bin`) and DS firmware settings (`firmware.bin`) live
in the install by default. Set `GEN5_STATE_DIR` to keep them elsewhere, for
example a per-game saves directory the launcher can back up, and
`GEN5_SETTINGS_FILE` to move `pc-settings.json`. Rebuilding never touches
them.

## Install layout

```text
<directory>/<game>/<rom-crc32>/
  manifest.json        the import: game, game code, ROM CRC-32, file list
  rom.nds              the copy of the ROM `run` and `update` use
  romfs/               the ROM's files
  code/                the ARM9 and overlay code, decompressed
  recomp/              the generated C
  recomp/build/        objects, compiler logs (*.log) and game.dll
  build.json           which gen5 built game.dll
  save.bin             the game's save (unless GEN5_STATE_DIR is set)
  firmware.bin         the DS firmware settings (likewise)
  pc-settings.json     the PC preferences
```

An install takes about 1.2 GB. Deleting it deletes the save unless the save
is kept elsewhere with `GEN5_STATE_DIR`.

## Requirements

- Windows 10 or later, 64-bit. `gen5.exe` and the game library need only
  system DLLs.
- A graphics card with Direct3D 12 or Vulkan drivers.
- A MinGW-w64 gcc to build with, on `PATH` or given with `--cc`. A portable
  one such as [WinLibs](https://winlibs.com/) (UCRT) works without
  installing it.

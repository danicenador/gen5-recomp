//! The command-line help: `gen5 help`, `gen5 help <command>` and
//! `gen5 <command> --help`.

/// One command's help.
pub struct Command {
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
    pub details: &'static str,
}

pub const COMMANDS: &[Command] = &[
    Command {
        name: "install",
        usage: "gen5 install <rom.nds> [--data-dir=<directory>] [--cc=<c-compiler>] [--json]",
        summary: "Install a game from a ROM in one step",
        details: "\
Runs every step a game needs: imports the ROM into
<directory>/<game>/<rom-crc32>/ (default directory: .gen5-cache) with a
copy of the ROM itself, finds the functions, translates them to C and
builds the game library. The ROM can be moved or deleted afterwards. A full
build takes a few minutes; running install again only redoes what changed.

Options:
  --data-dir=DIR  Where installs go (default .gen5-cache).
  --cc=PATH       The C compiler (default gcc, MinGW-w64 on Windows).
  --json          Report progress as JSON lines, for a launcher; writing
                  `cancel` on a line of stdin stops it (exit code 2).
                  docs/launcher.md describes the events.

Example:
  gen5 install \"roms/Pokemon White Version 2.nds\"
  gen5 run .gen5-cache/pokemon-white-2/95AB5437",
    },
    Command {
        name: "update",
        usage: "gen5 update <install> [--cc=<c-compiler>] [--json]",
        summary: "Rebuild an install after updating gen5",
        details: "\
Translates and builds the game again with this gen5, reusing whatever did
not change. Options are those of install. `gen5 status` says whether an
install needs it.

Example:
  gen5 update .gen5-cache/pokemon-white-2/95AB5437",
    },
    Command {
        name: "status",
        usage: "gen5 status <install> [--json]",
        summary: "Say whether an install is ready to play",
        details: "\
Prints the game, whether the install keeps a copy of its ROM, the gen5
that built it and its state: missing, not-built, outdated (built by another
gen5: run update) or ready.",
    },
    Command {
        name: "inspect",
        usage: "gen5 inspect <rom.nds> [--json]",
        summary: "Check a ROM and say which game it is",
        details: "\
Validates the header checksums, the section ranges and the NitroFS tables,
and prints the title, game code and size. Black, White, Black 2 and White 2
are recognized by their game code. With --json it prints the game, its
CRC-32 and whether it is supported (English Black 2 / White 2), experimental
(another Gen V game or region) or unsupported, as one JSON object.

Example:
  gen5 inspect \"roms/Pokemon White Version 2.nds\"",
    },
    Command {
        name: "list",
        usage: "gen5 list <rom.nds> [path-prefix]",
        summary: "List the files in a ROM's NitroFS",
        details: "\
Prints every file in the ROM's file system, or only those whose path starts
with the prefix.

Example:
  gen5 list \"roms/Pokemon White Version 2.nds\" a/0",
    },
    Command {
        name: "code-info",
        usage: "gen5 code-info <rom.nds>",
        summary: "Show the ARM9/ARM7 binaries, autoload blocks and overlays",
        details: "\
Prints the load and entry addresses of the ARM9 and ARM7 binaries, the ARM9
autoload blocks (ITCM, DTCM, ...) and every overlay.",
    },
    Command {
        name: "import",
        usage: "gen5 import <rom.nds> [cache-directory]",
        summary: "Step 1 of install: copy the ROM's files and code into a cache",
        details: "\
Copies every NitroFS file and the decompressed ARM9, ARM7 and overlay code
into <cache-directory>/<game>/<rom-crc32>/ (default cache directory:
.gen5-cache). Prints the game's directory, which every later step takes as
<cache>.

Example:
  gen5 import \"roms/Pokemon White Version 2.nds\"
  -> .gen5-cache/pokemon-white-2/95AB5437",
    },
    Command {
        name: "analyze",
        usage: "gen5 analyze <cache>",
        summary: "Step 2 of install: find every function in the exported code",
        details: "\
Decodes the ARM9 binary and its overlays and writes
<cache>/analysis/functions.json. Build gen5 with --release: it is much
slower in a debug build.",
    },
    Command {
        name: "codegen",
        usage: "gen5 codegen <cache>",
        summary: "Step 3 of install: translate every function to C",
        details: "\
Writes the generated C into <cache>/recomp/. Run it again after updating
gen5, since the generator changes with it; files whose contents did not
change are left alone, so the next build only recompiles the others.",
    },
    Command {
        name: "build",
        usage: "gen5 build <cache> [c-compiler]",
        summary: "Step 4 of install: compile the generated C into the game library",
        details: "\
Compiles <cache>/recomp/*.c in parallel into <cache>/recomp/build/game.dll,
reusing objects whose sources have not changed. The compiler defaults to
gcc (MinGW-w64 on Windows) and must be on PATH. A full build takes a few
minutes.",
    },
    Command {
        name: "run",
        usage: "gen5 run <cache> [rom.nds] [scale] [--fps=ds|display|uncapped|N]\n           \
                [--split-screens | --single-screen] [--widescreen] [--continue]\n           \
                [--resolution=WxH]",
        summary: "Play the game in a window",
        details: "\
The ROM may be left out for a game set up with `gen5 install`, which keeps a
copy of it.

Options:
  scale             Resolution multiple, 1 to 8 (also --scale=N or
                    GEN5_SCALE). Keys 1-8 change it while playing.
  --fps=MODE        ds (59.83 Hz), display (the monitor's rate, drawing
                    between game frames), uncapped, or a number.
  --split-screens   Top and bottom screens in two windows.
  --single-screen   One full-screen 16:9 screen; the bottom screen slides
                    up on L2 or Space, and by itself in menus and for
                    battle commands.
  --widescreen      Draw the top screen at 16:9 (also GEN5_WIDESCREEN=1).
  --continue        Start in the saved game (also GEN5_AUTO_CONTINUE=1).
  --resolution=WxH  Exact pixel resolution (also GEN5_RESOLUTION=1920x1080).

Settings changed while playing are kept in <cache>/pc-settings.json and
used at the next launch; the options given here override them.

Keys (the defaults; PC Settings can remap them):
  Arrows            D-pad
  X / Z             A / B
  S / A             X / Y
  Q / W             L / R
  Enter             Start
  Right Shift, Backspace
                    Select
  Mouse             Stylus on the bottom screen
  Tab               Fast-forward on/off; + and - set its speed (2x-8x)
  Space             Raise or lower the bottom screen (--single-screen)
  F                 Drawing between game frames on/off (--fps other than ds)
  H                 Switch the top screen between 16:9 and 4:3
  F11, Alt+Enter    Full screen
  F1                PC Settings: resolution, layout, speed, controls
                    (the game pauses while it is open)
  Esc               Quit (asks first)

Game controllers work too; see docs/playing.md.

Example:
  gen5 run .gen5-cache/pokemon-white-2/95AB5437 3",
    },
    Command {
        name: "boot",
        usage: "gen5 boot <cache> [rom.nds] [frames [png-directory]]",
        summary: "Run the game headless, for testing and diagnosis",
        details: "\
Runs for the given number of frames (default 60) without a window and ends
with a report. With a directory, it saves both screens as PNG every 30
frames (GEN5_SCREEN_EVERY).

Common environment variables:
  GEN5_INPUT=first-last:button+button,...   Hold buttons over frame ranges
  GEN5_INPUT_FILE=path                      The same, one entry per line
  GEN5_TOUCH=first-last:x:y,...             Touch the bottom screen
  GEN5_STATE_DIR=dir                        Keep save.bin and firmware.bin
                                            there instead of the cache
  GEN5_BOOT_SECONDS=N                       Watchdog (default 20, 0 = off)
  GEN5_SPEED=N                              Run as if fast-forwarding at N
Button names: a b x y l r start select up down left right.
docs/debugging.md lists the other diagnostic variables.

Example:
  GEN5_INPUT=1300-1305:start gen5 boot .gen5-cache/pokemon-white-2/95AB5437 \\
    \"roms/Pokemon White Version 2.nds\" 1600 screens",
    },
    Command {
        name: "debug",
        usage: "gen5 debug run|boot <cache> <rom.nds> [--checkpoint=<directory>] [arguments]",
        summary: "Play or boot in an isolated session that keeps its saves",
        details: "\
Runs `run` or `boot` with a fresh save of its own under debug/runs/, and
keeps snapshots of the save in the session's history/ directory as the game
writes it. --checkpoint starts from a copy of a saved checkpoint. The other
arguments are those of `run` or `boot`, and the ROM path is required. See
debug/checkpoints/README.md.

Example:
  gen5 debug run .gen5-cache/pokemon-white-2/95AB5437 \\
    \"roms/Pokemon White Version 2.nds\" --checkpoint=debug/checkpoints/white2/<scenario>",
    },
    Command {
        name: "checkpoint",
        usage: "gen5 checkpoint <snapshot-directory> <destination-directory> \"description\" \"reproduction steps\"",
        summary: "Keep a debug session's save as a named checkpoint",
        details: "\
Copies a snapshot from a debug session (a history/frame-* directory, or the
stopped session's directory) to a new directory, with a description and the
steps to reproduce what it is for. The destination must not exist.",
    },
    Command {
        name: "disasm",
        usage: "gen5 disasm <cache> <main|autoloadN|ovN> <address>",
        summary: "Disassemble the function at an address",
        details: "\
Prints the function found at the address in one module: main, an autoload
block (autoload0, ...) or an overlay (ov011, ...), with what the analysis
found about it.

Example:
  gen5 disasm .gen5-cache/pokemon-white-2/95AB5437 ov011 0x0218B360",
    },
    Command {
        name: "version",
        usage: "gen5 version [--json]",
        summary: "Show gen5's version",
        details: "\
Prints the version and the commit gen5 was built from (also --version).",
    },
    Command {
        name: "help",
        usage: "gen5 help [command]",
        summary: "Show this help, or a command's",
        details: "`gen5 <command> --help` shows the same.",
    },
];

const GETTING_STARTED: &str = "\
Getting started (have gcc on PATH; MinGW-w64 on Windows):
  1. gen5 install \"roms/Pokemon White Version 2.nds\"
  2. gen5 run     .gen5-cache/pokemon-white-2/<crc>
`install` prints the directory to use, and runs import, analyze, codegen and
build in one go. Through cargo, put the arguments after `--`:
cargo run --release -- install <rom.nds>";

/// Finds a command by name.
pub fn command(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.name == name)
}

/// The overview: what gen5 does, how to get a game running and every
/// command in one line.
pub fn overview() -> String {
    let width = COMMANDS
        .iter()
        .map(|command| command.name.len())
        .max()
        .unwrap_or(0);
    let mut text = String::from(
        "gen5: a PC port of Pokémon Black, White, Black 2 and White 2 by static\n\
         recompilation of the Nintendo DS code\n\n\
         Usage: gen5 <command> [arguments]\n\n\
         Commands:\n",
    );
    for command in COMMANDS {
        text.push_str(&format!("  {:width$}  {}\n", command.name, command.summary));
    }
    text.push('\n');
    text.push_str(GETTING_STARTED);
    text.push_str("\n\nRun `gen5 help <command>` for a command's arguments and options.");
    text
}

/// One command's help.
pub fn details(command: &Command) -> String {
    format!(
        "{}\n\nUsage: {}\n\n{}",
        command.summary, command.usage, command.details
    )
}

/// The usage lines of every command, for a mistyped command line.
pub fn usage() -> String {
    let mut text = String::from("Usage:\n");
    for command in COMMANDS {
        text.push_str("  ");
        text.push_str(command.usage);
        text.push('\n');
    }
    text.push_str("Run `gen5 help` for more.");
    text
}

/// What `help` and `--help` print for the arguments after `gen5`, or None
/// when they do not ask for help.
pub fn requested(args: &[String]) -> Option<Result<String, String>> {
    let wants = |arg: &String| matches!(arg.as_str(), "--help" | "-h");
    match args {
        [] => Some(Ok(overview())),
        [first, rest @ ..] if first == "help" || wants(first) => Some(match rest {
            [] => Ok(overview()),
            [name] => command(name)
                .map(details)
                .ok_or_else(|| format!("unknown command '{name}'\n\n{}", usage())),
            _ => Err(format!("too many arguments\n\n{}", usage())),
        }),
        [first, rest @ ..] if rest.iter().any(wants) => Some(
            command(first)
                .map(details)
                .ok_or_else(|| format!("unknown command '{first}'\n\n{}", usage())),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn help_is_found_for_every_spelling() {
        assert_eq!(requested(&args(&[])), Some(Ok(overview())));
        assert_eq!(requested(&args(&["help"])), Some(Ok(overview())));
        assert_eq!(requested(&args(&["--help"])), Some(Ok(overview())));
        assert_eq!(requested(&args(&["-h"])), Some(Ok(overview())));
        let run = details(command("run").unwrap());
        assert_eq!(requested(&args(&["help", "run"])), Some(Ok(run.clone())));
        assert_eq!(requested(&args(&["run", "--help"])), Some(Ok(run.clone())));
        assert_eq!(requested(&args(&["run", "cache", "-h"])), Some(Ok(run)));
        assert!(matches!(requested(&args(&["help", "fly"])), Some(Err(_))));
        assert!(matches!(requested(&args(&["fly", "--help"])), Some(Err(_))));
        assert_eq!(requested(&args(&["run", "cache", "rom"])), None);
    }

    #[test]
    fn every_command_is_listed_once() {
        for command in COMMANDS {
            assert_eq!(
                COMMANDS
                    .iter()
                    .filter(|other| other.name == command.name)
                    .count(),
                1
            );
            assert!(command.usage.starts_with(&format!("gen5 {}", command.name)));
            assert!(overview().contains(command.summary));
        }
    }
}

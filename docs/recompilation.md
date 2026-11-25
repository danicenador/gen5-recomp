# How the recompiler works

`gen5 install` runs four steps, which can also be run one at a time while
working on the recompiler. Each step after `import` takes the cache
directory that `import` prints:

```text
cargo run --release -- import "roms/Pokemon White Version 2.nds"
cargo run --release -- analyze ".gen5-cache/pokemon-white-2/95AB5437"
cargo run --release -- codegen ".gen5-cache/pokemon-white-2/95AB5437"
cargo run --release -- build ".gen5-cache/pokemon-white-2/95AB5437"
cargo run --release -- run ".gen5-cache/pokemon-white-2/95AB5437" "roms/Pokemon White Version 2.nds"
```

`codegen` runs the analysis itself; `analyze` writes it out for inspection.

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
  Thumb `bx pc` state switches and interworking returns. A Thumb `bl` into
  code that tears down or reads deep into the caller's stack frame (a shared
  epilogue) is a long branch, not a call, and so is a `bl` to another such
  `bl`, as White 2's evolution uses to set its next step and leave.

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
- An overlay function first checks that RAM still holds the first 16 bytes
  it was compiled from (fewer for a shorter function). If they changed
  (another overlay is loaded there, or the game rewrote the code), it hands
  the call to the runtime instead.
- Switch tables become C `switch` statements.
- Literal-pool loads are folded into constants.
- Indirect calls, ambiguous cross-overlay calls, BIOS calls (`swi`) and
  CP15/PSR access go through the runtime.

Unit tests compile generated code with gcc and run it to check ARM semantics:
flags, shifter carry, unaligned and signed loads, writeback, push and pop, long
multiplies, switch tables, runtime calls and the overlay entry check. The
tests are skipped if gcc is not installed.

For White 2, the generator emits 39,948 functions (209 MB of C in 260 files) in
about 7 seconds. All files compile with `gcc -O1 -Wall` without warnings. The
build takes about 200 seconds on 14 threads. Linked against a stub runtime, it
is a 52 MB executable.

## Building the game library

`build` compiles `recomp/*.c` with gcc in parallel into
`recomp/build/game.dll`, reusing objects whose sources have not changed.
`gen5host.c` forwards every `rt_*` hook to callbacks the runtime registers,
so the library builds once and the Rust runtime loads it with `libloading`.

# Debug save checkpoints

Commit selected, useful in-game saves here, grouped by game and scenario (for
example `white2/before-rival-battle/`). Automatic history lives in `debug/runs/`
and is ignored by Git.

From the repository root:

```powershell
cargo run --release -- debug run .gen5-cache/pokemon-white-2/95AB5437 "roms/Pokemon White Version 2.nds"
```

The command prints a unique session directory. A fresh session starts with no
save and fresh firmware. Existing player saves are never used implicitly.
Each session keeps its working `save.bin`, `firmware.bin`, and `checkpoint.json`.
Its `history/frame-*/` directories contain snapshots after 120 game frames
without flash writes. Identical save images are skipped. These are **unverified
candidates**, not proof that the game completed a save: a pause can occur during
a save, and exiting before the quiet interval leaves only the working files.
Firmware is captured alongside each save. No RAM, CPU or exact-frame state is
stored.

After saving in the game and closing it, select a history directory (or the
stopped session directory) and give it a destination, description and concrete
reproduction steps:

```powershell
cargo run --release -- checkpoint debug/runs/<session>/history/frame-0000012345 debug/checkpoints/white2/before-rival-battle "Before the first rival battle" "Continue, leave the house, then approach the rival."
```

The destination must not exist. It contains `save.bin`, `firmware.bin`, and
`checkpoint.json` with the ROM CRC-32, game code, description, reproduction steps,
source and snapshot frame (when available). Checkpoints require a 512 KiB game
save and 256 KiB firmware. Verify a candidate by loading it and choosing Continue
before committing it; record the observed result in the reproduction notes.
Do not copy working files while the game is running.

```powershell
cargo run --release -- debug run .gen5-cache/pokemon-white-2/95AB5437 "roms/Pokemon White Version 2.nds" --checkpoint=debug/checkpoints/white2/before-rival-battle --single-screen
```

The checkpoint is copied into a new session; playing never modifies the source.
The actual ROM checksum must match both the imported cache and the checkpoint.
The supplied checkpoint also appears as `history/initial/` in the new session.
Normal run options work after the ROM argument. For headless debugging, replace
`debug run` with `debug boot` and pass the normal frame count and screenshot
directory. Existing input-script and diagnostic environment variables still work;
the launcher overrides `GEN5_STATE_DIR` and `GEN5_DEBUG_HISTORY` for its child only.

Only add selected checkpoints to Git. ROMs, generated code, automatic history,
and unrelated playthroughs should stay out of checkpoint commits. No gameplay
checkpoint is bundled until it has been produced and checked in-game.

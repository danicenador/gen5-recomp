# The Nintendo DS runtime

The runtime loads the game library that `build` makes and runs it on a Rust
model of the DS hardware.

## Hardware

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
  Fog follows the hardware: each pixel keeps a fog flag (set by opaque
  polygons with fog enabled, cleared by any polygon without it, the rear
  plane's from CLEAR_COLOR), and flagged pixels move toward the fog color
  and alpha by the density table at their depth. Driftveil's gym is dark
  this way, and the sewers and Route 4's sandstorm haze use it too.
  Edge marking, antialiasing, wireframe, shadow polygons and rear-plane
  bitmaps remain unsupported. Lighting and depth are functional approximations rather than
  hardware-exact arithmetic.
- `gpu2d::capture`: frame-based display capture into LCDC VRAM, from engine A,
  the 3D output, another VRAM bank, or a blend of both sources. Captures wrap
  within the destination bank and finish without a window or screenshot sink.
  Main-memory display FIFO capture is not implemented.
- `input`: buttons and touch screen, shared between the window and the
  machine.
- `speed`: frame pacing and fast-forward, with real time kept for the sound
  driver, for the key update's repeat countdown, and for held keys in the
  menus that replace the field.
- `io`: I/O registers. Display timing advances when the game polls it, and
  there are vblank and interrupt flags, immediate DMA, the divider and
  square-root units, and IPCSYNC and the IPC FIFO.
- `card`: game card reads (`0xB7` data blocks and `0xB8` chip ID) from the
  ROM file, including card-triggered DMA and the transfer-complete
  interrupt. Like a retail card, addresses wrap at the chip size from the
  header and reads below 0x8000 return data from 0x8000; Black 2 and
  White 2's anti-piracy code checks both. Bytes written to AUXSPIDATA in
  SPI mode go to the card's backup bus: on cards whose game code starts
  with `I` an infrared chip sits in front of the flash and answers its ID
  command (0x08) with 0xAA. Black 2 and White 2 ask for it before a battle
  awards experience, so without it no Pokémon gains EXP. The flash itself
  is not on this bus here (saves go through the ARM7 stand-in) and reads
  0xFF.
- `cpu`: banked registers, CPSR and SPSR, and exception entry and return.
- `machine`: boot as the firmware leaves memory, CP15, BIOS calls (waits,
  division, copies, CRC, LZ77 and RLE), and interrupt delivery through the
  handler at the end of DTCM. Overlay-aware dispatch picks the overlay whose
  code is currently in RAM. Overlays loaded at the same address often start
  a function with the same bytes (the anti-piracy calls at the top of the
  Pokémon World Tournament's ov134 and a dozen others), so where several
  match the first 16 bytes, it compares on until one is left.
- `interpreter`: runs code with no recompiled function, such as routines the
  game copies to ITCM before running them, or jumps into a function's middle.
  Calls from it go back to native code. Black 2 and White 2 keep some overlay
  functions encrypted and decrypt them in RAM before calling them (the party
  screen's anti-piracy checks). The recompiled code for those functions comes
  from the encrypted bytes, so it must not run. Calls to callees that do not
  decode cleanly go through the runtime. Every overlay function also checks
  its bytes in RAM when it is entered, so a direct call to code that was
  decrypted is caught as well. In both cases the runtime finds the decrypted
  code in RAM and interprets it.

## The ARM7 stand-in

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

## Threads, timers and checks

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

## 3D interpolation

3D interpolation pairs each polygon with the same one in the next list. It
also works out the camera's motion between the two lists from the matrices
the game drew the paired scenery with: a static object's model-to-clip
matrix changes only by the camera, so `before⁻¹ · after` is the camera's
motion, and the motion most paired objects agree on wins over characters
that move by themselves. Repeated identical meshes (stepping stones, gym
flags, fence posts) pair with the copy where that motion takes them. A
sprite animated from a sheet, such as the walking player, picks each frame
by texture coordinates; it pairs with the one polygon of the same shape in
the next list whatever its texture coordinates. A polygon that still cannot
be paired, such as a flag's cloth whose shape
changes every list, moves with the paired polygons drawn through the same
matrix, or else with the camera, instead of standing still for the
in-between frames.

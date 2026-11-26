# Playing

`gen5 run <install>` plays a game set up with `gen5 install` (see the
[quick start](../README.md#quick-start)). For a cache made with the separate
steps of [the recompiler](recompilation.md), give the ROM too:
`gen5 run <cache> <rom.nds>`. `gen5 help run` lists the options and keys.

## Window, keys and controllers

On first launch, `run` opens a single full-screen view at the monitor's
resolution, paced to the DS's 59.83 frames per second. Later launches use
the saved PC preferences. Keys: arrows for the D-pad, X = A, Z = B, S = X, A = Y,
Q = L, W = R, Enter = Start, Right Shift or Backspace = Select, and the
mouse on the lower screen as the stylus. Escape asks whether to quit.

The DS soft-reset chord, **L + R + Start + Select**, restarts the game.
The runtime handles the game's reset request after its own shutdown sequence,
flushes the saved data and firmware, and starts a fresh game process.
Soft resets always show the normal opening and main menu, even when
**Start in saved game** is enabled. The preference still applies on the next launch.

Game controllers work too. The face buttons map by position, as on a DS
(bottom = B, right = A, top = X, left = Y, whatever the controller prints
on them); the shoulder buttons are L and R, Start and Back/View/Select are
Start and Select, and the D-pad or left stick move. The right trigger
turns fast-forward on and off, like Tab, and in single-screen mode the left
trigger raises and lowers the bottom screen, like Space, and the right
stick moves a stylus on it that pressing the stick touches with.

## PC settings menu

Press **F1** or click the controller's **left stick** to open PC Settings
at any time. The game pauses while the menu is open. Use the D-pad or arrow
keys to select a row, left/right to change it, A (keyboard X or Enter) to
confirm, and B (keyboard Z or Escape) to go back. Closing the menu resumes
from fresh game frames so time spent in settings does not add input delay.
Mouse clicks work too;
the mouse wheel scrolls the mappings. Menu navigation keeps these defaults
even after gameplay controls are remapped, and F1 remains a recovery shortcut.

On the supported English Black 2 / White 2 builds, the original **Options**
screen also has a **Y: PC Settings** button beside its existing bottom-screen
buttons. Tap it or press the DS Y button (keyboard A by default) to open the
same submenu. Closing it returns to the original Options screen, preserving
any edits to the game's own options. This entry observes the game's actual
Options input routine rather than guessing from X presses. Other builds keep
the F1 / left-stick entry point when their Options routine does not match.

After updating gen5, run `gen5 update <install>` to rebuild the game
library. That is how fixes such as the control-flow fixes used during
evolution and the Options observer reach an install. No ROM patch or save
conversion is needed. The observer's identification was checked against
the local English Black 2 and White 2 ROMs; function names were cross-referenced
with [pokebw2's Options source](https://github.com/fuddlesworth/pokebw2/blob/main/src/ov140/config_panel.c).

The submenu controls resolution (Auto, common pixel sizes and the monitor's
own size), aspect ratio, single /
stacked / separate-window layouts, fullscreen, Manual / Smart bottom-screen
behavior, fast-forward and its 2x-8x speed, 3D interpolation, stylus sensitivity,
stick dead zone, and keyboard/controller mappings. Manual mode leaves the
bottom screen under the player's control; Smart mode follows scene changes
and still allows manual toggles. Layout changes take effect without restarting
the game. Controller binding names are physical positions: East is the right
face button, South the bottom one, North the top one, and West the left one.

To remap, select an action and press the new key or controller button. A button
assigned elsewhere moves to the new action. Escape cancels capture; Delete
clears the selected binding. Restore Defaults asks for a second confirmation.
Changes, including the existing display/speed shortcuts, are saved in
`<cache>/pc-settings.json`, separately from `save.bin` and `firmware.bin`.
`GEN5_SETTINGS_FILE=path` selects an isolated preferences file for testing.
**Start in saved game** skips the opening and selects the game's validated
Continue entry on the next launch, with the wireless C-Gear off. It is off
by default. The menu stays hidden while the game checks the save and enters
Continue, so its buttons do not flash before the saved game appears.
`--continue` or `GEN5_AUTO_CONTINUE=1` also enables it for one launch. Missing
or damaged saves retain the game's normal checks and menu. English Black 2
and White 2 are supported; other builds keep their normal startup.
Explicit launch flags and environment settings override saved preferences for
that launch only: the file keeps its value for each one unless you change that
setting while playing. A settings-file error is reported instead of silently
overwriting it.

## Screen layouts

Add `--split-screens` to `run` to put the top and bottom DS screens in
separate, independently resizable windows. Keyboard controls work in either
window; the mouse acts as the stylus only in the bottom-screen window.
Closing either window quits. A layout chosen in the PC Settings menu is saved;
new preferences use a single screen.

Add `--single-screen` to `run` to play on one screen. The window opens full
screen, and the top screen fits the chosen aspect ratio (H still switches
it to 4:3). The bottom screen is hidden until the left trigger (L2)
or Space is pressed: then it slides up from the lower edge in 180 ms, 4:3
and two thirds as tall as the top screen (720 lines on a 1080-line monitor),
centered with its lower edge on the screen's, and the top screen blurs
behind it. Pressing again slides it back down; pressing midway turns it
around. The game keeps running and taking buttons while it is up, and the
mouse is the stylus on it once it has fully risen.

F11 or Alt+Enter switches
between borderless full screen and a window, in any layout. On returning from
Alt+Tab the display surface is refreshed, and minimized or occluded windows
skip presentation until restored. `--single-screen` and
`--split-screens` cannot be combined.

## Resolution

Auto resolution reads the monitor's exact pixel size. Any size can be selected
with `--resolution=1920x1080` or `GEN5_RESOLUTION=1417x907`, including in
headless `boot` screenshots. Rendering uses that pixel size directly instead
of rounding up to a DS resolution multiple. Fit resolution preserves aspect
ratios from 4:3 through 16:9 (including 16:10); wider or taller screens show
black bars. The bottom screen and stylus retain their 4:3 proportions.
The old 1-8 shortcuts and explicit integer scales remain available.

`run` takes an optional resolution multiple after the ROM, `run <cache>
<rom> 4` (or `GEN5_SCALE=4`), and keys 1 to 8 change it while playing. At
1 the software renderer draws the DS's own 256x192 per screen. Above 1 the
GPU draws the 3D layer at that multiple, so models and the field gain
detail, while 2D backgrounds and sprites are drawn at the DS resolution and
repeated, so they stay sharp. A window smaller than the result shows it
filtered down. 3D the game shows through display capture (the title
screen's Kyurem) stays at the DS resolution, since the game reads captured
pixels back.
`boot` saves screenshots at the same multiple with `GEN5_SCALE`, and
`GEN5_GPU=1` uses the GPU renderer at 1, to compare it with the software one.

## The bottom screen in single-screen mode

The bottom screen also follows the scene by itself: it rises in menus played
on it and in battles while they wait for a command, and drops back on the
overworld and while a battle plays out. The runtime tells scenes apart by
the ARM9 overlays in RAM (see `scene_wants_panel` in
`src/runtime/screen.rs`); a scene has to hold for 100 ms first, so map
transitions pass unnoticed, and scenes it cannot tell (the title screen)
leave the bottom screen where it is. A battle's bottom screen shows a Poké
Ball on BG2 and BG3 while the battle plays out and turns BG1 on whenever it
waits for the player: the command buttons (FIGHT, BAG, RUN, POKéMON, then the
moves) and, after a level-up, the question whether to forget a move for a new
one and the list of moves to forget. So the bottom screen rises with those
and drops once they are answered. Until a battle has drawn its own bottom
screen, the C-Gear the field left there is ignored. An evolution plays on the
top screen with the bottom screen down, and raises it only when the Pokémon
wants to learn a move and asks which to forget. In the
opening it rises for the naming keyboard, which the game shows with the
screens swapped (engine A on the bottom), and drops for Professor Juniper's
speeches. The X menu loads no overlays of its own, so the runtime reads the
game's field events instead (`src/nds/field_menu.rs`): the bottom screen
stays up while the menu or any screen opened from it (the Pokédex, the
trainer card, the save prompt, ...) is up, and drops once the menu closes,
whether by B, by X or by itself after saving, or gives way to something on
the field, such as an Escape Rope used from the bag. L2 or Space still moves
it at any time; the next scene change takes over again. On a controller, the right
stick moves a stylus over the raised bottom screen, shown as a ring, and
pressing the stick in touches with it (the ring fills); it hides three
seconds after it last moved. `GEN5_OVERLAY_LOG=path` writes, for `run` or
`boot`, each frame at which the overlays in RAM change, with those loaded
and unloaded, each frame at which the field menu opens or closes or the
field events running change (their functions, the running one first), and
each frame at which the bottom screen's engine or layers change, for finding
the overlays, events and layers of new scenes.

## Widescreen

Add `--widescreen` (or set `GEN5_WIDESCREEN=1`) to draw the top screen at
16:9, 342x192 per DS-sized screen, and press H to switch between 16:9 and
4:3 while playing. The 3D camera sees further to the left and right instead
of the picture being stretched: the left and right clip planes are moved out
by 43 columns, so polygons and box tests beside the screen are kept and
drawn there, with polygon RAM of their own so the DS's own view never loses
polygons to them. Viewports narrower than the screen keep their edges.
A flat 3D picture drawn across the whole screen, all of it at one depth
(an orthographic projection, which no 3D scene uses), is made for the DS's
256 columns, so instead the sides show it reflected at the screen's edges:
the curtain that falls over the field as a wild battle starts in a cave or
the Castelia sewers covers the whole 16:9 screen.
Backgrounds and sprites stay in the middle 256 columns; beside them the top
screen shows the 3D layer, with the same blending and master brightness, or
black when it shows no 3D (menus and other 2D screens). Under the 3D layer
the sides mirror what is under it at that end of each line, so a background
behind the 3D, such as the gradient behind Professor Juniper, carries on
instead of the backdrop color. Foreground text and dialogue boxes stay in
the original 256 columns, including translucent ones. A uniform overlay
covering the entire screen, such as the season banner's black background
and its fade, extends into both margins while its lettering stays centered.
Letterbox bars, opaque black lines across the whole screen from its top or
bottom edge (the evolution scene's), also reach across both margins.
Where opaque 2D is in front of the 3D there, the last column carries on, and
where it is in front at the end of the line as well, the backdrop is used.
The bottom screen stays 4:3 and is centered under the top one. Display
capture still writes the DS's 256 columns, but in widescreen it also keeps
what the screen showed beside them (the 3D layer drawn 342 columns wide in
software). The game's transitions show a captured screen as a direct-color
bitmap background (walking through a door, a battle starting, an item
received), and while such a bitmap is shown and still holds the captured
pixels, or the same pixels darkened or tinted in place channel by channel,
it carries on into the sides through its own affine transform: from VRAM
where it maps to the captured columns, from the kept sides beside them.
Widescreen uses the GPU renderer at every resolution, and `boot` with
`GEN5_WIDESCREEN=1` saves widescreen screenshots.

## Fast-forward

Tab turns fast-forward on and off, and + and - (on any keyboard layout, or
the numpad) set its speed (2x to 8x, 3x to begin with; the window title
shows it). Fast-forward runs the game that
many frames per DS frame of real time, so walking, battles, text and
animations speed up, but it leaves three things at normal speed:

- Music and sound effects. The sound driver and mixer follow real time
  instead of the game's frames, so music keeps its tempo and pitch. Fades
  and jingles the game times itself still happen sooner.
- Key repeat in the field's menus (the X menu, shops, yes/no boxes).
  Holding a direction moves the cursor as often as at normal speed, and a
  short tap does not repeat. The runtime finds the repeat countdown of
  GameFreak's key update by its instructions (the same in Black 2 and
  White 2) and holds it back on the extra frames. Keys the game reads
  directly while held, such as walking, are not slowed down.
- Menus that replace the field, such as the bag, options and Pokédex. They
  count held keys their own way, in game frames: options and the Pokédex
  list keep their own counters, and the bag takes a repeat only once its
  pocket has finished sliding. So while one of these menus runs and a
  direction, L or R pressed in it is held, the game runs at normal speed,
  and the cursor moves as it would without fast-forward. The runtime tells
  them from the field by the UI update count in the same key system: the
  field map clears it on each of its 30-per-second steps, so on the field
  it only reads 0 or 1, while elsewhere it counts on. Battles, the title
  screen and the fade through a door count on too, so a direction pressed
  there also runs at normal speed while held. A key still held from the
  field when it gives way, such as walking through a door or into a wild
  battle, keeps the game fast until it is released.

The window shows about one frame per DS frame of real time while
fast-forwarding, which leaves more time for the game; how fast it can go
depends on the host. Where the host cannot keep up (drawing at a high
frame rate takes time from the game too), the game runs as fast as it
can and its frames are timed by when they really come, so drawing between
them stays smooth instead of catching up with them and standing still.
`GEN5_SPEED=N` starts `run` fast-forwarding at N, and
makes `boot` run as if fast-forwarding at N, counting each frame as 1/N of a
DS frame of real time for sound and key repeat (or as a whole one where a
key pressed in a menu away from the field runs it at normal speed), so
`GEN5_AUDIO_WAV` records the music at normal speed.

## Saves and settings

The game's save (`save.bin`, the cartridge's 512 KiB of backup flash) and
the DS firmware settings (`firmware.bin`) are kept in the install directory,
beside `pc-settings.json` with the PC preferences. `gen5 update` leaves all
three alone. `GEN5_STATE_DIR=dir` keeps `save.bin` and `firmware.bin` in
another directory, and `GEN5_SETTINGS_FILE=path` the preferences, so a
launcher can keep them outside the install.

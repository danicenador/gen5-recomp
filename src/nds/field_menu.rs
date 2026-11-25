//! Whether the field menu (the X menu) is open, read from the game's state.
//!
//! On the overworld, Black 2 and White 2 run everything that takes over from
//! walking as a field event (`GMEVENT`, from `game_event.c`): a script, a
//! warp, or the field menu. The game system keeps the event running now,
//! and each event points to the one that started it, so the field menu is
//! open while the first event of that chain runs the field menu's function.
//! That covers every screen opened from the menu (the Pokédex, the trainer
//! card, the save prompt, ...), which come back to it, and it ends however
//! the menu closes, by B or X or by itself after saving.
//!
//! The menu also stays first in the chain while it hands over to something
//! that happens on the field: an Escape Rope used from the bag spins the
//! player out of the cave, warps and lands them as events under the menu's,
//! which ends only after that. The menu opens its screens through an event
//! of its own overlay, while those actions run code from elsewhere, so the
//! menu counts as closed once the event it started runs code outside its
//! overlay.

use super::memory::Memory;

/// The overlay holding the field menu's event function, in RAM whenever the
/// game is past the title screen.
pub const OVERLAY: u16 = 12;

/// In the game system: the event running now (0 when none is).
const SYSTEM_EVENT: u32 = 0x18;
/// In an event: the event that started it (0 for the first).
const EVENT_PARENT: u32 = 0x00;
/// In an event: the function that runs it each frame.
const EVENT_FUNCTION: u32 = 0x04;
/// In an event: the game system it belongs to.
const EVENT_SYSTEM: u32 = 0x10;
/// Events nest only a few deep; a longer chain is not one.
const MAX_DEPTH: usize = 16;

/// The game system's set-up stores it in a static: `ldr r0, =pointer;
/// ldr r2, [sp, #4]; str r5, [r0]; adds r0, r5, #0; movs r1, #3`. The
/// pattern follows the `ldr`.
const SYSTEM_STORE: [u8; 8] = [0x01, 0x9A, 0x05, 0x60, 0x28, 0x1C, 0x03, 0x21];

/// The start of the field menu's event function (Thumb): it saves
/// registers and switches on its step (`ldr r1, [r4, #0x14]`, ten cases)
/// through a table of halfword offsets.
const MENU_EVENT_START: [u8; 26] = [
    0x78, 0xB5, 0x87, 0xB0, 0x14, 0x1C, 0x61, 0x69, 0x05, 0x1C, 0x09, 0x29, 0x41, 0xD8, 0x49, 0x18,
    0x79, 0x44, 0xC9, 0x88, 0x09, 0x04, 0x09, 0x14, 0x8F, 0x44,
];

/// Where to look for the field menu in RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldMenu {
    /// The static holding the game system's address.
    system_pointer: u32,
    /// The field menu's event function, with the Thumb bit.
    function: u32,
    /// Where the field menu's overlay is in RAM: its first and last addresses
    /// plus one.
    overlay: (u32, u32),
}

impl FieldMenu {
    /// Find the game system's static in the ARM9 binary `arm9`, loaded at
    /// `arm9_base`, and the field menu's event function in `overlay`, the
    /// code of `OVERLAY` and its load address. `None` when either was not
    /// recognised.
    pub fn find(arm9: &[u8], arm9_base: u32, overlay: Option<(u32, &[u8])>) -> Option<Self> {
        let store = unique(arm9, &SYSTEM_STORE)?;
        let load = store.checked_sub(2)?;
        let instruction = u16::from_le_bytes(arm9.get(load..load + 2)?.try_into().ok()?);
        // ldr r0, [pc, #imm8 * 4], with pc the word-aligned address + 4.
        if instruction & 0xFF00 != 0x4800 {
            return None;
        }
        let literal = ((arm9_base + load as u32 + 4) & !3) + u32::from(instruction & 0xFF) * 4;
        let offset = literal.checked_sub(arm9_base)? as usize;
        let system_pointer = u32::from_le_bytes(arm9.get(offset..offset + 4)?.try_into().ok()?);
        if !in_main_ram(system_pointer) {
            return None;
        }
        let (overlay_base, overlay) = overlay?;
        let function = overlay_base + unique(overlay, &MENU_EVENT_START)? as u32;
        Some(Self {
            system_pointer,
            function: function | 1,
            overlay: (overlay_base, overlay_base + overlay.len() as u32),
        })
    }

    /// The functions of the events running now, from the one running to
    /// the first of its chain, for GEN5_OVERLAY_LOG.
    pub fn chain(&self, ram: &[u8]) -> Vec<u32> {
        let system = read32(ram, self.system_pointer);
        let mut functions = Vec::new();
        if !in_main_ram(system) {
            return functions;
        }
        let mut event = read32(ram, system + SYSTEM_EVENT);
        while functions.len() < MAX_DEPTH
            && in_main_ram(event)
            && read32(ram, event + EVENT_SYSTEM) == system
        {
            functions.push(read32(ram, event + EVENT_FUNCTION));
            event = read32(ram, event + EVENT_PARENT);
        }
        functions
    }

    /// Whether the field menu is open, given main RAM. Anything that does
    /// not look like the game system and its events counts as closed.
    pub fn is_open(&self, ram: &[u8]) -> bool {
        let system = read32(ram, self.system_pointer);
        if !in_main_ram(system) {
            return false;
        }
        let mut event = read32(ram, system + SYSTEM_EVENT);
        // The function of the event the first one started, if any.
        let mut started = None;
        for _ in 0..MAX_DEPTH {
            if !in_main_ram(event) || read32(ram, event + EVENT_SYSTEM) != system {
                return false;
            }
            let parent = read32(ram, event + EVENT_PARENT);
            if parent == 0 {
                let (start, end) = self.overlay;
                return read32(ram, event + EVENT_FUNCTION) == self.function
                    && started.is_none_or(|function: u32| (start..end).contains(&(function & !1)));
            }
            started = Some(read32(ram, event + EVENT_FUNCTION));
            event = parent;
        }
        false
    }
}

/// Where `pattern` is in `code`, if it is there exactly once.
fn unique(code: &[u8], pattern: &[u8]) -> Option<usize> {
    let mut found = code
        .windows(pattern.len())
        .enumerate()
        .filter(|(_, window)| *window == pattern)
        .map(|(offset, _)| offset);
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

fn in_main_ram(address: u32) -> bool {
    address >> 24 == 0x02 && address & 3 == 0
}

fn read32(ram: &[u8], address: u32) -> u32 {
    let offset = Memory::main_offset(address);
    ram.get(offset..offset + 4)
        .map_or(0, |bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARM9: u32 = 0x0200_4000;
    const OVERLAY_BASE: u32 = 0x0215_0400;
    /// Where White 2 keeps them.
    const POINTER: u32 = 0x0214_1134;
    const MENU: u32 = 0x0215_A8B4;
    const SYSTEM: u32 = 0x0223_B2C0;

    fn code() -> (Vec<u8>, Vec<u8>) {
        let mut arm9 = vec![0; 0x200];
        // At 0x02004100: ldr r0, [pc, #0x84], then the pattern; the literal
        // is at (0x02004104 & !3) + 0x84 = 0x02004188.
        arm9[0x100..0x102].copy_from_slice(&0x4821_u16.to_le_bytes());
        arm9[0x102..0x10A].copy_from_slice(&SYSTEM_STORE);
        arm9[0x188..0x18C].copy_from_slice(&POINTER.to_le_bytes());
        // As long as White 2's ov012.
        let mut overlay = vec![0; 0x1_E280];
        let at = (MENU - OVERLAY_BASE) as usize;
        overlay[at..at + MENU_EVENT_START.len()].copy_from_slice(&MENU_EVENT_START);
        (arm9, overlay)
    }

    fn write(ram: &mut [u8], address: u32, value: u32) {
        let offset = Memory::main_offset(address);
        ram[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn event(ram: &mut [u8], at: u32, parent: u32, function: u32) {
        write(ram, at + EVENT_PARENT, parent);
        write(ram, at + EVENT_FUNCTION, function);
        write(ram, at + EVENT_SYSTEM, SYSTEM);
    }

    #[test]
    fn finds_the_game_system_and_the_menu_event() {
        let (arm9, overlay) = code();
        let menu = FieldMenu::find(&arm9, ARM9, Some((OVERLAY_BASE, &overlay)));
        assert_eq!(
            menu,
            Some(FieldMenu {
                system_pointer: POINTER,
                function: MENU | 1,
                overlay: (OVERLAY_BASE, OVERLAY_BASE + 0x1_E280),
            })
        );
        // Without the overlay, or with the pattern twice, there is no telling.
        assert_eq!(FieldMenu::find(&arm9, ARM9, None), None);
        let mut twice = overlay.clone();
        twice[0x10..0x10 + MENU_EVENT_START.len()].copy_from_slice(&MENU_EVENT_START);
        assert_eq!(
            FieldMenu::find(&arm9, ARM9, Some((OVERLAY_BASE, &twice))),
            None
        );
        assert_eq!(
            FieldMenu::find(&[0; 0x200], ARM9, Some((OVERLAY_BASE, &overlay))),
            None
        );
    }

    #[test]
    fn the_menu_is_open_while_its_event_chain_runs() {
        let (arm9, overlay) = code();
        let menu = FieldMenu::find(&arm9, ARM9, Some((OVERLAY_BASE, &overlay))).unwrap();
        let mut ram = vec![0; 0x40_0000];
        // Before the game system exists, and on the field with no event.
        assert!(!menu.is_open(&ram));
        write(&mut ram, POINTER, SYSTEM);
        assert!(!menu.is_open(&ram));
        // The menu itself (as White 2 keeps it after X).
        event(&mut ram, 0x0224_72D0, 0, MENU | 1);
        write(&mut ram, SYSTEM + SYSTEM_EVENT, 0x0224_72D0);
        assert!(menu.is_open(&ram));
        // A screen opened from it, and the save prompt under that.
        event(&mut ram, 0x0224_739C, 0x0224_72D0, 0x0215_B54D);
        event(&mut ram, 0x0224_7484, 0x0224_739C, 0x0215_C589);
        write(&mut ram, SYSTEM + SYSTEM_EVENT, 0x0224_7484);
        assert!(menu.is_open(&ram));
        // Back to the menu, which hands over to an Escape Rope used from the
        // bag: its event and the spin under it run code from another
        // overlay, so the menu has given way to the field.
        write(&mut ram, SYSTEM + SYSTEM_EVENT, 0x0224_72D0);
        assert!(menu.is_open(&ram));
        event(&mut ram, 0x0224_7600, 0x0224_72D0, 0x0217_D5E9);
        write(&mut ram, SYSTEM + SYSTEM_EVENT, 0x0224_7600);
        assert!(!menu.is_open(&ram));
        event(&mut ram, 0x0224_7700, 0x0224_7600, 0x0215_C5DD);
        write(&mut ram, SYSTEM + SYSTEM_EVENT, 0x0224_7700);
        assert!(!menu.is_open(&ram));
        write(&mut ram, SYSTEM + SYSTEM_EVENT, 0x0224_7484);
        assert!(menu.is_open(&ram));
        // A script started by talking to someone is not the menu.
        event(&mut ram, 0x0224_8000, 0, 0x0215_0001);
        write(&mut ram, SYSTEM + SYSTEM_EVENT, 0x0224_8000);
        assert!(!menu.is_open(&ram));
        // Nor is a chain that leaves the game system or loops.
        write(&mut ram, 0x0224_72D0 + EVENT_SYSTEM, 0x0220_0000);
        write(&mut ram, SYSTEM + SYSTEM_EVENT, 0x0224_7484);
        assert!(!menu.is_open(&ram));
        write(&mut ram, 0x0224_72D0 + EVENT_SYSTEM, SYSTEM);
        write(&mut ram, 0x0224_72D0 + EVENT_PARENT, 0x0224_7484);
        assert!(!menu.is_open(&ram));
        // Closed: no event.
        write(&mut ram, SYSTEM + SYSTEM_EVENT, 0);
        assert!(!menu.is_open(&ram));
    }
}

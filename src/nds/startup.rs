//! Optional one-shot startup through the game's validated Continue menu.
use crate::recomp::{
    analysis::decode_at,
    arm::Op,
    program::{ModuleId, Program},
};

#[derive(Clone, Copy, Debug)]
pub(super) struct Hooks {
    pub boot: u32,
    pub queue: u32,
    pub menu: u32,
    pub select: u32,
    pub tick: u32,
    pub finish: u32,
    pub wipe_out: u32,
    pub select_word: u32,
}

impl Hooks {
    pub fn find(program: &Program) -> Option<Self> {
        let main = program.module(ModuleId::Main)?;
        let menu = program.module(ModuleId::Overlay(162))?;
        // English BW2 StartMenuEventTick and StartMenu_Select. Verify code
        // and callback tables before using their work-structure layout.
        const BOOT: &[u8] = &[
            0x38, 0xb5, 0x0c, 0x1c, 0x20, 0x68, 0x03, 0x28, 0x32, 0xd8, 0x00, 0x18, 0x78, 0x44,
            0xc0, 0x88,
        ];
        if !matches!(menu.bytes.len(), 19584 | 19648)
            || menu.bytes.get(0xb58..0xb5a) != Some(&[0xf8, 0xb5])
            || menu.bytes.get(0xb24..0xb28) != Some(&[0x38, 0xb5, 0x19, 0x25])
            || menu.bytes.get(0xed0..0xeda)
                != Some(&[0x70, 0xb5, 0x5b, 0x25, 0x04, 0x1c, 0xad, 0x00, 0x60, 0x59])
        {
            return None;
        }
        let menu_table = (0..menu.bytes.len().saturating_sub(12))
            .step_by(4)
            .find(|&offset| {
                let address = menu.base + offset as u32;
                menu.u32_at(address) == Some((menu.base + 0xa94) | 1)
                    && menu.u32_at(address + 4) == Some((menu.base + 0xb24) | 1)
                    && menu.u32_at(address + 8) == Some((menu.base + 0xb58) | 1)
            })?;
        let mut starts = main
            .bytes
            .windows(BOOT.len())
            .enumerate()
            .filter(|(at, bytes)| at % 2 == 0 && *bytes == BOOT);
        let boot = main.base + starts.next()?.0 as u32;
        if starts.next().is_some() {
            return None;
        }
        let mut address = boot;
        let mut queue = None;
        while address < boot + 0x94 {
            let ins = decode_at(main, address, true)?;
            if let Op::Branch {
                target,
                link: true,
                exchange: false,
            } = ins.op
            {
                if queue.is_some_and(|queue| queue != target) {
                    return None;
                }
                queue = Some(target);
            }
            address = ins.end();
        }
        Some(Self {
            boot,
            queue: queue?,
            menu: menu.base + menu_table as u32,
            select: menu.base + 0xed0,
            tick: menu.base + 0xb24,
            finish: menu.base + 0xb58,
            wipe_out: menu.base + 0x3160,
            select_word: menu.u32_at(menu.base + 0xed0)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires locally supplied English Black 2 and White 2 ROMs"]
    fn startup_hooks_match_both_local_games_and_reject_changed_code() {
        use crate::formats::nds::{Cpu, NdsRom};
        use crate::recomp::program::Module;
        for name in [
            "Pokemon White Version 2.nds",
            "Pokemon Black Version 2 .nds",
        ] {
            let mut rom = NdsRom::open(&std::path::Path::new("roms").join(name)).unwrap();
            let arm9 = rom.arm9_image().unwrap();
            let overlay = rom
                .overlay_table(Cpu::Arm9)
                .unwrap()
                .into_iter()
                .find(|entry| entry.id == 162)
                .unwrap();
            let bytes = rom.overlay_image(&overlay).unwrap();
            let mut program = Program {
                entry: arm9.entry,
                modules: vec![
                    Module {
                        id: ModuleId::Main,
                        base: arm9.load_address,
                        bytes: arm9.bytes,
                        bss_size: 0,
                        sinit: None,
                    },
                    Module {
                        id: ModuleId::Overlay(162),
                        base: overlay.ram_address,
                        bytes,
                        bss_size: 0,
                        sinit: None,
                    },
                ],
            };
            assert!(Hooks::find(&program).is_some(), "{name}");
            program.modules[1].bytes[0xb58] ^= 1;
            assert!(Hooks::find(&program).is_none());
            program.modules[1].bytes[0xb58] ^= 1;
            program.modules[1].bytes[0xed0] ^= 1;
            assert!(Hooks::find(&program).is_none());
        }
    }
}

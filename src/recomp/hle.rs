//! SDK routines the runtime implements instead of the recompiled code.
//!
//! NitroSDK's thread context switch saves the registers and later resumes
//! a thread by jumping to its saved `pc`, which generated C cannot do. The
//! routines are hand-written assembly that is identical across games built
//! with the same SDK, so they are found by instruction pattern, and codegen
//! emits `rt_hle(cpu, id)` as their body.

use super::program::Module;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Hle {
    /// `OS_SaveContext(OSContext *context)`: returns 0 when saving.
    SaveContext = 1,
    /// `OS_LoadContext(OSContext *context)`: switches to the context.
    LoadContext = 2,
    /// The thread switch at the end of the SDK's IRQ handler, reached when
    /// an interrupt made a higher-priority thread ready. r12 holds the
    /// thread info block; the IRQ stack holds the handler's saved `lr`.
    IrqThreadSwitch = 3,
}

/// (word, mask) pairs: masked bits must match; the rest are literal-pool
/// offsets that vary between builds.
const SAVE_CONTEXT: &[(u32, u32)] = &[
    (0xE92D_4001, !0),     // stmdb sp!, {r0, lr}
    (0xE280_0048, !0),     // add r0, r0, #0x48
    (0xE59F_1000, !0xFFF), // ldr r1, =CP_SaveContext
    (0xE12F_FF31, !0),     // blx r1
    (0xE8BD_4001, !0),     // ldmia sp!, {r0, lr}
    (0xE280_1000, !0),     // add r1, r0, #0
    (0xE10F_2000, !0),     // mrs r2, cpsr
    (0xE481_2004, !0),     // str r2, [r1], #4
];

const LOAD_CONTEXT: &[(u32, u32)] = &[
    (0xE92D_4001, !0),     // stmdb sp!, {r0, lr}
    (0xE280_0048, !0),     // add r0, r0, #0x48
    (0xE59F_1000, !0xFFF), // ldr r1, =CPi_RestoreContext
    (0xE12F_FF31, !0),     // blx r1
    (0xE8BD_4001, !0),     // ldmia sp!, {r0, lr}
    (0xE10F_1000, !0),     // mrs r1, cpsr
    (0xE3C1_101F, !0),     // bic r1, r1, #0x1F
    (0xE381_10D3, !0),     // orr r1, r1, #0xD3
];

const IRQ_THREAD_SWITCH: &[(u32, u32)] = &[
    (0xE3A0_1000, !0), // mov r1, #0
    (0xE1CC_10B0, !0), // strh r1, [r12]       (clear the reschedule flag)
    (0xE3A0_30D2, !0), // mov r3, #0xD2
    (0xE121_F003, !0), // msr cpsr_c, r3
    (0xE28C_2008, !0), // add r2, r12, #8      (the thread list)
];

fn matches(module: &Module, address: u32, pattern: &[(u32, u32)]) -> bool {
    pattern.iter().enumerate().all(|(index, &(word, mask))| {
        module
            .u32_at(address + 4 * index as u32)
            .is_some_and(|value| value & mask == word & mask)
    })
}

/// The runtime replacement for the ARM function at `address`, if any.
pub fn detect(module: &Module, address: u32, thumb: bool) -> Option<Hle> {
    if thumb {
        return None;
    }
    if matches(module, address, SAVE_CONTEXT) {
        Some(Hle::SaveContext)
    } else if matches(module, address, LOAD_CONTEXT) {
        Some(Hle::LoadContext)
    } else {
        None
    }
}

/// A runtime replacement that starts at an instruction inside a function.
pub fn detect_inline(module: &Module, address: u32, thumb: bool) -> Option<Hle> {
    (!thumb && matches(module, address, IRQ_THREAD_SWITCH)).then_some(Hle::IrqThreadSwitch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recomp::program::ModuleId;

    #[test]
    fn detects_context_routines_by_pattern() {
        let words: Vec<u32> = SAVE_CONTEXT
            .iter()
            .map(|&(word, _)| word | 0x38)
            .chain(LOAD_CONTEXT.iter().map(|&(word, _)| word))
            .collect();
        let module = Module {
            id: ModuleId::Main,
            base: 0x0200_0000,
            bytes: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
            bss_size: 0,
            sinit: None,
        };
        // The first pattern has a literal offset in its third word only;
        // other words are exact, so OR-ing 0x38 everywhere must not match.
        assert_eq!(detect(&module, 0x0200_0000, false), None);
        assert_eq!(detect(&module, 0x0200_0020, false), Some(Hle::LoadContext));
        assert_eq!(detect(&module, 0x0200_0020, true), None);
    }
}

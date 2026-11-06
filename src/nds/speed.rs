//! Fast-forward that speeds up the game but not its music or its menus.
//!
//! While fast-forwarding, the game runs `factor` frames for every DS frame
//! of real time, so walking, battles, text and animations all speed up.
//! Two things keep following real time instead:
//!
//! - Sound: the ARM7's driver and mixer are run for the real time that has
//!   passed rather than for the frames the game ran, so music and sound
//!   effects keep their tempo and pitch.
//! - Key repeat: GameFreak's UI library counts frames while a key is held
//!   before repeating it. Its countdown is held back on the extra frames, so
//!   holding a direction in a menu moves the cursor as often as at normal
//!   speed and a short tap does not repeat. Code that reads held keys
//!   directly, such as walking, still runs at the faster speed.

use super::io::CYCLES_PER_FRAME;
use super::memory::Memory;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// DS display refresh: 33.51 MHz / (355 * 263 * 6) ≈ 59.83 Hz.
pub const FRAME_TIME: Duration = Duration::from_nanos(16_715_646);

pub const MIN_FACTOR: u32 = 2;
pub const MAX_FACTOR: u32 = 8;
pub const DEFAULT_FACTOR: u32 = 3;

/// Whether to fast-forward and how fast, shared by the window (which sets
/// it) and the machine (which follows it).
pub struct Speed {
    fast: AtomicBool,
    factor: AtomicU32,
}

impl Default for Speed {
    fn default() -> Self {
        Self {
            fast: AtomicBool::new(false),
            factor: AtomicU32::new(DEFAULT_FACTOR),
        }
    }
}

impl Speed {
    /// Fast-forwarding at `factor` from the start (1: normal speed).
    pub fn fixed(factor: u32) -> Self {
        let speed = Self::default();
        if factor > 1 {
            speed.set_factor(factor);
            speed.fast.store(true, Ordering::Relaxed);
        }
        speed
    }

    /// Turn fast-forward on or off; returns whether it is now on.
    pub fn toggle(&self) -> bool {
        !self.fast.fetch_xor(true, Ordering::Relaxed)
    }

    pub fn is_fast(&self) -> bool {
        self.fast.load(Ordering::Relaxed)
    }

    /// The speed fast-forward uses, whether or not it is on.
    pub fn factor(&self) -> u32 {
        self.factor.load(Ordering::Relaxed)
    }

    pub fn set_factor(&self, factor: u32) {
        self.factor
            .store(factor.clamp(MIN_FACTOR, MAX_FACTOR), Ordering::Relaxed);
    }

    /// Game frames per DS frame of real time right now.
    pub fn current(&self) -> u32 {
        if self.is_fast() { self.factor() } else { 1 }
    }
}

/// One frame's share of real time, taken when the frame starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    /// Game frames per DS frame of real time.
    pub factor: u32,
    /// Real time the previous frame took, in DS frames.
    pub real_frames: f64,
    /// Sound hardware cycles that real time is worth.
    pub sound_cycles: u64,
}

impl Step {
    pub fn is_fast(&self) -> bool {
        self.factor > 1
    }
}

/// Real time across frames: paces the game and measures what each frame
/// took.
pub struct Pace {
    speed: Arc<Speed>,
    /// Sleep to hold the game to `factor` times the DS frame rate and
    /// measure real time. Without it (headless runs) nothing waits and each
    /// frame counts as 1/factor of a DS frame, so runs are repeatable.
    real_time: bool,
    next: Instant,
    last: Instant,
    /// Real time since a frame was last shown while fast-forwarding.
    since_shown: Duration,
    /// Sound cycles not yet handed out, in cycle-nanoseconds.
    cycle_remainder: u128,
}

/// Real time counted for one frame at most, so that a stall (loading, a
/// slow host) does not turn into a burst of sound or repeats.
const MAX_STEP: Duration = Duration::from_nanos(4 * 16_715_646);

impl Pace {
    pub fn new(speed: Arc<Speed>, real_time: bool) -> Self {
        let now = Instant::now();
        Self {
            speed,
            real_time,
            next: now,
            last: now,
            since_shown: Duration::ZERO,
            cycle_remainder: 0,
        }
    }

    /// Called as each frame starts.
    pub fn start(&mut self) -> Step {
        let factor = self.speed.current();
        let real = if self.real_time {
            let now = Instant::now();
            let real = (now - self.last).min(MAX_STEP);
            self.last = now;
            real
        } else {
            FRAME_TIME / factor
        };
        let sound_cycles = if factor > 1 {
            let frame = FRAME_TIME.as_nanos();
            let total = self.cycle_remainder + real.as_nanos() * u128::from(CYCLES_PER_FRAME);
            self.cycle_remainder = total % frame;
            (total / frame) as u64
        } else {
            self.cycle_remainder = 0;
            CYCLES_PER_FRAME
        };
        Step {
            factor,
            real_frames: real.as_secs_f64() / FRAME_TIME.as_secs_f64(),
            sound_cycles,
        }
    }

    /// Whether this frame should be shown. Fast-forwarding in real time,
    /// about one frame per DS frame is: the display cannot show more, and
    /// drawing fewer leaves more time for the game.
    pub fn show(&mut self, step: &Step) -> bool {
        if !step.is_fast() || !self.real_time {
            self.since_shown = Duration::ZERO;
            return true;
        }
        self.since_shown += FRAME_TIME.mul_f64(step.real_frames);
        if self.since_shown < FRAME_TIME {
            return false;
        }
        self.since_shown = (self.since_shown - FRAME_TIME).min(FRAME_TIME);
        true
    }

    /// Called as each frame ends: when the next frame is due, which is
    /// when this one is due on screen. The window times interpolation by
    /// these steady deadlines. Not paced, it is now.
    pub fn deadline(&mut self, step: &Step) -> Instant {
        let now = Instant::now();
        if !self.real_time {
            return now;
        }
        // After a long stall carry on from now instead of catching up.
        self.next += FRAME_TIME / step.factor;
        if now > self.next && now - self.next > FRAME_TIME * 6 {
            self.next = now;
        }
        self.next
    }

    /// Then wait until that deadline.
    pub fn wait(&self) {
        let now = Instant::now();
        if self.real_time && self.next > now {
            std::thread::sleep(self.next - now);
        }
    }
}

/// The repeat logic of GameFreak's key update (`GFL_UI` in Black 2 and
/// White 2): when the keys are the same as last frame and not none, the
/// countdown at +0x3C is decremented and, at zero, the held keys repeat and
/// the countdown restarts at the repeat speed (+0x40); otherwise it
/// restarts at the repeat wait (+0x44). Found by these instructions, which
/// follow the update's first 0x52 bytes.
const KEY_UPDATE_BODY: [u8; 60] = [
    0xE1, 0x68, 0x00, 0x0C, 0x41, 0x40, 0x01, 0x40, 0x21, 0x61, 0xE1, 0x68, 0x41, 0x40, 0x01,
    0x40, 0x61, 0x61, 0x00, 0x28, 0x09, 0xD0, 0xE1, 0x68, 0x81, 0x42, 0x06, 0xD1, 0xE1, 0x6B,
    0x49, 0x1E, 0xE1, 0x63, 0x04, 0xD1, 0x60, 0x61, 0x21, 0x6C, 0x00, 0xE0, 0x61, 0x6C, 0xE1,
    0x63, 0xE0, 0x60, 0x21, 0x69, 0xE1, 0x61, 0xA0, 0x61, 0x60, 0x69, 0x20, 0x62, 0x20, 0x1C,
];
const KEY_UPDATE_BODY_OFFSET: usize = 0x52;
/// `push {r3, r4, r5, lr}; adds r5, r0, #0`: the update's first two
/// instructions; r0 is the UI system.
const KEY_UPDATE_START: [u8; 4] = [0x38, 0xB5, 0x05, 0x1C];

/// In the key work: the repeat countdown.
const REPEAT_COUNTDOWN: u32 = 0x3C;
/// In the UI system: a byte counting UI updates (one per game frame).
const UPDATE_COUNT: u32 = 0x3E;

/// Holds the game's key repeat countdown to real time while
/// fast-forwarding.
#[derive(Debug, Default)]
pub struct KeyRepeat {
    /// Where the game keeps its pointer to the UI system, whose first word
    /// points to the key work. `None` when the code was not recognised.
    system_pointer: Option<u32>,
    /// Real frames not yet spent on a countdown step.
    credit: f64,
    /// What was done before the last frame, to undo if the game did not
    /// update its keys in it (it took longer than a frame).
    last: Option<Last>,
}

#[derive(Debug, Clone, Copy)]
struct Last {
    work: u32,
    /// The UI update count at the time.
    count: u8,
    /// A countdown step was held back (otherwise credit was spent).
    held: bool,
}

impl KeyRepeat {
    /// Find the key update in the ARM9 binary `code`, loaded at `base`.
    pub fn find(code: &[u8], base: u32) -> Self {
        Self {
            system_pointer: find_system_pointer(code, base),
            ..Self::default()
        }
    }

    pub fn is_found(&self) -> bool {
        self.system_pointer.is_some()
    }

    /// Called as each frame starts, before the game updates its keys.
    pub fn frame(&mut self, ram: &mut [u8], step: Option<&Step>) {
        let Some(pointer) = self.system_pointer else {
            return;
        };
        let system = read32(ram, pointer);
        let work = in_main_ram(system).then(|| read32(ram, system));
        let work = work.filter(|&work| in_main_ram(work));
        let count = ram[Memory::main_offset(system + UPDATE_COUNT)];
        // If the game did not update its keys in the last frame, a step held
        // back must not carry over, and credit spent was not used.
        if let Some(last) = self.last.take()
            && work == Some(last.work)
            && count == last.count
        {
            if last.held {
                let countdown = read32(ram, last.work + REPEAT_COUNTDOWN);
                if countdown > 1 {
                    write32(ram, last.work + REPEAT_COUNTDOWN, countdown - 1);
                }
            } else {
                self.credit += 1.0;
            }
        }
        let (Some(work), Some(step)) = (work, step.filter(|step| step.is_fast())) else {
            self.credit = 0.0;
            return;
        };
        // Let the countdown run once per DS frame of real time.
        self.credit = (self.credit + step.real_frames).min(2.0);
        if self.credit >= 1.0 - 1e-6 {
            self.credit -= 1.0;
            self.last = Some(Last {
                work,
                count,
                held: false,
            });
            return;
        }
        // Otherwise add one, which the update takes off again if the keys
        // are still held, and which is overwritten if they changed.
        let countdown = read32(ram, work + REPEAT_COUNTDOWN);
        if countdown != 0 && countdown < 0x1_0000 {
            write32(ram, work + REPEAT_COUNTDOWN, countdown + 1);
            self.last = Some(Last {
                work,
                count,
                held: true,
            });
        }
    }
}

fn in_main_ram(address: u32) -> bool {
    address >> 24 == 0x02 && address & 3 == 0
}

fn read32(ram: &[u8], address: u32) -> u32 {
    let offset = Memory::main_offset(address);
    u32::from_le_bytes(ram[offset..offset + 4].try_into().unwrap())
}

fn write32(ram: &mut [u8], address: u32, value: u32) {
    let offset = Memory::main_offset(address);
    ram[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// The address of the UI system pointer: the key update is called from
/// `push {r3, lr}; bl get_system; bl key_update; pop {r3, pc}`, and
/// `get_system` begins `push {r3, lr}; ldr r0, =pointer; ldr r0, [r0]`.
fn find_system_pointer(code: &[u8], base: u32) -> Option<u32> {
    let halfword = |address: u32| -> Option<u16> {
        let offset = address.checked_sub(base)? as usize;
        Some(u16::from_le_bytes(code.get(offset..offset + 2)?.try_into().ok()?))
    };
    let word = |address: u32| -> Option<u32> {
        let offset = address.checked_sub(base)? as usize;
        Some(u32::from_le_bytes(code.get(offset..offset + 4)?.try_into().ok()?))
    };
    // Thumb BL: two halfwords holding a 22-bit halfword offset from pc + 4.
    let bl_target = |address: u32| -> Option<u32> {
        let (high, low) = (halfword(address)?, halfword(address + 2)?);
        if high & 0xF800 != 0xF000 || low & 0xF800 != 0xF800 {
            return None;
        }
        let offset = ((i32::from(high & 0x7FF) << 21) >> 9) + (i32::from(low & 0x7FF) << 1);
        Some(address.wrapping_add(4).wrapping_add_signed(offset))
    };

    let body = code
        .windows(KEY_UPDATE_BODY.len())
        .position(|window| window == KEY_UPDATE_BODY)?;
    let start = body.checked_sub(KEY_UPDATE_BODY_OFFSET)?;
    if code.get(start..start + 4)? != KEY_UPDATE_START {
        return None;
    }
    let update = base + start as u32;
    let caller = (0..code.len().saturating_sub(12))
        .step_by(2)
        .map(|offset| base + offset as u32)
        .find(|&address| {
            halfword(address) == Some(0xB508)
                && halfword(address + 10) == Some(0xBD08)
                && bl_target(address + 6) == Some(update)
        })?;
    let get_system = bl_target(caller + 2)?;
    if halfword(get_system)? != 0xB508 || halfword(get_system + 4)? != 0x6800 {
        return None;
    }
    // ldr r0, [pc, #imm8 * 4], with pc the word-aligned address + 4.
    let load = halfword(get_system + 2)?;
    if load & 0xFF00 != 0x4800 {
        return None;
    }
    let literal = ((get_system + 2 + 4) & !3) + u32::from(load & 0xFF) * 4;
    word(literal).filter(|&pointer| in_main_ram(pointer))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = 0x0200_4000;

    /// The relevant part of White 2's code: the system getter at 0x100 with
    /// its literal at 0x114, the wrapper at 0x200 and the update at 0x300.
    fn code() -> Vec<u8> {
        let mut code = vec![0; 0x400];
        let put = |code: &mut Vec<u8>, offset: usize, bytes: &[u8]| {
            code[offset..offset + bytes.len()].copy_from_slice(bytes);
        };
        // push {r3, lr}; ldr r0, [pc, #0x10]; ldr r0, [r0]; ...; .word
        put(&mut code, 0x100, &[0x08, 0xB5, 0x04, 0x48, 0x00, 0x68]);
        put(&mut code, 0x114, &0x0214_18C4_u32.to_le_bytes());
        let bl = |from: u32, to: u32| -> [u8; 4] {
            let offset = (to as i32 - (from as i32 + 4)) >> 1;
            let high = 0xF000 | ((offset >> 11) & 0x7FF) as u16;
            let low = 0xF800 | (offset & 0x7FF) as u16;
            let [a, b] = high.to_le_bytes();
            let [c, d] = low.to_le_bytes();
            [a, b, c, d]
        };
        put(&mut code, 0x200, &[0x08, 0xB5]);
        put(&mut code, 0x202, &bl(BASE + 0x202, BASE + 0x100));
        put(&mut code, 0x206, &bl(BASE + 0x206, BASE + 0x300));
        put(&mut code, 0x20A, &[0x08, 0xBD]);
        put(&mut code, 0x300, &KEY_UPDATE_START);
        put(&mut code, 0x300 + KEY_UPDATE_BODY_OFFSET, &KEY_UPDATE_BODY);
        code
    }

    #[test]
    fn finds_the_ui_system_pointer() {
        assert_eq!(find_system_pointer(&code(), BASE), Some(0x0214_18C4));
        let mut changed = code();
        changed[0x300] = 0;
        assert_eq!(find_system_pointer(&changed, BASE), None);
    }

    const POINTER: u32 = 0x0214_18C4;
    const SYSTEM: u32 = 0x0220_0000;
    const WORK: u32 = 0x0220_1000;

    fn ram() -> Vec<u8> {
        let mut ram = vec![0; 0x40_0000];
        write32(&mut ram, POINTER, SYSTEM);
        write32(&mut ram, SYSTEM, WORK);
        ram
    }

    /// The game's update, for keys held since the last frame.
    fn update_held(ram: &mut [u8], repeats: &mut u32) {
        let count = Memory::main_offset(SYSTEM + UPDATE_COUNT);
        ram[count] = ram[count].wrapping_add(1);
        let countdown = read32(ram, WORK + REPEAT_COUNTDOWN) - 1;
        if countdown == 0 {
            *repeats += 1;
            write32(ram, WORK + REPEAT_COUNTDOWN, 4);
        } else {
            write32(ram, WORK + REPEAT_COUNTDOWN, countdown);
        }
    }

    fn step(factor: u32) -> Step {
        Step {
            factor,
            real_frames: 1.0 / f64::from(factor),
            sound_cycles: 0,
        }
    }

    #[test]
    fn repeats_follow_real_time() {
        let mut ram = ram();
        write32(&mut ram, WORK + REPEAT_COUNTDOWN, 4);
        let mut repeat = KeyRepeat {
            system_pointer: Some(POINTER),
            ..KeyRepeat::default()
        };
        // 40 DS frames of real time at 3x: 120 game frames, 10 repeats,
        // as 40 frames at normal speed would give.
        let mut repeats = 0;
        for _ in 0..120 {
            repeat.frame(&mut ram, Some(&step(3)));
            update_held(&mut ram, &mut repeats);
        }
        assert_eq!(repeats, 10);
        // At normal speed it is left alone.
        let mut repeats = 0;
        for _ in 0..40 {
            repeat.frame(&mut ram, Some(&step(1)));
            update_held(&mut ram, &mut repeats);
        }
        assert_eq!(repeats, 10);
    }

    #[test]
    fn a_frame_without_an_update_gives_the_step_back() {
        let mut ram = ram();
        write32(&mut ram, WORK + REPEAT_COUNTDOWN, 4);
        let mut repeat = KeyRepeat {
            system_pointer: Some(POINTER),
            ..KeyRepeat::default()
        };
        repeat.frame(&mut ram, Some(&step(4)));
        assert_eq!(read32(&ram, WORK + REPEAT_COUNTDOWN), 5);
        // The game did not get to its key update before the next frame.
        repeat.frame(&mut ram, Some(&step(4)));
        assert_eq!(read32(&ram, WORK + REPEAT_COUNTDOWN), 5);
        repeat.frame(&mut ram, None);
        assert_eq!(read32(&ram, WORK + REPEAT_COUNTDOWN), 4);
    }

    #[test]
    fn a_frame_without_an_update_keeps_its_credit() {
        let mut ram = ram();
        write32(&mut ram, WORK + REPEAT_COUNTDOWN, 4);
        let mut repeat = KeyRepeat {
            system_pointer: Some(POINTER),
            ..KeyRepeat::default()
        };
        let mut repeats = 0;
        // 2x: steps alternate between held back and counted.
        for frame in 0..80 {
            repeat.frame(&mut ram, Some(&step(2)));
            // The game misses its update now and then, as when it lags.
            if frame % 7 != 3 {
                update_held(&mut ram, &mut repeats);
            }
        }
        // 40 DS frames of real time, 10 repeats, as at normal speed.
        assert_eq!(repeats, 10);
    }

    #[test]
    fn nothing_happens_before_the_ui_exists() {
        let mut ram = vec![0; 0x40_0000];
        let mut repeat = KeyRepeat {
            system_pointer: Some(POINTER),
            ..KeyRepeat::default()
        };
        repeat.frame(&mut ram, Some(&step(4)));
        assert!(ram.iter().all(|&byte| byte == 0));
    }

    #[test]
    fn headless_steps_count_a_fraction_of_a_frame() {
        let mut pace = Pace::new(Arc::new(Speed::fixed(4)), false);
        let steps: Vec<Step> = (0..4).map(|_| pace.start()).collect();
        assert!(steps.iter().all(|step| step.factor == 4 && pace.show(step)));
        let cycles: u64 = steps.iter().map(|step| step.sound_cycles).sum();
        assert!(cycles.abs_diff(CYCLES_PER_FRAME) <= 1);
        let mut pace = Pace::new(Arc::new(Speed::default()), false);
        assert_eq!(pace.start().sound_cycles, CYCLES_PER_FRAME);
    }
}

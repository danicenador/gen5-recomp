//! The DS sound hardware: sixteen channels mixed at the ARM7 clock / 1024.
//!
//! Each channel plays 8-bit PCM, 16-bit PCM or IMA-ADPCM from main RAM, or
//! (channels 8-13) a square wave, or (channels 14-15) noise. A channel
//! steps through its samples at 16.76 MHz / (0x10000 - timer) and holds
//! each sample until the next one, as the hardware does.

use super::tables::{ADPCM_INDEX, ADPCM_STEPS, VOLUME_SHIFT};

/// ARM7 cycles per mixer output sample.
pub const CYCLES_PER_SAMPLE: u64 = 1024;
/// The mixer's output rate: 33.513982 MHz / 1024.
pub const OUTPUT_RATE: f64 = 33_513_982.0 / 1024.0;
/// Channel timer ticks (half the ARM7 clock) per output sample.
const TIMER_TICKS_PER_SAMPLE: u32 = 512;

const FORMAT_PCM8: u32 = 0;
const FORMAT_PCM16: u32 = 1;
const FORMAT_ADPCM: u32 = 2;
const FORMAT_PSG: u32 = 3;

const REPEAT_LOOP: u32 = 1;

/// One channel's registers and playback position.
#[derive(Clone, Copy, Default)]
pub struct Channel {
    /// SOUNDxCNT: volume, divider, hold, pan, duty, repeat, format, start.
    pub control: u32,
    /// SOUNDxSAD: the sample data address.
    pub source: u32,
    /// SOUNDxTMR, as written (0x10000 - period).
    pub timer: u16,
    /// SOUNDxPNT: the loop start, in words.
    pub loop_start: u16,
    /// SOUNDxLEN: the length after the loop start, in words.
    pub length: u32,
    /// The next sample to fetch (for ADPCM, the next nibble).
    position: u32,
    /// Timer ticks towards the next sample.
    counter: u32,
    /// The sample being output.
    current: i16,
    adpcm_sample: i32,
    adpcm_index: i32,
    /// ADPCM state saved when playback reaches the loop start.
    adpcm_loop: Option<(i32, i32)>,
    noise: u16,
}

impl Channel {
    pub fn active(&self) -> bool {
        self.control & 0x8000_0000 != 0
    }

    fn format(&self) -> u32 {
        (self.control >> 29) & 3
    }

    fn start(&mut self, index: usize, ram: &[u8]) {
        self.position = 0;
        self.counter = 0;
        self.current = 0;
        self.adpcm_loop = None;
        self.noise = 0x7FFF;
        if self.format() == FORMAT_ADPCM && index < 16 {
            let header = read32(ram, self.source);
            self.adpcm_sample = i32::from(header as u16 as i16);
            self.adpcm_index = ((header >> 16) & 0x7F).min(88) as i32;
        }
    }

    /// Samples before the loop start and in total.
    fn bounds(&self) -> (u32, u32) {
        let start_bytes = u32::from(self.loop_start) * 4;
        let total_bytes = start_bytes + (self.length & 0x3F_FFFF) * 4;
        match self.format() {
            FORMAT_PCM8 => (start_bytes, total_bytes),
            FORMAT_PCM16 => (start_bytes / 2, total_bytes / 2),
            _ => (
                start_bytes.saturating_sub(4) * 2,
                total_bytes.saturating_sub(4) * 2,
            ),
        }
    }

    /// Move to the next sample, stopping or looping at the end.
    fn step(&mut self, ram: &[u8]) {
        match self.format() {
            FORMAT_PSG => {
                self.position = (self.position + 1) & 7;
                let duty = (self.control >> 24) & 7;
                let high = duty != 7 && 7 - self.position <= duty;
                self.current = if high { 0x7FFF } else { -0x7FFF };
                return;
            }
            _ => {}
        }
        let (loop_start, total) = self.bounds();
        if self.position >= total {
            if (self.control >> 27) & 3 == REPEAT_LOOP && total > loop_start {
                self.position = loop_start;
                if let Some((sample, index)) = self.adpcm_loop {
                    self.adpcm_sample = sample;
                    self.adpcm_index = index;
                }
            } else {
                self.control &= !0x8000_0000;
                self.current = 0;
                return;
            }
        }
        self.current = match self.format() {
            FORMAT_PCM8 => (read8(ram, self.source + self.position) as i8 as i16) << 8,
            FORMAT_PCM16 => read16(ram, self.source + self.position * 2) as i16,
            _ => {
                if self.position == loop_start && self.adpcm_loop.is_none() {
                    self.adpcm_loop = Some((self.adpcm_sample, self.adpcm_index));
                }
                let byte = read8(ram, self.source + 4 + self.position / 2);
                let nibble = if self.position & 1 == 0 {
                    byte & 0xF
                } else {
                    byte >> 4
                };
                self.decode_adpcm(nibble);
                self.adpcm_sample as i16
            }
        };
        self.position += 1;
    }

    fn decode_adpcm(&mut self, nibble: u8) {
        let step = i32::from(ADPCM_STEPS[self.adpcm_index as usize]);
        let mut difference = step >> 3;
        if nibble & 1 != 0 {
            difference += step >> 2;
        }
        if nibble & 2 != 0 {
            difference += step >> 1;
        }
        if nibble & 4 != 0 {
            difference += step;
        }
        self.adpcm_sample = if nibble & 8 != 0 {
            (self.adpcm_sample - difference).max(-0x7FFF)
        } else {
            (self.adpcm_sample + difference).min(0x7FFF)
        };
        self.adpcm_index =
            (self.adpcm_index + i32::from(ADPCM_INDEX[usize::from(nibble & 7)])).clamp(0, 88);
    }

    fn step_noise(&mut self) {
        let carry = self.noise & 1;
        self.noise >>= 1;
        if carry != 0 {
            self.noise ^= 0x6000;
            self.current = -0x7FFF;
        } else {
            self.current = 0x7FFF;
        }
    }

    /// Advance by one output sample and return it, panned, as (left, right).
    fn output(&mut self, index: usize, ram: &[u8]) -> (i32, i32) {
        let period = 0x1_0000 - u32::from(self.timer);
        self.counter += TIMER_TICKS_PER_SAMPLE;
        // A timer of zero would mean a 0x10000-tick period; tiny periods
        // (ultrasonic rates) are clamped to keep this loop bounded.
        let period = period.max(16);
        while self.counter >= period && self.active() {
            self.counter -= period;
            if self.format() == FORMAT_PSG && index >= 14 {
                self.step_noise();
            } else if self.format() == FORMAT_PSG && !(8..14).contains(&index) {
                self.current = 0;
            } else {
                self.step(ram);
            }
        }
        if !self.active() {
            return (0, 0);
        }
        let volume = (self.control & 0x7F) as i32;
        let shift = VOLUME_SHIFT[((self.control >> 8) & 3) as usize];
        let sample = ((i32::from(self.current) * volume) >> 7) >> shift;
        let pan = ((self.control >> 16) & 0x7F) as i32;
        ((sample * (128 - pan)) >> 7, (sample * pan) >> 7)
    }
}

pub struct Hardware {
    pub channels: [Channel; 16],
    /// SOUNDCNT bits 0-6.
    pub master_volume: u8,
    /// SOUNDCNT bit 15.
    pub enabled: bool,
}

impl Default for Hardware {
    fn default() -> Self {
        Self {
            channels: [Channel::default(); 16],
            master_volume: 0,
            enabled: false,
        }
    }
}

impl Hardware {
    /// Write SOUNDxCNT; setting the start bit restarts the channel.
    pub fn set_control(&mut self, index: usize, value: u32, ram: &[u8]) {
        let channel = &mut self.channels[index];
        let starting = value & 0x8000_0000 != 0 && !channel.active();
        channel.control = value;
        if starting {
            channel.start(index, ram);
        }
    }

    /// Mix one output sample as signed 16-bit stereo.
    pub fn mix(&mut self, ram: &[u8]) -> [i16; 2] {
        let (mut left, mut right) = (0_i32, 0_i32);
        for (index, channel) in self.channels.iter_mut().enumerate() {
            if channel.active() {
                let (l, r) = channel.output(index, ram);
                left += l;
                right += r;
            }
        }
        if !self.enabled {
            return [0, 0];
        }
        // The hardware sums 16-bit channel outputs, applies the master
        // volume and keeps a 10-bit result: a single full-volume channel
        // reaches a quarter of full scale.
        let master = i32::from(self.master_volume);
        let scale = |value: i32| (((value * master) >> 7) >> 2).clamp(-0x8000, 0x7FFF) as i16;
        [scale(left), scale(right)]
    }
}

fn main_ram(address: u32) -> Option<usize> {
    (address >> 24 == 2).then_some((address & 0x3F_FFFF) as usize)
}

pub fn read8(ram: &[u8], address: u32) -> u8 {
    main_ram(address).map_or(0, |offset| ram[offset])
}

pub fn read16(ram: &[u8], address: u32) -> u16 {
    u16::from(read8(ram, address)) | (u16::from(read8(ram, address + 1)) << 8)
}

pub fn read32(ram: &[u8], address: u32) -> u32 {
    u32::from(read16(ram, address)) | (u32::from(read16(ram, address + 2)) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ram_with(data: &[u8]) -> Vec<u8> {
        let mut ram = vec![0; 4 * 1024 * 1024];
        ram[0x1000..0x1000 + data.len()].copy_from_slice(data);
        ram
    }

    #[test]
    fn one_shot_pcm8_plays_then_stops() {
        let ram = ram_with(&[0x40, 0x40, 0x40, 0x40]);
        let mut hardware = Hardware {
            master_volume: 127,
            enabled: true,
            ..Default::default()
        };
        let channel = &mut hardware.channels[0];
        channel.source = 0x0200_1000;
        channel.loop_start = 0;
        channel.length = 1;
        // One sample per output sample.
        channel.timer = (0x1_0000 - 512) as u16;
        // Volume 127, pan centre, one-shot, PCM8, start.
        hardware.set_control(0, 0x8000_0000 | (2 << 27) | (64 << 16) | 127, &ram);
        let first = hardware.mix(&ram);
        assert!(first[0] > 0 && first[1] > 0);
        for _ in 0..4 {
            hardware.mix(&ram);
        }
        assert!(!hardware.channels[0].active());
    }

    #[test]
    fn adpcm_decodes_and_loops() {
        // Header: sample 0, index 0; then nibbles that rise.
        let ram = ram_with(&[0, 0, 0, 0, 0x77, 0x77, 0x77, 0x77]);
        let mut hardware = Hardware {
            master_volume: 127,
            enabled: true,
            ..Default::default()
        };
        let channel = &mut hardware.channels[2];
        channel.source = 0x0200_1000;
        channel.loop_start = 1;
        channel.length = 1;
        channel.timer = (0x1_0000 - 512) as u16;
        hardware.set_control(2, 0x8000_0000 | (2 << 29) | (1 << 27) | (127 << 16) | 127, &ram);
        let samples: Vec<i16> = (0..20).map(|_| hardware.mix(&ram)[1]).collect();
        assert!(hardware.channels[2].active());
        assert!(samples[7] > samples[0]);
        // After the loop the decoder restarts from the saved state.
        assert_eq!(samples[0], samples[8]);
    }
}

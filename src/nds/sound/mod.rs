//! Sound: the ARM7's sound driver and the DS sound hardware.
//!
//! - `driver`: the NitroSDK driver, fed by the ARM9's command lists.
//! - `hardware`: the sixteen channels and the mixer.
//! - `tables`: the driver's and the BIOS's lookup tables.
//!
//! Time comes from the display: each frame is 263 lines of 2130 cycles.
//! The driver's interval timer fires every 2728 OS ticks (64 cycles each),
//! and the mixer produces one stereo sample every 1024 cycles. Samples go to
//! an `AudioQueue` that the host's audio output drains.

pub mod driver;
pub mod hardware;
mod tables;

use std::collections::VecDeque;
use std::sync::Mutex;

pub use hardware::OUTPUT_RATE;

/// ARM7 cycles between runs of the driver's main loop.
const INTERVAL_CYCLES: u64 = 2728 * 64;

/// Mixed samples waiting for the host's audio device.
#[derive(Default)]
pub struct AudioQueue {
    samples: Mutex<VecDeque<[i16; 2]>>,
}

/// Samples kept at most (about a quarter of a second); older ones are
/// dropped so a stalled output cannot build up latency.
const QUEUE_LIMIT: usize = 8192;

impl AudioQueue {
    pub fn push(&self, samples: &[[i16; 2]]) {
        let Ok(mut queue) = self.samples.lock() else {
            return;
        };
        queue.extend(samples.iter().copied());
        let excess = queue.len().saturating_sub(QUEUE_LIMIT);
        queue.drain(..excess);
    }

    /// Move up to `out.len()` samples into `out`; returns how many.
    pub fn pop(&self, out: &mut [[i16; 2]]) -> usize {
        let Ok(mut queue) = self.samples.lock() else {
            return 0;
        };
        let count = out.len().min(queue.len());
        for (slot, sample) in out.iter_mut().zip(queue.drain(..count)) {
            *slot = sample;
        }
        count
    }

    pub fn len(&self) -> usize {
        self.samples.lock().map_or(0, |queue| queue.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Default)]
pub struct Sound {
    pub driver: driver::Driver,
    /// Cycles since the driver last ran its interval.
    since_interval: u64,
    /// Cycles not yet worth a whole output sample.
    sample_remainder: u64,
    /// Samples mixed since the last push to the output.
    pending: Vec<[i16; 2]>,
}

impl Sound {
    /// Run the sound hardware and driver for `cycles` ARM7 cycles. Returns
    /// the mixed samples (also when nobody listens: notes must still end).
    pub fn advance(&mut self, ram: &mut [u8], mut cycles: u64) -> &[[i16; 2]] {
        self.pending.clear();
        while cycles > 0 {
            let to_interval = INTERVAL_CYCLES - self.since_interval;
            let to_alarm = self
                .driver
                .next_alarm()
                .map_or(u64::MAX, |due| due.saturating_sub(self.driver.now).max(1));
            let step = cycles.min(to_interval).min(to_alarm);
            self.mix(ram, step);
            cycles -= step;
            self.since_interval += step;
            self.driver.now += step;
            self.driver.fire_alarms();
            if self.since_interval == INTERVAL_CYCLES {
                self.since_interval = 0;
                self.driver.interval(ram, true);
            }
        }
        &self.pending
    }

    fn mix(&mut self, ram: &[u8], cycles: u64) {
        let total = self.sample_remainder + cycles;
        self.sample_remainder = total % hardware::CYCLES_PER_SAMPLE;
        for _ in 0..total / hardware::CYCLES_PER_SAMPLE {
            let sample = self.driver.hardware.mix(ram);
            self.pending.push(sample);
        }
    }
}

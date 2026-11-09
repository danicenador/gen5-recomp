//! Buttons and the touch screen, set by the window and read by the
//! machine. The ARM9 reads A, B, Select, Start, the D-pad, L and R from
//! KEYINPUT; X and Y come from the ARM7 through shared RAM, and touch
//! samples are left there when the ARM7 answers a sampling request.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

pub const A: u32 = 1 << 0;
pub const B: u32 = 1 << 1;
pub const SELECT: u32 = 1 << 2;
pub const START: u32 = 1 << 3;
pub const RIGHT: u32 = 1 << 4;
pub const LEFT: u32 = 1 << 5;
pub const UP: u32 = 1 << 6;
pub const DOWN: u32 = 1 << 7;
pub const R: u32 = 1 << 8;
pub const L: u32 = 1 << 9;
pub const X: u32 = 1 << 10;
pub const Y: u32 = 1 << 11;

const TOUCHING: u32 = 1 << 16;

#[derive(Default)]
pub struct Input {
    modal: AtomicBool,
    frame: AtomicU64,
    options_until: AtomicU64,
    gpu_requested: AtomicBool,
    /// Pressed buttons, using the constants above.
    buttons: AtomicU32,
    /// Bottom-screen pixel: x in bits 0-7, y in bits 8-15, bit 16 set while
    /// the pen is down.
    touch: AtomicU32,
}

impl Input {
    pub fn set_gpu_requested(&self, requested: bool) {
        self.gpu_requested.store(requested, Ordering::Relaxed);
    }
    pub fn gpu_requested(&self) -> bool {
        self.gpu_requested.load(Ordering::Relaxed)
    }
    pub fn set_modal(&self, modal: bool) {
        self.set_buttons(0);
        self.set_touch(None);
        self.modal.store(modal, Ordering::Release);
    }
    /// Wait only at a guest frame boundary, keeping the host UI responsive.
    pub fn wait_for_ui(&self) -> bool {
        let waited = self.modal.load(Ordering::Acquire);
        while self.modal.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        waited
    }
    pub fn set_frame(&self, frame: u64) {
        self.frame.store(frame, Ordering::Relaxed);
    }
    pub fn mark_native_options(&self, frame: u64) {
        self.options_until.store(frame + 2, Ordering::Relaxed);
    }
    pub fn native_options(&self) -> bool {
        let until = self.options_until.load(Ordering::Relaxed);
        until != 0 && self.frame.load(Ordering::Relaxed) <= until
    }
    pub fn set_buttons(&self, pressed: u32) {
        self.buttons.store(pressed, Ordering::Relaxed);
    }

    pub fn buttons(&self) -> u32 {
        if self.modal.load(Ordering::Acquire) {
            0
        } else {
            self.buttons.load(Ordering::Relaxed)
        }
    }

    pub fn set_touch(&self, point: Option<(u8, u8)>) {
        let value = point.map_or(0, |(x, y)| u32::from(x) | (u32::from(y) << 8) | TOUCHING);
        self.touch.store(value, Ordering::Relaxed);
    }

    fn touch(&self) -> Option<(u32, u32)> {
        if self.modal.load(Ordering::Acquire) {
            return None;
        }
        let value = self.touch.load(Ordering::Relaxed);
        (value & TOUCHING != 0).then_some((value & 0xFF, (value >> 8) & 0xFF))
    }

    /// KEYINPUT: bits clear while pressed.
    pub fn keyinput(&self) -> u16 {
        !(self.buttons() & 0x3FF) as u16 & 0x3FF
    }

    /// The ARM7's button word (`button_XY` at 0x027FFFA8), in KEYINPUT's
    /// layout so `PAD_Read` can OR the two: X (bit 10) and Y (bit 11) read 0
    /// while pressed, the debug button (bit 13) reads released and the lid
    /// (bit 15) open.
    pub fn button_xy(&self) -> u16 {
        let buttons = self.buttons();
        let mut value = 0x2C00_u16;
        if buttons & X != 0 {
            value &= !(1 << 10);
        }
        if buttons & Y != 0 {
            value &= !(1 << 11);
        }
        value
    }

    /// A touch sample (`SPITpData`): raw x in bits 0-11, raw y in 12-23,
    /// bit 24 while touching, validity 0. Raw values are pixels * 16, which
    /// the firmware image's calibration maps back to pixels.
    pub fn touch_sample(&self) -> u32 {
        match self.touch() {
            Some((x, y)) => (x << 4) | ((y << 4) << 12) | (1 << 24),
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_menu_releases_input_and_pauses_until_closed() {
        let input = std::sync::Arc::new(Input::default());
        input.set_buttons(A | DOWN);
        input.set_touch(Some((20, 30)));
        input.set_modal(true);
        assert_eq!(input.keyinput(), 0x3ff);
        assert_eq!(input.touch_sample(), 0);
        let (send, recv) = std::sync::mpsc::channel();
        let worker_input = input.clone();
        let worker = std::thread::spawn(move || {
            send.send(worker_input.wait_for_ui()).unwrap();
        });
        assert!(
            recv.recv_timeout(std::time::Duration::from_millis(20))
                .is_err()
        );
        input.set_modal(false);
        assert!(
            recv.recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
        worker.join().unwrap();
        assert_eq!(input.buttons(), 0);
        assert_eq!(input.touch_sample(), 0);
        input.set_frame(10);
        input.mark_native_options(10);
        assert!(input.native_options());
        input.set_frame(13);
        assert!(!input.native_options());
    }

    #[test]
    fn registers_are_active_low() {
        let input = Input::default();
        assert_eq!(input.keyinput(), 0x3FF);
        assert_eq!(input.button_xy(), 0x2C00);
        input.set_buttons(A | START | X);
        assert_eq!(input.keyinput(), 0x3FF & !(A | START) as u16);
        assert_eq!(input.button_xy(), 0x2800);
        input.set_touch(Some((10, 20)));
        assert_eq!(input.touch_sample(), 160 | (320 << 12) | (1 << 24));
    }
}

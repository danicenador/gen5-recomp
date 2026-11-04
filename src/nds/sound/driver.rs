//! The NitroSDK sound driver that normally runs on the ARM7.
//!
//! The ARM9 hands it linked lists of commands in main RAM. It plays
//! sequences (SSEQ) with instrument banks (SBNK) and wave archives (SWAR)
//! that the ARM9 loaded, allocates the hardware channels to notes, runs
//! their envelopes, sweeps and LFOs every 5.2 ms, and reports player and
//! channel status in the shared work area the ARM9 polls. The structure
//! and arithmetic follow the SDK's driver, so tempo, envelopes and channel
//! stealing behave as on hardware.

use super::hardware::{Hardware, read8, read16, read32};
use super::tables::{
    ATTACK_COEFFICIENTS, CHANNEL_ALLOCATION_ORDER, DECIBEL_SQUARE, LFO_SINE, VOLUME_DB_MIN,
    VOLUME_SHIFT, pitch_table, volume_table,
};

pub const PLAYER_COUNT: usize = 16;
pub const TRACK_COUNT: usize = 32;
pub const CHANNEL_COUNT: usize = 16;
const ALARM_COUNT: usize = 8;
const TRACKS_PER_PLAYER: usize = 16;
const NO_TRACK: u8 = 0xFF;
const TIMER_RATE: u16 = 240;
const MAX_CALL_DEPTH: usize = 3;

/// Where `SNDDriverInfo` says the driver's work lives on the ARM7; the
/// ARM9 only uses it to turn copied pointers back into indices.
const WORK_ADDRESS: u32 = 0x037F_8000;
const WORK_CHANNELS: u32 = 0;
const WORK_PLAYERS: u32 = 0x540;
const WORK_TRACKS: u32 = 0x780;
const WORK_ALARMS: u32 = 0xF80;
const WORK_SIZE: u32 = 0x1180;
const CHANNEL_SIZE: u32 = 0x54;
const PLAYER_SIZE: u32 = 0x24;
const TRACK_SIZE: u32 = 0x40;

/// `SNDSharedWork` field offsets.
const SHARED_FINISHED_TAG: u32 = 0;
const SHARED_PLAYER_STATUS: u32 = 4;
const SHARED_CHANNEL_STATUS: u32 = 8;
const SHARED_CAPTURE_STATUS: u32 = 0xA;
const SHARED_PLAYERS: u32 = 0x20;
const SHARED_PLAYER_SIZE: u32 = 0x24;
const SHARED_GLOBALS: u32 = 0x260;

const ENV_ATTACK: u8 = 0;
const ENV_DECAY: u8 = 1;
const ENV_SUSTAIN: u8 = 2;
const ENV_RELEASE: u8 = 3;

const KIND_PCM: u8 = 0;
const KIND_PSG: u8 = 1;
const KIND_NOISE: u8 = 2;

const SYNC_START: u8 = 1;
const SYNC_STOP: u8 = 2;
const SYNC_TIMER: u8 = 4;
const SYNC_VOLUME: u8 = 8;
const SYNC_PAN: u8 = 16;

const LFO_PITCH: u8 = 0;
const LFO_VOLUME: u8 = 1;
const LFO_PAN: u8 = 2;

const INST_PCM: u8 = 1;
const INST_PSG: u8 = 2;
const INST_NOISE: u8 = 3;
const INST_DIRECT_PCM: u8 = 4;
const INST_DUMMY: u8 = 5;
const INST_DRUM_TABLE: u8 = 16;
const INST_KEY_SPLIT: u8 = 17;

pub mod command {
    pub const START_SEQ: u32 = 0x00;
    pub const STOP_SEQ: u32 = 0x01;
    pub const PREPARE_SEQ: u32 = 0x02;
    pub const START_PREPARED_SEQ: u32 = 0x03;
    pub const PAUSE_SEQ: u32 = 0x04;
    pub const SKIP_SEQ: u32 = 0x05;
    pub const PLAYER_PARAM: u32 = 0x06;
    pub const TRACK_PARAM: u32 = 0x07;
    pub const MUTE_TRACK: u32 = 0x08;
    pub const ALLOCATABLE_CHANNEL: u32 = 0x09;
    pub const PLAYER_LOCAL_VAR: u32 = 0x0A;
    pub const PLAYER_GLOBAL_VAR: u32 = 0x0B;
    pub const START_TIMER: u32 = 0x0C;
    pub const STOP_TIMER: u32 = 0x0D;
    pub const SETUP_CHANNEL_PCM: u32 = 0x0E;
    pub const SETUP_CHANNEL_PSG: u32 = 0x0F;
    pub const SETUP_CHANNEL_NOISE: u32 = 0x10;
    pub const SETUP_CAPTURE: u32 = 0x11;
    pub const SETUP_ALARM: u32 = 0x12;
    pub const CHANNEL_TIMER: u32 = 0x13;
    pub const CHANNEL_VOLUME: u32 = 0x14;
    pub const CHANNEL_PAN: u32 = 0x15;
    pub const SURROUND_DECAY: u32 = 0x16;
    pub const MASTER_VOLUME: u32 = 0x17;
    pub const MASTER_PAN: u32 = 0x18;
    pub const OUTPUT_SELECTOR: u32 = 0x19;
    pub const LOCK_CHANNEL: u32 = 0x1A;
    pub const UNLOCK_CHANNEL: u32 = 0x1B;
    pub const STOP_UNLOCKED_CHANNEL: u32 = 0x1C;
    pub const SET_SHARED_WORK: u32 = 0x1D;
    pub const INVALIDATE_SEQ: u32 = 0x1E;
    pub const INVALIDATE_BANK: u32 = 0x1F;
    pub const INVALIDATE_WAVE: u32 = 0x20;
    pub const READ_DRIVER_INFO: u32 = 0x21;
    pub const COUNT: usize = 0x22;
}

#[derive(Clone, Copy, Default)]
struct WaveParam {
    format: u8,
    looped: u8,
    rate: u16,
    timer: u16,
    loop_start: u16,
    loop_length: u32,
}

impl WaveParam {
    fn read(ram: &[u8], address: u32) -> Self {
        Self {
            format: read8(ram, address),
            looped: read8(ram, address + 1),
            rate: read16(ram, address + 2),
            timer: read16(ram, address + 4),
            loop_start: read16(ram, address + 6),
            loop_length: read32(ram, address + 8),
        }
    }
}

#[derive(Clone, Copy)]
struct LfoParam {
    target: u8,
    speed: u8,
    depth: u8,
    range: u8,
    delay: u16,
}

impl Default for LfoParam {
    fn default() -> Self {
        Self {
            target: LFO_PITCH,
            speed: 16,
            depth: 0,
            range: 1,
            delay: 0,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Lfo {
    param: LfoParam,
    delay_counter: u16,
    counter: u16,
}

impl Lfo {
    fn start(&mut self) {
        self.counter = 0;
        self.delay_counter = 0;
    }

    fn update(&mut self) {
        if self.delay_counter < self.param.delay {
            self.delay_counter += 1;
            return;
        }
        let step = u32::from(self.param.speed) << 6;
        let mut phase = (u32::from(self.counter) + step) >> 8;
        while phase >= 0x80 {
            phase -= 0x80;
        }
        let low = (u32::from(self.counter) + step) & 0xFF;
        self.counter = (low | (phase << 8)) as u16;
    }

    fn value(&self) -> i32 {
        if self.param.depth == 0 || self.delay_counter < self.param.delay {
            return 0;
        }
        i32::from(sine(i32::from(self.counter >> 8)))
            * i32::from(self.param.depth)
            * i32::from(self.param.range)
    }
}

fn sine(x: i32) -> i8 {
    let x = x.clamp(0, 0x7F) as usize;
    if x < 0x20 {
        LFO_SINE[x]
    } else if x < 0x40 {
        LFO_SINE[0x40 - x]
    } else if x < 0x60 {
        -LFO_SINE[x - 0x40]
    } else {
        -LFO_SINE[0x20 - (x - 0x60)]
    }
}

/// A channel as the driver sees it: envelope, pitch and volume state that
/// it turns into hardware register values every tick.
#[derive(Clone, Copy, Default)]
struct ExChannel {
    id: u8,
    kind: u8,
    env_status: u8,
    active: bool,
    start: bool,
    auto_sweep: bool,
    sync: u8,
    pan_range: u8,
    root_key: u8,
    user_decay2: i16,
    midi_key: u8,
    velocity: u8,
    init_pan: i8,
    user_pan: i8,
    user_decay: i16,
    user_pitch: i16,
    env_attenuation: i32,
    sweep_counter: i32,
    sweep_length: i32,
    env_attack: u8,
    env_sustain: u8,
    env_decay: u16,
    env_release: u16,
    priority: u8,
    pan: u8,
    volume: u16,
    timer: u16,
    lfo: Lfo,
    sweep_pitch: i16,
    length: i32,
    wave: WaveParam,
    /// The sample data address, or the PSG duty cycle.
    data: u32,
    /// The track whose note this is (the SDK's channel callback).
    owner: Option<u8>,
    /// The next channel of the same track.
    next: Option<u8>,
}

#[derive(Clone, Copy, Default)]
struct Player {
    active: bool,
    prepared: bool,
    paused: bool,
    id: u8,
    priority: u8,
    volume: u8,
    ext_fader: i16,
    tracks: [u8; TRACKS_PER_PLAYER],
    tempo: u16,
    tempo_ratio: u16,
    tempo_counter: u16,
    bank: u32,
}

#[derive(Clone, Copy, Default)]
struct Track {
    active: bool,
    note_wait: bool,
    muted: bool,
    tie: bool,
    note_finish_wait: bool,
    portamento: bool,
    compare: bool,
    channel_mask_set: bool,
    pan_range: u8,
    program: u16,
    volume: u8,
    expression: u8,
    pitch_bend: i8,
    bend_range: u8,
    pan: i8,
    ext_pan: i8,
    ext_fader: i16,
    ext_pitch: i16,
    env_attack: u8,
    env_decay: u8,
    env_sustain: u8,
    env_release: u8,
    priority: u8,
    transpose: i8,
    portamento_key: u8,
    portamento_time: u8,
    sweep_pitch: i16,
    modulation: LfoParam,
    channel_mask: u16,
    wait: i32,
    base: u32,
    cur: u32,
    call_stack: [u32; MAX_CALL_DEPTH],
    loop_count: [u8; MAX_CALL_DEPTH],
    depth: u8,
    channels: Option<u8>,
}

#[derive(Clone, Copy, Default)]
struct Alarm {
    enabled: bool,
    id: u8,
    tick: u32,
    period: u32,
    /// When it fires next, in ARM7 cycles.
    due: u64,
}

#[derive(Clone, Copy)]
struct Instrument {
    kind: u8,
    wave: [u16; 2],
    root_key: u8,
    attack: u8,
    decay: u8,
    sustain: u8,
    release: u8,
    pan: u8,
}

impl Instrument {
    fn read(ram: &[u8], kind: u8, address: u32) -> Self {
        Self {
            kind,
            wave: [read16(ram, address), read16(ram, address + 2)],
            root_key: read8(ram, address + 4),
            attack: read8(ram, address + 5),
            decay: read8(ram, address + 6),
            sustain: read8(ram, address + 7),
            release: read8(ram, address + 8),
            pan: read8(ram, address + 9),
        }
    }
}

/// Sequence value encodings.
#[derive(Clone, Copy, PartialEq)]
enum Value {
    U8,
    U16,
    Variable,
    Random,
    VariableLength,
}

pub struct Driver {
    pub hardware: Hardware,
    channels: [ExChannel; CHANNEL_COUNT],
    players: [Player; PLAYER_COUNT],
    tracks: [Track; TRACK_COUNT],
    alarms: [Alarm; ALARM_COUNT],
    locked: u32,
    weak_locked: u32,
    /// `SNDSharedWork` in main RAM, once the ARM9 sends it.
    pub shared: u32,
    random: u32,
    master_pan: i32,
    original_volume: [u8; CHANNEL_COUNT],
    original_pan: [u8; CHANNEL_COUNT],
    surround_decay: i32,
    /// ARM7 time, in cycles, for alarms.
    pub now: u64,
    /// Words for the ARM9 on the sound FIFO tag (alarm notifications).
    pub outbox: Vec<u32>,
    /// How often each command was received, for reports.
    pub command_counts: [u64; command::COUNT],
    /// GEN5_SOUND=off: acknowledge commands without carrying them out.
    pub silent: bool,
}

impl Default for Driver {
    fn default() -> Self {
        let mut driver = Self {
            hardware: Hardware::default(),
            channels: [ExChannel::default(); CHANNEL_COUNT],
            players: [Player::default(); PLAYER_COUNT],
            tracks: [Track::default(); TRACK_COUNT],
            alarms: [Alarm::default(); ALARM_COUNT],
            locked: 0,
            weak_locked: 0,
            shared: 0,
            random: 0x1234_5678,
            master_pan: -1,
            original_volume: [0; CHANNEL_COUNT],
            original_pan: [0; CHANNEL_COUNT],
            surround_decay: 0,
            now: 0,
            outbox: Vec::new(),
            command_counts: [0; command::COUNT],
            silent: std::env::var("GEN5_SOUND").is_ok_and(|value| value == "off"),
        };
        for (index, channel) in driver.channels.iter_mut().enumerate() {
            channel.id = index as u8;
        }
        for (index, player) in driver.players.iter_mut().enumerate() {
            player.id = index as u8;
        }
        // The sound thread's start-up: enable output at full volume.
        driver.hardware.enabled = true;
        driver.hardware.master_volume = 0x7F;
        driver
    }
}

fn write8(ram: &mut [u8], address: u32, value: u8) {
    if address >> 24 == 2 {
        ram[(address & 0x3F_FFFF) as usize] = value;
    }
}

fn write16(ram: &mut [u8], address: u32, value: u16) {
    write8(ram, address, value as u8);
    write8(ram, address + 1, (value >> 8) as u8);
}

fn write32(ram: &mut [u8], address: u32, value: u32) {
    write16(ram, address, value as u16);
    write16(ram, address + 2, (value >> 16) as u16);
}

fn decay_coefficient(value: i32) -> u16 {
    match value {
        127 => 0xFFFF,
        126 => 0x3C00,
        0..50 => (value * 2 + 1) as u16,
        _ => (0x1E00 / (126 - value).max(1)) as u16,
    }
}

/// `SND_CalcTimer`: the timer for a sample's base timer shifted by
/// `pitch` 64ths of a semitone.
fn calc_timer(timer: u16, pitch: i32) -> u16 {
    let mut octave = 0;
    let mut normalized = -pitch;
    while normalized < 0 {
        octave -= 1;
        normalized += 768;
    }
    while normalized >= 768 {
        octave += 1;
        normalized -= 768;
    }
    let mut result = u64::from(pitch_table()[normalized as usize]) + 0x1_0000;
    result *= u64::from(timer);
    let shift = octave - 16;
    if shift <= 0 {
        result >>= -shift;
    } else if shift < 32 {
        if result & (!0_u64 << (32 - shift)) != 0 {
            return 0xFFFF;
        }
        result <<= shift;
    } else {
        return 0xFFFF;
    }
    result.clamp(0x10, 0xFFFF) as u16
}

/// `SND_CalcChannelVolume`: volume and divider for an attenuation.
fn calc_channel_volume(value: i32) -> u16 {
    let value = value.clamp(VOLUME_DB_MIN, 0);
    let volume = u16::from(volume_table()[(value - VOLUME_DB_MIN) as usize]);
    let divider = if value < -240 {
        3
    } else if value < -120 {
        2
    } else if value < -60 {
        1
    } else {
        0
    };
    volume | (divider << 8)
}

impl Driver {
    fn random(&mut self) -> u16 {
        self.random = self.random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.random >> 16) as u16
    }

    // --- Commands ------------------------------------------------------

    /// A word from the ARM9 on the sound FIFO tag: a command list address,
    /// or 0 to wake the driver.
    pub fn message(&mut self, ram: &mut [u8], data: u32) {
        if data >= 0x0200_0000 {
            self.run_commands(ram, data);
        } else if data == 0 {
            self.interval(ram, false);
        }
    }

    fn run_commands(&mut self, ram: &mut [u8], list: u32) {
        let mut address = list;
        for _ in 0..4096 {
            if address == 0 {
                break;
            }
            let id = read32(ram, address + 4);
            let arg = [
                read32(ram, address + 8),
                read32(ram, address + 12),
                read32(ram, address + 16),
                read32(ram, address + 20),
            ];
            self.command(ram, id, arg);
            address = read32(ram, address);
        }
        if self.shared != 0 {
            let tag = read32(ram, self.shared + SHARED_FINISHED_TAG);
            write32(ram, self.shared + SHARED_FINISHED_TAG, tag.wrapping_add(1));
        }
    }

    fn command(&mut self, ram: &mut [u8], id: u32, arg: [u32; 4]) {
        use command::*;
        if let Some(count) = self.command_counts.get_mut(id as usize) {
            *count += 1;
        }
        if self.silent && id != SET_SHARED_WORK {
            return;
        }
        let player = (arg[0] & 0xF) as usize;
        match id {
            START_SEQ => {
                self.prepare_seq(ram, player, arg[1], arg[2], arg[3]);
                self.players[player].prepared = true;
            }
            STOP_SEQ => self.stop_seq(ram, player),
            PREPARE_SEQ => self.prepare_seq(ram, player, arg[1], arg[2], arg[3]),
            START_PREPARED_SEQ => self.players[player].prepared = true,
            PAUSE_SEQ => self.pause_seq(player, arg[1] != 0),
            SKIP_SEQ => self.skip_seq(ram, player, arg[1]),
            PLAYER_PARAM => self.set_player_param(player, arg[1], arg[2], arg[3]),
            TRACK_PARAM => self.set_track_param(
                (arg[0] & 0xF) as usize,
                arg[1],
                arg[2],
                arg[3],
                arg[0] >> 24,
            ),
            MUTE_TRACK => {
                for track in self.player_tracks(player, arg[1]) {
                    self.track_mute(track, player, arg[2]);
                }
            }
            ALLOCATABLE_CHANNEL => {
                for track in self.player_tracks(player, arg[1]) {
                    self.tracks[track].channel_mask = arg[2] as u16;
                    self.tracks[track].channel_mask_set = true;
                }
            }
            PLAYER_LOCAL_VAR => {
                if self.shared != 0 && arg[1] < 16 {
                    let address = self.shared
                        + SHARED_PLAYERS
                        + player as u32 * SHARED_PLAYER_SIZE
                        + arg[1] * 2;
                    write16(ram, address, arg[2] as u16);
                }
            }
            PLAYER_GLOBAL_VAR => {
                if self.shared != 0 && arg[0] < 16 {
                    write16(ram, self.shared + SHARED_GLOBALS + arg[0] * 2, arg[1] as u16);
                }
            }
            START_TIMER => self.start_timer(ram, arg[0], arg[2]),
            STOP_TIMER => self.stop_timer(ram, arg[0], arg[2], arg[3]),
            SETUP_CHANNEL_PCM => self.setup_channel_pcm(
                ram,
                (arg[0] & 0xF) as usize,
                arg[1] & 0x07FF_FFFF,
                (arg[3] >> 24) & 3,
                (arg[3] >> 26) & 3,
                arg[3] & 0xFFFF,
                arg[2] & 0x3F_FFFF,
                (arg[2] >> 24) & 0x7F,
                (arg[2] >> 22) & 3,
                arg[0] >> 16,
                (arg[3] >> 16) & 0x7F,
            ),
            SETUP_CHANNEL_PSG => self.setup_channel_psg(
                ram,
                (arg[0] & 0xF) as usize,
                arg[3],
                arg[1] & 0x7F,
                (arg[1] >> 8) & 3,
                (arg[2] >> 8) & 0xFFFF,
                arg[2] & 0x7F,
            ),
            SETUP_CHANNEL_NOISE => self.setup_channel_noise(
                ram,
                (arg[0] & 0xF) as usize,
                arg[1] & 0x7F,
                (arg[1] >> 8) & 3,
                (arg[2] >> 8) & 0xFFFF,
                arg[2] & 0x7F,
            ),
            // Capture (used for reverb) is not emulated; it reports idle.
            SETUP_CAPTURE => {}
            SETUP_ALARM => {
                let alarm = &mut self.alarms[(arg[0] & 7) as usize];
                alarm.enabled = false;
                alarm.tick = arg[1];
                alarm.period = arg[2];
                alarm.id = arg[3] as u8;
            }
            CHANNEL_TIMER => {
                for channel in mask_bits(arg[0]) {
                    self.set_channel_timer(channel, arg[1]);
                }
            }
            CHANNEL_VOLUME => {
                for channel in mask_bits(arg[0]) {
                    self.set_channel_volume(channel, arg[1], arg[2]);
                }
            }
            CHANNEL_PAN => {
                for channel in mask_bits(arg[0]) {
                    self.set_channel_pan(channel, arg[1]);
                }
            }
            SURROUND_DECAY => {
                self.surround_decay = arg[0] as i32;
                for channel in 0..CHANNEL_COUNT {
                    if (1 << channel) & 0xFFF5 != 0 {
                        let pan = (self.hardware.channels[channel].control >> 16) & 0x7F;
                        let volume = self.surround(u32::from(self.original_volume[channel]), pan);
                        let control = &mut self.hardware.channels[channel].control;
                        *control = (*control & !0x7F) | volume;
                    }
                }
            }
            MASTER_VOLUME => self.hardware.master_volume = (arg[0] & 0x7F) as u8,
            MASTER_PAN => {
                self.master_pan = arg[0] as i32;
                for channel in 0..CHANNEL_COUNT {
                    let pan = if self.master_pan >= 0 {
                        arg[0] & 0x7F
                    } else {
                        u32::from(self.original_pan[channel])
                    };
                    let control = &mut self.hardware.channels[channel].control;
                    *control = (*control & !(0x7F << 16)) | (pan << 16);
                }
            }
            OUTPUT_SELECTOR => {}
            LOCK_CHANNEL => {
                for channel in mask_bits(arg[0]) {
                    if self.locked & (1 << channel) != 0 {
                        continue;
                    }
                    self.release_for_direct_use(ram, channel);
                }
                if arg[1] & 1 != 0 {
                    self.weak_locked |= arg[0];
                } else {
                    self.locked |= arg[0];
                }
            }
            UNLOCK_CHANNEL => {
                if arg[1] & 1 != 0 {
                    self.weak_locked &= !arg[0];
                } else {
                    self.locked &= !arg[0];
                }
            }
            STOP_UNLOCKED_CHANNEL => {
                for channel in mask_bits(arg[0]) {
                    if self.locked & (1 << channel) == 0 {
                        self.release_for_direct_use(ram, channel);
                    }
                }
            }
            SET_SHARED_WORK => self.shared = arg[0],
            INVALIDATE_SEQ => self.invalidate_seq(ram, arg[0], arg[1]),
            INVALIDATE_BANK => {
                for player in 0..PLAYER_COUNT {
                    let bank = self.players[player].bank;
                    if self.players[player].active && arg[0] <= bank && bank <= arg[1] {
                        self.player_stop(player);
                    }
                }
            }
            INVALIDATE_WAVE => {
                for channel in 0..CHANNEL_COUNT {
                    let state = self.channels[channel];
                    if state.active
                        && state.kind == KIND_PCM
                        && arg[0] <= state.data
                        && state.data <= arg[1]
                    {
                        self.channels[channel].start = false;
                        self.stop_channel(channel, false);
                    }
                }
            }
            READ_DRIVER_INFO => self.read_driver_info(ram, arg[0]),
            _ => {}
        }
    }

    /// Stop a channel and detach it from its note, for locking.
    fn release_for_direct_use(&mut self, _ram: &mut [u8], channel: usize) {
        if let Some(owner) = self.channels[channel].owner {
            self.channel_callback(channel, false, owner);
        }
        self.stop_channel(channel, false);
        let state = &mut self.channels[channel];
        state.priority = 0;
        state.owner = None;
        state.sync = 0;
        state.active = false;
    }

    fn start_timer(&mut self, ram: &mut [u8], channels: u32, alarms: u32) {
        for channel in mask_bits(channels) {
            let control = self.hardware.channels[channel].control | 0x8000_0000;
            self.hardware.set_control(channel, control, ram);
        }
        for index in mask_bits(alarms).filter(|index| *index < ALARM_COUNT) {
            let now = self.now;
            let alarm = &mut self.alarms[index];
            // One-shot alarms fire `tick` OS ticks (64 cycles) from now;
            // periodic ones start at `tick` and repeat every `period`.
            alarm.due = now + u64::from(alarm.tick) * 64;
            alarm.enabled = true;
        }
        self.update_shared_work(ram);
    }

    fn stop_timer(&mut self, ram: &mut [u8], channels: u32, alarms: u32, hold: u32) {
        for index in mask_bits(alarms).filter(|index| *index < ALARM_COUNT) {
            let alarm = &mut self.alarms[index];
            if alarm.enabled {
                alarm.id = alarm.id.wrapping_add(1);
                alarm.enabled = false;
            }
        }
        for channel in mask_bits(channels) {
            self.stop_channel(channel, hold & 1 != 0);
        }
        self.update_shared_work(ram);
    }

    /// The earliest enabled alarm, in ARM7 cycles.
    pub fn next_alarm(&self) -> Option<u64> {
        self.alarms
            .iter()
            .filter(|alarm| alarm.enabled)
            .map(|alarm| alarm.due)
            .min()
    }

    /// Fire the alarms that are due: each sends its index and id to the ARM9.
    pub fn fire_alarms(&mut self) {
        for (index, alarm) in self.alarms.iter_mut().enumerate() {
            if !alarm.enabled || alarm.due > self.now {
                continue;
            }
            self.outbox.push(index as u32 | (u32::from(alarm.id) << 8));
            if alarm.period == 0 {
                alarm.enabled = false;
            } else {
                alarm.due += u64::from(alarm.period) * 64;
            }
        }
    }

    fn read_driver_info(&mut self, ram: &mut [u8], address: u32) {
        for (index, channel) in self.channels.iter().enumerate() {
            let base = address + WORK_CHANNELS + index as u32 * CHANNEL_SIZE;
            serialize_channel(channel, ram, base);
        }
        for (index, player) in self.players.iter().enumerate() {
            let mut bytes = [0; PLAYER_SIZE as usize];
            player_bytes(player, &mut bytes);
            for (offset, byte) in bytes.iter().enumerate() {
                write8(
                    ram,
                    address + WORK_PLAYERS + index as u32 * PLAYER_SIZE + offset as u32,
                    *byte,
                );
            }
        }
        for (index, track) in self.tracks.iter().enumerate() {
            let mut bytes = [0; TRACK_SIZE as usize];
            track_bytes(track, &mut bytes);
            for (offset, byte) in bytes.iter().enumerate() {
                write8(
                    ram,
                    address + WORK_TRACKS + index as u32 * TRACK_SIZE + offset as u32,
                    *byte,
                );
            }
        }
        for (index, alarm) in self.alarms.iter().enumerate() {
            let base = address + WORK_ALARMS + index as u32 * 0x40;
            write8(ram, base, u8::from(alarm.enabled));
            write8(ram, base + 1, alarm.id);
            write32(ram, base + 4, alarm.tick);
            write32(ram, base + 0xC, alarm.period);
        }
        for channel in 0..CHANNEL_COUNT {
            let control = self.hardware.channels[channel].control;
            write32(ram, address + WORK_SIZE + channel as u32 * 4, control);
        }
        write32(ram, address + WORK_SIZE + 0x40, WORK_ADDRESS);
        write32(ram, address + WORK_SIZE + 0x44, self.locked);
    }

    // --- The sound thread's loop ---------------------------------------

    /// One pass of the sound thread: `step` is set for the 5.2 ms interval
    /// timer and clear when the ARM9 wakes the driver.
    pub fn interval(&mut self, ram: &mut [u8], step: bool) {
        self.update_ex_channels(ram);
        self.seq_main(ram, step);
        self.ex_channel_main(step);
        self.update_shared_work(ram);
        self.random();
    }

    fn update_shared_work(&mut self, ram: &mut [u8]) {
        if self.shared == 0 {
            return;
        }
        let mut active = 0_u16;
        for (index, channel) in self.hardware.channels.iter().enumerate() {
            if channel.active() {
                active |= 1 << index;
            }
        }
        write16(ram, self.shared + SHARED_CHANNEL_STATUS, active);
        write16(ram, self.shared + SHARED_CAPTURE_STATUS, 0);
    }

    // --- Hardware channel registers (SND_channel) ----------------------

    fn surround(&self, volume: u32, pan: u32) -> u32 {
        let (volume, pan, decay) = (volume as i64, pan as i64, i64::from(self.surround_decay));
        let result = if pan < 24 {
            (volume * (decay * (pan + 40) + ((0x7FFF - decay) << 6))) >> 21
        } else if pan <= 104 {
            volume
        } else {
            (volume * (-decay * (pan - 40) + ((decay + 0x7FFF) << 6))) >> 21
        };
        result.clamp(0, 127) as u32
    }

    /// Pan and volume after the master pan and surround settings.
    fn output_pan_volume(&mut self, channel: usize, volume: u32, pan: u32) -> (u32, u32) {
        self.original_pan[channel] = pan as u8;
        let pan = if self.master_pan >= 0 {
            (self.master_pan & 0x7F) as u32
        } else {
            pan
        };
        self.original_volume[channel] = volume as u8;
        let volume = if self.surround_decay > 0 && (1 << channel) & 0xFFF5 != 0 {
            self.surround(volume, pan)
        } else {
            volume
        };
        (volume, pan)
    }

    #[allow(clippy::too_many_arguments)]
    fn setup_channel_pcm(
        &mut self,
        ram: &[u8],
        channel: usize,
        data: u32,
        format: u32,
        repeat: u32,
        loop_start: u32,
        loop_length: u32,
        volume: u32,
        divider: u32,
        timer: u32,
        pan: u32,
    ) {
        let (volume, pan) = self.output_pan_volume(channel, volume, pan);
        let control = (format << 29) | (repeat << 27) | (pan << 16) | (divider << 8) | volume;
        self.hardware.set_control(channel, control, ram);
        let registers = &mut self.hardware.channels[channel];
        registers.timer = (0x1_0000 - timer) as u16;
        registers.loop_start = loop_start as u16;
        registers.length = loop_length;
        registers.source = data;
    }

    fn setup_channel_psg(
        &mut self,
        ram: &[u8],
        channel: usize,
        duty: u32,
        volume: u32,
        divider: u32,
        timer: u32,
        pan: u32,
    ) {
        let (volume, pan) = self.output_pan_volume(channel, volume, pan);
        let control = 0x6000_0000 | ((duty & 7) << 24) | (pan << 16) | (divider << 8) | volume;
        self.hardware.set_control(channel, control, ram);
        self.hardware.channels[channel].timer = (0x1_0000 - timer) as u16;
    }

    fn setup_channel_noise(
        &mut self,
        ram: &[u8],
        channel: usize,
        volume: u32,
        divider: u32,
        timer: u32,
        pan: u32,
    ) {
        let (volume, pan) = self.output_pan_volume(channel, volume, pan);
        let control = 0x6000_0000 | (pan << 16) | (divider << 8) | volume;
        self.hardware.set_control(channel, control, ram);
        self.hardware.channels[channel].timer = (0x1_0000 - timer) as u16;
    }

    fn stop_channel(&mut self, channel: usize, hold: bool) {
        let control = &mut self.hardware.channels[channel].control;
        *control &= !0x8000_0000;
        if hold {
            *control |= 0x8000;
        }
    }

    fn set_channel_volume(&mut self, channel: usize, volume: u32, divider: u32) {
        self.original_volume[channel] = volume as u8;
        let control = self.hardware.channels[channel].control;
        let volume = if self.surround_decay > 0 && (1 << channel) & 0xFFF5 != 0 {
            self.surround(volume, (control >> 16) & 0x7F)
        } else {
            volume
        };
        self.hardware.channels[channel].control =
            (control & !0xFFFF) | ((divider & 3) << 8) | (volume & 0x7F);
    }

    fn set_channel_timer(&mut self, channel: usize, timer: u32) {
        self.hardware.channels[channel].timer = (0x1_0000 - timer) as u16;
    }

    fn set_channel_pan(&mut self, channel: usize, pan: u32) {
        self.original_pan[channel] = pan as u8;
        let pan = if self.master_pan >= 0 {
            (self.master_pan & 0x7F) as u32
        } else {
            pan & 0x7F
        };
        let mut control = self.hardware.channels[channel].control;
        control = (control & !(0x7F << 16)) | (pan << 16);
        if self.surround_decay > 0 && (1 << channel) & 0xFFF5 != 0 {
            let volume = self.surround(u32::from(self.original_volume[channel]), pan);
            control = (control & !0x7F) | volume;
        }
        self.hardware.channels[channel].control = control;
    }

    // --- Driver channels (SND_exChannel) -------------------------------

    fn update_ex_channels(&mut self, ram: &[u8]) {
        for index in 0..CHANNEL_COUNT {
            let channel = self.channels[index];
            if channel.sync == 0 {
                continue;
            }
            if channel.sync & SYNC_STOP != 0 {
                self.stop_channel(index, false);
            }
            let volume = u32::from(channel.volume & 0xFF);
            let divider = u32::from(channel.volume >> 8);
            let (timer, pan) = (u32::from(channel.timer), u32::from(channel.pan));
            if channel.sync & SYNC_START != 0 {
                match channel.kind {
                    KIND_PCM => self.setup_channel_pcm(
                        ram,
                        index,
                        channel.data,
                        u32::from(channel.wave.format),
                        if channel.wave.looped != 0 { 1 } else { 2 },
                        u32::from(channel.wave.loop_start),
                        channel.wave.loop_length,
                        volume,
                        divider,
                        timer,
                        pan,
                    ),
                    KIND_PSG => self.setup_channel_psg(
                        ram,
                        index,
                        channel.data,
                        volume,
                        divider,
                        timer,
                        pan,
                    ),
                    _ => self.setup_channel_noise(ram, index, volume, divider, timer, pan),
                }
            } else {
                if channel.sync & SYNC_TIMER != 0 {
                    self.set_channel_timer(index, timer);
                }
                if channel.sync & SYNC_VOLUME != 0 {
                    self.set_channel_volume(index, volume, divider);
                }
                if channel.sync & SYNC_PAN != 0 {
                    self.set_channel_pan(index, pan);
                }
            }
        }
        for index in 0..CHANNEL_COUNT {
            let sync = self.channels[index].sync;
            if sync == 0 {
                continue;
            }
            if sync & SYNC_START != 0 {
                let control = self.hardware.channels[index].control | 0x8000_0000;
                self.hardware.set_control(index, control, ram);
            }
            self.channels[index].sync = 0;
        }
    }

    fn ex_channel_main(&mut self, step: bool) {
        for index in 0..CHANNEL_COUNT {
            if !self.channels[index].active {
                continue;
            }
            if self.channels[index].start {
                self.channels[index].sync |= SYNC_START;
                self.channels[index].start = false;
            } else if !self.hardware.channels[index].active() {
                self.channel_finished(index);
                continue;
            }
            let mut volume = i32::from(DECIBEL_SQUARE[usize::from(self.channels[index].velocity & 0x7F)]);
            let channel = &self.channels[index];
            let mut pitch = (i32::from(channel.midi_key) - i32::from(channel.root_key)) * 0x40;
            volume += self.update_envelope(index, step);
            pitch += self.update_sweep(index, step);
            let channel = &self.channels[index];
            volume += i32::from(channel.user_decay) + i32::from(channel.user_decay2);
            pitch += i32::from(channel.user_pitch);
            let lfo = self.update_lfo(index, step);
            let channel = &mut self.channels[index];
            let mut pan = 0;
            match channel.lfo.param.target {
                LFO_VOLUME => {
                    if volume > -0x8000 {
                        volume += lfo;
                    }
                }
                LFO_PAN => pan += lfo,
                _ => pitch += lfo,
            }
            pan += i32::from(channel.init_pan);
            if channel.pan_range != 127 {
                pan = (pan * i32::from(channel.pan_range) + 0x40) >> 7;
            }
            pan += i32::from(channel.user_pan);
            if channel.env_status == ENV_RELEASE && volume <= -723 {
                channel.sync = SYNC_STOP;
                self.channel_finished(index);
                continue;
            }
            let volume = calc_channel_volume(volume);
            let mut timer = calc_timer(channel.wave.timer, pitch);
            if channel.kind == KIND_PSG {
                timer &= 0xFFFC;
            }
            let pan = (pan + 0x40).clamp(0, 127) as u8;
            if volume != channel.volume {
                channel.volume = volume;
                channel.sync |= SYNC_VOLUME;
            }
            if timer != channel.timer {
                channel.timer = timer;
                channel.sync |= SYNC_TIMER;
            }
            if pan != channel.pan {
                channel.pan = pan;
                channel.sync |= SYNC_PAN;
            }
        }
    }

    /// A channel's note ended (its sample ran out or its release reached
    /// silence): tell its track and free it.
    fn channel_finished(&mut self, index: usize) {
        match self.channels[index].owner {
            Some(track) => self.channel_callback(index, true, track),
            None => self.channels[index].priority = 0,
        }
        self.channels[index].volume = 0;
        self.channels[index].active = false;
    }

    /// The sequence player's channel callback: `finished` when the note
    /// ended, clear when the channel is taken away.
    fn channel_callback(&mut self, channel: usize, finished: bool, track: u8) {
        if finished {
            self.channels[channel].priority = 0;
            self.free_channel(channel);
        }
        let track = &mut self.tracks[usize::from(track)];
        let id = channel as u8;
        if track.channels == Some(id) {
            track.channels = self.channels[channel].next;
            return;
        }
        let mut current = track.channels;
        while let Some(index) = current {
            let next = self.channels[usize::from(index)].next;
            if next == Some(id) {
                self.channels[usize::from(index)].next = self.channels[channel].next;
                return;
            }
            current = next;
        }
    }

    fn free_channel(&mut self, channel: usize) {
        self.channels[channel].owner = None;
    }

    fn update_envelope(&mut self, index: usize, step: bool) -> i32 {
        let channel = &mut self.channels[index];
        if step {
            match channel.env_status {
                ENV_ATTACK => {
                    channel.env_attenuation =
                        -((-channel.env_attenuation * i32::from(channel.env_attack)) >> 8);
                    if channel.env_attenuation == 0 {
                        channel.env_status = ENV_DECAY;
                    }
                }
                ENV_DECAY => {
                    let sustain =
                        i32::from(DECIBEL_SQUARE[usize::from(channel.env_sustain & 0x7F)]) << 7;
                    channel.env_attenuation -= i32::from(channel.env_decay);
                    if channel.env_attenuation <= sustain {
                        channel.env_attenuation = sustain;
                        channel.env_status = ENV_SUSTAIN;
                    }
                }
                ENV_RELEASE => channel.env_attenuation -= i32::from(channel.env_release),
                _ => {}
            }
        }
        channel.env_attenuation >> 7
    }

    fn update_sweep(&mut self, index: usize, step: bool) -> i32 {
        let channel = &mut self.channels[index];
        if channel.sweep_pitch == 0 || channel.sweep_counter >= channel.sweep_length {
            return 0;
        }
        let result = i64::from(channel.sweep_pitch)
            * i64::from(channel.sweep_length - channel.sweep_counter)
            / i64::from(channel.sweep_length);
        if step && channel.auto_sweep {
            channel.sweep_counter += 1;
        }
        result as i32
    }

    fn update_lfo(&mut self, index: usize, step: bool) -> i32 {
        let channel = &mut self.channels[index];
        let mut result = i64::from(channel.lfo.value());
        if result != 0 {
            result = match channel.lfo.param.target {
                LFO_VOLUME => result * 60,
                _ => result << 6,
            };
            result >>= 14;
        }
        if step {
            channel.lfo.update();
        }
        result as i32
    }

    fn set_attack(&mut self, index: usize, attack: u8) {
        self.channels[index].env_attack = if attack < 109 {
            255 - attack
        } else {
            ATTACK_COEFFICIENTS[usize::from(127 - attack.min(127))]
        };
    }

    fn set_decay(&mut self, index: usize, decay: u8) {
        self.channels[index].env_decay = decay_coefficient(i32::from(decay));
    }

    fn set_sustain(&mut self, index: usize, sustain: u8) {
        self.channels[index].env_sustain = sustain;
    }

    fn set_release(&mut self, index: usize, release: u8) {
        self.channels[index].env_release = decay_coefficient(i32::from(release));
    }

    fn release_channel(&mut self, index: usize) {
        self.channels[index].env_status = ENV_RELEASE;
    }

    fn volume_compare(a: &ExChannel, b: &ExChannel) -> i32 {
        let level = |channel: &ExChannel| {
            (i32::from(channel.volume & 0xFF) << 4)
                >> VOLUME_SHIFT[usize::from((channel.volume >> 8) & 3)]
        };
        match level(a).cmp(&level(b)) {
            std::cmp::Ordering::Less => 1,
            std::cmp::Ordering::Greater => -1,
            std::cmp::Ordering::Equal => 0,
        }
    }

    /// Pick a channel for a new note, stealing the lowest-priority (then
    /// quietest) one in `mask` if the new note's priority allows it.
    fn alloc_channel(
        &mut self,
        mut mask: u32,
        priority: i32,
        ignore_weak_lock: bool,
        owner: u8,
    ) -> Option<usize> {
        mask &= !self.locked;
        if !ignore_weak_lock {
            mask &= !self.weak_locked;
        }
        let mut best: Option<usize> = None;
        for &candidate in &CHANNEL_ALLOCATION_ORDER {
            let candidate = usize::from(candidate);
            if mask & (1 << candidate) == 0 {
                continue;
            }
            let Some(current) = best else {
                best = Some(candidate);
                continue;
            };
            let (chosen, channel) = (&self.channels[current], &self.channels[candidate]);
            if channel.priority > chosen.priority {
                continue;
            }
            if channel.priority != chosen.priority || Self::volume_compare(chosen, channel) < 0 {
                best = Some(candidate);
            }
        }
        let index = best?;
        if priority < i32::from(self.channels[index].priority) {
            return None;
        }
        if let Some(previous) = self.channels[index].owner {
            self.channel_callback(index, false, previous);
        }
        let channel = &mut self.channels[index];
        channel.sync = SYNC_STOP;
        channel.active = false;
        channel.next = None;
        channel.owner = Some(owner);
        channel.length = 0;
        channel.priority = priority.clamp(0, 255) as u8;
        channel.volume = 127;
        channel.start = false;
        channel.auto_sweep = true;
        channel.midi_key = 60;
        channel.root_key = 60;
        channel.velocity = 127;
        channel.init_pan = 0;
        channel.user_decay = 0;
        channel.user_decay2 = 0;
        channel.user_pitch = 0;
        channel.user_pan = 0;
        channel.pan_range = 127;
        channel.sweep_pitch = 0;
        channel.sweep_length = 0;
        channel.sweep_counter = 0;
        channel.lfo.param = LfoParam::default();
        self.set_attack(index, 127);
        self.set_decay(index, 127);
        self.set_sustain(index, 127);
        self.set_release(index, 127);
        Some(index)
    }

    fn start_channel(&mut self, index: usize, length: i32) {
        let channel = &mut self.channels[index];
        channel.env_attenuation = -92544;
        channel.env_status = ENV_ATTACK;
        channel.length = length;
        channel.lfo.start();
        channel.start = true;
        channel.active = true;
    }

    // --- Banks (SND_bank) ----------------------------------------------

    fn read_instrument(&self, ram: &[u8], bank: u32, program: i32, key: i32) -> Option<Instrument> {
        if program < 0 || bank == 0 || program as u32 >= read32(ram, bank + 0x38) {
            return None;
        }
        let offset = read32(ram, bank + 0x3C + program as u32 * 4);
        let kind = offset as u8;
        let address = bank + (offset >> 8);
        match kind {
            INST_PCM | INST_PSG | INST_NOISE | INST_DIRECT_PCM | INST_DUMMY => {
                Some(Instrument::read(ram, kind, address))
            }
            INST_DRUM_TABLE => {
                let (min, max) = (read8(ram, address), read8(ram, address + 1));
                if key < i32::from(min) || key > i32::from(max) {
                    return None;
                }
                let entry = address + 2 + (key - i32::from(min)) as u32 * 0xC;
                Some(Instrument::read(ram, read8(ram, entry), entry + 2))
            }
            INST_KEY_SPLIT => {
                let mut index = 0;
                while key > i32::from(read8(ram, address + index)) {
                    index += 1;
                    if index >= 8 {
                        return None;
                    }
                }
                let entry = address + 8 + index * 0xC;
                Some(Instrument::read(ram, read8(ram, entry), entry + 2))
            }
            _ => None,
        }
    }

    fn wave_data(ram: &[u8], bank: u32, archive: u16, wave: u16) -> Option<u32> {
        if archive >= 4 {
            return None;
        }
        let archive = read32(ram, bank + 0x18 + u32::from(archive) * 8);
        if archive == 0 || u32::from(wave) >= read32(ram, archive + 0x38) {
            return None;
        }
        let offset = read32(ram, archive + 0x3C + u32::from(wave) * 4);
        match offset {
            0 => None,
            _ if offset < 0x0200_0000 => Some(archive + offset),
            _ => Some(offset),
        }
    }

    fn note_on(
        &mut self,
        ram: &[u8],
        index: usize,
        key: i32,
        velocity: i32,
        mut length: i32,
        bank: u32,
        instrument: &Instrument,
    ) -> bool {
        let mut release = instrument.release;
        if release == 0xFF {
            length = -1;
            release = 0;
        }
        let started = match instrument.kind {
            INST_PCM | INST_DIRECT_PCM => {
                let wave = if instrument.kind == INST_PCM {
                    Self::wave_data(ram, bank, instrument.wave[1], instrument.wave[0])
                } else {
                    Some((u32::from(instrument.wave[1]) << 16) | u32::from(instrument.wave[0]))
                };
                match wave {
                    Some(wave) => {
                        let channel = &mut self.channels[index];
                        channel.kind = KIND_PCM;
                        channel.wave = WaveParam::read(ram, wave);
                        channel.data = wave + 0xC;
                        self.start_channel(index, length);
                        true
                    }
                    None => false,
                }
            }
            INST_PSG if (8..=13).contains(&index) => {
                let channel = &mut self.channels[index];
                channel.kind = KIND_PSG;
                channel.data = u32::from(instrument.wave[0]);
                channel.wave.timer = 8006;
                self.start_channel(index, length);
                true
            }
            INST_NOISE if (14..=15).contains(&index) => {
                let channel = &mut self.channels[index];
                channel.kind = KIND_NOISE;
                channel.wave.timer = 8006;
                self.start_channel(index, length);
                true
            }
            _ => false,
        };
        if !started {
            return false;
        }
        let channel = &mut self.channels[index];
        channel.midi_key = key as u8;
        channel.root_key = instrument.root_key;
        channel.velocity = velocity as u8;
        channel.init_pan = (instrument.pan as i8).wrapping_sub(0x40);
        self.set_attack(index, instrument.attack);
        self.set_decay(index, instrument.decay);
        self.set_sustain(index, instrument.sustain);
        self.set_release(index, release);
        true
    }

    // --- Sequences (SND_seq) -------------------------------------------

    fn player_tracks(&self, player: usize, mask: u32) -> Vec<usize> {
        (0..TRACKS_PER_PLAYER)
            .filter(|index| mask & (1 << index) != 0)
            .filter_map(|index| self.player_track(player, index))
            .collect()
    }

    fn player_track(&self, player: usize, index: usize) -> Option<usize> {
        let track = *self.players[player].tracks.get(index)?;
        (track != NO_TRACK).then_some(usize::from(track))
    }

    fn seq_main(&mut self, ram: &mut [u8], step: bool) {
        let mut status = 0_u32;
        for player in 0..PLAYER_COUNT {
            if !self.players[player].active {
                continue;
            }
            if self.players[player].prepared {
                if step && !self.players[player].paused {
                    self.player_seq_main(ram, player);
                }
                self.player_update_channels(player);
            }
            if self.players[player].active {
                status |= 1 << player;
            }
        }
        if self.shared != 0 {
            write32(ram, self.shared + SHARED_PLAYER_STATUS, status);
        }
    }

    fn prepare_seq(&mut self, ram: &mut [u8], player: usize, seq: u32, offset: u32, bank: u32) {
        if self.players[player].active {
            self.player_stop(player);
        }
        self.player_init(ram, player, bank);
        let Some(first) = self.allocate_track() else {
            return;
        };
        self.track_init(first);
        self.tracks[first].base = seq;
        self.tracks[first].cur = seq.wrapping_add(offset);
        self.players[player].tracks[0] = first as u8;
        if read8(ram, self.tracks[first].cur) == 0xFE {
            self.tracks[first].cur += 1;
            let mut mask = self.read_u16(ram, first) >> 1;
            let mut index = 1;
            while mask != 0 {
                if mask & 1 != 0 {
                    let Some(track) = self.allocate_track() else {
                        break;
                    };
                    self.track_init(track);
                    self.players[player].tracks[index] = track as u8;
                }
                index += 1;
                mask >>= 1;
            }
        }
        self.players[player].active = true;
        self.players[player].prepared = false;
        if self.shared != 0 {
            let status = read32(ram, self.shared + SHARED_PLAYER_STATUS);
            write32(ram, self.shared + SHARED_PLAYER_STATUS, status | (1 << player));
        }
    }

    fn stop_seq(&mut self, ram: &mut [u8], player: usize) {
        if self.players[player].active {
            self.player_stop(player);
            if self.shared != 0 {
                let status = read32(ram, self.shared + SHARED_PLAYER_STATUS);
                write32(ram, self.shared + SHARED_PLAYER_STATUS, status & !(1 << player));
            }
        }
    }

    fn pause_seq(&mut self, player: usize, pause: bool) {
        self.players[player].paused = pause;
        if pause {
            for index in 0..TRACKS_PER_PLAYER {
                if let Some(track) = self.player_track(player, index) {
                    self.track_release_channels(track, player, 127);
                    self.track_free_channels(track);
                }
            }
        }
    }

    fn skip_seq(&mut self, ram: &mut [u8], player: usize, ticks: u32) {
        for index in 0..TRACKS_PER_PLAYER {
            if let Some(track) = self.player_track(player, index) {
                self.track_release_channels(track, player, 127);
                self.track_free_channels(track);
            }
        }
        let mut done = 0;
        while done < ticks {
            if self.player_step_ticks(ram, player, false) {
                self.player_stop(player);
                break;
            }
            done += 1;
        }
        self.add_tick_counter(ram, player, done);
    }

    fn add_tick_counter(&mut self, ram: &mut [u8], player: usize, ticks: u32) {
        if self.shared == 0 {
            return;
        }
        let id = u32::from(self.players[player].id);
        let address = self.shared + SHARED_PLAYERS + id * SHARED_PLAYER_SIZE + 0x20;
        let count = read32(ram, address);
        write32(ram, address, count.wrapping_add(ticks));
    }

    fn set_player_param(&mut self, player: usize, offset: u32, data: u32, size: u32) {
        let mut bytes = [0; PLAYER_SIZE as usize];
        player_bytes(&self.players[player], &mut bytes);
        if patch(&mut bytes, offset, data, size) {
            player_from_bytes(&mut self.players[player], &bytes);
        }
    }

    fn set_track_param(&mut self, player: usize, mask: u32, offset: u32, data: u32, size: u32) {
        for track in self.player_tracks(player, mask) {
            let mut bytes = [0; TRACK_SIZE as usize];
            track_bytes(&self.tracks[track], &mut bytes);
            if patch(&mut bytes, offset, data, size) {
                track_from_bytes(&mut self.tracks[track], &bytes);
            }
        }
    }

    fn invalidate_seq(&mut self, _ram: &mut [u8], start: u32, end: u32) {
        for player in 0..PLAYER_COUNT {
            if !self.players[player].active {
                continue;
            }
            let hit = (0..TRACKS_PER_PLAYER)
                .filter_map(|index| self.player_track(player, index))
                .any(|track| (start..=end).contains(&self.tracks[track].cur));
            if hit {
                self.player_stop(player);
            }
        }
    }

    fn read_u8(&mut self, ram: &[u8], track: usize) -> u8 {
        let value = read8(ram, self.tracks[track].cur);
        self.tracks[track].cur = self.tracks[track].cur.wrapping_add(1);
        value
    }

    fn read_u16(&mut self, ram: &[u8], track: usize) -> u16 {
        let low = u16::from(self.read_u8(ram, track));
        low | (u16::from(self.read_u8(ram, track)) << 8)
    }

    fn read_u24(&mut self, ram: &[u8], track: usize) -> u32 {
        let low = u32::from(self.read_u16(ram, track));
        low | (u32::from(self.read_u8(ram, track)) << 16)
    }

    fn read_variable_length(&mut self, ram: &[u8], track: usize) -> i32 {
        let mut value = 0_i32;
        for _ in 0..5 {
            let byte = self.read_u8(ram, track);
            value = (value << 7) | i32::from(byte & 0x7F);
            if byte & 0x80 == 0 {
                break;
            }
        }
        value
    }

    fn parse_value(&mut self, ram: &[u8], track: usize, player: usize, kind: Value) -> i32 {
        match kind {
            Value::U8 => i32::from(self.read_u8(ram, track)),
            Value::U16 => i32::from(self.read_u16(ram, track)),
            Value::VariableLength => self.read_variable_length(ram, track),
            Value::Variable => {
                let variable = self.read_u8(ram, track);
                self.variable_address(player, variable)
                    .map_or(0, |address| i32::from(read16(ram, address) as i16))
            }
            Value::Random => {
                let low = i32::from(self.read_u16(ram, track) as i16);
                let high = i32::from(self.read_u16(ram, track) as i16);
                let random = i32::from(self.random());
                ((random * (high - low + 1)) >> 16) + low
            }
        }
    }

    fn variable_address(&self, player: usize, variable: u8) -> Option<u32> {
        if self.shared == 0 {
            return None;
        }
        let variable = u32::from(variable);
        if variable < 16 {
            let id = u32::from(self.players[player].id);
            Some(self.shared + SHARED_PLAYERS + id * SHARED_PLAYER_SIZE + variable * 2)
        } else if variable < 32 {
            Some(self.shared + SHARED_GLOBALS + (variable - 16) * 2)
        } else {
            None
        }
    }

    fn track_init(&mut self, track: usize) {
        let active = self.tracks[track].active;
        self.tracks[track] = Track {
            active,
            note_wait: true,
            compare: true,
            priority: 64,
            volume: 127,
            expression: 127,
            env_attack: 255,
            env_decay: 255,
            env_sustain: 255,
            env_release: 255,
            pan_range: 127,
            bend_range: 2,
            portamento_key: 60,
            channel_mask: 0xFFFF,
            ..Track::default()
        };
    }

    fn player_init(&mut self, ram: &mut [u8], player: usize, bank: u32) {
        let state = &mut self.players[player];
        state.paused = false;
        state.bank = bank;
        state.tempo = 120;
        state.tempo_ratio = 256;
        state.tempo_counter = TIMER_RATE;
        state.volume = 0x7F;
        state.ext_fader = 0;
        state.priority = 64;
        state.tracks = [NO_TRACK; TRACKS_PER_PLAYER];
        if self.shared != 0 {
            let base =
                self.shared + SHARED_PLAYERS + u32::from(state.id) * SHARED_PLAYER_SIZE;
            write32(ram, base + 0x20, 0);
            for variable in 0..16 {
                write16(ram, base + variable * 2, 0xFFFF);
            }
        }
    }

    fn allocate_track(&mut self) -> Option<usize> {
        let index = self.tracks.iter().position(|track| !track.active)?;
        self.tracks[index].active = true;
        Some(index)
    }

    fn track_release_channels(&mut self, track: usize, player: usize, release: i32) {
        self.track_update_channels(track, player, false);
        let mut current = self.tracks[track].channels;
        while let Some(index) = current {
            let index = usize::from(index);
            if self.channels[index].active {
                if release >= 0 {
                    self.set_release(index, (release & 0xFF) as u8);
                }
                self.channels[index].priority = 1;
                self.release_channel(index);
            }
            current = self.channels[index].next;
        }
    }

    fn track_free_channels(&mut self, track: usize) {
        let mut current = self.tracks[track].channels;
        while let Some(index) = current {
            self.free_channel(usize::from(index));
            current = self.channels[usize::from(index)].next;
        }
        self.tracks[track].channels = None;
    }

    fn player_seq_main(&mut self, ram: &mut [u8], player: usize) {
        let mut ticks = 0;
        while self.players[player].tempo_counter >= TIMER_RATE {
            self.players[player].tempo_counter -= TIMER_RATE;
            ticks += 1;
        }
        let mut done = 0;
        while done < ticks {
            if self.player_step_ticks(ram, player, true) {
                self.player_stop(player);
                break;
            }
            done += 1;
        }
        self.add_tick_counter(ram, player, done);
        let state = &mut self.players[player];
        let increment = (u32::from(state.tempo) * u32::from(state.tempo_ratio)) >> 8;
        state.tempo_counter = state.tempo_counter.wrapping_add(increment as u16);
    }

    fn track_stop(&mut self, track: usize, player: usize) {
        self.track_release_channels(track, player, -1);
        self.track_free_channels(track);
    }

    fn player_stop_track(&mut self, player: usize, index: usize) {
        let Some(track) = self.player_track(player, index) else {
            return;
        };
        self.track_stop(track, player);
        self.tracks[track].active = false;
        self.players[player].tracks[index] = NO_TRACK;
    }

    fn player_stop(&mut self, player: usize) {
        for index in 0..TRACKS_PER_PLAYER {
            self.player_stop_track(player, index);
        }
        self.players[player].active = false;
    }

    fn track_update_channels(&mut self, track: usize, player: usize, release: bool) {
        let state = &self.tracks[track];
        let owner = &self.players[player];
        let volume = i32::from(DECIBEL_SQUARE[usize::from(state.volume & 0x7F)])
            + i32::from(DECIBEL_SQUARE[usize::from(state.expression & 0x7F)])
            + i32::from(DECIBEL_SQUARE[usize::from(owner.volume & 0x7F)]);
        let fader = i32::from(state.ext_fader) + i32::from(owner.ext_fader);
        let pitch = ((i32::from(state.pitch_bend) * (i32::from(state.bend_range) << 6)) >> 7)
            + i32::from(state.ext_pitch);
        let mut pan = i32::from(state.pan);
        if state.pan_range != 127 {
            pan = (pan * i32::from(state.pan_range) + 0x40) >> 7;
        }
        pan += i32::from(state.ext_pan);
        let volume = volume.max(-0x8000);
        let fader = fader.max(-0x8000);
        let pan = pan.clamp(-128, 127);
        let (pan_range, modulation) = (state.pan_range, state.modulation);
        let mut current = state.channels;
        while let Some(index) = current {
            let index = usize::from(index);
            let channel = &mut self.channels[index];
            current = channel.next;
            channel.user_decay2 = fader as i16;
            if channel.env_status == ENV_RELEASE {
                continue;
            }
            channel.user_decay = volume as i16;
            channel.user_pitch = pitch as i16;
            channel.user_pan = pan as i8;
            channel.pan_range = pan_range;
            channel.lfo.param = modulation;
            if channel.length == 0 && release {
                channel.priority = 1;
                channel.env_status = ENV_RELEASE;
            }
        }
    }

    fn player_update_channels(&mut self, player: usize) {
        for index in 0..TRACKS_PER_PLAYER {
            if let Some(track) = self.player_track(player, index) {
                self.track_update_channels(track, player, true);
            }
        }
    }

    fn track_play_note(
        &mut self,
        ram: &[u8],
        track: usize,
        player: usize,
        key: i32,
        velocity: i32,
        length: i32,
    ) {
        let mut channel = None;
        if self.tracks[track].tie {
            channel = self.tracks[track].channels.map(usize::from);
            if let Some(index) = channel {
                self.channels[index].midi_key = key as u8;
                self.channels[index].velocity = velocity as u8;
            }
        }
        let index = match channel {
            Some(index) => index,
            None => {
                let bank = self.players[player].bank;
                let program = i32::from(self.tracks[track].program);
                let Some(instrument) = self.read_instrument(ram, bank, program, key) else {
                    return;
                };
                let allowed: u32 = match instrument.kind {
                    INST_PCM | INST_DIRECT_PCM => 0xFFFF,
                    INST_PSG => 0x3F00,
                    INST_NOISE => 0xC000,
                    _ => return,
                };
                let state = &self.tracks[track];
                let mask = allowed & u32::from(state.channel_mask);
                let priority =
                    i32::from(self.players[player].priority) + i32::from(state.priority);
                let tie = state.tie;
                let Some(index) =
                    self.alloc_channel(mask, priority, state.channel_mask_set, track as u8)
                else {
                    return;
                };
                let note_length = if tie { -1 } else { length };
                if !self.note_on(ram, index, key, velocity, note_length, bank, &instrument) {
                    self.channels[index].priority = 0;
                    self.free_channel(index);
                    return;
                }
                self.channels[index].next = self.tracks[track].channels;
                self.tracks[track].channels = Some(index as u8);
                index
            }
        };
        let state = self.tracks[track];
        if state.env_attack != 0xFF {
            self.set_attack(index, state.env_attack);
        }
        if state.env_decay != 0xFF {
            self.set_decay(index, state.env_decay);
        }
        if state.env_sustain != 0xFF {
            self.set_sustain(index, state.env_sustain);
        }
        if state.env_release != 0xFF {
            self.set_release(index, state.env_release);
        }
        let channel = &mut self.channels[index];
        channel.sweep_pitch = state.sweep_pitch;
        if state.portamento {
            channel.sweep_pitch = channel
                .sweep_pitch
                .wrapping_add(((i32::from(state.portamento_key) - key) << 6) as i16);
        }
        if state.portamento_time == 0 {
            channel.sweep_length = length;
            channel.auto_sweep = false;
        } else {
            let time = i32::from(state.portamento_time);
            channel.sweep_length = (time * time * i32::from(channel.sweep_pitch).abs()) >> 11;
        }
        channel.sweep_counter = 0;
    }

    /// Run one tick of a track. Returns false when the track ended.
    fn track_step_ticks(&mut self, ram: &mut [u8], track: usize, player: usize, play: bool) -> bool {
        let mut current = self.tracks[track].channels;
        while let Some(index) = current {
            let channel = &mut self.channels[usize::from(index)];
            if channel.length > 0 {
                channel.length -= 1;
            }
            if !channel.auto_sweep && channel.sweep_counter < channel.sweep_length {
                channel.sweep_counter += 1;
            }
            current = channel.next;
        }
        if self.tracks[track].note_finish_wait {
            if self.tracks[track].channels.is_some() {
                return true;
            }
            self.tracks[track].note_finish_wait = false;
        }
        if self.tracks[track].wait > 0 {
            self.tracks[track].wait -= 1;
            if self.tracks[track].wait > 0 {
                return true;
            }
        }
        // A sequence that loops without ever waiting would hang the
        // driver; give up on the track after this many commands.
        let mut budget = 100_000;
        while self.tracks[track].wait == 0 && !self.tracks[track].note_finish_wait {
            budget -= 1;
            if budget == 0 {
                return false;
            }
            let mut special = None;
            let mut run = true;
            let mut command = self.read_u8(ram, track);
            if command == 0xA2 {
                command = self.read_u8(ram, track);
                run = self.tracks[track].compare;
            }
            if command == 0xA0 {
                command = self.read_u8(ram, track);
                special = Some(Value::Random);
            }
            if command == 0xA1 {
                command = self.read_u8(ram, track);
                special = Some(Value::Variable);
            }
            if command & 0x80 == 0 {
                let velocity = i32::from(self.read_u8(ram, track));
                let length =
                    self.parse_value(ram, track, player, special.unwrap_or(Value::VariableLength));
                let key = i32::from(command) + i32::from(self.tracks[track].transpose);
                if !run {
                    continue;
                }
                let key = key.clamp(0, 127);
                if !self.tracks[track].muted && play {
                    let length = if length > 0 { length } else { -1 };
                    self.track_play_note(ram, track, player, key, velocity, length);
                }
                let state = &mut self.tracks[track];
                state.portamento_key = key as u8;
                if state.note_wait {
                    state.wait = length;
                    if length == 0 {
                        state.note_finish_wait = true;
                    }
                }
                continue;
            }
            match command & 0xF0 {
                0x80 => {
                    let value = self.parse_value(
                        ram,
                        track,
                        player,
                        special.unwrap_or(Value::VariableLength),
                    );
                    if !run {
                        continue;
                    }
                    match command {
                        0x80 => self.tracks[track].wait = value,
                        0x81 if value < 0x1_0000 => self.tracks[track].program = value as u16,
                        _ => {}
                    }
                }
                0x90 => match command {
                    0x93 => {
                        let index = usize::from(self.read_u8(ram, track));
                        let offset = self.read_u24(ram, track);
                        if !run {
                            continue;
                        }
                        if let Some(other) = self.player_track(player, index)
                            && other != track
                        {
                            self.track_stop(other, player);
                            let base = self.tracks[track].base;
                            self.tracks[other].base = base;
                            self.tracks[other].cur = base.wrapping_add(offset);
                        }
                    }
                    0x94 => {
                        let offset = self.read_u24(ram, track);
                        if run {
                            let state = &mut self.tracks[track];
                            state.cur = state.base.wrapping_add(offset);
                        }
                    }
                    0x95 => {
                        let offset = self.read_u24(ram, track);
                        let state = &mut self.tracks[track];
                        if run && usize::from(state.depth) < MAX_CALL_DEPTH {
                            state.call_stack[usize::from(state.depth)] = state.cur;
                            state.depth += 1;
                            state.cur = state.base.wrapping_add(offset);
                        }
                    }
                    _ => {}
                },
                0xC0 | 0xD0 => {
                    let value =
                        self.parse_value(ram, track, player, special.unwrap_or(Value::U8)) as u8;
                    if run {
                        self.track_byte_command(track, player, command, value);
                    }
                }
                0xE0 => {
                    let value =
                        self.parse_value(ram, track, player, special.unwrap_or(Value::U16)) as i16;
                    if !run {
                        continue;
                    }
                    match command {
                        0xE3 => self.tracks[track].sweep_pitch = value,
                        0xE1 => self.players[player].tempo = value as u16,
                        0xE0 => self.tracks[track].modulation.delay = value as u16,
                        _ => {}
                    }
                }
                0xB0 => {
                    let variable = self.read_u8(ram, track);
                    let value =
                        self.parse_value(ram, track, player, special.unwrap_or(Value::U16)) as i16;
                    if !run {
                        continue;
                    }
                    if let Some(address) = self.variable_address(player, variable) {
                        self.variable_command(ram, track, command, address, value);
                    }
                }
                0xF0 => {
                    if !run {
                        continue;
                    }
                    let state = &mut self.tracks[track];
                    match command {
                        0xFD => {
                            if state.depth != 0 {
                                state.depth -= 1;
                                state.cur = state.call_stack[usize::from(state.depth)];
                            }
                        }
                        0xFC => {
                            if state.depth == 0 {
                                continue;
                            }
                            let top = usize::from(state.depth) - 1;
                            let mut count = state.loop_count[top];
                            if count != 0 {
                                count -= 1;
                                if count == 0 {
                                    state.depth -= 1;
                                    continue;
                                }
                            }
                            state.loop_count[top] = count;
                            state.cur = state.call_stack[top];
                        }
                        0xFF => return false,
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        true
    }

    fn track_byte_command(&mut self, track: usize, player: usize, command: u8, value: u8) {
        let state = &mut self.tracks[track];
        match command {
            0xC1 => state.volume = value,
            0xD5 => state.expression = value,
            0xC2 => self.players[player].volume = value,
            0xC5 => state.bend_range = value,
            0xC6 => state.priority = value,
            0xC7 => state.note_wait = value != 0,
            0xCF => state.portamento_time = value,
            0xCA => state.modulation.depth = value,
            0xCB => state.modulation.speed = value,
            0xCC => state.modulation.target = value,
            0xCD => state.modulation.range = value,
            0xD0 => state.env_attack = value,
            0xD1 => state.env_decay = value,
            0xD2 => state.env_sustain = value,
            0xD3 => state.env_release = value,
            0xD4 => {
                if usize::from(state.depth) < MAX_CALL_DEPTH {
                    state.call_stack[usize::from(state.depth)] = state.cur;
                    state.loop_count[usize::from(state.depth)] = value;
                    state.depth += 1;
                }
            }
            0xC8 => {
                state.tie = value & 1 != 0;
                self.track_release_channels(track, player, -1);
                self.track_free_channels(track);
            }
            0xD7 => self.track_mute(track, player, u32::from(value)),
            0xC9 => {
                state.portamento_key = value.wrapping_add(state.transpose as u8);
                state.portamento = true;
            }
            0xCE => state.portamento = value & 1 != 0,
            0xC3 => state.transpose = value as i8,
            0xC4 => state.pitch_bend = value as i8,
            0xC0 => state.pan = value.wrapping_sub(0x40) as i8,
            _ => {}
        }
    }

    fn variable_command(&mut self, ram: &mut [u8], track: usize, command: u8, address: u32, value: i16) {
        let current = read16(ram, address) as i16;
        let result = match command {
            0xB0 => value,
            0xB1 => current.wrapping_add(value),
            0xB2 => current.wrapping_sub(value),
            0xB3 => current.wrapping_mul(value),
            0xB4 => {
                if value == 0 {
                    current
                } else {
                    current.wrapping_div(value)
                }
            }
            0xB5 => {
                if value >= 0 {
                    current.wrapping_shl(value as u32)
                } else {
                    current.wrapping_shr(value.unsigned_abs() as u32)
                }
            }
            0xB6 => {
                let negative = value < 0;
                let range = i32::from(value).abs();
                let random = (i32::from(self.random()) * (range + 1)) >> 16;
                (if negative { -random } else { random }) as i16
            }
            0xB8..=0xBD => {
                self.tracks[track].compare = match command {
                    0xB8 => current == value,
                    0xB9 => current >= value,
                    0xBA => current > value,
                    0xBB => current <= value,
                    0xBC => current < value,
                    _ => current != value,
                };
                return;
            }
            _ => return,
        };
        write16(ram, address, result as u16);
    }

    fn player_step_ticks(&mut self, ram: &mut [u8], player: usize, play: bool) -> bool {
        let mut playing = false;
        for index in 0..TRACKS_PER_PLAYER {
            let Some(track) = self.player_track(player, index) else {
                continue;
            };
            if self.tracks[track].cur == 0 {
                continue;
            }
            if self.track_step_ticks(ram, track, player, play) {
                playing = true;
            } else {
                self.player_stop_track(player, index);
            }
        }
        !playing
    }

    fn track_mute(&mut self, track: usize, player: usize, mode: u32) {
        match mode {
            0 => self.tracks[track].muted = false,
            1 => self.tracks[track].muted = true,
            2 => {
                self.tracks[track].muted = true;
                self.track_release_channels(track, player, -1);
            }
            3 => {
                self.tracks[track].muted = true;
                self.track_release_channels(track, player, 127);
                self.track_free_channels(track);
            }
            _ => {}
        }
    }

    /// Players with a sequence loaded, for reports.
    pub fn active_players(&self) -> u32 {
        self.players
            .iter()
            .enumerate()
            .filter(|(_, player)| player.active)
            .fold(0, |mask, (index, _)| mask | (1 << index))
    }
}

fn mask_bits(mask: u32) -> impl Iterator<Item = usize> {
    (0..CHANNEL_COUNT).filter(move |index| mask & (1 << index) != 0)
}

/// Write `size` bytes of `data` at `offset`; false if out of range.
fn patch(bytes: &mut [u8], offset: u32, data: u32, size: u32) -> bool {
    let (offset, size) = (offset as usize, size as usize);
    if !matches!(size, 1 | 2 | 4) || offset + size > bytes.len() {
        return false;
    }
    bytes[offset..offset + size].copy_from_slice(&data.to_le_bytes()[..size]);
    true
}

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn get16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn get32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap_or([0; 4]))
}

fn lfo_param_bytes(param: &LfoParam, bytes: &mut [u8]) {
    bytes[0] = param.target;
    bytes[1] = param.speed;
    bytes[2] = param.depth;
    bytes[3] = param.range;
    put16(bytes, 4, param.delay);
}

fn lfo_param_from_bytes(bytes: &[u8]) -> LfoParam {
    LfoParam {
        target: bytes[0],
        speed: bytes[1],
        depth: bytes[2],
        range: bytes[3],
        delay: get16(bytes, 4),
    }
}

/// `SNDPlayer` as the SDK lays it out.
fn player_bytes(player: &Player, bytes: &mut [u8]) {
    bytes[0] = u8::from(player.active) | (u8::from(player.prepared) << 1) | (u8::from(player.paused) << 2);
    bytes[1] = player.id;
    bytes[4] = player.priority;
    bytes[5] = player.volume;
    put16(bytes, 6, player.ext_fader as u16);
    bytes[8..0x18].copy_from_slice(&player.tracks);
    put16(bytes, 0x18, player.tempo);
    put16(bytes, 0x1A, player.tempo_ratio);
    put16(bytes, 0x1C, player.tempo_counter);
    put32(bytes, 0x20, player.bank);
}

fn player_from_bytes(player: &mut Player, bytes: &[u8]) {
    player.active = bytes[0] & 1 != 0;
    player.prepared = bytes[0] & 2 != 0;
    player.paused = bytes[0] & 4 != 0;
    player.priority = bytes[4];
    player.volume = bytes[5];
    player.ext_fader = get16(bytes, 6) as i16;
    player.tempo = get16(bytes, 0x18);
    player.tempo_ratio = get16(bytes, 0x1A);
    player.tempo_counter = get16(bytes, 0x1C);
    player.bank = get32(bytes, 0x20);
}

/// `SNDTrack` as the SDK lays it out.
fn track_bytes(track: &Track, bytes: &mut [u8]) {
    let flags = [
        track.active,
        track.note_wait,
        track.muted,
        track.tie,
        track.note_finish_wait,
        track.portamento,
        track.compare,
        track.channel_mask_set,
    ];
    bytes[0] = flags
        .iter()
        .enumerate()
        .fold(0, |byte, (bit, set)| byte | (u8::from(*set) << bit));
    bytes[1] = track.pan_range;
    put16(bytes, 2, track.program);
    bytes[4] = track.volume;
    bytes[5] = track.expression;
    bytes[6] = track.pitch_bend as u8;
    bytes[7] = track.bend_range;
    bytes[8] = track.pan as u8;
    bytes[9] = track.ext_pan as u8;
    put16(bytes, 0xA, track.ext_fader as u16);
    put16(bytes, 0xC, track.ext_pitch as u16);
    bytes[0xE] = track.env_attack;
    bytes[0xF] = track.env_decay;
    bytes[0x10] = track.env_sustain;
    bytes[0x11] = track.env_release;
    bytes[0x12] = track.priority;
    bytes[0x13] = track.transpose as u8;
    bytes[0x14] = track.portamento_key;
    bytes[0x15] = track.portamento_time;
    put16(bytes, 0x16, track.sweep_pitch as u16);
    lfo_param_bytes(&track.modulation, &mut bytes[0x18..0x1E]);
    put16(bytes, 0x1E, track.channel_mask);
    put32(bytes, 0x20, track.wait as u32);
    put32(bytes, 0x24, track.base);
    put32(bytes, 0x28, track.cur);
    for (index, address) in track.call_stack.iter().enumerate() {
        put32(bytes, 0x2C + index * 4, *address);
    }
    bytes[0x38..0x3B].copy_from_slice(&track.loop_count);
    bytes[0x3B] = track.depth;
    let head = track
        .channels
        .map_or(0, |index| WORK_ADDRESS + WORK_CHANNELS + u32::from(index) * CHANNEL_SIZE);
    put32(bytes, 0x3C, head);
}

fn track_from_bytes(track: &mut Track, bytes: &[u8]) {
    let flag = |bit: u8| bytes[0] & (1 << bit) != 0;
    track.active = flag(0);
    track.note_wait = flag(1);
    track.muted = flag(2);
    track.tie = flag(3);
    track.note_finish_wait = flag(4);
    track.portamento = flag(5);
    track.compare = flag(6);
    track.channel_mask_set = flag(7);
    track.pan_range = bytes[1];
    track.program = get16(bytes, 2);
    track.volume = bytes[4];
    track.expression = bytes[5];
    track.pitch_bend = bytes[6] as i8;
    track.bend_range = bytes[7];
    track.pan = bytes[8] as i8;
    track.ext_pan = bytes[9] as i8;
    track.ext_fader = get16(bytes, 0xA) as i16;
    track.ext_pitch = get16(bytes, 0xC) as i16;
    track.env_attack = bytes[0xE];
    track.env_decay = bytes[0xF];
    track.env_sustain = bytes[0x10];
    track.env_release = bytes[0x11];
    track.priority = bytes[0x12];
    track.transpose = bytes[0x13] as i8;
    track.portamento_key = bytes[0x14];
    track.portamento_time = bytes[0x15];
    track.sweep_pitch = get16(bytes, 0x16) as i16;
    track.modulation = lfo_param_from_bytes(&bytes[0x18..0x1E]);
    track.channel_mask = get16(bytes, 0x1E);
    track.wait = get32(bytes, 0x20) as i32;
    track.base = get32(bytes, 0x24);
    track.cur = get32(bytes, 0x28);
}

/// `SNDExChannel` as the SDK lays it out, for `SND_ReadDriverInfo`.
fn serialize_channel(channel: &ExChannel, ram: &mut [u8], base: u32) {
    let mut bytes = [0_u8; CHANNEL_SIZE as usize];
    bytes[0] = channel.id;
    bytes[1] = channel.kind;
    bytes[2] = channel.env_status;
    bytes[3] = u8::from(channel.active)
        | (u8::from(channel.start) << 1)
        | (u8::from(channel.auto_sweep) << 2)
        | (channel.sync << 3);
    bytes[4] = channel.pan_range;
    bytes[5] = channel.root_key;
    put16(&mut bytes, 6, channel.user_decay2 as u16);
    bytes[8] = channel.midi_key;
    bytes[9] = channel.velocity;
    bytes[0xA] = channel.init_pan as u8;
    bytes[0xB] = channel.user_pan as u8;
    put16(&mut bytes, 0xC, channel.user_decay as u16);
    put16(&mut bytes, 0xE, channel.user_pitch as u16);
    put32(&mut bytes, 0x10, channel.env_attenuation as u32);
    put32(&mut bytes, 0x14, channel.sweep_counter as u32);
    put32(&mut bytes, 0x18, channel.sweep_length as u32);
    bytes[0x1C] = channel.env_attack;
    bytes[0x1D] = channel.env_sustain;
    put16(&mut bytes, 0x1E, channel.env_decay);
    put16(&mut bytes, 0x20, channel.env_release);
    bytes[0x22] = channel.priority;
    bytes[0x23] = channel.pan;
    put16(&mut bytes, 0x24, channel.volume);
    put16(&mut bytes, 0x26, channel.timer);
    lfo_param_bytes(&channel.lfo.param, &mut bytes[0x28..0x2E]);
    put16(&mut bytes, 0x2E, channel.lfo.delay_counter);
    put16(&mut bytes, 0x30, channel.lfo.counter);
    put16(&mut bytes, 0x32, channel.sweep_pitch as u16);
    put32(&mut bytes, 0x34, channel.length as u32);
    bytes[0x38] = channel.wave.format;
    bytes[0x39] = channel.wave.looped;
    put16(&mut bytes, 0x3A, channel.wave.rate);
    put16(&mut bytes, 0x3C, channel.wave.timer);
    put16(&mut bytes, 0x3E, channel.wave.loop_start);
    put32(&mut bytes, 0x40, channel.wave.loop_length);
    put32(&mut bytes, 0x44, channel.data);
    if let Some(owner) = channel.owner {
        // A non-null callback, and the owning track as its argument.
        put32(&mut bytes, 0x48, WORK_ADDRESS + WORK_SIZE);
        put32(
            &mut bytes,
            0x4C,
            WORK_ADDRESS + WORK_TRACKS + u32::from(owner) * TRACK_SIZE,
        );
    }
    let next = channel
        .next
        .map_or(0, |index| WORK_ADDRESS + WORK_CHANNELS + u32::from(index) * CHANNEL_SIZE);
    put32(&mut bytes, 0x50, next);
    for (offset, byte) in bytes.iter().enumerate() {
        write8(ram, base + offset as u32, *byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: usize = 4 * 1024 * 1024;

    fn put(ram: &mut [u8], address: u32, bytes: &[u8]) {
        let offset = (address & 0x3F_FFFF) as usize;
        ram[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    /// A bank with one PCM instrument whose wave is a short looped PCM8
    /// square, and a sequence that plays one note and ends.
    fn setup() -> (Vec<u8>, Driver) {
        let mut ram = vec![0; RAM];
        let (shared, bank, archive, sequence, list): (u32, u32, u32, u32, u32) =
            (0x0210_0000, 0x0211_0000, 0x0212_0000, 0x0213_0000, 0x0214_0000);
        // Bank: archive link 0, one instrument (PCM at +0x40).
        put(&mut ram, bank + 0x18, &archive.to_le_bytes());
        put(&mut ram, bank + 0x38, &1_u32.to_le_bytes());
        put(&mut ram, bank + 0x3C, &((0x40 << 8) | u32::from(INST_PCM)).to_le_bytes());
        // wave 0 of archive 0, root key 60, attack 127, decay 127,
        // sustain 127, release 100, pan 64.
        put(&mut ram, bank + 0x40, &[0, 0, 0, 0, 60, 127, 127, 127, 100, 64]);
        // Archive: one wave at +0x40.
        put(&mut ram, archive + 0x38, &1_u32.to_le_bytes());
        put(&mut ram, archive + 0x3C, &0x40_u32.to_le_bytes());
        // Wave: PCM8, looped, 32 kHz-ish timer, loop start 0, length 1 word.
        put(&mut ram, archive + 0x40, &[0, 1, 0, 0x80, 0x00, 0xFC, 0, 0, 1, 0, 0, 0]);
        put(&mut ram, archive + 0x4C, &[0x60, 0x60, 0xA0, 0xA0]);
        // Sequence: note 60 velocity 127 for 48 ticks, wait 48, end.
        put(&mut ram, sequence, &[60, 127, 48, 0x80, 48, 0xFF]);
        let mut driver = Driver::default();
        driver.command(&mut ram, command::SET_SHARED_WORK, [shared, 0, 0, 0]);
        // A command list: start sequence on player 0.
        put(&mut ram, list, &0_u32.to_le_bytes());
        put(&mut ram, list + 4, &command::START_SEQ.to_le_bytes());
        put(&mut ram, list + 8, &0_u32.to_le_bytes());
        put(&mut ram, list + 12, &sequence.to_le_bytes());
        put(&mut ram, list + 16, &0_u32.to_le_bytes());
        put(&mut ram, list + 20, &bank.to_le_bytes());
        driver.message(&mut ram, list);
        (ram, driver)
    }

    #[test]
    fn sequence_plays_a_note_then_ends() {
        let (mut ram, mut driver) = setup();
        let shared = driver.shared;
        assert_eq!(read32(&ram, shared), 1, "the command list was counted");
        assert_eq!(read32(&ram, shared + 4), 1, "player 0 is active");
        let mut heard = false;
        // The note's release (rate 100) takes about 310 intervals after the
        // sequence ends at about 200.
        for _ in 0..700 {
            driver.interval(&mut ram, true);
            for _ in 0..170 {
                let [left, right] = driver.hardware.mix(&ram);
                heard |= left != 0 || right != 0;
            }
        }
        assert!(heard, "the note reached the mixer");
        assert_eq!(read32(&ram, shared + 4), 0, "the sequence finished");
        assert_eq!(driver.hardware.channels.iter().filter(|c| c.active()).count(), 0);
        assert!(read32(&ram, shared + 0x20 + 0x20) >= 96, "ticks were counted");
    }

    #[test]
    fn timer_follows_pitch() {
        assert_eq!(calc_timer(0x1000, 0), 0x1000);
        // An octave up halves the timer period.
        assert_eq!(calc_timer(0x1000, 768), 0x800);
        assert_eq!(calc_timer(0x1000, -768), 0x2000);
    }

    #[test]
    fn track_params_patch_by_offset() {
        let (mut ram, mut driver) = setup();
        let _ = &mut ram;
        // extFader (offset 0x0A, s16) of track 0.
        driver.set_track_param(0, 1, 0x0A, (-100_i16) as u16 as u32, 2);
        let track = driver.player_track(0, 0).unwrap();
        assert_eq!(driver.tracks[track].ext_fader, -100);
        driver.set_player_param(0, 0x1A, 512, 2);
        assert_eq!(driver.players[0].tempo_ratio, 512);
    }
}

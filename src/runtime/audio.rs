//! Play the game's mixed sound on the host's default output device.
//!
//! The DS mixes at 32.73 kHz; the device usually runs at 44.1 or 48 kHz.
//! The callback resamples linearly and nudges its rate to keep the queue
//! near a small target, so the game's frame pacing and the device clock
//! can drift apart without gaps or growing latency.

use crate::nds::sound::{AudioQueue, OUTPUT_RATE};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;

/// Samples the queue should hold (about 50 ms) for steady playback.
const TARGET_FILL: f64 = 1600.0;

pub struct Output {
    _stream: cpal::Stream,
}

/// Open the default output device; `None` (with a message) when there is
/// no usable device, in which case the game simply runs silently.
pub fn start(queue: Arc<AudioQueue>) -> Option<Output> {
    match open(queue) {
        Ok(stream) => Some(Output { _stream: stream }),
        Err(error) => {
            eprintln!("audio disabled: {error}");
            None
        }
    }
}

fn open(queue: Arc<AudioQueue>) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no default output device")?;
    let supported = device
        .default_output_config()
        .map_err(|error| error.to_string())?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = usize::from(config.channels);
    let mut resampler = Resampler::new(queue, f64::from(config.sample_rate));
    let error = |error| eprintln!("audio stream error: {error}");
    let stream = match format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    let [left, right] = resampler.next();
                    write_frame(frame, left, right);
                }
            },
            error,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    let [left, right] = resampler.next();
                    let convert = |value: f32| (value * 32767.0) as i16;
                    write_frame(frame, convert(left), convert(right));
                }
            },
            error,
            None,
        ),
        other => return Err(format!("unsupported sample format {other}")),
    }
    .map_err(|error| error.to_string())?;
    stream.play().map_err(|error| error.to_string())?;
    Ok(stream)
}

fn write_frame<T: Copy>(frame: &mut [T], left: T, right: T) {
    match frame {
        [only] => *only = left,
        [first, second, rest @ ..] => {
            *first = left;
            *second = right;
            for sample in rest {
                *sample = left;
            }
        }
        [] => {}
    }
}

struct Resampler {
    queue: Arc<AudioQueue>,
    /// Source samples per device sample at nominal speed.
    ratio: f64,
    /// Position between `previous` and `current`.
    phase: f64,
    previous: [f32; 2],
    current: [f32; 2],
    buffer: Vec<[i16; 2]>,
    read: usize,
    /// Device samples until the rate is adjusted again.
    countdown: u32,
    adjust: f64,
}

impl Resampler {
    fn new(queue: Arc<AudioQueue>, device_rate: f64) -> Self {
        Self {
            queue,
            ratio: OUTPUT_RATE / device_rate,
            phase: 0.0,
            previous: [0.0; 2],
            current: [0.0; 2],
            buffer: Vec::new(),
            read: 0,
            countdown: 0,
            adjust: 1.0,
        }
    }

    fn fetch(&mut self) -> [f32; 2] {
        if self.read == self.buffer.len() {
            self.buffer.resize(512, [0; 2]);
            let count = self.queue.pop(&mut self.buffer);
            self.buffer.truncate(count);
            self.read = 0;
            if count == 0 {
                // Starved: fade towards silence rather than click.
                return self.current.map(|value| value * 0.995);
            }
        }
        let [left, right] = self.buffer[self.read];
        self.read += 1;
        [f32::from(left) / 32768.0, f32::from(right) / 32768.0]
    }

    fn next(&mut self) -> [f32; 2] {
        if self.countdown == 0 {
            // Speed up slightly when the queue runs full, slow down when
            // it runs low (at most 0.5%, which is not audible as pitch).
            self.countdown = 1024;
            let fill = (self.queue.len() + self.buffer.len() - self.read) as f64;
            let error = (fill - TARGET_FILL) / TARGET_FILL;
            self.adjust = 1.0 + (error * 0.005).clamp(-0.005, 0.005);
            if fill > TARGET_FILL * 4.0 {
                // Far behind (after a stall): drop the backlog.
                let mut skip = vec![[0; 2]; (fill - TARGET_FILL) as usize];
                self.queue.pop(&mut skip);
            }
        }
        self.countdown -= 1;
        self.phase += self.ratio * self.adjust;
        while self.phase >= 1.0 {
            self.phase -= 1.0;
            self.previous = self.current;
            self.current = self.fetch();
        }
        let t = self.phase as f32;
        [
            self.previous[0] + (self.current[0] - self.previous[0]) * t,
            self.previous[1] + (self.current[1] - self.previous[1]) * t,
        ]
    }
}

/// Write mixed samples to a 16-bit stereo WAV file as they arrive.
pub struct WavWriter {
    file: std::fs::File,
    bytes: u32,
}

impl WavWriter {
    pub fn create(path: &std::path::Path) -> std::io::Result<Self> {
        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        let rate = OUTPUT_RATE.round() as u32;
        let mut header = Vec::with_capacity(44);
        header.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        header.extend_from_slice(&16_u32.to_le_bytes());
        header.extend_from_slice(&1_u16.to_le_bytes());
        header.extend_from_slice(&2_u16.to_le_bytes());
        header.extend_from_slice(&rate.to_le_bytes());
        header.extend_from_slice(&(rate * 4).to_le_bytes());
        header.extend_from_slice(&4_u16.to_le_bytes());
        header.extend_from_slice(&16_u16.to_le_bytes());
        header.extend_from_slice(b"data\0\0\0\0");
        file.write_all(&header)?;
        Ok(Self { file, bytes: 0 })
    }

    /// Append everything waiting in `queue` and keep the header current,
    /// so the file is valid even if the run stops abruptly.
    pub fn drain(&mut self, queue: &AudioQueue) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let mut samples = vec![[0_i16; 2]; queue.len()];
        let count = queue.pop(&mut samples);
        if count == 0 {
            return Ok(());
        }
        let mut data = Vec::with_capacity(count * 4);
        for [left, right] in &samples[..count] {
            data.extend_from_slice(&left.to_le_bytes());
            data.extend_from_slice(&right.to_le_bytes());
        }
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&data)?;
        self.bytes += data.len() as u32;
        self.file.seek(SeekFrom::Start(4))?;
        self.file.write_all(&(36 + self.bytes).to_le_bytes())?;
        self.file.seek(SeekFrom::Start(40))?;
        self.file.write_all(&self.bytes.to_le_bytes())?;
        Ok(())
    }
}

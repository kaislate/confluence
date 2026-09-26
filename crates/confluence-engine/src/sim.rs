//! Simulated devices and a simulated-time driver, so the whole engine can run
//! headless and deterministically (CI, soak tests).

use std::f64::consts::TAU;

use confluence_core::bridge::{InputDeviceSide, OutputDeviceSide};

use crate::audio::AudioEngine;
use crate::engine::MasterChannels;

/// A capture device with its own (drifting) clock producing a sine on channel 0
/// and its negation on channel 1 (other channels silent).
pub struct SimInput {
    side: InputDeviceSide,
    rate: f64,
    block: usize,
    channels: usize,
    tone_hz: f64,
    amplitude: f64,
    frames: u64,
    buf: Vec<f32>,
}

impl SimInput {
    /// `true_rate` is the device's actual rate (nominal × (1 + ppm·1e-6)).
    pub fn new(
        side: InputDeviceSide,
        channels: usize,
        true_rate: f64,
        block: usize,
        tone_hz: f64,
        amplitude: f64,
    ) -> Self {
        Self { side, rate: true_rate, block, channels, tone_hz, amplitude, frames: 0, buf: vec![0.0; block * channels] }
    }

    fn next_time(&self) -> f64 {
        (self.frames + self.block as u64) as f64 / self.rate
    }

    fn run(&mut self, time: f64) {
        for n in 0..self.block {
            let s = (self.amplitude * (TAU * self.tone_hz * (self.frames + n as u64) as f64 / self.rate).sin()) as f32;
            let frame = &mut self.buf[n * self.channels..(n + 1) * self.channels];
            frame.fill(0.0);
            frame[0] = s;
            if self.channels > 1 {
                frame[1] = -s;
            }
        }
        self.side.write_interleaved(&self.buf, time);
        self.frames += self.block as u64;
    }
}

/// A playback device with its own clock that records one channel.
pub struct SimOutput {
    side: OutputDeviceSide,
    rate: f64,
    block: usize,
    channels: usize,
    record_channel: usize,
    frames: u64,
    buf: Vec<f32>,
    /// Recorded samples of `record_channel`, with the device time of the first one.
    pub recording: Vec<f32>,
    pub record_from: f64,
}

impl SimOutput {
    pub fn new(
        side: OutputDeviceSide,
        channels: usize,
        true_rate: f64,
        block: usize,
        record_channel: usize,
        record_from: f64,
    ) -> Self {
        Self {
            side,
            rate: true_rate,
            block,
            channels,
            record_channel,
            frames: 0,
            buf: vec![0.0; block * channels],
            recording: Vec::new(),
            record_from,
        }
    }

    fn next_time(&self) -> f64 {
        (self.frames + self.block as u64) as f64 / self.rate
    }

    fn run(&mut self, time: f64) {
        self.side.read_interleaved(&mut self.buf, time);
        if time >= self.record_from {
            self.recording.extend(self.buf.iter().skip(self.record_channel).step_by(self.channels));
        }
        self.frames += self.block as u64;
    }
}

/// A simulated hardware master: its own drifting clock drives the engine, and
/// it plays a sine into its first input channel.
pub struct SimMaster {
    /// The device's actual rate (nominal × (1 + ppm·1e-6)).
    pub true_rate: f64,
    pub channels: MasterChannels,
    pub tone_hz: f64,
    pub amplitude: f64,
}

/// Drives an [`AudioEngine`] in simulated time, clocked either exactly at
/// `rate` (internal clock) or by a drifting [`SimMaster`], together with any
/// number of simulated soft devices.
pub struct Simulation {
    pub audio: AudioEngine,
    rate: f64,
    blocks: u64,
    master: Option<SimMaster>,
    pub inputs: Vec<SimInput>,
    pub outputs: Vec<SimOutput>,
}

impl Simulation {
    pub fn new(audio: AudioEngine, rate: f64) -> Self {
        Self { audio, rate, blocks: 0, master: None, inputs: Vec::new(), outputs: Vec::new() }
    }

    /// As [`new`](Self::new), but a hardware master's callback drives the engine.
    pub fn with_master(audio: AudioEngine, master: SimMaster) -> Self {
        let rate = master.true_rate;
        Self { audio, rate, blocks: 0, master: Some(master), inputs: Vec::new(), outputs: Vec::new() }
    }

    fn next_master_time(&self) -> f64 {
        (self.blocks + 1) as f64 * self.audio.block() as f64 / self.rate
    }

    fn run_master(&mut self, t: f64) {
        let block = self.audio.block();
        match &self.master {
            None => self.audio.process_block(t),
            Some(m) => {
                let start = self.blocks * block as u64;
                if m.channels.inputs > 0 {
                    let ch = self.audio.inputs_mut().channel_mut(m.channels.first_input);
                    for (n, s) in ch.iter_mut().enumerate() {
                        let k = (start + n as u64) as f64;
                        *s = (m.amplitude * (TAU * m.tone_hz * k / m.true_rate).sin()) as f32;
                    }
                }
                self.audio.process_master_block(t, block as u32);
            }
        }
        self.blocks += 1;
    }

    /// Advances simulated time to `until` seconds, running every due callback
    /// in time order. `control` runs every `control_period` seconds (the
    /// control thread: call `Engine::tick` there).
    pub fn run_until(&mut self, until: f64, control_period: f64, mut control: impl FnMut(f64)) {
        let mut next_control = 0.0;
        loop {
            let t_master = self.next_master_time();
            let (mut t, mut which) = (t_master, Next::Master);
            for (i, d) in self.inputs.iter().enumerate() {
                if d.next_time() < t {
                    (t, which) = (d.next_time(), Next::Input(i));
                }
            }
            for (i, d) in self.outputs.iter().enumerate() {
                if d.next_time() < t {
                    (t, which) = (d.next_time(), Next::Output(i));
                }
            }
            if next_control < t {
                (t, which) = (next_control, Next::Control);
            }
            if t > until {
                return;
            }
            match which {
                Next::Master => self.run_master(t),
                Next::Input(i) => self.inputs[i].run(t),
                Next::Output(i) => self.outputs[i].run(t),
                Next::Control => {
                    control(t);
                    next_control += control_period;
                }
            }
        }
    }
}

enum Next {
    Master,
    Input(usize),
    Output(usize),
    Control,
}

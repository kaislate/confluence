//! Two CLAP plugins for Confluence's tests, in one file:
//!
//! - **Confluence Test Gain** (`dev.confluence.test.gain`): stereo gain (with an
//!   editor of its own on Windows, see `gui`) and a
//!   `Gain` parameter (dB), a `Fail` switch that makes processing fail, and a
//!   read-only `Peak` parameter the plugin reports itself. Its state is the
//!   parameter values.
//! - **Confluence Test Crash** (`dev.confluence.test.crash`): aborts the process
//!   when created, for the engine's load-check tests. Never create it in-process.
//! - **Confluence Test Plain** (`dev.confluence.test.plain`): the gain plugin
//!   without an editor of its own.
//! - **Confluence Test Exit** (`dev.confluence.test.exit`): ends the process with
//!   exit code 0 when created (a check must not mistake that for success).

use std::ffi::CStr;
use std::fmt::Write as _;
use std::io::{Read, Write as _};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPorts, PluginAudioPortsImpl,
};
use clack_extensions::latency::{PluginLatency, PluginLatencyImpl};
use clack_extensions::params::{
    ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter, PluginAudioProcessorParams, PluginMainThreadParams,
    PluginParams,
};
use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_plugin::entry::prelude::*;
use clack_plugin::events::event_types::ParamValueEvent;
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};

#[cfg(windows)]
pub mod gui;

pub const GAIN_ID: &str = "dev.confluence.test.gain";
pub const CRASH_ID: &str = "dev.confluence.test.crash";
/// The gain plugin without an editor.
pub const PLAIN_ID: &str = "dev.confluence.test.plain";
/// Ends the process "successfully" (exit code 0) when created.
pub const EXIT_ID: &str = "dev.confluence.test.exit";
/// Gain in dB, −60…+12.
pub const PARAM_GAIN: u32 = 1;
/// 0 or 1: at 1, processing fails.
pub const PARAM_FAIL: u32 = 2;
/// Read-only: the last block's peak (0…1), reported by the plugin.
pub const PARAM_PEAK: u32 = 3;

/// Test hook: milliseconds this file takes to load.
pub const SLOW_LOAD_ENV: &str = "CONFLUENCE_TEST_PLUGIN_SLOW_LOAD_MS";
/// Test hook: a file written when a slow load finishes.
pub const SLOW_LOAD_MARK_ENV: &str = "CONFLUENCE_TEST_PLUGIN_SLOW_LOAD_MARK";

/// Gain processors deactivated so far (in this process), for host tests.
pub static DEACTIVATIONS: AtomicUsize = AtomicUsize::new(0);

/// An `f64` shared between threads.
struct AtomicF64(AtomicU64);

impl AtomicF64 {
    const fn zero() -> Self {
        AtomicF64(AtomicU64::new(0))
    }
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
    fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

pub struct GainShared {
    gain_db: AtomicF64,
    fail: AtomicF64,
    peak: AtomicF64,
    /// Gain was changed in the editor: report it on the next block.
    edited: std::sync::atomic::AtomicBool,
    /// This instance offers an editor.
    with_gui: bool,
}

impl GainShared {
    fn new(with_gui: bool) -> Self {
        GainShared {
            gain_db: AtomicF64::zero(),
            fail: AtomicF64::zero(),
            peak: AtomicF64::zero(),
            edited: std::sync::atomic::AtomicBool::new(false),
            with_gui,
        }
    }

    /// A change made in the plugin's own editor.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn edit_gain(&self, db: f64) {
        self.set(PARAM_GAIN, db);
        self.edited.store(true, Ordering::Release);
    }
}

impl PluginShared<'_> for GainShared {}

impl GainShared {
    fn get(&self, id: u32) -> Option<f64> {
        match id {
            PARAM_GAIN => Some(self.gain_db.get()),
            PARAM_FAIL => Some(self.fail.get()),
            PARAM_PEAK => Some(self.peak.get()),
            _ => None,
        }
    }

    fn set(&self, id: u32, v: f64) {
        match id {
            PARAM_GAIN => self.gain_db.set(v.clamp(-60.0, 12.0)),
            PARAM_FAIL => self.fail.set(if v >= 0.5 { 1.0 } else { 0.0 }),
            _ => {}
        }
    }

    fn handle(&self, event: &UnknownEvent) {
        if let Some(CoreEventSpace::ParamValue(e)) = event.as_core_event() {
            if let Some(id) = e.param_id() {
                self.set(id.get(), e.value());
            }
        }
    }
}

pub struct GainMain<'a> {
    shared: &'a GainShared,
    #[cfg(windows)]
    editor: gui::Editor,
}

impl<'a> PluginMainThread<'a, GainShared> for GainMain<'a> {}

pub struct GainProcessor<'a> {
    shared: &'a GainShared,
}

pub struct TestGain;

impl Plugin for TestGain {
    type AudioProcessor<'a> = GainProcessor<'a>;
    type Shared<'a> = GainShared;
    type MainThread<'a> = GainMain<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, shared: Option<&GainShared>) {
        builder
            .register::<PluginAudioPorts>()
            .register::<PluginParams>()
            .register::<PluginState>()
            .register::<PluginLatency>();
        #[cfg(windows)]
        if shared.is_some_and(|s| s.with_gui) {
            builder.register::<clack_extensions::gui::PluginGui>();
        }
        #[cfg(not(windows))]
        let _ = shared;
    }
}

impl<'a> PluginAudioProcessor<'a, GainShared, GainMain<'a>> for GainProcessor<'a> {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &GainMain<'a>,
        shared: &'a GainShared,
        _config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(GainProcessor { shared })
    }

    fn process(&mut self, _process: Process, mut audio: Audio, events: Events) -> Result<ProcessStatus, PluginError> {
        for e in events.input {
            self.shared.handle(e);
        }
        if self.shared.fail.get() >= 0.5 {
            return Err(PluginError::Message("test failure"));
        }
        let g = 10f32.powf(self.shared.gain_db.get() as f32 / 20.0);
        let mut peak = 0f32;
        let mut pair = audio.port_pair(0).ok_or(PluginError::Message("no audio port"))?;
        let mut channels = pair.channels()?.into_f32().ok_or(PluginError::Message("expected f32 audio"))?;
        for ch in channels.iter_mut() {
            let out = match ch {
                ChannelPair::InPlace(b) => b,
                ChannelPair::InputOutput(i, o) => {
                    o.copy_from_slice(i);
                    o
                }
                ChannelPair::InputOnly(_) | ChannelPair::OutputOnly(_) => continue,
            };
            for x in out.iter_mut() {
                *x *= g;
                peak = peak.max(x.abs());
            }
        }
        if self.shared.edited.swap(false, Ordering::AcqRel) {
            let ev = ParamValueEvent::new(0, ClapId::new(PARAM_GAIN), Pckn::match_all(), self.shared.gain_db.get());
            let _ = events.output.try_push(ev);
        }
        let peak = f64::from(peak.min(1.0));
        if (peak - self.shared.peak.get()).abs() > 0.01 {
            self.shared.peak.set(peak);
            let ev = ParamValueEvent::new(0, ClapId::new(PARAM_PEAK), Pckn::match_all(), peak);
            let _ = events.output.try_push(ev);
        }
        Ok(ProcessStatus::Continue)
    }

    fn deactivate(self, _main_thread: &GainMain<'a>) {
        DEACTIVATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

impl PluginAudioPortsImpl for GainMain<'_> {
    fn count(&self, _is_input: bool) -> u32 {
        1
    }

    fn get(&self, index: u32, _is_input: bool, writer: &mut AudioPortInfoWriter) {
        if index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: b"main",
                channel_count: 2,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::STEREO),
                in_place_pair: None,
            });
        }
    }
}

impl PluginLatencyImpl for GainMain<'_> {
    fn get(&self) -> u32 {
        0
    }
}

impl PluginStateImpl for GainMain<'_> {
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        output.write_all(&self.shared.gain_db.get().to_le_bytes())?;
        output.write_all(&self.shared.fail.get().to_le_bytes())?;
        Ok(())
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut b = [0u8; 8];
        input.read_exact(&mut b)?;
        self.shared.set(PARAM_GAIN, f64::from_le_bytes(b));
        input.read_exact(&mut b)?;
        self.shared.set(PARAM_FAIL, f64::from_le_bytes(b));
        Ok(())
    }
}

impl PluginMainThreadParams for GainMain<'_> {
    fn count(&self) -> u32 {
        3
    }

    fn get_info(&self, index: u32, info: &mut ParamInfoWriter) {
        let (id, name, min, max, default, flags): (u32, &[u8], f64, f64, f64, ParamInfoFlags) = match index {
            0 => (PARAM_GAIN, b"Gain", -60.0, 12.0, 0.0, ParamInfoFlags::IS_AUTOMATABLE),
            1 => (PARAM_FAIL, b"Fail", 0.0, 1.0, 0.0, ParamInfoFlags::IS_STEPPED),
            2 => (PARAM_PEAK, b"Peak", 0.0, 1.0, 0.0, ParamInfoFlags::IS_READONLY),
            _ => return,
        };
        info.set(&ParamInfo {
            id: ClapId::new(id),
            flags,
            cookie: Default::default(),
            name,
            module: b"",
            min_value: min,
            max_value: max,
            default_value: default,
        });
    }

    fn get_value(&self, id: ClapId) -> Option<f64> {
        self.shared.get(id.get())
    }

    fn value_to_text(&self, id: ClapId, value: f64, writer: &mut ParamDisplayWriter) -> std::fmt::Result {
        match id.get() {
            PARAM_GAIN => write!(writer, "{value:.1} dB"),
            PARAM_FAIL => write!(writer, "{}", if value >= 0.5 { "on" } else { "off" }),
            PARAM_PEAK => write!(writer, "{value:.2}"),
            _ => Err(std::fmt::Error),
        }
    }

    fn text_to_value(&self, id: ClapId, text: &CStr) -> Option<f64> {
        let t = text.to_str().ok()?.trim();
        match id.get() {
            PARAM_GAIN => t.trim_end_matches("dB").trim().parse().ok(),
            _ => t.parse().ok(),
        }
    }

    fn flush(&self, input: &InputEvents, _output: &mut OutputEvents) {
        for e in input {
            self.shared.handle(e);
        }
    }
}

impl PluginAudioProcessorParams for GainProcessor<'_> {
    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        for e in input {
            self.shared.handle(e);
        }
    }
}

/// Aborts the process when created.
pub struct TestCrash;

impl Plugin for TestCrash {
    type AudioProcessor<'a> = ();
    type Shared<'a> = ();
    type MainThread<'a> = ();
}

pub struct Factory {
    gain: PluginDescriptor,
    crash: PluginDescriptor,
    exit: PluginDescriptor,
    plain: PluginDescriptor,
}

impl PluginFactoryImpl for Factory {
    fn plugin_count(&self) -> u32 {
        4
    }

    fn plugin_descriptor(&self, index: u32) -> Option<&PluginDescriptor> {
        match index {
            0 => Some(&self.gain),
            1 => Some(&self.crash),
            2 => Some(&self.exit),
            3 => Some(&self.plain),
            _ => None,
        }
    }

    fn create_plugin<'a>(&'a self, host_info: HostInfo<'a>, plugin_id: &CStr) -> Option<PluginInstance<'a>> {
        let gain = |descriptor, with_gui| {
            PluginInstance::new::<TestGain>(
                host_info,
                descriptor,
                move |_host| Ok(GainShared::new(with_gui)),
                |_host, shared| {
                    Ok(GainMain {
                        shared,
                        #[cfg(windows)]
                        editor: gui::Editor::default(),
                    })
                },
            )
        };
        if plugin_id.to_bytes() == GAIN_ID.as_bytes() {
            Some(gain(&self.gain, true))
        } else if plugin_id.to_bytes() == PLAIN_ID.as_bytes() {
            Some(gain(&self.plain, false))
        } else if plugin_id.to_bytes() == CRASH_ID.as_bytes() {
            Some(PluginInstance::new::<TestCrash>(
                host_info,
                &self.crash,
                |_host| std::process::abort(),
                |_host, _shared| Ok(()),
            ))
        } else if plugin_id.to_bytes() == EXIT_ID.as_bytes() {
            Some(PluginInstance::new::<TestCrash>(
                host_info,
                &self.exit,
                |_host| std::process::exit(0),
                |_host, _shared| Ok(()),
            ))
        } else {
            None
        }
    }
}

/// The file's entry: one factory with all four plugins.
pub struct Entry {
    factory: PluginFactoryWrapper<Factory>,
}

impl clack_plugin::entry::Entry for Entry {
    fn new(_bundle_path: Option<&CStr>) -> Result<Self, EntryLoadError> {
        use clack_plugin::plugin::features::{AUDIO_EFFECT, STEREO, UTILITY};
        // Test hook: a slow load, which then leaves a mark (a load that was
        // stopped part-way never does).
        if let Ok(ms) = std::env::var(SLOW_LOAD_ENV) {
            std::thread::sleep(std::time::Duration::from_millis(ms.parse().unwrap_or(0)));
            if let Ok(mark) = std::env::var(SLOW_LOAD_MARK_ENV) {
                let _ = std::fs::write(mark, b"finished");
            }
        }
        Ok(Entry {
            factory: PluginFactoryWrapper::new(Factory {
                gain: PluginDescriptor::new(GAIN_ID, "Confluence Test Gain")
                    .with_vendor("Confluence")
                    .with_version("1.0.0")
                    .with_features([AUDIO_EFFECT, STEREO]),
                crash: PluginDescriptor::new(CRASH_ID, "Confluence Test Crash")
                    .with_vendor("Confluence")
                    .with_version("1.0.0")
                    .with_features([AUDIO_EFFECT, UTILITY]),
                plain: PluginDescriptor::new(PLAIN_ID, "Confluence Test Plain")
                    .with_vendor("Confluence")
                    .with_version("1.0.0")
                    .with_features([AUDIO_EFFECT, STEREO]),
                exit: PluginDescriptor::new(EXIT_ID, "Confluence Test Exit")
                    .with_vendor("Confluence")
                    .with_version("1.0.0")
                    .with_features([AUDIO_EFFECT, UTILITY]),
            }),
        })
    }

    fn declare_factories<'a>(&'a self, builder: &mut EntryFactories<'a>) {
        builder.register_factory(&self.factory);
    }
}

clack_export_entry!(Entry);

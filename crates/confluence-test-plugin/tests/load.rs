//! The test plugins, loaded in-process through clack-host as the engine would.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use clack_extensions::params::{ParamInfoBuffer, PluginParams};
use clack_host::events::event_types::ParamValueEvent;
use clack_host::prelude::*;
use confluence_test_plugin::{Entry, CRASH_ID, EXIT_ID, GAIN_ID, PARAM_GAIN};

struct Shared;
impl SharedHandler<'_> for Shared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct TestHost;
impl HostHandlers for TestHost {
    type Shared<'a> = Shared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

fn entry() -> PluginEntry {
    PluginEntry::load_from_clack::<Entry>(c"confluence_test_plugin.dll").unwrap()
}

#[test]
fn both_plugins_are_listed() {
    let entry = entry();
    let factory = entry.get_plugin_factory().unwrap();
    let found: Vec<(String, String)> = factory
        .plugin_descriptors()
        .map(|d| (d.id().unwrap().to_str().unwrap().to_string(), d.name().unwrap().to_str().unwrap().to_string()))
        .collect();
    assert_eq!(
        found,
        vec![
            (GAIN_ID.to_string(), "Confluence Test Gain".to_string()),
            (CRASH_ID.to_string(), "Confluence Test Crash".to_string()),
            (EXIT_ID.to_string(), "Confluence Test Exit".to_string()),
        ]
    );
}

#[test]
fn the_gain_plugin_applies_its_gain_and_names_its_values() {
    let entry = entry();
    let info = HostInfo::new("test", "test", "https://example.com", "0").unwrap();
    let id = std::ffi::CString::new(GAIN_ID).unwrap();
    let mut instance = PluginInstance::<TestHost>::new(|_| Shared, |_| (), &entry, &id, &info).unwrap();

    let params: PluginParams = instance.plugin_handle().get_extension().unwrap();
    let handle = instance.plugin_handle();
    assert_eq!(params.count(&handle), 3);
    let mut buf = ParamInfoBuffer::new();
    let gain = params.get_info(&handle, 0, &mut buf).unwrap();
    assert_eq!(gain.name, b"Gain");
    assert_eq!((gain.min_value, gain.max_value, gain.default_value), (-60.0, 12.0, 0.0));
    let mut text = [0u8; 64];
    let shown = params.value_to_text(&handle, PARAM_GAIN.into(), -6.0, &mut text).unwrap();
    assert_eq!(shown, b"-6.0 dB");

    let config = PluginAudioConfiguration { sample_rate: 48_000.0, min_frames_count: 1, max_frames_count: 64 };
    let stopped = instance.activate(|_, _| (), config).unwrap();
    let mut proc = stopped.start_processing().unwrap();
    let mut input = [[1.0f32; 64]; 2];
    let mut output = [[0.0f32; 64]; 2];
    let mut in_ports = AudioPorts::with_capacity(2, 1);
    let mut out_ports = AudioPorts::with_capacity(2, 1);
    let ev = ParamValueEvent::new(0, PARAM_GAIN.into(), Pckn::match_all(), -6.020_6);
    let events = [ev];
    let input_events = InputEvents::from_buffer(&events);
    let mut out_buf = EventBuffer::new();
    let mut output_events = OutputEvents::from_buffer(&mut out_buf);
    let ins = in_ports.with_input_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_input_only(input.iter_mut().map(InputChannel::variable)),
    }]);
    let mut outs = out_ports.with_output_buffers([AudioPortBuffer {
        latency: 0,
        channels: AudioPortBufferType::f32_output_only(output.iter_mut().map(|b| b.as_mut_slice())),
    }]);
    proc.process(&ins, &mut outs, &input_events, &mut output_events, None, None).unwrap();
    for ch in &output {
        assert!(ch.iter().all(|&x| (x - 0.5).abs() < 1e-3), "{:?}", &ch[..4]);
    }
    instance.deactivate(proc.stop_processing());
}

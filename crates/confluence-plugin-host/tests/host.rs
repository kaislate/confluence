//! The plugin host against the in-process test plugins.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use confluence_core::buffer::PlanarBuffer;
use confluence_core::processor::{BusIo, Processor};
use confluence_plugin_host::{PluginLink, PluginThread, Source};
use confluence_test_plugin::{DEACTIVATIONS, GAIN_ID, PARAM_FAIL, PARAM_GAIN, PARAM_PEAK};

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 64;

fn source() -> Source {
    Source::InProcess(|| {
        clack_host::entry::PluginEntry::load_from_clack::<confluence_test_plugin::Entry>(c"test_plugin.dll")
            .map_err(|e| e.to_string())
    })
}

/// Polls until every text asked of the plugin has arrived.
fn settle(link: &mut PluginLink) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while link.texts_pending() {
        assert!(Instant::now() < deadline, "texts never arrived");
        link.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn load(t: &PluginThread, channels: u32) -> (PluginLink, Box<dyn Processor>) {
    t.load(source(), GAIN_ID, RATE, BLOCK, channels).unwrap()
}

/// Runs `p` on `channels` sends of `x`; returns the returns' first samples.
fn run(p: &mut dyn Processor, channels: usize, x: f32) -> Vec<f32> {
    let mut sends = PlanarBuffer::new(channels, BLOCK as usize);
    let mut returns = PlanarBuffer::new(channels, BLOCK as usize);
    for c in 0..channels {
        sends.channel_mut(c).fill(x);
        returns.channel_mut(c).fill(9.0);
    }
    p.process(BusIo::new(&sends, 0, &mut returns, 0, channels)).unwrap();
    (0..channels).map(|c| returns.channel(c)[0]).collect()
}

#[test]
fn loads_and_describes_the_plugin() {
    let t = PluginThread::start().unwrap();
    let (link, _p) = load(&t, 2);
    assert_eq!(link.info().id, GAIN_ID);
    assert_eq!(link.info().name, "Confluence Test Gain");
    assert_eq!(link.info().vendor, "Confluence");
    assert_eq!(link.latency(), 0);
}

#[test]
fn parameters_are_listed_with_their_text() {
    let t = PluginThread::start().unwrap();
    let (link, _p) = load(&t, 2);
    let names: Vec<&str> = link.params().iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Gain", "Fail", "Peak"]);
    let gain = &link.params()[0];
    assert_eq!((gain.id, gain.min, gain.max, gain.default, gain.value), (PARAM_GAIN, -60.0, 12.0, 0.0, 0.0));
    assert_eq!(gain.text, "0.0 dB");
    assert!(link.params()[1].stepped && !link.params()[1].read_only);
    assert!(link.params()[2].read_only);
}

#[test]
fn a_parameter_change_reaches_the_audio_and_its_text_follows() {
    let t = PluginThread::start().unwrap();
    let (mut link, mut p) = load(&t, 2);
    assert_eq!(run(p.as_mut(), 2, 1.0), [1.0, 1.0]);
    link.set_param(PARAM_GAIN, -6.020_6).unwrap();
    let out = run(p.as_mut(), 2, 1.0);
    assert!(out.iter().all(|x| (x - 0.5).abs() < 1e-3), "{out:?}");
    settle(&mut link);
    assert_eq!(link.params()[0].text, "-6.0 dB");
    link.set_param(PARAM_GAIN, 100.0).unwrap();
    assert_eq!(link.params()[0].value, 12.0, "clamped to the range");
    assert!(link.set_param(99, 0.0).unwrap_err().contains("has no parameter 99"));
    assert!(link.set_param(PARAM_PEAK, 0.5).is_err(), "read-only");
}

#[test]
fn a_value_the_plugin_reports_reaches_the_link() {
    let t = PluginThread::start().unwrap();
    let (mut link, mut p) = load(&t, 2);
    run(p.as_mut(), 2, 0.5);
    assert!(link.poll(), "the peak changed");
    settle(&mut link);
    let peak = link.params().iter().find(|q| q.id == PARAM_PEAK).unwrap();
    assert!((peak.value - 0.5).abs() < 1e-6, "{peak:?}");
    assert_eq!(peak.text, "0.50");
    assert!(!link.poll(), "nothing new");
}

#[test]
fn state_round_trips() {
    let t = PluginThread::start().unwrap();
    let (mut a, mut pa) = load(&t, 2);
    a.set_param(PARAM_GAIN, -12.0).unwrap();
    run(pa.as_mut(), 2, 1.0); // the plugin takes the change in its process call
    let saved = a.save_state().unwrap();
    let (mut b, _pb) = load(&t, 2);
    b.load_state(&saved).unwrap();
    settle(&mut b);
    assert_eq!(b.params()[0].value, -12.0);
    assert_eq!(b.params()[0].text, "-12.0 dB");
}

#[test]
fn buses_narrower_and_wider_than_the_plugin_are_mapped_safely() {
    let t = PluginThread::start().unwrap();
    let (_l1, mut mono) = load(&t, 1);
    assert_eq!(run(mono.as_mut(), 1, 1.0), [1.0]);
    let (_l4, mut quad) = load(&t, 4);
    assert_eq!(run(quad.as_mut(), 4, 1.0), [1.0, 1.0, 0.0, 0.0], "returns past the plugin's outputs are silent");
}

#[test]
fn a_failing_plugin_reports_an_error() {
    let t = PluginThread::start().unwrap();
    let (mut link, mut p) = load(&t, 2);
    link.set_param(PARAM_FAIL, 1.0).unwrap();
    let mut sends = PlanarBuffer::new(2, BLOCK as usize);
    let mut returns = PlanarBuffer::new(2, BLOCK as usize);
    assert!(p.process(BusIo::new(&sends, 0, &mut returns, 0, 2)).is_err());
    sends.clear();
}

#[test]
fn a_reclaimed_processor_is_deactivated_on_the_plugin_thread() {
    let t = PluginThread::start().unwrap();
    let (link, mut p) = load(&t, 2);
    run(p.as_mut(), 2, 1.0);
    p.stop();
    let before = DEACTIVATIONS.load(Ordering::Relaxed);
    t.reclaim(p);
    drop(link);
    let deadline = Instant::now() + Duration::from_secs(5);
    while DEACTIVATIONS.load(Ordering::Relaxed) == before {
        assert!(Instant::now() < deadline, "not deactivated");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn an_unknown_plugin_id_is_a_clear_error() {
    let t = PluginThread::start().unwrap();
    let err = t.load(source(), "no.such.plugin", RATE, BLOCK, 2).err().unwrap();
    assert!(err.contains("has no plugin no.such.plugin"), "{err}");
}

/// A parameter change must not wait for the plugin thread (it may be busy
/// loading another plugin for seconds): its text arrives later, through `poll`.
#[test]
fn a_parameter_change_does_not_wait_for_a_busy_plugin_thread() {
    let t = PluginThread::start().unwrap();
    let (mut link, _p) = load(&t, 2);
    let slow = Source::InProcess(|| {
        std::thread::sleep(Duration::from_secs(2));
        clack_host::entry::PluginEntry::load_from_clack::<confluence_test_plugin::Entry>(c"slow.dll")
            .map_err(|e| e.to_string())
    });
    let busy = t.clone();
    let loading = std::thread::spawn(move || busy.load(slow, GAIN_ID, RATE, BLOCK, 2).map(|_| ()));
    std::thread::sleep(Duration::from_millis(200)); // the plugin thread is now loading
    let started = Instant::now();
    link.set_param(PARAM_GAIN, -6.0).unwrap();
    assert!(started.elapsed() < Duration::from_millis(500), "set_param waited {:?}", started.elapsed());
    assert_eq!(link.params()[0].value, -6.0, "the value is known at once");
    let deadline = Instant::now() + Duration::from_secs(10);
    while link.params()[0].text != "-6.0 dB" {
        assert!(Instant::now() < deadline, "the plugin's text never arrived: {:?}", link.params()[0].text);
        link.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
    loading.join().unwrap().unwrap();
}

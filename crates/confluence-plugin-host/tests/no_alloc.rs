//! A hosted plugin's audio side never allocates: not on its first block (when
//! processing starts), not while taking parameter changes or reporting values.
#![cfg(debug_assertions)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use assert_no_alloc::{assert_no_alloc, AllocDisabler};
use confluence_core::buffer::PlanarBuffer;
use confluence_core::processor::BusIo;
use confluence_plugin_host::{PluginThread, Source};
use confluence_test_plugin::{GAIN_ID, PARAM_GAIN};

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[test]
fn processing_with_parameter_changes_does_not_allocate() {
    let t = PluginThread::start().unwrap();
    let src = Source::InProcess(|| {
        clack_host::entry::PluginEntry::load_from_clack::<confluence_test_plugin::Entry>(c"test_plugin.dll")
            .map_err(|e| e.to_string())
    });
    let (mut link, mut p) = t.load(src, GAIN_ID, 48_000.0, 64, 2).unwrap();
    let mut sends = PlanarBuffer::new(2, 64);
    let mut returns = PlanarBuffer::new(2, 64);
    for c in 0..2 {
        sends.channel_mut(c).fill(0.25);
    }
    for n in 0..20 {
        // Sent from the control side (it may allocate there: text lookups).
        link.set_param(PARAM_GAIN, -(n as f64)).unwrap();
    }
    assert_no_alloc(|| {
        for n in 0..10 {
            sends.channel_mut(0).fill(0.1 * n as f32); // the peak changes: values are reported
            p.process(BusIo::new(&sends, 0, &mut returns, 0, 2)).unwrap();
        }
    });
    assert!(link.poll());
}

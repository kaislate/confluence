//! Proves the audio-thread entry points never allocate. `AllocDisabler`
//! only exists in debug builds, so this file is compiled there only.
#![cfg(debug_assertions)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use assert_no_alloc::{assert_no_alloc, AllocDisabler};
use confluence_core::buffer::PlanarBuffer;
use confluence_core::gain::PointParams;
use confluence_core::matrix::matrix;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[test]
fn router_process_with_pending_snapshot_does_not_allocate() {
    let (mut ctl, mut router) = matrix(8, 8, Duration::from_millis(10), 48_000.0);
    let inputs = PlanarBuffer::new(8, 256);
    let mut outputs = PlanarBuffer::new(8, 256);
    for n in 0..8 {
        ctl.set_point(n, 7 - n, PointParams::default()).unwrap();
    }
    ctl.tick(); // snapshot now waiting in the mailbox
    assert_no_alloc(|| {
        router.process(&inputs, &mut outputs);
        router.process(&inputs, &mut outputs);
    });
    ctl.tick(); // old snapshot freed here, on the control side
}

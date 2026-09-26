//! Randomized check of the sparse router against a dense reference mix.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::needless_range_loop)]

use std::time::Duration;

use confluence_core::buffer::PlanarBuffer;
use confluence_core::gain::PointParams;
use confluence_core::matrix::matrix;
use proptest::prelude::*;

const N: usize = 6;
const FRAMES: usize = 64;

fn point() -> impl Strategy<Value = (u32, u32, PointParams)> {
    (0..N as u32, 0..N as u32, -60.0f32..12.0, any::<bool>(), any::<bool>())
        .prop_map(|(i, o, gain_db, mute, invert)| (i, o, PointParams { gain_db, mute, invert }))
}

proptest! {
    #[test]
    fn sparse_router_matches_dense_reference(
        points in prop::collection::vec(point(), 0..20),
        seed in any::<u32>(),
    ) {
        let (mut ctl, mut router) = matrix(N, N, Duration::from_millis(1), 48_000.0);
        for &(i, o, p) in &points {
            ctl.set_point(i, o, p).unwrap();
        }
        ctl.tick();

        let mut inputs = PlanarBuffer::new(N, FRAMES);
        let mut outputs = PlanarBuffer::new(N, FRAMES);
        let mut x = seed.max(1);
        for ch in 0..N {
            for s in inputs.channel_mut(ch) {
                x ^= x << 13; x ^= x >> 17; x ^= x << 5;
                *s = (x as f32 / u32::MAX as f32) * 2.0 - 1.0;
            }
        }
        // 1 ms ramp = 48 samples, so the second block is fully settled.
        router.process(&inputs, &mut outputs);
        router.process(&inputs, &mut outputs);

        let mut gains = [[0.0f32; N]; N];
        for (i, o, p) in ctl.points() {
            gains[i as usize][o as usize] = p.effective_gain();
        }
        for o in 0..N {
            for n in 0..FRAMES {
                let expected: f32 = (0..N).map(|i| gains[i][o] * inputs.channel(i)[n]).sum();
                let got = outputs.channel(o)[n];
                prop_assert!((got - expected).abs() <= 1e-4 * (1.0 + expected.abs()),
                    "out {o} frame {n}: got {got}, expected {expected}");
            }
        }
    }
}

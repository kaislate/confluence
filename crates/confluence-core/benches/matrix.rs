//! Router throughput: cost scales with active points, not matrix size.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;
use std::time::Duration;

use confluence_core::buffer::PlanarBuffer;
use confluence_core::gain::PointParams;
use confluence_core::matrix::matrix;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

fn router(c: &mut Criterion) {
    let mut group = c.benchmark_group("router_512x512_128_frames");
    for points in [1_000u32, 10_000] {
        let (mut ctl, mut r) = matrix(512, 512, Duration::from_millis(10), 48_000.0);
        let mut x = 12345u32;
        let mut added = 0;
        while added < points {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let (i, o) = (x % 512, (x >> 9) % 512);
            if ctl.point(i, o).is_none() {
                ctl.set_point(i, o, PointParams { gain_db: -3.0, mute: false, invert: false }).unwrap();
                added += 1;
            }
        }
        ctl.tick();
        let mut inputs = PlanarBuffer::new(512, 128);
        let mut outputs = PlanarBuffer::new(512, 128);
        inputs.set_frames(128);
        outputs.set_frames(128);
        for _ in 0..8 {
            r.process(&inputs, &mut outputs); // settle ramps
        }
        group.bench_with_input(BenchmarkId::from_parameter(points), &points, |b, _| {
            b.iter(|| r.process(black_box(&inputs), black_box(&mut outputs)))
        });
    }
    group.finish();
}

criterion_group!(benches, router);
criterion_main!(benches);

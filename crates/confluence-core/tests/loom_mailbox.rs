//! Exhaustive interleaving check of the mailbox.
//! Run with: RUSTFLAGS="--cfg loom" cargo test -p confluence-core --test loom_mailbox --release
#![cfg(loom)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use confluence_core::mailbox::channel;

#[test]
fn values_arrive_once_in_order_and_are_dropped_exactly_once() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<Box<u32>>(1);
        let producer = loom::thread::spawn(move || {
            for i in 0..2 {
                let mut v = Box::new(i);
                loop {
                    match tx.try_send(v) {
                        Ok(()) => break,
                        Err(back) => {
                            v = back;
                            loom::thread::yield_now();
                        }
                    }
                }
            }
        });
        let mut got = Vec::new();
        while got.len() < 2 {
            match rx.try_recv() {
                Some(v) => got.push(*v),
                None => loom::thread::yield_now(),
            }
        }
        producer.join().unwrap();
        assert_eq!(got, vec![0, 1]);
    });
}

//! Bounded single-producer/single-consumer mailbox for handing owned values
//! (e.g. `Box<Snapshot>`) between the control thread and the audio thread.
//!
//! `try_send` and `try_recv` never allocate, lock or free: a slot only ever
//! holds `None` when written and is left `None` when read.

use crate::sync::{Arc, AtomicUsize, Ordering, UnsafeCell};

struct Shared<T> {
    slots: Box<[UnsafeCell<Option<T>>]>,
    /// Next index the receiver will read. Written only by the receiver.
    head: AtomicUsize,
    /// Next index the sender will write. Written only by the sender.
    tail: AtomicUsize,
}

// SAFETY: a slot is accessed by exactly one side at a time, arbitrated by the
// head/tail release/acquire handshake below; values are moved, never shared.
unsafe impl<T: Send> Send for Shared<T> {}
unsafe impl<T: Send> Sync for Shared<T> {}

pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

/// Creates a mailbox holding at most `capacity` (≥ 1) values in flight.
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let capacity = capacity.max(1);
    let slots = (0..capacity).map(|_| UnsafeCell::new(None)).collect();
    let shared = Arc::new(Shared { slots, head: AtomicUsize::new(0), tail: AtomicUsize::new(0) });
    (Sender { shared: shared.clone() }, Receiver { shared })
}

impl<T> Sender<T> {
    /// Moves `value` into the mailbox, or hands it back if the mailbox is full.
    pub fn try_send(&mut self, value: T) -> Result<(), T> {
        let s = &*self.shared;
        let tail = s.tail.load(Ordering::Relaxed);
        let head = s.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) == s.slots.len() {
            return Err(value);
        }
        // SAFETY: the slot at `tail` is not visible to the receiver until the
        // release store of `tail + 1` below, and the receiver has finished with
        // it (it advanced `head` past it with a release store we acquired).
        s.slots[tail % s.slots.len()].with_mut(|p| unsafe { *p = Some(value) });
        s.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }
}

impl<T> Receiver<T> {
    /// Takes the oldest value, if any.
    pub fn try_recv(&mut self) -> Option<T> {
        let s = &*self.shared;
        let head = s.head.load(Ordering::Relaxed);
        let tail = s.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        // SAFETY: the sender published this slot with the release store of
        // `tail` that we acquired, and will not touch it again until we
        // release `head + 1`.
        let value = s.slots[head % s.slots.len()].with_mut(|p| unsafe { (*p).take() });
        s.head.store(head.wrapping_add(1), Ordering::Release);
        value
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn fifo_order_and_capacity() {
        let (mut tx, mut rx) = channel::<u32>(2);
        assert!(tx.try_send(1).is_ok());
        assert!(tx.try_send(2).is_ok());
        assert_eq!(tx.try_send(3), Err(3));
        assert_eq!(rx.try_recv(), Some(1));
        assert!(tx.try_send(3).is_ok());
        assert_eq!(rx.try_recv(), Some(2));
        assert_eq!(rx.try_recv(), Some(3));
        assert_eq!(rx.try_recv(), None);
    }

    #[test]
    fn works_across_threads() {
        let (mut tx, mut rx) = channel::<Box<u64>>(4);
        let producer = std::thread::spawn(move || {
            for i in 0..10_000u64 {
                let mut v = Box::new(i);
                loop {
                    match tx.try_send(v) {
                        Ok(()) => break,
                        Err(back) => {
                            v = back;
                            std::thread::yield_now();
                        }
                    }
                }
            }
        });
        let mut next = 0u64;
        while next < 10_000 {
            if let Some(v) = rx.try_recv() {
                assert_eq!(*v, next);
                next += 1;
            }
        }
        assert!(producer.join().is_ok());
    }
}

//! Shared-memory stream protocol (spec §3.1): one layout for every
//! cross-process audio stream. This first part is the lock-free frame ring.

pub mod ring;

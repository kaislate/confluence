//! Confluence network audio: unicast PCM streams between engines over RTP/UDP.

pub mod discovery;
pub mod host;
pub mod packet;
pub mod receiver;

/// The UDP port engines listen on unless told otherwise.
pub const DEFAULT_PORT: u16 = 6990;

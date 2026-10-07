//! The Confluence RTP packet: an RTP (RFC 3550) header, a header extension
//! naming the stream and its layout, and interleaved PCM.
//!
//! Extension (profile `0xC0F1`): format (1), total channels (1), first channel
//! (1), channels here (1), sample rate (4), frames (2), name length (1), name,
//! zero padding to a 4-byte boundary. Samples are big-endian.

/// Largest datagram sent (room for encryption later).
pub const MAX_DATAGRAM: usize = 1400;
/// Most channels in one stream.
pub const MAX_CHANNELS: u8 = 64;
/// Longest stream name, in bytes.
pub const MAX_NAME: usize = 64;
/// RTP payload type used (dynamic).
pub const PAYLOAD_TYPE: u8 = 96;
const PROFILE: u16 = 0xC0F1;
const RTP_HEADER: usize = 12;
const MAX_FRAMES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// 24-bit signed integer.
    L24 = 1,
    /// 32-bit float.
    F32 = 2,
}

impl Format {
    pub fn bytes(self) -> usize {
        match self {
            Format::L24 => 3,
            Format::F32 => 4,
        }
    }

    fn from_u8(v: u8) -> Option<Format> {
        match v {
            1 => Some(Format::L24),
            2 => Some(Format::F32),
            _ => None,
        }
    }
}

/// Everything in a packet but its samples.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub seq: u16,
    /// Of the first frame, in frames of the stream's rate.
    pub timestamp: u32,
    pub ssrc: u32,
    pub format: Format,
    pub total_channels: u8,
    /// The channels this packet carries: `first_channel..first_channel + channels`.
    pub first_channel: u8,
    pub channels: u8,
    pub rate: u32,
    pub stream: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Too short to be a packet.
    Short,
    /// Not a Confluence RTP packet.
    NotOurs,
    /// Channel layout, rate or frame count out of range.
    Layout,
    /// The stream name is not one line of UTF-8 text.
    Name,
    /// The samples do not match the header.
    Payload,
}

/// A parsed packet borrowing its samples.
#[derive(Debug)]
pub struct Packet<'a> {
    pub header: Header,
    frames: usize,
    payload: &'a [u8],
}

impl Packet<'_> {
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Writes the samples, interleaved, into `out` (`frames × channels`).
    pub fn decode(&self, out: &mut [f32]) {
        let n = out.len().min(self.frames * self.header.channels as usize);
        match self.header.format {
            Format::L24 => {
                for (o, b) in out[..n].iter_mut().zip(self.payload.chunks_exact(3)) {
                    let v = i32::from_be_bytes([b[0], b[1], b[2], 0]) >> 8;
                    *o = v as f32 / 8_388_607.0;
                }
            }
            Format::F32 => {
                for (o, b) in out[..n].iter_mut().zip(self.payload.chunks_exact(4)) {
                    *o = f32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                }
            }
        }
    }
}

fn extension_len(name_bytes: usize) -> usize {
    (11 + name_bytes).div_ceil(4) * 4
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && !name.chars().any(char::is_control)
}

/// Writes a packet for `samples` (interleaved, `h.channels` per frame) to `out`.
pub fn build(h: &Header, samples: &[f32], out: &mut Vec<u8>) {
    out.clear();
    let channels = h.channels.max(1) as usize;
    let frames = samples.len() / channels;
    out.push(0x90); // V=2, X=1
    out.push(PAYLOAD_TYPE);
    out.extend_from_slice(&h.seq.to_be_bytes());
    out.extend_from_slice(&h.timestamp.to_be_bytes());
    out.extend_from_slice(&h.ssrc.to_be_bytes());
    let name = &h.stream.as_bytes()[..h.stream.len().min(MAX_NAME)];
    let ext = extension_len(name.len());
    out.extend_from_slice(&PROFILE.to_be_bytes());
    out.extend_from_slice(&((ext / 4) as u16).to_be_bytes());
    out.extend_from_slice(&[h.format as u8, h.total_channels, h.first_channel, h.channels]);
    out.extend_from_slice(&h.rate.to_be_bytes());
    out.extend_from_slice(&(frames as u16).to_be_bytes());
    out.push(name.len() as u8);
    out.extend_from_slice(name);
    out.resize(RTP_HEADER + 4 + ext, 0);
    for &v in &samples[..frames * channels] {
        match h.format {
            Format::L24 => {
                let i = (v.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                out.extend_from_slice(&i.to_be_bytes()[1..]);
            }
            Format::F32 => out.extend_from_slice(&v.to_be_bytes()),
        }
    }
}

/// Reads a packet; anything that is not a well-formed Confluence packet is an error.
pub fn parse(buf: &[u8]) -> Result<Packet<'_>, ParseError> {
    if buf.len() < RTP_HEADER + 4 {
        return Err(ParseError::Short);
    }
    // Version 2, no padding, extension, no CSRCs; our payload type.
    if buf[0] != 0x90 || buf[1] & 0x7F != PAYLOAD_TYPE {
        return Err(ParseError::NotOurs);
    }
    let u16_at = |i: usize| u16::from_be_bytes([buf[i], buf[i + 1]]);
    let u32_at = |i: usize| u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
    if u16_at(12) != PROFILE {
        return Err(ParseError::NotOurs);
    }
    let ext = u16_at(14) as usize * 4;
    let body = RTP_HEADER + 4;
    if ext < 12 || buf.len() < body + ext {
        return Err(ParseError::Short);
    }
    let e = &buf[body..body + ext];
    let format = Format::from_u8(e[0]).ok_or(ParseError::Layout)?;
    let (total, first, channels) = (e[1], e[2], e[3]);
    let rate = u32::from_be_bytes([e[4], e[5], e[6], e[7]]);
    let frames = u16::from_be_bytes([e[8], e[9]]) as usize;
    let name_len = e[10] as usize;
    if total == 0
        || total > MAX_CHANNELS
        || channels == 0
        || first as usize + channels as usize > total as usize
        || !(8_000..=384_000).contains(&rate)
        || frames == 0
        || frames > MAX_FRAMES
    {
        return Err(ParseError::Layout);
    }
    if 11 + name_len > ext {
        return Err(ParseError::Short);
    }
    let stream = std::str::from_utf8(&e[11..11 + name_len]).map_err(|_| ParseError::Name)?;
    if !valid_name(stream) || extension_len(name_len) != ext {
        return Err(ParseError::Name);
    }
    let payload = &buf[body + ext..];
    if payload.len() != frames * channels as usize * format.bytes() {
        return Err(ParseError::Payload);
    }
    let header = Header {
        seq: u16_at(2),
        timestamp: u32_at(4),
        ssrc: u32_at(8),
        format,
        total_channels: total,
        first_channel: first,
        channels,
        rate,
        stream: stream.to_string(),
    };
    Ok(Packet { header, frames, payload })
}

/// How a block of `frames` frames of `total` channels is split into packets
/// that each fit [`MAX_DATAGRAM`]: (first channel, channels) per packet.
pub fn split(total: u8, frames: usize, format: Format, stream: &str) -> Vec<(u8, u8)> {
    let overhead = RTP_HEADER + 4 + extension_len(stream.len().min(MAX_NAME));
    let per = ((MAX_DATAGRAM - overhead) / (frames.max(1) * format.bytes())).clamp(1, MAX_CHANNELS as usize) as u8;
    (0..total).step_by(per as usize).map(|first| (first, per.min(total - first))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(first: u8, channels: u8, total: u8, format: Format) -> Header {
        Header {
            seq: 65_530,
            timestamp: 4_000_000_000,
            ssrc: 0xDEAD_BEEF,
            format,
            total_channels: total,
            first_channel: first,
            channels,
            rate: 48_000,
            stream: "Main".into(),
        }
    }

    fn ramp(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels).map(|i| ((i % 200) as f32 / 100.0) - 1.0).collect()
    }

    #[test]
    fn f32_packets_round_trip_exactly() {
        let h = header(0, 2, 2, Format::F32);
        let samples = ramp(48, 2);
        let mut buf = Vec::new();
        build(&h, &samples, &mut buf);
        let p = parse(&buf).unwrap();
        assert_eq!(p.header, h);
        assert_eq!(p.frames(), 48);
        let mut out = vec![0.0; 96];
        p.decode(&mut out);
        assert_eq!(out, samples);
    }

    #[test]
    fn l24_packets_round_trip_within_one_step() {
        let h = header(4, 3, 8, Format::L24);
        let mut samples = ramp(48, 3);
        samples[0] = 1.5; // clipped
        samples[1] = -1.5;
        let mut buf = Vec::new();
        build(&h, &samples, &mut buf);
        assert!(buf.len() <= MAX_DATAGRAM);
        let p = parse(&buf).unwrap();
        assert_eq!(p.header, h);
        let mut out = vec![0.0; 144];
        p.decode(&mut out);
        assert!((out[0] - 1.0).abs() < 1e-6 && (out[1] + 1.0).abs() < 1e-6, "clipped to full scale");
        for (a, b) in samples.iter().zip(&out).skip(2) {
            assert!((a - b).abs() <= 1.0 / 8_388_607.0, "{a} {b}");
        }
    }

    #[test]
    fn many_channels_are_split_into_datagrams_that_fit() {
        for (total, frames, format) in
            [(64u8, 48usize, Format::L24), (64, 192, Format::F32), (2, 48, Format::L24), (1, 192, Format::F32)]
        {
            let ranges = split(total, frames, format, "Main");
            let mut next = 0u8;
            for &(first, count) in &ranges {
                assert_eq!(first, next, "contiguous");
                assert!(count > 0);
                let h = Header {
                    first_channel: first,
                    channels: count,
                    total_channels: total,
                    format,
                    ..header(0, 1, 1, format)
                };
                let mut buf = Vec::new();
                build(&h, &ramp(frames, count as usize), &mut buf);
                assert!(buf.len() <= MAX_DATAGRAM, "{total} ch {frames} frames: {} bytes", buf.len());
                next += count;
            }
            assert_eq!(next, total, "every channel sent");
        }
    }

    #[test]
    fn every_truncation_and_bit_flip_is_rejected_or_read_safely() {
        let h = header(0, 2, 2, Format::L24);
        let mut buf = Vec::new();
        build(&h, &ramp(48, 2), &mut buf);
        for n in 0..buf.len() {
            assert!(parse(&buf[..n]).is_err(), "truncated to {n}");
        }
        for i in 0..buf.len() * 8 {
            let mut b = buf.clone();
            b[i / 8] ^= 1 << (i % 8);
            if let Ok(p) = parse(&b) {
                let mut out = vec![0.0; p.frames() * p.header.channels as usize];
                p.decode(&mut out);
            }
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut x: u64 = 0x1234_5678_9ABC_DEF1;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut valid = Vec::new();
        build(&header(0, 2, 2, Format::F32), &ramp(48, 2), &mut valid);
        for round in 0..20_000 {
            let len = (next() % 1500) as usize;
            let mut b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if round % 2 == 0 && len > 4 {
                // A plausible start makes the parser go deeper.
                let k = len.min(valid.len()).min(4 + (next() % 40) as usize);
                b[..k].copy_from_slice(&valid[..k]);
            }
            if let Ok(p) = parse(&b) {
                let mut out = vec![0.0; p.frames() * p.header.channels as usize];
                p.decode(&mut out);
            }
        }
    }

    #[test]
    fn lying_fields_are_rejected() {
        let mut buf = Vec::new();
        let bad = |h: Header| {
            let mut b = Vec::new();
            build(&h, &ramp(48, h.channels.max(1) as usize), &mut b);
            parse(&b).err()
        };
        assert_eq!(bad(header(0, 0, 2, Format::L24)), Some(ParseError::Layout));
        assert_eq!(bad(header(1, 2, 2, Format::L24)), Some(ParseError::Layout), "channels past the total");
        assert_eq!(bad(header(0, 2, 65, Format::L24)), Some(ParseError::Layout), "more than 64");
        assert_eq!(bad(Header { rate: 1000, ..header(0, 2, 2, Format::L24) }), Some(ParseError::Layout));
        assert_eq!(bad(Header { stream: "a\nb".into(), ..header(0, 2, 2, Format::L24) }), Some(ParseError::Name));
        build(&header(0, 2, 2, Format::L24), &ramp(48, 2), &mut buf);
        buf.pop(); // payload no longer whole frames
        buf.push(0);
        buf.push(0);
        assert_eq!(parse(&buf).err(), Some(ParseError::Payload));
        buf[0] = 0x10; // RTP version 0
        assert_eq!(parse(&buf).err(), Some(ParseError::NotOurs));
    }
}

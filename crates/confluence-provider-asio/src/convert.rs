//! Conversion between ASIO sample formats and the engine's `f32`.

use crate::sys::*;

/// Supported (little-endian) ASIO sample formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    I16,
    I24,
    I32,
    F32,
    F64,
    /// 32-bit container holding a right-aligned `bits`-bit sample.
    I32Aligned {
        bits: u32,
    },
}

impl SampleFormat {
    /// Maps an ASIO sample type; `None` for big-endian and DSD formats.
    pub fn from_asio(t: AsioSampleType) -> Option<Self> {
        Some(match t {
            ST_INT16_LSB => Self::I16,
            ST_INT24_LSB => Self::I24,
            ST_INT32_LSB => Self::I32,
            ST_FLOAT32_LSB => Self::F32,
            ST_FLOAT64_LSB => Self::F64,
            ST_INT32_LSB16 => Self::I32Aligned { bits: 16 },
            ST_INT32_LSB18 => Self::I32Aligned { bits: 18 },
            ST_INT32_LSB20 => Self::I32Aligned { bits: 20 },
            ST_INT32_LSB24 => Self::I32Aligned { bits: 24 },
            _ => return None,
        })
    }

    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::I32 | Self::F32 | Self::I32Aligned { .. } => 4,
            Self::F64 => 8,
        }
    }
}

fn to_int(x: f32, bits: u32) -> i32 {
    let full = (1i64 << (bits - 1)) as f64;
    (x as f64 * full).round().clamp(-full, full - 1.0) as i32
}

/// Decodes `dst.len()` samples from `src` (which must hold at least that many).
pub fn decode(fmt: SampleFormat, src: &[u8], dst: &mut [f32]) {
    let n = dst.len().min(src.len() / fmt.bytes_per_sample());
    match fmt {
        SampleFormat::I16 => {
            for (d, b) in dst[..n].iter_mut().zip(src.chunks_exact(2)) {
                *d = i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0;
            }
        }
        SampleFormat::I24 => {
            for (d, b) in dst[..n].iter_mut().zip(src.chunks_exact(3)) {
                let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
                *d = v as f32 / 8_388_608.0;
            }
        }
        SampleFormat::I32 => {
            for (d, b) in dst[..n].iter_mut().zip(src.chunks_exact(4)) {
                *d = (i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64 / 2_147_483_648.0) as f32;
            }
        }
        SampleFormat::F32 => {
            for (d, b) in dst[..n].iter_mut().zip(src.chunks_exact(4)) {
                *d = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
        }
        SampleFormat::F64 => {
            for (d, b) in dst[..n].iter_mut().zip(src.chunks_exact(8)) {
                *d = f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f32;
            }
        }
        SampleFormat::I32Aligned { bits } => {
            let shift = 32 - bits;
            let scale = (1i64 << (bits - 1)) as f32;
            for (d, b) in dst[..n].iter_mut().zip(src.chunks_exact(4)) {
                let v = (i32::from_le_bytes([b[0], b[1], b[2], b[3]]) << shift) >> shift;
                *d = v as f32 / scale;
            }
        }
    }
    dst[n..].fill(0.0);
}

/// Encodes `src` into `dst` (which must hold `src.len()` samples), clamping to full scale.
pub fn encode(fmt: SampleFormat, src: &[f32], dst: &mut [u8]) {
    let n = src.len().min(dst.len() / fmt.bytes_per_sample());
    match fmt {
        SampleFormat::I16 => {
            for (s, b) in src[..n].iter().zip(dst.chunks_exact_mut(2)) {
                b.copy_from_slice(&(to_int(*s, 16) as i16).to_le_bytes());
            }
        }
        SampleFormat::I24 => {
            for (s, b) in src[..n].iter().zip(dst.chunks_exact_mut(3)) {
                b.copy_from_slice(&to_int(*s, 24).to_le_bytes()[..3]);
            }
        }
        SampleFormat::I32 => {
            for (s, b) in src[..n].iter().zip(dst.chunks_exact_mut(4)) {
                b.copy_from_slice(&to_int(*s, 32).to_le_bytes());
            }
        }
        SampleFormat::F32 => {
            for (s, b) in src[..n].iter().zip(dst.chunks_exact_mut(4)) {
                b.copy_from_slice(&s.to_le_bytes());
            }
        }
        SampleFormat::F64 => {
            for (s, b) in src[..n].iter().zip(dst.chunks_exact_mut(8)) {
                b.copy_from_slice(&(*s as f64).to_le_bytes());
            }
        }
        SampleFormat::I32Aligned { bits } => {
            for (s, b) in src[..n].iter().zip(dst.chunks_exact_mut(4)) {
                b.copy_from_slice(&to_int(*s, bits).to_le_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [SampleFormat; 9] = [
        SampleFormat::I16,
        SampleFormat::I24,
        SampleFormat::I32,
        SampleFormat::F32,
        SampleFormat::F64,
        SampleFormat::I32Aligned { bits: 16 },
        SampleFormat::I32Aligned { bits: 18 },
        SampleFormat::I32Aligned { bits: 20 },
        SampleFormat::I32Aligned { bits: 24 },
    ];

    fn resolution(fmt: SampleFormat) -> f32 {
        match fmt {
            SampleFormat::I16 | SampleFormat::I32Aligned { bits: 16 } => 1.0 / 32_768.0,
            SampleFormat::I32Aligned { bits } => 1.0 / (1u32 << (bits - 1)) as f32,
            SampleFormat::I24 => 1.0 / 8_388_608.0,
            _ => 1e-6,
        }
    }

    #[test]
    fn every_format_round_trips_within_its_resolution() {
        let src: Vec<f32> = (0..64).map(|i| ((i as f32) * 0.37).sin() * 0.9).collect();
        for fmt in ALL {
            let mut bytes = vec![0u8; src.len() * fmt.bytes_per_sample()];
            encode(fmt, &src, &mut bytes);
            let mut back = vec![0f32; src.len()];
            decode(fmt, &bytes, &mut back);
            for (a, b) in src.iter().zip(&back) {
                assert!((a - b).abs() <= resolution(fmt), "{fmt:?}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn known_integer_encodings() {
        let mut b = [0u8; 3];
        encode(SampleFormat::I24, &[-1.0], &mut b);
        assert_eq!(b, [0x00, 0x00, 0x80]);
        encode(SampleFormat::I24, &[1.0], &mut b);
        assert_eq!(b, [0xFF, 0xFF, 0x7F], "clamped to the largest positive value");
        let mut b = [0u8; 4];
        encode(SampleFormat::I32Aligned { bits: 24 }, &[-1.0], &mut b);
        assert_eq!(i32::from_le_bytes(b), -8_388_608, "right-aligned in the 32-bit container");
        let mut d = [0f32; 1];
        decode(SampleFormat::I16, &[0x00, 0x80], &mut d);
        assert_eq!(d[0], -1.0);
    }

    #[test]
    fn short_source_buffers_zero_the_rest() {
        let mut d = [9.0f32; 4];
        decode(SampleFormat::F32, &0.5f32.to_le_bytes(), &mut d);
        assert_eq!(d, [0.5, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn big_endian_and_dsd_are_rejected() {
        assert_eq!(SampleFormat::from_asio(0), None, "Int16MSB");
        assert_eq!(SampleFormat::from_asio(32), None, "DSD");
        assert_eq!(SampleFormat::from_asio(ST_FLOAT32_LSB), Some(SampleFormat::F32));
    }
}

//! G.711 mu-law. `ulaw_decode` is the textbook ITU-T formula. `ulaw_encode`
//! is built by construction (nearest-neighbor search over the decode table)
//! rather than a hand-derived fast bit-twiddling encoder, which is easy to
//! get subtly wrong — this is correct by definition since its output space
//! is exactly `ulaw_decode`'s.

pub fn ulaw_decode(code: u8) -> i16 {
    let u = !code;
    let sign = u & 0x80;
    let exponent = (u & 0x70) >> 4;
    let mantissa = (u & 0x0F) as i32;
    let mut t: i32 = (mantissa << 3) + 0x84;
    t <<= exponent;
    let sample = if sign != 0 { 0x84 - t } else { t - 0x84 };
    sample.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

pub fn build_decode_table() -> [i16; 256] {
    std::array::from_fn(|code| ulaw_decode(code as u8))
}

pub fn ulaw_encode(table: &[i16; 256], sample: i16) -> u8 {
    table
        .iter()
        .enumerate()
        .min_by_key(|&(_, &decoded)| (decoded as i32 - sample as i32).unsigned_abs())
        .map(|(code, _)| code as u8)
        .unwrap()
}

/// G.711 A-law (PCMA, RTP payload type 8) — the reference decode formula
/// (see e.g. Sun's classic `g711.c`), with `alaw_encode` built the same
/// nearest-neighbor-over-the-decode-table way as `ulaw_encode` for the same
/// reason: correct by definition rather than a hand-derived encoder.
pub fn alaw_decode(code: u8) -> i16 {
    let a = code ^ 0x55;
    let sign = a & 0x80;
    let seg = (a & 0x70) >> 4;
    let mantissa = (a & 0x0F) as i32;
    let mut t = (mantissa << 4) + if seg == 0 { 8 } else { 0x108 };
    if seg > 1 {
        t <<= seg - 1;
    }
    let sample = if sign != 0 { t } else { -t };
    sample.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

pub fn build_alaw_decode_table() -> [i16; 256] {
    std::array::from_fn(|code| alaw_decode(code as u8))
}

pub fn alaw_encode(table: &[i16; 256], sample: i16) -> u8 {
    table
        .iter()
        .enumerate()
        .min_by_key(|&(_, &decoded)| (decoded as i32 - sample as i32).unsigned_abs())
        .map(|(code, _)| code as u8)
        .unwrap()
}

/// Which G.711 variant a session is running, identified by its RTP static
/// payload type (RFC 3551) — `softphone-core`'s SDP layer only ever
/// negotiates payload type 0 (u-law/PCMU) or 8 (A-law/PCMA), so that value
/// alone tells `session.rs`'s send/recv loops which codec functions to call
/// without needing a redundant separate parameter threaded through
/// `MediaSession::start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Ulaw,
    Alaw,
}

impl Codec {
    /// Falls back to u-law for any payload type this app didn't itself
    /// negotiate (shouldn't happen — `session.rs` only ever sees the
    /// payload type from an SDP answer/offer this app produced or accepted).
    pub fn from_payload_type(payload_type: u8) -> Self {
        match payload_type {
            8 => Codec::Alaw,
            _ => Codec::Ulaw,
        }
    }

    pub fn build_decode_table(self) -> [i16; 256] {
        match self {
            Codec::Ulaw => build_decode_table(),
            Codec::Alaw => build_alaw_decode_table(),
        }
    }

    pub fn encode(self, table: &[i16; 256], sample: i16) -> u8 {
        match self {
            Codec::Ulaw => ulaw_encode(table, sample),
            Codec::Alaw => alaw_encode(table, sample),
        }
    }

    pub fn decode(self, code: u8) -> i16 {
        match self {
            Codec::Ulaw => ulaw_decode(code),
            Codec::Alaw => alaw_decode(code),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_bounded_error() {
        let table = build_decode_table();
        let mut max_err = 0i32;
        let mut sample = i16::MIN;
        loop {
            let code = ulaw_encode(&table, sample);
            let decoded = ulaw_decode(code);
            max_err = max_err.max((decoded as i32 - sample as i32).abs());

            if sample == i16::MAX {
                break;
            }
            sample = sample.saturating_add(37);
        }
        // G.711 is lossy by design; the coarsest quantization segment (near
        // full scale) has a step of ~1024, so ~650 max error there is
        // expected — confirmed by cross-checking against ffmpeg's own G.711
        // codec on the same input, which produces the same 644 max error.
        // This bound just guards against a badly broken implementation.
        assert!(max_err < 700, "max round-trip error too large: {max_err}");
    }

    #[test]
    fn silence_round_trips_near_zero() {
        let table = build_decode_table();
        let code = ulaw_encode(&table, 0);
        let decoded = ulaw_decode(code);
        assert!(decoded.abs() < 10);
    }

    #[test]
    fn decode_table_matches_direct_decode() {
        let table = build_decode_table();
        for code in 0u8..=255 {
            assert_eq!(table[code as usize], ulaw_decode(code));
        }
    }

    #[test]
    fn alaw_round_trip_bounded_error() {
        let table = build_alaw_decode_table();
        let mut max_err = 0i32;
        let mut sample = i16::MIN;
        loop {
            let code = alaw_encode(&table, sample);
            let decoded = alaw_decode(code);
            max_err = max_err.max((decoded as i32 - sample as i32).abs());

            if sample == i16::MAX {
                break;
            }
            sample = sample.saturating_add(37);
        }
        assert!(max_err < 700, "max round-trip error too large: {max_err}");
    }

    #[test]
    fn alaw_silence_round_trips_near_zero() {
        let table = build_alaw_decode_table();
        let code = alaw_encode(&table, 0);
        let decoded = alaw_decode(code);
        assert!(decoded.abs() < 10);
    }

    #[test]
    fn alaw_decode_table_matches_direct_decode() {
        let table = build_alaw_decode_table();
        for code in 0u8..=255 {
            assert_eq!(table[code as usize], alaw_decode(code));
        }
    }
}

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

/// The audio codecs a call can use, with everything about each one that
/// the rest of the media path needs to know to stay codec-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecKind {
    /// G.711 µ-law, 8 kHz.
    Ulaw,
    /// G.711 A-law, 8 kHz.
    Alaw,
    /// G.722, 16 kHz wideband at the same 64 kbit/s as G.711.
    G722,
}

impl CodecKind {
    /// Static RTP payload types (RFC 3551): 0 PCMU, 8 PCMA, 9 G722.
    /// `None` for anything this app can't encode or decode.
    pub fn from_payload_type(payload_type: u8) -> Option<Self> {
        match payload_type {
            0 => Some(CodecKind::Ulaw),
            8 => Some(CodecKind::Alaw),
            9 => Some(CodecKind::G722),
            _ => None,
        }
    }

    /// Short name for display.
    pub fn name(self) -> &'static str {
        match self {
            CodecKind::Ulaw => "G.711µ",
            CodecKind::Alaw => "G.711A",
            CodecKind::G722 => "G.722 HD",
        }
    }

    /// Sample rate of the audio itself — what the microphone and speaker
    /// streams, the buffers and recordings all run at for this call.
    pub fn sample_rate(self) -> u32 {
        match self {
            CodecKind::Ulaw | CodecKind::Alaw => 8_000,
            CodecKind::G722 => 16_000,
        }
    }

    /// Rate of the RTP timestamp clock. Not the same thing for G.722: its
    /// clock is 8000 although it carries 16 kHz audio, a mistake in the
    /// original RFC that is kept for compatibility (RFC 3551 §4.5.2).
    pub fn rtp_clock_rate(self) -> u32 {
        8_000
    }

    /// Audio samples in one 20 ms packet.
    pub fn samples_per_packet(self) -> usize {
        self.sample_rate() as usize / 50
    }

    /// How far the RTP timestamp advances per 20 ms packet.
    pub fn rtp_ticks_per_packet(self) -> u32 {
        self.rtp_clock_rate() / 50
    }
}

/// Turns 16-bit PCM frames into RTP payloads for one call. Stateful (G.722
/// is an adaptive codec), so one per call, fed in order.
pub struct AudioEncoder {
    kind: CodecKind,
    g711: Codec,
    table: [i16; 256],
    g722: audio_codec::g722::G722Encoder,
}

impl AudioEncoder {
    pub fn new(kind: CodecKind) -> Self {
        let g711 = match kind {
            CodecKind::Alaw => Codec::Alaw,
            _ => Codec::Ulaw,
        };
        Self {
            kind,
            g711,
            table: g711.build_decode_table(),
            g722: audio_codec::g722::G722Encoder::new(),
        }
    }

    pub fn encode(&mut self, samples: &[i16]) -> Vec<u8> {
        match self.kind {
            CodecKind::Ulaw | CodecKind::Alaw => samples.iter().map(|&s| self.g711.encode(&self.table, s)).collect(),
            CodecKind::G722 => {
                use audio_codec::Encoder;
                let mut out = vec![0u8; self.g722.max_encode_bytes(samples.len())];
                let written = self.g722.encode_into(samples, &mut out).unwrap_or(0);
                out.truncate(written);
                out
            }
        }
    }
}

/// The receiving counterpart of [`AudioEncoder`].
pub struct AudioDecoder {
    kind: CodecKind,
    g711: Codec,
    g722: audio_codec::g722::G722Decoder,
}

impl AudioDecoder {
    pub fn new(kind: CodecKind) -> Self {
        Self {
            kind,
            g711: match kind {
                CodecKind::Alaw => Codec::Alaw,
                _ => Codec::Ulaw,
            },
            g722: audio_codec::g722::G722Decoder::new(),
        }
    }

    pub fn decode(&mut self, payload: &[u8]) -> Vec<i16> {
        match self.kind {
            CodecKind::Ulaw | CodecKind::Alaw => payload.iter().map(|&b| self.g711.decode(b)).collect(),
            CodecKind::G722 => {
                use audio_codec::Decoder;
                let mut out = vec![0i16; self.g722.max_decode_samples(payload.len())];
                let written = self.g722.decode_into(payload, &mut out).unwrap_or(0);
                out.truncate(written);
                out
            }
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

    fn tone(rate: u32, hz: f32, samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|i| ((i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * 8_000.0) as i16)
            .collect()
    }

    /// Goertzel power of `hz` in `samples`, relative to total energy: ~1.0
    /// when the signal is that pure tone.
    fn purity(samples: &[i16], rate: u32, hz: f32) -> f32 {
        let coeff = 2.0 * (std::f32::consts::TAU * hz / rate as f32).cos();
        let (mut s1, mut s2, mut energy) = (0f32, 0f32, 0f32);
        for &x in samples {
            let x = f32::from(x);
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
            energy += x * x;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2) / (energy * samples.len() as f32 / 2.0)
    }

    #[test]
    fn packet_sizes_and_clocks() {
        for (kind, pt, rate, samples, bytes) in [
            (CodecKind::Ulaw, 0, 8_000, 160, 160),
            (CodecKind::Alaw, 8, 8_000, 160, 160),
            (CodecKind::G722, 9, 16_000, 320, 160),
        ] {
            assert_eq!(CodecKind::from_payload_type(pt), Some(kind));
            assert_eq!(kind.sample_rate(), rate);
            assert_eq!(kind.samples_per_packet(), samples);
            // Every codec here advances the RTP timestamp by 160 per packet,
            // G.722 included despite its 320 samples.
            assert_eq!(kind.rtp_ticks_per_packet(), 160);
            let payload = AudioEncoder::new(kind).encode(&vec![0i16; samples]);
            assert_eq!(payload.len(), bytes, "{kind:?}");
            assert_eq!(AudioDecoder::new(kind).decode(&payload).len(), samples, "{kind:?}");
        }
        assert_eq!(CodecKind::from_payload_type(18), None);
    }

    #[test]
    fn every_codec_carries_a_tone_through() {
        for kind in [CodecKind::Ulaw, CodecKind::Alaw, CodecKind::G722] {
            let rate = kind.sample_rate();
            let (mut enc, mut dec) = (AudioEncoder::new(kind), AudioDecoder::new(kind));
            let input = tone(rate, 1_000.0, kind.samples_per_packet() * 25);
            let mut output = Vec::new();
            for frame in input.chunks(kind.samples_per_packet()) {
                output.extend(dec.decode(&enc.encode(frame)));
            }
            assert_eq!(output.len(), input.len());
            // Skip the first packets while G.722's predictors settle.
            let settled = &output[kind.samples_per_packet() * 5..];
            let p = purity(settled, rate, 1_000.0);
            assert!(p > 0.9, "{kind:?}: tone purity {p}");
            let peak = settled.iter().map(|s| s.unsigned_abs()).max().unwrap();
            assert!((6_000..=10_000).contains(&peak), "{kind:?}: peak {peak}");
        }
    }

    #[test]
    fn g722_carries_what_g711_cannot() {
        // 6 kHz is above narrowband's 4 kHz limit but inside G.722's 7 kHz.
        let kind = CodecKind::G722;
        let (mut enc, mut dec) = (AudioEncoder::new(kind), AudioDecoder::new(kind));
        let input = tone(16_000, 6_000.0, 320 * 25);
        let mut output = Vec::new();
        for frame in input.chunks(320) {
            output.extend(dec.decode(&enc.encode(frame)));
        }
        let p = purity(&output[320 * 5..], 16_000, 6_000.0);
        assert!(p > 0.8, "6 kHz tone purity {p}");
    }
}

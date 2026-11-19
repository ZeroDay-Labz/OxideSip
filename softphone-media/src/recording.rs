//! Writing a finished call recording (see `session.rs`'s `RecordMixer`) to
//! a WAV file on disk.

use std::io;
use std::path::Path;

/// Minimal canonical 44-byte-header PCM WAV: mono, 16-bit, at
/// `sample_rate` — the rate of the call the samples came from
/// (`MediaSession::sample_rate`: 8 kHz for G.711, 16 kHz for G.722), written
/// as recorded with no resampling.
pub fn write_wav(path: &Path, samples: &[i16], sample_rate: u32) -> io::Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = sample_rate * 2;
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&byte_rate.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes()); // block align
    bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, bytes)
}

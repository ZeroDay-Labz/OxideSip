//! Writing a finished call recording (see `session.rs`'s `RecordMixer`) to
//! a WAV file on disk.

use std::io;
use std::path::Path;

/// This call audio path is fixed at mono 8kHz 16-bit PCM throughout the
/// app (see `codec.rs`/`session.rs`), so recordings are written the same
/// way — no resampling, no format negotiation.
const SAMPLE_RATE: u32 = 8000;

/// Minimal canonical 44-byte-header PCM WAV — same approach as
/// `tone.rs`'s internal writer, exposed here as a small public utility
/// since recordings are written from `softphone-ui`, not this crate.
pub fn write_wav(path: &Path, samples: &[i16]) -> io::Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = SAMPLE_RATE * 2;
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
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

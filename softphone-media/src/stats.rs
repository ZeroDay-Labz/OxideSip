//! Live receive-quality numbers for one call, for the UI to show.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Written by the RTP receive loop and the realtime playback callback, read
/// by [`crate::MediaSession::stats`]. Plain atomics: nothing here may block
/// the audio thread.
#[derive(Default)]
pub(crate) struct StatsShared {
    pub buffered_samples: AtomicU32,
    pub margin_samples: AtomicU32,
    pub underruns: AtomicU32,
    pub received: AtomicU64,
    pub lost: AtomicU64,
    /// RFC 3550 interarrival jitter in milliseconds, as `f32` bits.
    pub jitter_ms_bits: AtomicU32,
}

/// A snapshot of how a call's incoming audio is doing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CallStats {
    /// Audio currently waiting in the playback buffer: the delay this app
    /// is adding on the receive side right now.
    pub buffer_ms: u32,
    /// The cushion the adaptive buffer is currently aiming to keep.
    pub margin_ms: u32,
    /// Times playback ran out of audio.
    pub underruns: u32,
    /// Audio packets received.
    pub received: u64,
    /// Audio packets that never arrived.
    pub lost: u64,
    /// RFC 3550 interarrival jitter.
    pub jitter_ms: f32,
}

impl CallStats {
    /// Share of expected packets that were lost, 0.0–100.0.
    pub fn loss_percent(&self) -> f32 {
        let expected = self.received + self.lost;
        if expected == 0 {
            0.0
        } else {
            self.lost as f32 * 100.0 / expected as f32
        }
    }
}

impl StatsShared {
    pub fn snapshot(&self, sample_rate: u32) -> CallStats {
        let ms = |samples: u32| samples * 1000 / sample_rate;
        CallStats {
            buffer_ms: ms(self.buffered_samples.load(Ordering::Relaxed)),
            margin_ms: ms(self.margin_samples.load(Ordering::Relaxed)),
            underruns: self.underruns.load(Ordering::Relaxed),
            received: self.received.load(Ordering::Relaxed),
            lost: self.lost.load(Ordering::Relaxed),
            jitter_ms: f32::from_bits(self.jitter_ms_bits.load(Ordering::Relaxed)),
        }
    }
}

/// RFC 3550 §6.4.1 / appendix A.3, A.8 receiver statistics for one stream:
/// packets lost (expected minus received, from extended sequence numbers)
/// and interarrival jitter.
#[derive(Default)]
pub(crate) struct ReceiverStats {
    base_seq: Option<u16>,
    max_seq: u16,
    cycles: u64,
    received: u64,
    last_transit: Option<f64>,
    jitter: f64,
}

impl ReceiverStats {
    /// Call once per audio packet with its sequence number, RTP timestamp
    /// and arrival time (seconds on any monotonic clock). `clock_rate` is the
    /// RTP timestamp rate.
    pub fn on_packet(&mut self, seq: u16, timestamp: u32, arrival_secs: f64, clock_rate: u32) {
        match self.base_seq {
            None => {
                self.base_seq = Some(seq);
                self.max_seq = seq;
            }
            Some(_) => {
                let delta = seq.wrapping_sub(self.max_seq);
                if delta != 0 && delta < 0x8000 {
                    if seq < self.max_seq {
                        self.cycles += 1 << 16;
                    }
                    self.max_seq = seq;
                }
            }
        }
        self.received += 1;

        // Transit time difference between consecutive packets, in RTP
        // timestamp units; only differences matter, so the two clocks'
        // offsets (and the timestamp's wrapping) cancel out.
        let transit = arrival_secs * f64::from(clock_rate) - f64::from(timestamp);
        if let Some(last) = self.last_transit {
            let mut d = (transit - last).abs();
            // A timestamp wrap or a new talk spurt makes one huge jump; skip it.
            if d > f64::from(clock_rate) * 10.0 {
                d = 0.0;
            }
            self.jitter += (d - self.jitter) / 16.0;
        }
        self.last_transit = Some(transit);
    }

    pub fn lost(&self) -> u64 {
        let Some(base) = self.base_seq else { return 0 };
        let expected = self.cycles + u64::from(self.max_seq) - u64::from(base) + 1;
        expected.saturating_sub(self.received)
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn jitter_ms(&self, clock_rate: u32) -> f32 {
        (self.jitter * 1000.0 / f64::from(clock_rate)) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_stream_has_no_loss_or_jitter() {
        let mut s = ReceiverStats::default();
        for i in 0..500u32 {
            s.on_packet(i as u16, i * 160, f64::from(i) * 0.020, 8000);
        }
        assert_eq!((s.received(), s.lost()), (500, 0));
        assert!(s.jitter_ms(8000) < 0.01);
    }

    #[test]
    fn gaps_count_as_lost_across_a_sequence_wrap() {
        let mut s = ReceiverStats::default();
        // 65530..=65535 and 0..=9 is 16 packets; drop three of them.
        let seqs: Vec<u16> = (65530..=65535u16).chain(0..=9).collect();
        for (i, &seq) in seqs.iter().enumerate() {
            if [65533, 2, 3].contains(&seq) {
                continue;
            }
            s.on_packet(seq, i as u32 * 160, i as f64 * 0.020, 8000);
        }
        assert_eq!((s.received(), s.lost()), (13, 3));
    }

    #[test]
    fn reordered_and_duplicate_packets_are_not_loss() {
        let mut s = ReceiverStats::default();
        for (i, seq) in [1u16, 2, 4, 3, 5, 5].into_iter().enumerate() {
            s.on_packet(seq, u32::from(seq) * 160, i as f64 * 0.020, 8000);
        }
        assert_eq!(s.lost(), 0);
    }

    #[test]
    fn uneven_arrival_shows_up_as_jitter() {
        let mut s = ReceiverStats::default();
        for i in 0..400u32 {
            // Every other packet 10ms late.
            let late = if i % 2 == 1 { 0.010 } else { 0.0 };
            s.on_packet(i as u16, i * 160, f64::from(i) * 0.020 + late, 8000);
        }
        let j = s.jitter_ms(8000);
        assert!((9.0..=10.5).contains(&j), "jitter {j}");
    }

    #[test]
    fn loss_percent() {
        let stats = CallStats {
            received: 95,
            lost: 5,
            ..CallStats::default()
        };
        assert_eq!(stats.loss_percent(), 5.0);
        assert_eq!(CallStats::default().loss_percent(), 0.0);
    }
}

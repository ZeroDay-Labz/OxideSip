//! Automatic level control for speech, one instance per direction.
//!
//! Callers arrive at very different levels (a PSTN leg is often 15–20 dB
//! quieter than a desk phone on the same PBX) and so do microphones. This
//! brings speech toward one target level so the volume slider means the same
//! thing on every call: quiet speech is raised (up to `MAX_GAIN`), loud
//! speech is lowered, and nothing is ever allowed to clip. Background noise
//! between words is left alone rather than pumped up.

/// Where speech peaks are steered to: about -9 dBFS.
const TARGET_PEAK: f32 = 11_600.0;
/// Most a quiet signal is boosted: +18 dB.
const MAX_GAIN: f32 = 8.0;
/// Most a loud signal is cut: -6 dB.
const MIN_GAIN: f32 = 0.5;
/// Frames peaking below this (about -40 dBFS) are treated as background
/// noise: the gain is held, not adapted.
const NOISE_GATE: f32 = 330.0;
/// Hard ceiling for any output sample, just under full scale.
const CEILING: f32 = 32_000.0;
/// How fast the speech-level estimate falls back per 20ms frame when speech
/// gets quieter (it follows louder speech immediately).
const ENVELOPE_RELEASE: f32 = 0.985;
/// Per-frame step toward a higher gain (slow: about a second to settle) and
/// toward a lower one (fast: loud speech is tamed within a few frames).
const GAIN_RISE: f32 = 0.04;
const GAIN_FALL: f32 = 0.4;

pub struct Agc {
    gain: f32,
    /// Running estimate of the speech peak level.
    envelope: f32,
}

impl Default for Agc {
    fn default() -> Self {
        Self {
            gain: 1.0,
            envelope: 0.0,
        }
    }
}

impl Agc {
    /// Levels one frame (nominally 20ms) in place.
    pub fn process(&mut self, frame: &mut [i16]) {
        if frame.is_empty() {
            return;
        }
        let peak = frame.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as f32;
        let previous_gain = self.gain;

        if peak > NOISE_GATE {
            self.envelope = if peak > self.envelope {
                peak
            } else {
                (self.envelope * ENVELOPE_RELEASE).max(peak)
            };
            let wanted = (TARGET_PEAK / self.envelope).clamp(MIN_GAIN, MAX_GAIN);
            let rate = if wanted < self.gain { GAIN_FALL } else { GAIN_RISE };
            self.gain += (wanted - self.gain) * rate;
        }
        // Whatever the adaptation says, this frame must not clip.
        if peak * self.gain > CEILING {
            self.gain = CEILING / peak;
        }

        // Ramp across the frame so a gain change is never a step.
        let step = (self.gain - previous_gain) / frame.len() as f32;
        let mut gain = previous_gain;
        for sample in frame.iter_mut() {
            gain += step;
            *sample = (f32::from(*sample) * gain).clamp(-CEILING, CEILING) as i16;
        }
    }

    /// The gain currently applied (1.0 = unchanged).
    pub fn gain(&self) -> f32 {
        self.gain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `seconds` of a 440Hz tone of `amplitude` through `agc` in 20ms
    /// frames; returns the peak of the last frame and the highest peak ever.
    fn run_tone(agc: &mut Agc, amplitude: f32, seconds: f32) -> (u16, u16) {
        let mut phase = 0f32;
        let (mut last, mut highest) = (0, 0);
        for _ in 0..(seconds * 50.0) as usize {
            let mut frame = [0i16; 160];
            for s in &mut frame {
                *s = (phase.sin() * amplitude) as i16;
                phase += std::f32::consts::TAU * 440.0 / 8000.0;
            }
            agc.process(&mut frame);
            last = frame.iter().map(|s| s.unsigned_abs()).max().unwrap();
            highest = highest.max(last);
        }
        (last, highest)
    }

    #[test]
    fn quiet_speech_is_raised_to_the_target() {
        let mut agc = Agc::default();
        let (peak, _) = run_tone(&mut agc, 3_000.0, 4.0);
        assert!((10_500..=12_500).contains(&peak), "peak {peak}");
    }

    #[test]
    fn very_quiet_speech_gets_the_maximum_boost_and_no_more() {
        let mut agc = Agc::default();
        let (peak, _) = run_tone(&mut agc, 600.0, 6.0);
        assert!((agc.gain() - MAX_GAIN).abs() < 0.2, "gain {}", agc.gain());
        assert!((4_400..=4_900).contains(&peak), "peak {peak}");
    }

    #[test]
    fn loud_speech_is_lowered_quickly() {
        let mut agc = Agc::default();
        let (peak, _) = run_tone(&mut agc, 30_000.0, 0.5);
        assert!((14_500..=16_000).contains(&peak), "peak {peak}");
    }

    #[test]
    fn background_noise_is_not_amplified() {
        let mut agc = Agc::default();
        let (peak, _) = run_tone(&mut agc, 200.0, 5.0);
        assert_eq!(agc.gain(), 1.0);
        assert!(peak <= 200);
    }

    #[test]
    fn a_sudden_shout_after_a_boost_never_clips() {
        let mut agc = Agc::default();
        run_tone(&mut agc, 600.0, 6.0); // gain is now at maximum
        let (_, highest) = run_tone(&mut agc, 32_000.0, 1.0);
        assert!(f32::from(highest) <= CEILING, "peak {highest}");
    }

    #[test]
    fn gain_holds_through_pauses_between_words() {
        let mut agc = Agc::default();
        run_tone(&mut agc, 3_000.0, 4.0);
        let before = agc.gain();
        run_tone(&mut agc, 50.0, 3.0);
        assert_eq!(agc.gain(), before);
    }
}

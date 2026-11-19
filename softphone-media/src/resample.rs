//! A small streaming sample-rate converter, for the one place two different
//! rates meet inside the app: bridging two calls that negotiated different
//! codecs (say G.722 at 16 kHz and G.711 at 8 kHz) into a conference.
//! Everything that touches a sound device is resampled by PipeWire instead.

/// Linear interpolation between neighbouring samples, with a two-sample
/// average first when reducing the rate to take the edge off aliasing.
/// Plenty for speech on a conference leg; not a general-purpose resampler.
pub struct LinearResampler {
    /// Input samples advanced per output sample (`from / to`).
    step: f64,
    /// Position of the next output sample, in input samples, relative to
    /// the start of the next chunk (so it can be as low as -1.0: between the
    /// previous chunk's last sample and this chunk's first).
    pos: f64,
    /// The previous chunk's last (filtered) sample.
    prev: i16,
    /// The previous chunk's last raw sample, for the averaging filter.
    prev_raw: i16,
}

impl LinearResampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Self {
            step: f64::from(from_rate) / f64::from(to_rate),
            pos: 0.0,
            prev: 0,
            prev_raw: 0,
        }
    }

    /// Converts `input`, appending the result to `out`. Chunks may be any
    /// length; state carries over so consecutive calls join seamlessly.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if input.is_empty() {
            return;
        }
        let filtered: Vec<i16>;
        let samples: &[i16] = if self.step > 1.0 {
            let mut last = self.prev_raw;
            filtered = input
                .iter()
                .map(|&s| {
                    let averaged = ((i32::from(s) + i32::from(last)) / 2) as i16;
                    last = s;
                    averaged
                })
                .collect();
            &filtered
        } else {
            input
        };
        self.prev_raw = input[input.len() - 1];

        let last_index = (samples.len() - 1) as f64;
        while self.pos <= last_index {
            let index = self.pos.floor();
            let frac = self.pos - index;
            let (a, b) = if index < 0.0 {
                (self.prev, samples[0])
            } else {
                let i = index as usize;
                (samples[i], samples[(i + 1).min(samples.len() - 1)])
            };
            out.push((f64::from(a) + (f64::from(b) - f64::from(a)) * frac).round() as i16);
            self.pos += self.step;
        }
        self.pos -= samples.len() as f64;
        self.prev = samples[samples.len() - 1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f32, samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|i| ((i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * 8_000.0) as i16)
            .collect()
    }

    /// Runs `input` through in uneven chunks, as a real stream would arrive.
    fn run(from: u32, to: u32, input: &[i16]) -> Vec<i16> {
        let mut resampler = LinearResampler::new(from, to);
        let mut out = Vec::new();
        for chunk in input.chunks(37) {
            resampler.process(chunk, &mut out);
        }
        out
    }

    #[test]
    fn doubling_and_halving_give_the_right_amount_of_audio() {
        let up = run(8_000, 16_000, &tone(8_000, 440.0, 8_000));
        assert!((15_998..=16_000).contains(&up.len()), "{}", up.len());
        let down = run(16_000, 8_000, &tone(16_000, 440.0, 16_000));
        assert!((7_999..=8_000).contains(&down.len()), "{}", down.len());
    }

    #[test]
    fn a_tone_comes_out_as_the_same_tone() {
        for (from, to) in [(8_000, 16_000), (16_000, 8_000)] {
            let out = run(from, to, &tone(from, 440.0, from as usize));
            let expected = tone(to, 440.0, out.len());
            // Compare away from the very start; allow for interpolation
            // error and the averaging filter's slight level drop and delay.
            let worst = out[100..]
                .iter()
                .zip(&expected[100..])
                .map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs())
                .max()
                .unwrap();
            assert!(worst < 1_200, "{from}->{to}: worst sample error {worst}");
        }
    }

    #[test]
    fn chunk_boundaries_leave_no_seams() {
        let input = tone(8_000, 300.0, 4_000);
        let whole = {
            let mut r = LinearResampler::new(8_000, 16_000);
            let mut out = Vec::new();
            r.process(&input, &mut out);
            out
        };
        assert_eq!(run(8_000, 16_000, &input), whole);
    }
}

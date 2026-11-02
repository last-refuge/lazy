//! Converting between a device's sample rate and 48 kHz, by linear interpolation: plenty for
//! speech, and devices usually run at 48 or 44.1 kHz anyway.

/// Converts a stream of mono samples from one rate to another, carrying its position across calls.
pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Where the next output sample falls in `[last] + input`, in input samples.
    position: f64,
    /// The previous call's final sample.
    last: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Resampler {
        Resampler {
            step: f64::from(from) / f64::from(to),
            position: 1.0,
            last: 0.0,
        }
    }

    /// Converts `input`, appending to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        if self.step == 1.0 {
            out.extend_from_slice(input);
            return;
        }
        // x[0] is `last`, x[i + 1] is input[i].
        let x = |i: usize| if i == 0 { self.last } else { input[i - 1] };
        let len = input.len() + 1;
        while self.position + 1.0 < len as f64 {
            let i = self.position.floor() as usize;
            let frac = (self.position - i as f64) as f32;
            let (a, b) = (x(i), x(i + 1));
            out.push(a + (b - a) * frac);
            self.position += self.step;
        }
        // The last input sample becomes the next call's x[0].
        self.position -= input.len() as f64;
        self.last = input[input.len() - 1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_convert_in_proportion() {
        let mut down = Resampler::new(48_000, 44_100);
        let mut out = Vec::new();
        for _ in 0..100 {
            down.process(&[0.5; 480], &mut out);
        }
        assert!((out.len() as i64 - 44_100).abs() <= 2, "{}", out.len());
        assert!(out[10..].iter().all(|&s| (s - 0.5).abs() < 1e-6));

        let mut up = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..100 {
            up.process(&[0.25; 441], &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() <= 2, "{}", out.len());
    }

    #[test]
    fn the_same_rate_passes_through() {
        let mut same = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        same.process(&[1.0, 2.0], &mut out);
        assert_eq!(out, [1.0, 2.0]);
    }
}

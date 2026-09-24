//! Small allocation-free DSP building blocks for the audio thread.
//!
//! Currently just the input DC blocker ([`DcBlocker`]), used to strip a
//! microphone's constant DC offset before it reaches a noise-suppression
//! engine or the input level meter.

/// One-pole DC-blocking high-pass filter.
///
/// Implements the classic difference-equation DC blocker:
///
/// `y[n] = x[n] - x[n-1] + r * y[n-1]`
///
/// The transfer function has a zero at exactly `z = 1` (from the `x - x1`
/// term), so the DC gain is exactly zero regardless of `f32` rounding of the
/// pole `r`. `r` controls how quickly the filter's step response settles and
/// how much low end it removes; it is derived from a cutoff frequency via
/// `r = exp(-2*pi*fc/fs)`.
///
/// All fields are plain scalars, so `DcBlocker` is `Copy` and carries no heap
/// allocation: constructing, resetting, and running it are all safe on the
/// real-time audio thread.
#[derive(Debug, Clone, Copy)]
pub struct DcBlocker {
    /// Pole coefficient, `exp(-2*pi*fc/fs)`.
    r: f32,
    /// Previous (sanitized) input sample.
    x1: f32,
    /// Previous output sample.
    y1: f32,
    /// Whether the filter has seen its first sample since construction or
    /// the last [`reset`](Self::reset) call.
    primed: bool,
}

impl DcBlocker {
    /// Create a new DC blocker with the given cutoff frequency and sample
    /// rate.
    ///
    /// `r` is computed as `exp(-2*pi*cutoff_hz/sample_rate)` in `f64` and
    /// then cast to `f32`, which keeps the coefficient accurate even though
    /// the recurrence itself runs in `f32`. The filter starts unprimed: the
    /// first sample passed to [`process_in_place`](Self::process_in_place)
    /// primes it rather than producing a full-scale step.
    pub fn new(cutoff_hz: f32, sample_rate: u32) -> Self {
        debug_assert!(
            cutoff_hz > 0.0 && cutoff_hz < sample_rate as f32 / 2.0,
            "DcBlocker cutoff must be between 0 Hz and Nyquist, got {cutoff_hz} Hz at {sample_rate} Hz"
        );
        let r = (-2.0 * std::f64::consts::PI * f64::from(cutoff_hz) / f64::from(sample_rate)).exp();
        Self {
            r: r as f32,
            x1: 0.0,
            y1: 0.0,
            primed: false,
        }
    }

    /// Reset the filter state and re-arm priming.
    ///
    /// Call this whenever the input's DC level can jump discontinuously and
    /// unpredictably — a new capture device, a resumed stream after a stop —
    /// so the next sample primes the filter instead of injecting a DC step
    /// (an audible click) into the engine. Do *not* call this for events
    /// where the input DC is continuous, such as an engine swap.
    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.y1 = 0.0;
        self.primed = false;
    }

    /// Filter `buf` in place.
    ///
    /// Non-finite input samples (`NaN`, `+Inf`, `-Inf`) are sanitized to
    /// `0.0` before entering the recurrence, so one bad sample from a
    /// misbehaving capture device cannot permanently poison the carried
    /// filter state or produce non-finite output. The very first sample seen
    /// after construction or [`reset`](Self::reset) primes `x1` instead of
    /// producing an output step. After every sample, the computed output is
    /// flushed to exactly `0.0` once its magnitude decays below `1e-20`
    /// (well above the smallest `f32` denormal, ~1.4e-45): during sustained
    /// digital silence the geometric decay otherwise spends many samples
    /// grinding through ever-less-precise denormals (each multiply losing
    /// mantissa bits) before it can underflow to a true zero on its own.
    /// Flushing early guarantees exact silence output and keeps the
    /// recurrence out of the (slow, on most CPUs) denormal range, regardless
    /// of how the caller chunks its buffers (a 480-sample real-time block or
    /// one giant call give the same result — see `block_size_invariant`).
    ///
    /// No allocation, no locks, no I/O: safe to call from the real-time audio
    /// thread on every processed block.
    pub fn process_in_place(&mut self, buf: &mut [f32]) {
        for sample in buf.iter_mut() {
            let s = if sample.is_finite() { *sample } else { 0.0 };
            if !self.primed {
                self.x1 = s;
                self.primed = true;
            }
            let mut y = s - self.x1 + self.r * self.y1;
            self.x1 = s;
            if y.abs() < 1e-20 {
                y = 0.0;
            }
            self.y1 = y;
            *sample = y;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    /// RMS (root mean square) of a sample buffer.
    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
        (sum_sq / samples.len() as f32).sqrt()
    }

    #[test]
    fn removes_constant_offset_immediately_after_reset() {
        for &dc in &[0.109f32, 1.10, -0.5] {
            let mut db = DcBlocker::new(20.0, SR);
            let mut buf = vec![dc; 480];
            db.process_in_place(&mut buf);
            let max_abs = buf.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
            assert!(
                max_abs < 1e-6,
                "dc={dc}: first-block max |y| = {max_abs}, expected < 1e-6 (no step transient)"
            );
        }
    }

    #[test]
    fn settles_dc_step_within_100ms() {
        let mut db = DcBlocker::new(20.0, SR);
        // Prime on zeros first (no step of interest here).
        let mut zeros = vec![0.0f32; 480];
        db.process_in_place(&mut zeros);

        // 200 ms of a 0.1 DC step.
        let mut step = vec![0.1f32; 9_600];
        db.process_in_place(&mut step);

        // Every sample from 100 ms (4800 samples) onward must be settled.
        for (i, &y) in step.iter().enumerate().skip(4_800) {
            assert!(
                y.abs() < 1e-4,
                "sample {i} (>= 100ms after the step) = {y}, expected < 1e-4"
            );
        }
    }

    #[test]
    fn passband_100hz_to_8khz_within_half_db() {
        for &freq in &[100.0f32, 300.0, 1_000.0, 4_000.0, 8_000.0] {
            let mut db = DcBlocker::new(20.0, SR);
            let n = SR as usize; // 1 s
            let amp = 0.5f32;
            let mut buf: Vec<f32> = (0..n)
                .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SR as f32).sin())
                .collect();
            db.process_in_place(&mut buf);

            // Discard the first 200 ms so the filter's own settling doesn't
            // bias the RMS measurement.
            let skip = SR as usize / 5;
            let out_rms = rms(&buf[skip..]);
            let in_rms = amp / std::f32::consts::SQRT_2;
            let gain_db = 20.0 * (out_rms / in_rms).log10();
            assert!(
                gain_db.abs() <= 0.5,
                "freq={freq}Hz: gain = {gain_db:.3} dB, expected within +-0.5 dB"
            );
        }
    }

    #[test]
    fn dc_plus_tone_keeps_tone_rms() {
        let mut db = DcBlocker::new(20.0, SR);
        let n = SR as usize; // 1 s
        let dc = 0.109f32;
        let amp = 0.01f32;
        let freq = 300.0f32;
        let mut buf: Vec<f32> = (0..n)
            .map(|i| dc + amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SR as f32).sin())
            .collect();
        db.process_in_place(&mut buf);

        let skip = SR as usize / 5; // 200 ms
        let out_rms = rms(&buf[skip..]);
        let expected = amp / std::f32::consts::SQRT_2;
        let gain_db = 20.0 * (out_rms / expected).log10();
        assert!(
            gain_db.abs() <= 0.5,
            "dc+tone output rms = {out_rms}, expected ~{expected} (gain {gain_db:.3} dB)"
        );
    }

    #[test]
    fn block_size_invariant() {
        let n = SR as usize; // 1 s
        let dc = 0.109f32;
        let amp = 0.01f32;
        let freq = 1_000.0f32;
        let signal: Vec<f32> = (0..n)
            .map(|i| dc + amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SR as f32).sin())
            .collect();

        let mut whole = signal.clone();
        let mut db_whole = DcBlocker::new(20.0, SR);
        db_whole.process_in_place(&mut whole);

        for &chunk_size in &[480usize, 1, 1_024, 7] {
            let mut chunked = signal.clone();
            let mut db = DcBlocker::new(20.0, SR);
            for chunk in chunked.chunks_mut(chunk_size) {
                db.process_in_place(chunk);
            }
            for (i, (&a, &b)) in whole.iter().zip(chunked.iter()).enumerate() {
                assert!(
                    (a - b).abs() <= 1e-7,
                    "chunk_size={chunk_size}: sample {i} differs: whole={a}, chunked={b}"
                );
            }
        }
    }

    #[test]
    fn output_always_finite() {
        for &val in &[0.0f32, 1.0, -1.0, 2.0, 1.10] {
            let mut db = DcBlocker::new(20.0, SR);
            let mut buf = vec![val; 480];
            db.process_in_place(&mut buf);
            assert!(
                buf.iter().all(|v| v.is_finite()),
                "val={val}: non-finite output"
            );
        }

        // Alternating full-scale signal.
        let mut db = DcBlocker::new(20.0, SR);
        let mut buf: Vec<f32> = (0..480)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        db.process_in_place(&mut buf);
        assert!(buf.iter().all(|v| v.is_finite()));

        // A NaN/Inf sample amid a normal signal must not poison later output.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut db = DcBlocker::new(20.0, SR);
            let mut buf = vec![0.1f32; 960];
            buf[480] = bad;
            db.process_in_place(&mut buf);
            assert!(
                buf.iter().all(|v| v.is_finite()),
                "bad input {bad} produced a non-finite output"
            );
            assert!(
                buf[900].abs() < 1.0,
                "did not recover to finite, bounded output after bad sample: {}",
                buf[900]
            );
        }
    }

    #[test]
    fn flushes_to_exact_zero_in_silence() {
        let mut db = DcBlocker::new(20.0, SR);
        let mut signal = vec![0.5f32; 480];
        db.process_in_place(&mut signal);

        let mut silence = vec![0.0f32; SR as usize]; // 1 s
        db.process_in_place(&mut silence);

        let last_block = &silence[silence.len() - 480..];
        assert!(
            last_block.iter().all(|&v| v == 0.0),
            "last block after 1s of silence not exactly zero: {:?}",
            &last_block[..8]
        );
    }

    #[test]
    fn reset_rearms_priming() {
        let mut db = DcBlocker::new(20.0, SR);
        let mut buf = vec![0.109f32; 480];
        db.process_in_place(&mut buf);
        db.reset();
        let mut buf2 = vec![-0.3f32; 480];
        db.process_in_place(&mut buf2);
        let max_abs = buf2.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(
            max_abs < 1e-6,
            "after reset, first-block max |y| = {max_abs}, expected < 1e-6"
        );

        // Without reset, the same sequence must show a large first-sample
        // jump, proving this test can actually detect a missing reset.
        let mut db_no_reset = DcBlocker::new(20.0, SR);
        let mut buf = vec![0.109f32; 480];
        db_no_reset.process_in_place(&mut buf);
        let mut buf2 = vec![-0.3f32; 480];
        db_no_reset.process_in_place(&mut buf2);
        assert!(
            buf2[0].abs() > 0.3,
            "sanity check failed: without reset, the first sample should show \
             a large step, got {}",
            buf2[0]
        );
    }

    #[test]
    fn is_copy_and_in_place() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<DcBlocker>();

        let mut db = DcBlocker::new(20.0, SR);
        let mut buf = vec![0.109f32; 16];
        let ptr_before = buf.as_ptr();
        db.process_in_place(&mut buf);
        assert_eq!(
            buf.as_ptr(),
            ptr_before,
            "process_in_place must not reallocate the buffer"
        );
    }
}

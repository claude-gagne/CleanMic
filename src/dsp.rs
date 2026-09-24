//! Small allocation-free DSP building blocks for the audio thread.
//!
//! Holds the input DC blocker ([`DcBlocker`]), used to strip a microphone's
//! constant DC offset before it reaches a noise-suppression engine or the
//! input level meter, and the input auto-gain ([`AutoGain`]), a
//! speech-gated, boost-only leveler that runs immediately after the DC
//! blocker to bring a too-quiet microphone's speech up to a normal
//! conferencing level before the engine and the meter see it.

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

// ── Input auto-gain ─────────────────────────────────────────────────────────

/// Target speech level, in dBFS, that [`AutoGain`] tries to bring a speaker's
/// voice up to.
///
/// Chosen (quick task 260923-x24) to reproduce the owner's own working manual
/// fix: their laptop DMIC delivers speech at about -40 dBFS, and boosting it
/// with `wpctl set-volume 63 2.0` (PipeWire's cubic volume scale, about 8x
/// linear, so about +18 dB) landed around -22 dBFS and was usable. Speech
/// crest factor is roughly 12-20 dB, so -20 dBFS rms leaves about 19 dB of
/// headroom to [`LIMITER_CEILING_DBFS`] — the limiter only catches plosives
/// and laughs, not ordinary syllables. It also lands in the normal
/// conferencing range without maximizing loudness: downstream WebRTC AGCs
/// still fine-tune further, and a boosted -70 dBFS room floor only reaches
/// about -50 dBFS, which the suppression engines remove.
const TARGET_DBFS: f32 = -20.0;

/// Hard cap on the boost [`AutoGain`] will ever apply. Hot mics are out of
/// scope (see [`AutoGain`]'s boost-only combine) so there is no corresponding
/// floor below 0 dB.
const MAX_GAIN_DB: f32 = 30.0;

/// A frame must read at least this many dB above the tracked noise floor to
/// be treated as speech, so hiss and room tone are never mistaken for speech
/// (see [`AutoGain::frame_update`]).
const SPEECH_MARGIN_DB: f32 = 10.0;

/// A frame below this absolute level is never treated as speech, however far
/// above the (possibly very low) noise floor it reads.
const SPEECH_MIN_DBFS: f32 = -70.0;

/// A frame below this level is treated as digital silence: it never adapts
/// the gain, and (once the floor is primed) pulls the floor down to at most
/// this value. Exact zeros come from a muted or gated source.
const SILENCE_THRESHOLD_DBFS: f32 = -90.0;

/// After this many consecutive silent frames (500 ms at the 10 ms frame
/// rate), the next non-silent frame re-primes the noise floor to exactly
/// that frame's level instead of slowly rising back up from
/// [`SILENCE_THRESHOLD_DBFS`]. Without this, unmuting into a steady noisy
/// room would look like 30-40 dB of "speech" above a floor still stuck near
/// -90 dBFS and pump the gain. Short silent gaps (gated speech, below this
/// count) do not force a re-prime — they still pull the floor down on every
/// silent frame (see [`SILENCE_THRESHOLD_DBFS`]'s doc), which is what lets
/// the very next burst be recognized as speech.
const SILENT_REPRIME_FRAMES: u32 = 50;

/// Time constant for the noise floor's rise toward a higher frame level, in
/// the dB domain. The floor falls *instantly* to any lower frame (see
/// [`AutoGain::frame_update`]) but only rises this slowly, so a brief lull
/// cannot masquerade as a lower floor and cause a burst of false "speech".
const FLOOR_RISE_TAU_S: f32 = 1.0;

/// Time constant for the speech-level tracker, a power-domain one-pole
/// average over speech-gated frames only. Weights vowels like an
/// active-speech-level measurement.
const SPEECH_LEVEL_TAU_S: f32 = 0.5;

/// Maximum rate at which the gain is allowed to rise, in dB per second.
/// Converges over several seconds of speech rather than snapping.
const UP_SLEW_DB_PER_S: f32 = 6.0;

/// Maximum rate at which the gain is allowed to fall, in dB per second.
/// Three times [`UP_SLEW_DB_PER_S`] so the leveler backs off quickly from a
/// loud passage instead of over-boosting it.
const DOWN_SLEW_DB_PER_S: f32 = 18.0;

/// Time constant for the per-sample smoothing of the linear gain toward its
/// current frame-level target. Keeps the applied gain from stepping (zipper
/// noise) between frames.
const GAIN_SMOOTH_TAU_S: f32 = 0.010;

/// The limiter's output ceiling, in dBFS. `10^(-1/20) ≈ 0.891251` linear.
const LIMITER_CEILING_DBFS: f32 = -1.0;

/// Time constant for the limiter's per-sample release back toward unity
/// gain. Attack (reducing the limiter gain when a sample would exceed the
/// ceiling) is instantaneous — no look-ahead.
const LIMITER_RELEASE_TAU_S: f32 = 0.100;

/// Once a disabling [`AutoGain`]'s smoothed linear gain is within this much
/// of exactly `1.0`, it snaps to `1.0` and the instance switches to bit-exact
/// bypass rather than asymptotically approaching unity forever.
const SNAP_TO_BYPASS_EPS: f32 = 1e-4;

/// Compute a one-pole IIR coefficient `alpha` (in `y += alpha * (x - y)`
/// form) for a given update period and time constant, in `f64` for accuracy
/// (mirrors [`DcBlocker::new`]'s pole-coefficient derivation).
fn one_pole_alpha(period_s: f64, tau_s: f64) -> f64 {
    1.0 - (-period_s / tau_s).exp()
}

/// Speech-gated, boost-only automatic input gain.
///
/// Raises a too-quiet microphone's speech toward [`TARGET_DBFS`] while never
/// attenuating (a hot mic is untouched — out of scope) and never adapting to
/// silence or steady background noise. Runs once per captured block,
/// immediately after [`DcBlocker`] and before the active suppression engine
/// and the input level meter, so every downstream consumer sees the same
/// boosted samples (`LOCK-PLACEMENT`).
///
/// # Detector
///
/// Internally accumulates samples into 10 ms analysis frames
/// (`sample_rate / 100` samples), independent of however the caller chunks
/// its buffers — see [`block_size_invariant`](tests::auto_gain_block_size_invariant)
/// — and tracks a noise floor (falls instantly to a lower frame, rises with
/// a [`FLOOR_RISE_TAU_S`] one-pole toward a higher one) and a speech level
/// (a [`SPEECH_LEVEL_TAU_S`] power-domain one-pole over frames that clear
/// the [`SPEECH_MARGIN_DB`]/[`SPEECH_MIN_DBFS`] speech gate). The desired
/// gain is `clamp(TARGET_DBFS - speech_level_db, 0, MAX_GAIN_DB)`, slewed at
/// [`UP_SLEW_DB_PER_S`]/[`DOWN_SLEW_DB_PER_S`] and held (not slewed toward
/// anything) on non-speech frames.
///
/// # Known limitation: bounded noise-onset boost
///
/// A new steady noise source that appears mid-stream, well above the old
/// (already-primed) floor, briefly reads as "speech" until the floor rises
/// to meet it — this is bounded to a few dB (see
/// [`auto_gain_noise_onset_boost_is_bounded`](tests::auto_gain_noise_onset_boost_is_bounded))
/// and self-corrects as soon as the floor catches up or real speech is next
/// detected. This is a deliberate trade-off: the alternative (an
/// instantaneous floor jump) would defeat the speech-gated design by
/// treating the FIRST frame of real speech identically to a noise onset.
///
/// # Safety and hygiene
///
/// Non-finite input samples are treated as `0.0` for both the detector and
/// the output, so one bad sample cannot poison the carried state or produce
/// non-finite output (`process_in_place` keeps the gain and internal state
/// finite regardless). A boosted output whose magnitude is below `1e-20` is
/// flushed to exact `0.0`, mirroring [`DcBlocker`]. The limiter guarantees
/// `abs(output) <= max(0.891251, abs(input))` — the boost-only combine
/// (`applied gain = max(1.0, smoothed gain * limiter gain)`) means an
/// already-hot input at unity gain is never attenuated, even above the
/// ceiling.
///
/// # Enable / disable
///
/// Disabling sets the gain target to unity; per-sample smoothing keeps
/// running (about a 115 ms ramp down from +20 dB) until the smoothed gain is
/// within [`SNAP_TO_BYPASS_EPS`] of exactly `1.0`, at which point the
/// instance snaps to a genuine bypass: every subsequent sample is left
/// completely untouched (bit-exact, including non-finite bit patterns) until
/// re-enabled. Re-enabling from disabled performs a full [`reset`](Self::reset)
/// (no per-device memory — a different mic can have a very different
/// sensitivity, so unity-plus-reconverge is simpler and safer than carrying
/// a stale learned gain onto it); a redundant `set_enabled` call (already in
/// the requested state) is a no-op.
///
/// All fields are plain scalars, so `AutoGain` is `Copy` and carries no heap
/// allocation — safe to construct, reset, and run on the real-time audio
/// thread (see `tests/auto_gain_no_alloc.rs`).
#[derive(Debug, Clone, Copy)]
pub struct AutoGain {
    // -- Precomputed coefficients (derived from sample_rate in `new`) --
    /// Analysis frame length in samples (`sample_rate / 100`).
    frame_len: usize,
    /// Per-frame one-pole coefficient for the floor's rise toward a higher level.
    floor_rise_alpha: f32,
    /// Per-frame one-pole coefficient for the speech-level power average.
    speech_level_alpha: f32,
    /// Maximum gain increase per frame, in dB.
    up_step_db: f32,
    /// Maximum gain decrease per frame, in dB.
    down_step_db: f32,
    /// Per-sample one-pole coefficient for the linear gain smoothing.
    gain_smooth_alpha: f32,
    /// Per-sample one-pole coefficient for the limiter's release.
    limiter_release_alpha: f32,
    /// Linear limiter ceiling (`10^(LIMITER_CEILING_DBFS / 20)`).
    limiter_ceiling: f32,

    // -- Frame accumulator (independent of the caller's chunk size) --
    /// Running sum of squared samples in the current, not-yet-full frame.
    frame_accum: f32,
    /// Number of samples accumulated into `frame_accum` so far.
    frame_count: usize,

    // -- Detector state --
    /// Tracked noise floor, in dBFS.
    floor_db: f32,
    /// Whether `floor_db` has been primed by a non-silent frame yet.
    floor_primed: bool,
    /// Count of consecutive silent frames seen (see [`SILENT_REPRIME_FRAMES`]).
    silent_run: u32,
    /// Power-domain (mean-square) one-pole average over speech frames only.
    speech_level_power: f32,
    /// Whether `speech_level_power` has been primed by a speech frame yet.
    speech_primed: bool,

    // -- Gain state --
    /// Current frame-level control gain, in dB (slewed toward the desired gain).
    gain_db: f32,
    /// `10^(gain_db / 20)` — cached so the per-sample smoothing loop does not
    /// call `powf` on every sample.
    target_gain_linear: f32,
    /// Per-sample-smoothed linear gain, chasing `target_gain_linear`.
    smoothed_gain: f32,
    /// Per-sample limiter gain (releases toward `1.0`, attacks instantly).
    limiter_gain: f32,

    // -- Enable / bypass --
    /// Whether the detector is actively adapting. `false` while disabling or
    /// disabled (the gain target is unity and the detector is idle).
    enabled: bool,
    /// Once `true`, `process_in_place` leaves every sample completely
    /// untouched (bit-exact bypass). Only reachable while `!enabled`.
    bypassed: bool,
}

impl AutoGain {
    /// Create a new, enabled `AutoGain` at unity gain for the given sample rate.
    ///
    /// Coefficients are derived in `f64` and cast to `f32`, mirroring
    /// [`DcBlocker::new`]. The detector starts fully unprimed: it takes at
    /// least one non-silent frame to establish a noise floor and at least one
    /// speech-gated frame to establish a speech level, before any gain is
    /// applied.
    pub fn new(sample_rate: u32) -> Self {
        debug_assert!(
            sample_rate >= 100,
            "AutoGain sample_rate must be >= 100 Hz, got {sample_rate}"
        );
        let frame_len = ((sample_rate as usize) / 100).max(1);
        let frame_period_s = frame_len as f64 / f64::from(sample_rate);
        let sample_period_s = 1.0 / f64::from(sample_rate);

        let floor_rise_alpha = one_pole_alpha(frame_period_s, f64::from(FLOOR_RISE_TAU_S));
        let speech_level_alpha = one_pole_alpha(frame_period_s, f64::from(SPEECH_LEVEL_TAU_S));
        let gain_smooth_alpha = one_pole_alpha(sample_period_s, f64::from(GAIN_SMOOTH_TAU_S));
        let limiter_release_alpha =
            one_pole_alpha(sample_period_s, f64::from(LIMITER_RELEASE_TAU_S));
        let limiter_ceiling = (10f64.powf(f64::from(LIMITER_CEILING_DBFS) / 20.0)) as f32;

        Self {
            frame_len,
            floor_rise_alpha: floor_rise_alpha as f32,
            speech_level_alpha: speech_level_alpha as f32,
            up_step_db: UP_SLEW_DB_PER_S * frame_period_s as f32,
            down_step_db: DOWN_SLEW_DB_PER_S * frame_period_s as f32,
            gain_smooth_alpha: gain_smooth_alpha as f32,
            limiter_release_alpha: limiter_release_alpha as f32,
            limiter_ceiling,
            frame_accum: 0.0,
            frame_count: 0,
            floor_db: SILENCE_THRESHOLD_DBFS,
            floor_primed: false,
            silent_run: 0,
            speech_level_power: 0.0,
            speech_primed: false,
            gain_db: 0.0,
            target_gain_linear: 1.0,
            smoothed_gain: 1.0,
            limiter_gain: 1.0,
            enabled: true,
            bypassed: false,
        }
    }

    /// Whether the detector is currently enabled (not disabled/disabling).
    /// Reflects the requested state, not whether bypass has been reached yet.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The current frame-level control gain, in dB. For tests and logs.
    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// Enable or disable the auto-gain.
    ///
    /// Disabling sets the gain target to unity and lets the existing
    /// per-sample smoothing (and the limiter) carry the signal down smoothly;
    /// see the [`AutoGain`] type doc's "Enable / disable" section for the
    /// full ramp-then-bypass behavior. Enabling from a disabled state
    /// performs a full [`reset`](Self::reset). A redundant call (already in
    /// the requested state) is a no-op — in particular it never wipes a
    /// learned gain the UI's own state-sync re-sent unchanged.
    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled == self.enabled {
            return;
        }
        if enabled {
            self.reset_state();
            self.enabled = true;
            self.bypassed = false;
        } else {
            self.enabled = false;
            self.gain_db = 0.0;
            self.target_gain_linear = 1.0;
            self.maybe_snap_to_bypass();
        }
    }

    /// Reset to unity gain and an unprimed detector. Keeps the current
    /// enabled/bypassed state (see [`set_enabled`](Self::set_enabled) for the
    /// enable-from-disabled path, which resets AND re-enables).
    ///
    /// Call this whenever the input's characteristics change discontinuously
    /// and unpredictably — a new capture device, a PipeWire reconnect — so a
    /// gain learned for one microphone's sensitivity is never carried onto a
    /// different one. Do *not* call this for events where the input is
    /// continuous, such as Stop/Start or an engine swap: the whole point of
    /// this auto-gain is that the user does not have to re-learn their own
    /// voice level every time they toggle CleanMic off and on.
    pub fn reset(&mut self) {
        self.reset_state();
    }

    /// Shared reset body for [`reset`](Self::reset) and the
    /// enable-from-disabled path in [`set_enabled`](Self::set_enabled).
    /// Never touches `enabled`/`bypassed` — callers decide those.
    fn reset_state(&mut self) {
        self.frame_accum = 0.0;
        self.frame_count = 0;
        self.floor_db = SILENCE_THRESHOLD_DBFS;
        self.floor_primed = false;
        self.silent_run = 0;
        self.speech_level_power = 0.0;
        self.speech_primed = false;
        self.gain_db = 0.0;
        self.target_gain_linear = 1.0;
        self.smoothed_gain = 1.0;
        self.limiter_gain = 1.0;
    }

    /// If disabling and the smoothed gain has settled to within
    /// [`SNAP_TO_BYPASS_EPS`] of unity, snap to exact bit-exact bypass.
    /// Called both from [`set_enabled`](Self::set_enabled) (so an instance
    /// already at unity bypasses immediately) and once per sample from
    /// [`process_in_place`](Self::process_in_place) while disabling.
    fn maybe_snap_to_bypass(&mut self) {
        if !self.bypassed && (self.smoothed_gain - 1.0).abs() < SNAP_TO_BYPASS_EPS {
            self.smoothed_gain = 1.0;
            self.limiter_gain = 1.0;
            self.bypassed = true;
        }
    }

    /// Run the once-per-frame detector/gain update on a just-completed
    /// analysis frame (`self.frame_accum` over `self.frame_len` samples).
    /// See the [`AutoGain`] type doc's "Detector" section for the algorithm.
    fn frame_update(&mut self) {
        let mean_square = self.frame_accum / self.frame_len as f32;
        let frame_db = 10.0 * (mean_square + 1e-10).log10();

        if frame_db < SILENCE_THRESHOLD_DBFS {
            self.silent_run = self.silent_run.saturating_add(1);
            if self.floor_primed {
                self.floor_db = self.floor_db.min(SILENCE_THRESHOLD_DBFS);
            }
            return;
        }

        let needs_reprime = !self.floor_primed || self.silent_run >= SILENT_REPRIME_FRAMES;
        self.silent_run = 0;

        if needs_reprime {
            // First-ever non-silent frame, or the first one after a long
            // (>= SILENT_REPRIME_FRAMES) silence: prime the floor directly
            // to this frame's level and adapt nothing else this frame.
            self.floor_db = frame_db;
            self.floor_primed = true;
            return;
        }

        if frame_db < self.floor_db {
            self.floor_db = frame_db;
        } else {
            self.floor_db += self.floor_rise_alpha * (frame_db - self.floor_db);
        }

        let is_speech = frame_db > self.floor_db + SPEECH_MARGIN_DB && frame_db > SPEECH_MIN_DBFS;
        if !is_speech {
            return; // Gain held, speech level untouched.
        }

        if !self.speech_primed {
            self.speech_level_power = mean_square;
            self.speech_primed = true;
        } else {
            self.speech_level_power +=
                self.speech_level_alpha * (mean_square - self.speech_level_power);
        }
        let speech_level_db = 10.0 * (self.speech_level_power + 1e-10).log10();

        let desired_gain_db = (TARGET_DBFS - speech_level_db).clamp(0.0, MAX_GAIN_DB);
        if desired_gain_db > self.gain_db {
            self.gain_db = (self.gain_db + self.up_step_db).min(desired_gain_db);
        } else if desired_gain_db < self.gain_db {
            self.gain_db = (self.gain_db - self.down_step_db).max(desired_gain_db);
        }
        self.gain_db = self.gain_db.clamp(0.0, MAX_GAIN_DB);
        self.target_gain_linear = 10f32.powf(self.gain_db / 20.0);
    }

    /// Apply the auto-gain to `buf` in place.
    ///
    /// See the [`AutoGain`] type doc for the full algorithm, safety/hygiene
    /// guarantees, and enable/disable ramp behavior. No allocation, no locks,
    /// no I/O: safe to call from the real-time audio thread on every
    /// processed block, regardless of how the caller chunks its buffers (see
    /// [`auto_gain_block_size_invariant`](tests::auto_gain_block_size_invariant)).
    pub fn process_in_place(&mut self, buf: &mut [f32]) {
        for sample in buf.iter_mut() {
            if !self.enabled {
                self.maybe_snap_to_bypass();
            }
            if self.bypassed {
                continue;
            }

            let s = if sample.is_finite() { *sample } else { 0.0 };

            if self.enabled {
                self.frame_accum += s * s;
                self.frame_count += 1;
                if self.frame_count >= self.frame_len {
                    self.frame_update();
                    self.frame_accum = 0.0;
                    self.frame_count = 0;
                }
            }

            self.smoothed_gain +=
                self.gain_smooth_alpha * (self.target_gain_linear - self.smoothed_gain);

            let boosted = s * self.smoothed_gain;
            self.limiter_gain += self.limiter_release_alpha * (1.0 - self.limiter_gain);
            let boosted_abs = boosted.abs();
            if boosted_abs > 0.0 {
                let would_be = boosted_abs * self.limiter_gain;
                if would_be > self.limiter_ceiling {
                    self.limiter_gain = self.limiter_ceiling / boosted_abs;
                }
            }

            let applied_gain = (self.smoothed_gain * self.limiter_gain).max(1.0);
            let mut out = s * applied_gain;
            if out.abs() < 1e-20 {
                out = 0.0;
            }
            *sample = out;
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

    // ── AutoGain test signal generators ──────────────────────────────────
    //
    // Exact definitions per the quick task 260923-x24 plan, so the numeric
    // bounds below (derived from the plan's Python reference model) hold.

    /// A "burst" signal: 500 ms period, 300 ms on (with a 5 ms raised-cosine
    /// ramp at both ends of the on-segment) at `level_db` dBFS rms, 200 ms
    /// off (exact `0.0`). `start_sample` offsets the absolute sample index
    /// used for phase, so a signal can start mid-burst or mid-gap.
    fn burst_signal(n: usize, level_db: f32, start_sample: usize, sample_rate: u32) -> Vec<f32> {
        let fs = f64::from(sample_rate);
        let amp = f64::from(10f32.powf(level_db / 20.0)) * std::f64::consts::SQRT_2;
        const PERIOD: f64 = 0.5;
        const ON_DUR: f64 = 0.3;
        const RAMP: f64 = 0.005;
        (0..n)
            .map(|i| {
                let t = (start_sample + i) as f64 / fs;
                let phase = t.rem_euclid(PERIOD);
                if phase >= ON_DUR {
                    return 0.0f32;
                }
                let env = if phase < RAMP {
                    0.5 * (1.0 - (std::f64::consts::PI * phase / RAMP).cos())
                } else if phase > ON_DUR - RAMP {
                    0.5 * (1.0 - (std::f64::consts::PI * (ON_DUR - phase) / RAMP).cos())
                } else {
                    1.0
                };
                (amp * env * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as f32
            })
            .collect()
    }

    /// LCG-based uniform white noise at `level_db` dBFS rms. A local helper —
    /// deliberately not shared with `src/engine/dpdfnet.rs`'s own LCG helper
    /// per the plan.
    fn noise_signal(n: usize, level_db: f32, seed: u32) -> Vec<f32> {
        let amp = f64::from(10f32.powf(level_db / 20.0)) * 3f64.sqrt();
        let mut s: u32 = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let u = f64::from(s) / 2_147_483_648.0 - 1.0; // uniform in [-1, 1)
                (amp * u) as f32
            })
            .collect()
    }

    /// A steady sine tone at `level_db` dBFS rms.
    fn tone_signal(n: usize, level_db: f32, freq_hz: f64, sample_rate: u32) -> Vec<f32> {
        let fs = f64::from(sample_rate);
        let amp = f64::from(10f32.powf(level_db / 20.0)) * std::f64::consts::SQRT_2;
        (0..n)
            .map(|i| (amp * (2.0 * std::f64::consts::PI * freq_hz * i as f64 / fs).sin()) as f32)
            .collect()
    }

    /// Elementwise sum of two equal-length signals.
    fn mix(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b.iter()).map(|(&x, &y)| x + y).collect()
    }

    /// RMS of `samples`, as a linear (not dB) value.
    fn rms64(samples: &[f32]) -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
        (sum_sq / samples.len() as f64).sqrt()
    }

    /// Build and converge a fresh `AutoGain`: 10 s of -40 dBFS bursts over
    /// -75 dBFS noise, which the reference model converges to ~20 dB by 8 s.
    /// Reused by several tests below that need an already-boosted instance.
    fn converged_auto_gain(sample_rate: u32) -> AutoGain {
        let n = sample_rate as usize * 10;
        let mut sig = mix(
            &burst_signal(n, -40.0, 0, sample_rate),
            &noise_signal(n, -75.0, 0xC0FFEE),
        );
        let mut ag = AutoGain::new(sample_rate);
        ag.process_in_place(&mut sig);
        ag
    }

    // ── AutoGain tests ────────────────────────────────────────────────────

    #[test]
    fn auto_gain_converges_quiet_speech_to_target() {
        let n = SR as usize * 10;
        let burst = burst_signal(n, -40.0, 0, SR);
        let noise = noise_signal(n, -75.0, 12_345);
        let mut sig = mix(&burst, &noise);

        let mut ag = AutoGain::new(SR);
        let mut max_gain = 0.0f32;
        let mut gain_at_8s: Option<f32> = None;
        for (i, chunk) in sig.chunks_mut(480).enumerate() {
            ag.process_in_place(chunk);
            let g = ag.gain_db();
            max_gain = max_gain.max(g);
            let end_sample = (i + 1) * 480;
            if gain_at_8s.is_none() && end_sample >= SR as usize * 8 {
                gain_at_8s = Some(g);
            }
        }
        let gain_at_8s = gain_at_8s.expect("test signal shorter than 8s");
        assert!(
            (18.5..=21.5).contains(&gain_at_8s),
            "gain at 8s = {gain_at_8s}, expected 18.5..=21.5"
        );
        assert!(max_gain <= 21.5, "max gain over the run = {max_gain}");

        // Burst-segment rms over the last 2s of the (now boosted, in-place
        // processed) signal, sampling only the interior of each on-segment
        // to avoid the ramp edges.
        let tail_start = n - SR as usize * 2;
        let mut sum_sq = 0.0f64;
        let mut count = 0usize;
        for (idx, &v) in sig.iter().enumerate().skip(tail_start) {
            let t = idx as f64 / f64::from(SR);
            let phase = t.rem_euclid(0.5);
            if (0.01..0.29).contains(&phase) {
                sum_sq += f64::from(v) * f64::from(v);
                count += 1;
            }
        }
        assert!(count > 0, "no burst-interior samples in the last 2s");
        let burst_rms = (sum_sq / count as f64).sqrt().max(1e-12);
        let burst_db = 20.0 * burst_rms.log10();
        assert!(
            (-21.5..=-18.5).contains(&burst_db),
            "last-2s burst rms = {burst_db} dBFS, expected -21.5..=-18.5"
        );
    }

    #[test]
    fn auto_gain_holds_unity_in_silence() {
        let n = SR as usize * 20;
        let mut sig = vec![0.0f32; n];
        let mut ag = AutoGain::new(SR);
        for chunk in sig.chunks_mut(480) {
            ag.process_in_place(chunk);
            assert_eq!(ag.gain_db(), 0.0, "gain must stay at unity in silence");
        }
        assert!(
            sig.iter().all(|&v| v == 0.0),
            "output must be exact silence throughout"
        );
    }

    #[test]
    fn auto_gain_ignores_steady_noise_and_tones() {
        let n20 = SR as usize * 20;

        let mut low = noise_signal(n20, -60.0, 1);
        let mut ag_low = AutoGain::new(SR);
        ag_low.process_in_place(&mut low);
        assert!(
            ag_low.gain_db() <= 0.5,
            "steady -60dBFS noise: gain = {}",
            ag_low.gain_db()
        );

        let mut high = noise_signal(n20, -40.0, 2);
        let mut ag_high = AutoGain::new(SR);
        ag_high.process_in_place(&mut high);
        assert!(
            ag_high.gain_db() <= 0.5,
            "steady -40dBFS noise: gain = {}",
            ag_high.gain_db()
        );

        let mut tone = tone_signal(n20, -43.0, 1_000.0, SR);
        let mut ag_tone = AutoGain::new(SR);
        ag_tone.process_in_place(&mut tone);
        assert!(
            ag_tone.gain_db() <= 0.5,
            "steady -43dBFS tone: gain = {}",
            ag_tone.gain_db()
        );
    }

    #[test]
    fn auto_gain_reprimes_after_long_digital_silence() {
        // 5s zeros then 20s of steady white -50dBFS: must not pump.
        let mut sig_a = vec![0.0f32; SR as usize * 5];
        sig_a.extend(noise_signal(SR as usize * 20, -50.0, 3));
        let mut ag_a = AutoGain::new(SR);
        ag_a.process_in_place(&mut sig_a);
        assert!(
            ag_a.gain_db() <= 0.5,
            "post-silence steady noise: gain = {}",
            ag_a.gain_db()
        );

        // 3s zeros then 10s of -40 bursts over -75 noise: must still converge.
        let n = SR as usize * 10;
        let mut sig_b = vec![0.0f32; SR as usize * 3];
        sig_b.extend(mix(
            &burst_signal(n, -40.0, 0, SR),
            &noise_signal(n, -75.0, 4),
        ));
        let mut ag_b = AutoGain::new(SR);
        ag_b.process_in_place(&mut sig_b);
        assert!(
            ag_b.gain_db() >= 18.5,
            "post-silence speech: final gain = {}",
            ag_b.gain_db()
        );
    }

    #[test]
    fn auto_gain_adapts_on_gated_source_with_silent_gaps() {
        // -40dBFS bursts with exact-zero gaps, no noise floor, starting
        // 0.1s into a burst (guards the re-prime/deadlock fix: a naive
        // "ignore silent frames" detector never boosts here).
        let n = SR as usize * 8;
        let start = (0.1 * f64::from(SR)) as usize;
        let mut sig = burst_signal(n, -40.0, start, SR);
        let mut ag = AutoGain::new(SR);
        ag.process_in_place(&mut sig);
        assert!(
            ag.gain_db() >= 18.5,
            "gated source at 8s: gain = {}",
            ag.gain_db()
        );
    }

    #[test]
    fn auto_gain_noise_onset_boost_is_bounded() {
        // Documented limitation: a new steady noise source appearing well
        // above the old (already-primed) floor gets a bounded boost until
        // the floor rises to meet it.
        let mut sig = noise_signal(SR as usize * 5, -75.0, 5);
        sig.extend(noise_signal(SR as usize * 20, -50.0, 6));
        let mut ag = AutoGain::new(SR);
        let mut max_gain = 0.0f32;
        for chunk in sig.chunks_mut(480) {
            ag.process_in_place(chunk);
            max_gain = max_gain.max(ag.gain_db());
        }
        assert!(
            max_gain <= 8.0,
            "noise-onset boost = {max_gain}dB, expected <= 8.0 (documented limit)"
        );
    }

    #[test]
    fn auto_gain_never_exceeds_max_gain() {
        let n = SR as usize * 15;
        let burst = burst_signal(n, -65.0, 0, SR);
        let noise = noise_signal(n, -95.0, 7);
        let input = mix(&burst, &noise);
        let mut sig = input.clone();
        let mut ag = AutoGain::new(SR);
        for chunk in sig.chunks_mut(480) {
            ag.process_in_place(chunk);
            assert!(
                ag.gain_db() <= 30.0,
                "gain exceeded 30dB cap: {}",
                ag.gain_db()
            );
        }
        assert!(
            ag.gain_db() >= 29.5,
            "final gain = {}, expected >= 29.5",
            ag.gain_db()
        );

        let max_ratio = 31.6228f32 + 1e-3;
        for (&inp, &out) in input.iter().zip(sig.iter()) {
            if inp.abs() > 1e-9 {
                let ratio = (out / inp).abs();
                assert!(
                    ratio <= max_ratio,
                    "per-sample ratio {ratio} exceeds {max_ratio} (in={inp}, out={out})"
                );
            }
        }
    }

    #[test]
    fn auto_gain_limits_boosted_peaks() {
        let ceiling = 0.891_251_f32;
        let n = SR as usize * 10;
        let mut sig = mix(&burst_signal(n, -40.0, 0, SR), &noise_signal(n, -75.0, 8));
        let mut ag = AutoGain::new(SR);
        ag.process_in_place(&mut sig);

        let loud_n = SR as usize * 2;
        let mut loud = burst_signal(loud_n, -6.0, n, SR);
        ag.process_in_place(&mut loud);
        let max_abs = loud.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(
            max_abs <= ceiling + 1e-6,
            "max |out| during loud burst = {max_abs}, expected <= {}",
            ceiling + 1e-6
        );
        assert!(
            ag.gain_db() <= 1.0,
            "gain after loud passage = {}, expected <= 1.0",
            ag.gain_db()
        );

        // A hot (0.99 peak amplitude) tone at unity (fresh instance) must
        // pass through bit-exact: boost-only never attenuates.
        let mut hot: Vec<f32> = (0..SR as usize)
            .map(|i| 0.99 * (2.0 * std::f32::consts::PI * 300.0 * i as f32 / SR as f32).sin())
            .collect();
        let hot_input_bits: Vec<u32> = hot.iter().map(|v| v.to_bits()).collect();
        let mut fresh = AutoGain::new(SR);
        fresh.process_in_place(&mut hot);
        for (i, (&before, &after)) in hot_input_bits.iter().zip(hot.iter()).enumerate() {
            assert_eq!(
                before,
                after.to_bits(),
                "sample {i}: hot tone at unity must pass through bit-exact"
            );
        }
    }

    #[test]
    fn auto_gain_output_always_finite() {
        let mut ag = converged_auto_gain(SR);

        let n = SR as usize;
        let mut sig = mix(
            &burst_signal(n, -40.0, 10 * SR as usize, SR),
            &noise_signal(n, -75.0, 9),
        );
        let pre_injection_rms = {
            let mut probe = sig.clone();
            let mut probe_ag = ag;
            probe_ag.process_in_place(&mut probe);
            rms64(&probe)
        };

        // Inject non-finite samples mid-signal.
        let mid = n / 2;
        sig[mid] = f32::NAN;
        sig[mid + 1] = f32::INFINITY;
        sig[mid + 2] = f32::NEG_INFINITY;
        ag.process_in_place(&mut sig);
        assert!(
            sig.iter().all(|v| v.is_finite()),
            "non-finite input produced non-finite output"
        );
        assert!(ag.gain_db().is_finite(), "gain became non-finite");

        // The next 1s of the same signal shape must recover to within 1dB.
        let mut recovery = mix(
            &burst_signal(n, -40.0, 11 * SR as usize, SR),
            &noise_signal(n, -75.0, 9),
        );
        ag.process_in_place(&mut recovery);
        let recovery_rms = rms64(&recovery);
        let ratio_db = 20.0 * (recovery_rms.max(1e-12) / pre_injection_rms.max(1e-12)).log10();
        assert!(
            ratio_db.abs() <= 1.0,
            "recovery rms diverged by {ratio_db}dB from the pre-injection second"
        );

        // Converge, then 1s of zeros: last block must be exact 0.0.
        let mut ag2 = converged_auto_gain(SR);
        let mut zeros = vec![0.0f32; SR as usize];
        ag2.process_in_place(&mut zeros);
        let last_block = &zeros[zeros.len() - 480..];
        assert!(
            last_block.iter().all(|&v| v == 0.0),
            "last block after 1s silence not exact zero"
        );
    }

    #[test]
    fn auto_gain_disabled_is_bit_exact_passthrough() {
        let mut ag = AutoGain::new(SR);
        ag.set_enabled(false);
        assert!(!ag.is_enabled(), "sanity: should be disabled");

        let n = SR as usize;
        let mut sig = mix(&burst_signal(n, -40.0, 0, SR), &noise_signal(n, -75.0, 10));
        sig[100] = f32::NAN;
        sig[200] = 1e-40; // subnormal
        sig[300] = 1.5;
        let before: Vec<u32> = sig.iter().map(|v| v.to_bits()).collect();

        ag.process_in_place(&mut sig);

        for (i, (&b, &v)) in before.iter().zip(sig.iter()).enumerate() {
            assert_eq!(b, v.to_bits(), "sample {i} not bit-exact passthrough");
        }
    }

    #[test]
    fn auto_gain_disable_ramps_then_bypasses() {
        let mut ag = converged_auto_gain(SR);
        assert!(ag.gain_db() >= 15.0, "sanity: should be well boosted");
        ag.set_enabled(false);

        let total = SR as usize * 2; // 1s ramp window + 1s bit-exact check
        let input_val = 0.01f32;
        let mut prev_ratio = f32::INFINITY;
        let mut bypass_sample: Option<usize> = None;
        for i in 0..total {
            let mut one = [input_val];
            ag.process_in_place(&mut one);
            let ratio = one[0] / input_val;
            assert!(
                ratio <= prev_ratio + 1e-6,
                "sample {i}: effective gain increased ({prev_ratio} -> {ratio})"
            );
            assert!(
                ratio <= prev_ratio * 1.005 + 1e-9,
                "sample {i}: effective gain dropped more than 0.5% in one sample"
            );
            prev_ratio = ratio;
            if bypass_sample.is_none() && one[0].to_bits() == input_val.to_bits() {
                bypass_sample = Some(i);
            }
        }
        let bypass_sample = bypass_sample.expect("never reached bit-exact passthrough");
        assert!(
            bypass_sample <= 7_200,
            "took {bypass_sample} samples (> 150ms) to reach bypass"
        );

        // Stays bit-exact for the next 1s.
        for _ in 0..SR {
            let mut one = [input_val];
            ag.process_in_place(&mut one);
            assert_eq!(one[0].to_bits(), input_val.to_bits());
        }
    }

    #[test]
    fn auto_gain_enable_semantics() {
        let mut ag = converged_auto_gain(SR);
        let converged_gain = ag.gain_db();
        ag.set_enabled(true); // redundant — no-op
        assert_eq!(
            ag.gain_db(),
            converged_gain,
            "redundant enable must not change gain"
        );

        ag.set_enabled(false);
        ag.set_enabled(true); // enable from disabled — full reset
        assert_eq!(
            ag.gain_db(),
            0.0,
            "enable from disabled must reset to unity"
        );

        let n = SR as usize * 10;
        let mut sig = mix(&burst_signal(n, -40.0, 0, SR), &noise_signal(n, -75.0, 11));
        ag.process_in_place(&mut sig);
        assert!(
            ag.gain_db() >= 18.5,
            "re-enabled instance failed to reconverge: {}",
            ag.gain_db()
        );
    }

    #[test]
    fn auto_gain_reset_returns_to_unity_and_keeps_enabled_flag() {
        let mut ag = converged_auto_gain(SR);
        ag.reset();
        assert_eq!(ag.gain_db(), 0.0);
        assert!(ag.is_enabled(), "reset must not change the enabled flag");

        let mut disabled = AutoGain::new(SR);
        disabled.set_enabled(false);
        let mut buf = [0.3f32, -0.2, 1.5];
        let before: Vec<u32> = buf.iter().map(|v| v.to_bits()).collect();
        disabled.reset();
        assert!(
            !disabled.is_enabled(),
            "reset must not change the disabled flag"
        );
        disabled.process_in_place(&mut buf);
        for (b, v) in before.iter().zip(buf.iter()) {
            assert_eq!(
                *b,
                v.to_bits(),
                "reset on a disabled instance must stay bit-exact"
            );
        }
    }

    #[test]
    fn auto_gain_block_size_invariant() {
        let n = SR as usize * 3;
        let signal = mix(&burst_signal(n, -40.0, 0, SR), &noise_signal(n, -75.0, 12));

        let mut whole = signal.clone();
        let mut ag_whole = AutoGain::new(SR);
        ag_whole.process_in_place(&mut whole);

        for &chunk_size in &[480usize, 1, 1_024, 7] {
            let mut chunked = signal.clone();
            let mut ag = AutoGain::new(SR);
            for chunk in chunked.chunks_mut(chunk_size) {
                ag.process_in_place(chunk);
            }
            for (i, (&a, &b)) in whole.iter().zip(chunked.iter()).enumerate() {
                assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "chunk_size={chunk_size}: sample {i} differs: whole={a}, chunked={b}"
                );
            }
        }
    }

    #[test]
    fn auto_gain_is_copy_and_in_place() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<AutoGain>();
        assert!(!std::mem::needs_drop::<AutoGain>());

        let mut ag = AutoGain::new(SR);
        let mut buf = vec![0.01f32; 16];
        let ptr_before = buf.as_ptr();
        ag.process_in_place(&mut buf);
        assert_eq!(
            buf.as_ptr(),
            ptr_before,
            "process_in_place must not reallocate the buffer"
        );
    }
}

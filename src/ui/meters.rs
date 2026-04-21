//! Level meter logic for the CleanMic UI.
//!
//! Converts raw RMS values from the audio thread into display-ready dBFS levels
//! with exponential smoothing (fast attack, slow release) for visual smoothness.
//!
//! The core [`LevelMeter`] type is always compiled. The GTK4 progress-bar widget
//! wrapper is gated on `#[cfg(feature = "gui")]`.
//!
//! # dBFS Range
//!
//! Display is clamped to -60..=0 dBFS. Below -60 dBFS is treated as silence;
//! above 0 dBFS (clipping) is clamped to 0.

/// Minimum display level in dBFS. Values below this are treated as silence.
/// Set to -36 dBFS so typical USB mic self-noise (~-40 dBFS) falls below the
/// visible threshold, while speech (-20 to -6 dBFS) uses the full bar range.
pub const DBFS_MIN: f32 = -36.0;

/// Maximum display level in dBFS (0 dBFS = full scale).
pub const DBFS_MAX: f32 = 0.0;

/// Attack time constant: fraction of the gap closed per update at ~30 fps.
///
/// Attack: how fast the meter rises. 0.7 = responsive to speech onset,
/// but the audio-thread EMA pre-smoothing prevents flickering.
const ATTACK_COEFF: f32 = 0.7;

/// Release: how fast the meter falls. 0.4 = drops quickly when you stop
/// talking, which feels more responsive and natural.
const RELEASE_COEFF: f32 = 0.4;

/// Convert a linear RMS amplitude to dBFS, clamped to [`DBFS_MIN`]..=[`DBFS_MAX`].
///
/// * `rms == 1.0` → 0 dBFS
/// * `rms == 0.0` → `DBFS_MIN` (-60 dBFS)
/// * `rms == 0.5` → ≈ -6 dBFS
///
/// # Panics
///
/// Never panics. Negative `rms` values (invalid) return `DBFS_MIN`.
pub fn rms_to_dbfs(rms: f32) -> f32 {
    if rms <= 0.0 {
        return DBFS_MIN;
    }
    let db = 20.0 * rms.log10();
    db.clamp(DBFS_MIN, DBFS_MAX)
}

/// Convert a dBFS level to a normalized fraction in 0.0..=1.0 suitable for a
/// progress bar or drawing primitive.
///
/// Maps `DBFS_MIN` → 0.0 and `DBFS_MAX` → 1.0 linearly.
pub fn dbfs_to_fraction(dbfs: f32) -> f32 {
    let clamped = dbfs.clamp(DBFS_MIN, DBFS_MAX);
    (clamped - DBFS_MIN) / (DBFS_MAX - DBFS_MIN)
}

/// Audio level meter with exponential smoothing.
///
/// Call [`update`](LevelMeter::update) once per UI frame with the latest RMS
/// value from the audio thread. Read [`display_fraction`](LevelMeter::display_fraction)
/// or [`display_dbfs`](LevelMeter::display_dbfs) to get the smoothed display value.
#[derive(Debug, Clone)]
pub struct LevelMeter {
    /// Current smoothed level in dBFS.
    current_dbfs: f32,
}

impl Default for LevelMeter {
    fn default() -> Self {
        Self {
            current_dbfs: DBFS_MIN,
        }
    }
}

impl LevelMeter {
    /// Create a new meter initialized to silence.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the meter with a new RMS sample from the audio thread.
    ///
    /// Uses fast attack (large coefficient) when the signal is louder than the
    /// current display, and slow release (small coefficient) when it is quieter.
    /// This mirrors how hardware VU meters behave: they respond instantly to
    /// peaks but fall back slowly.
    pub fn update(&mut self, rms: f32) {
        let target_dbfs = rms_to_dbfs(rms);
        let coeff = if target_dbfs > self.current_dbfs {
            ATTACK_COEFF
        } else {
            RELEASE_COEFF
        };
        // Exponential approach: current += coeff * (target - current)
        self.current_dbfs += coeff * (target_dbfs - self.current_dbfs);
    }

    /// Reset the meter to silence immediately (e.g., when the pipeline stops).
    pub fn reset(&mut self) {
        self.current_dbfs = DBFS_MIN;
    }

    /// Return the current smoothed display level in dBFS.
    pub fn display_dbfs(&self) -> f32 {
        self.current_dbfs
    }

    /// Return the current smoothed display level as a fraction in 0.0..=1.0.
    pub fn display_fraction(&self) -> f32 {
        dbfs_to_fraction(self.current_dbfs)
    }
}

// ── UiState integration ───────────────────────────────────────────────────────

use crate::audio::LevelReport;

/// A pair of level meters: one for the raw input, one for the processed output.
#[derive(Debug, Default, Clone)]
pub struct LevelMeters {
    /// Meter for the raw microphone input signal.
    pub input: LevelMeter,
    /// Meter for the post-suppression output signal.
    pub output: LevelMeter,
}

impl LevelMeters {
    /// Create a new pair of meters initialized to silence.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update both meters from a [`LevelReport`] produced by the audio thread.
    pub fn update(&mut self, report: LevelReport) {
        self.input.update(report.input_rms);
        self.output.update(report.output_rms);
    }

    /// Reset both meters to silence (e.g., when the pipeline stops).
    pub fn reset(&mut self) {
        self.input.reset();
        self.output.reset();
    }
}

// ── GTK4 progress-bar widget (gui feature only) ───────────────────────────────

#[cfg(feature = "gui")]
pub mod widget {
    //! GTK4 wrapper that renders a [`LevelMeter`] as an `AdwActionRow` with a
    //! `gtk4::ProgressBar` suffix.
    //!
    //! Refresh the bar from a GLib timeout at ~30 fps by calling
    //! [`MeterRow::set_fraction`].

    use gtk4::prelude::*;
    use libadwaita::ActionRow;
    use libadwaita::prelude::*;

    use super::LevelMeter;

    /// An `AdwActionRow` with a `gtk4::ProgressBar` suffix for level display.
    pub struct MeterRow {
        /// The underlying row (add to a `PreferencesGroup`).
        pub row: ActionRow,
        bar: gtk4::ProgressBar,
    }

    impl MeterRow {
        /// Build a meter row with the given title (e.g., "Input" or "Output").
        pub fn new(title: &str) -> Self {
            let row = ActionRow::new();
            row.set_title(title);

            let bar = gtk4::ProgressBar::new();
            bar.set_valign(gtk4::Align::Center);
            bar.set_hexpand(true);
            bar.set_width_request(160);
            // Use a CSS class so themes can style the meter bar distinctively.
            bar.add_css_class("level-meter");

            row.add_suffix(&bar);

            Self { row, bar }
        }

        /// Update the progress bar from a [`LevelMeter`] reading.
        ///
        /// Call this from the GLib main loop (e.g., a `glib::timeout_add_local`
        /// at ~30 ms intervals).
        pub fn refresh(&self, meter: &LevelMeter) {
            self.bar.set_fraction(meter.display_fraction() as f64);
            // Update tooltip with dBFS reading for accessibility.
            let dbfs = meter.display_dbfs();
            self.bar.set_tooltip_text(Some(&format!("{dbfs:.1} dBFS")));
        }

        /// Directly set the fraction (0.0..=1.0) without going through a meter.
        pub fn set_fraction(&self, fraction: f64) {
            self.bar.set_fraction(fraction.clamp(0.0, 1.0));
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── dBFS conversion ───────────────────────────────────────────────────────

    #[test]
    fn full_scale_signal_is_zero_dbfs() {
        let db = rms_to_dbfs(1.0);
        assert!(
            (db - 0.0).abs() < 1e-5,
            "RMS=1.0 should map to 0 dBFS, got {db}"
        );
    }

    #[test]
    fn silence_clamps_to_min_dbfs() {
        let db = rms_to_dbfs(0.0);
        assert_eq!(
            db, DBFS_MIN,
            "RMS=0.0 should clamp to {DBFS_MIN} dBFS, got {db}"
        );
    }

    #[test]
    fn negative_rms_clamps_to_min_dbfs() {
        // Negative RMS is physically invalid; treat it as silence.
        let db = rms_to_dbfs(-0.5);
        assert_eq!(db, DBFS_MIN);
    }

    #[test]
    fn half_amplitude_is_approximately_minus_6_dbfs() {
        // 20 * log10(0.5) = 20 * (-0.301) ≈ -6.02 dBFS
        let db = rms_to_dbfs(0.5);
        assert!(
            (db - (-6.020_599_9)).abs() < 0.01,
            "RMS=0.5 should be ≈ -6 dBFS, got {db}"
        );
    }

    #[test]
    fn very_small_signal_clamps_to_min_dbfs() {
        // 1e-4 → 20 * log10(1e-4) = -80 dBFS, below floor → clamped to -60.
        let db = rms_to_dbfs(1e-4);
        assert_eq!(db, DBFS_MIN);
    }

    #[test]
    fn dbfs_to_fraction_maps_min_to_zero() {
        let f = dbfs_to_fraction(DBFS_MIN);
        assert!(
            (f - 0.0).abs() < 1e-6,
            "DBFS_MIN should map to 0.0, got {f}"
        );
    }

    #[test]
    fn dbfs_to_fraction_maps_max_to_one() {
        let f = dbfs_to_fraction(DBFS_MAX);
        assert!(
            (f - 1.0).abs() < 1e-6,
            "DBFS_MAX should map to 1.0, got {f}"
        );
    }

    #[test]
    fn dbfs_to_fraction_maps_midpoint_to_half() {
        // Midpoint of the -36..0 range is -18 dBFS.
        let f = dbfs_to_fraction(-18.0);
        assert!(
            (f - 0.5).abs() < 1e-6,
            "-18 dBFS should map to 0.5, got {f}"
        );
    }

    // ── Smoothing behaviour ───────────────────────────────────────────────────

    #[test]
    fn level_rises_quickly_on_attack() {
        let mut meter = LevelMeter::new();
        // Meter starts at silence (DBFS_MIN). Feed a strong signal.
        meter.update(1.0); // full scale

        // After one update with ATTACK_COEFF=0.7: new = -36 + 0.7 * (0 - (-36)) = -36 + 25.2 = -10.8.
        let db = meter.display_dbfs();
        assert!(
            db > -15.0,
            "After one strong update, meter should be near 0 dBFS (got {db} dBFS)"
        );
    }

    #[test]
    fn level_decays_slowly_on_release() {
        let mut meter = LevelMeter::new();

        // Drive the meter to full scale.
        for _ in 0..20 {
            meter.update(1.0);
        }
        let peak = meter.display_dbfs();
        assert!(
            peak > -1.0,
            "Meter should reach near 0 dBFS after repeated full-scale updates, got {peak}"
        );

        // Now feed silence and verify slow decay.
        meter.update(0.0);
        let after_one_release = meter.display_dbfs();

        // With RELEASE_COEFF=0.4: new = peak + 0.4 * (-36 - peak)
        // e.g., if peak ≈ 0: new ≈ 0 + 0.4 * (-36) = -14.4 — still above floor.
        assert!(
            after_one_release > DBFS_MIN + 1.0,
            "Meter should still be well above minimum after one silence update, got {after_one_release}"
        );

        // After many silence updates the meter should approach the floor.
        for _ in 0..100 {
            meter.update(0.0);
        }
        let after_many_releases = meter.display_dbfs();
        assert!(
            after_many_releases < DBFS_MIN + 1.0,
            "Meter should approach floor after many silence updates, got {after_many_releases}"
        );
    }

    #[test]
    fn meter_reset_goes_to_silence_immediately() {
        let mut meter = LevelMeter::new();
        for _ in 0..20 {
            meter.update(1.0);
        }
        meter.reset();
        assert_eq!(
            meter.display_dbfs(),
            DBFS_MIN,
            "Reset should snap meter to floor immediately"
        );
        assert!(
            (meter.display_fraction() - 0.0).abs() < 1e-6,
            "Fraction should be 0.0 after reset"
        );
    }

    #[test]
    fn level_meters_pair_updates_both_channels() {
        let mut meters = LevelMeters::new();
        meters.update(LevelReport {
            input_rms: 1.0,
            output_rms: 0.0,
        });

        let input_db = meters.input.display_dbfs();
        let output_db = meters.output.display_dbfs();

        assert!(
            input_db > -15.0,
            "Input meter should respond to full-scale signal, got {input_db}"
        );
        // Output was fed silence; it stays at the floor.
        assert!(
            output_db <= DBFS_MIN + 1.0,
            "Output meter should stay near floor when fed silence, got {output_db}"
        );
    }

    #[test]
    fn level_meters_reset_clears_both() {
        let mut meters = LevelMeters::new();
        for _ in 0..20 {
            meters.update(LevelReport {
                input_rms: 1.0,
                output_rms: 1.0,
            });
        }
        meters.reset();
        assert_eq!(meters.input.display_dbfs(), DBFS_MIN);
        assert_eq!(meters.output.display_dbfs(), DBFS_MIN);
    }
}

//! Settings persistence.
//!
//! Reads and writes user preferences (selected mic, engine, strength, mode,
//! monitor state, autostart, automatic mic volume) in TOML format under the
//! XDG config directory (`~/.config/cleanmic/config.toml`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::dpdfnet_policy::{self, Dpdfnet2DefaultDecision};
use crate::engine::{EngineType, ProcessingMode};

/// Owner-approved (provisional) initial normalized strength for DPDFNet-2
/// (D-14) — a distinct constant, never the legacy 0.5 default and never
/// copied from another engine's setting. Reviewed and approved unchanged
/// "for now" by the owner in the 15.2-03 checkpoint; see
/// `15.2-STRENGTH-ANCHORS.json`'s `owner_decision.status:
/// APPROVED_PROVISIONAL` for the full rationale — this is a provisional
/// sign-off, not a permanent freeze.
const DPDFNET2_DEFAULT_STRENGTH: f32 = 0.6;

/// Owner-approved (provisional) initial normalized strength for DPDFNet-8
/// (D-14) — its own distinct constant, independent of DPDFNet-2's. Same
/// 15.2-03 owner-approval status as `DPDFNET2_DEFAULT_STRENGTH` above.
const DPDFNET8_DEFAULT_STRENGTH: f32 = 0.55;

/// Each engine's independent initial normalized strength (D-14). RNNoise,
/// DeepFilterNet, and Khip keep the historical 0.5 "Balanced" default;
/// DPDFNet-2/DPDFNet-8 get distinct constants the owner approved
/// provisionally ("for now") in the 15.2-03 checkpoint.
fn default_strength_for(engine: EngineType) -> f32 {
    match engine {
        EngineType::RNNoise | EngineType::DeepFilterNet | EngineType::Khip => 0.5,
        EngineType::Dpdfnet2 => DPDFNET2_DEFAULT_STRENGTH,
        EngineType::Dpdfnet8 => DPDFNET8_DEFAULT_STRENGTH,
    }
}

/// Build a fully-populated per-engine strength map from each engine's own
/// default (used by `Config::default()`).
fn default_strengths_map() -> BTreeMap<EngineType, f32> {
    EngineType::all()
        .map(|engine| (engine, default_strength_for(engine)))
        .collect()
}

/// Clamp to `0.0..=1.0` and replace non-finite values with the safe midpoint
/// — never trust a hand-edited or corrupted TOML float at the DSP boundary.
fn sanitize_strength(value: f32) -> f32 {
    if !value.is_finite() {
        return 0.5;
    }
    value.clamp(0.0, 1.0)
}

/// Application configuration, persisted as TOML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// PipeWire node name of the selected input device, or `None` for the
    /// system default.
    pub input_device: Option<String>,

    /// Active noise suppression engine.
    pub engine: EngineType,

    /// Per-engine remembered normalized suppression strength (0.0..=1.0),
    /// keyed by `EngineType` (D-13). Switching engines restores this map's
    /// value for the newly-selected engine rather than sharing one global
    /// strength across every engine. Always fully populated for all five
    /// engines after `load`/`load_from` (see their seeding pass); use
    /// [`Config::strength_for`]/[`Config::set_strength_for`] rather than
    /// indexing this map directly.
    ///
    /// Explicit `default = "BTreeMap::new"` overrides the struct-level
    /// `#[serde(default)]` behavior (which would otherwise fill a missing
    /// field from `Config::default()`'s ALREADY-FULLY-POPULATED map) so a
    /// genuinely absent `[strengths]` table deserializes to an EMPTY map —
    /// the signal `Config::seed_missing_strengths` uses to detect "legacy or
    /// fresh file with no per-engine table at all" versus "new-format file
    /// with some engines present".
    #[serde(default = "BTreeMap::new")]
    pub strengths: BTreeMap<EngineType, f32>,

    /// Legacy pre-D-13 single global strength value. Retained ONLY so old
    /// `config.toml` files (which persisted a single top-level
    /// `strength = <float>`) still deserialize correctly; never written by
    /// `save`/`save_to` again — `strengths` is the sole authoritative
    /// persisted form going forward. Consumed once by `load_from`'s seeding
    /// pass (`Config::seed_missing_strengths`) and cleared to `None`
    /// immediately after; not part of the public API (`pub(crate)` only so
    /// that other in-crate modules' `Config { .., ..Config::default() }`
    /// functional-update-syntax construction sites — which require every
    /// field to be visible, not just the ones named explicitly — keep
    /// compiling).
    #[serde(rename = "strength", skip_serializing)]
    pub(crate) legacy_strength: Option<f32>,

    /// Processing mode (quality vs. CPU trade-off).
    pub mode: ProcessingMode,

    /// Whether the monitor (loopback to headphones) is enabled.
    pub monitor_enabled: bool,

    /// Whether the audio pipeline is active.
    pub enabled: bool,

    /// Whether the app should start on login.
    pub autostart: bool,

    /// Optional custom path to the Khip shared library.
    /// When `None`, the default search paths are used.
    pub khip_library_path: Option<std::path::PathBuf>,

    /// Whether the "CleanMic is still running in the tray" notification has
    /// been shown. Set to `true` after the first window close so the user
    /// is only notified once.
    pub tray_hint_shown: bool,

    /// Whether the "no tray host detected" notification has been shown.
    /// Set to `true` after the first notification so it is only shown once.
    pub tray_absent_notified: bool,

    /// Whether the "CleanMic is running" desktop notification has been shown
    /// after a hidden autostart launch. Set to `true` after the first hidden
    /// launch so subsequent autostart-hidden launches are silent.
    ///
    /// Kept separate from `tray_hint_shown` (which fires on the close button)
    /// because the two events are user-triggered vs. system-triggered and a
    /// user who has only ever closed-with-tray-hint-shown should still get
    /// the autostart-hidden-launch notification on the first such launch.
    pub autostart_hidden_notified: bool,

    /// The most recent update version the user has been notified about.
    ///
    /// Set to the tag string (e.g. "v1.2.0") after the first banner/desktop
    /// notification fires for that version. Prevents repeated notifications
    /// on every launch. Per D-06, D-12.
    pub last_seen_update_version: Option<String>,

    /// Automatic speech-gated input boost for too-quiet mics (quick task
    /// 260923-x24, see [`crate::dsp::AutoGain`]). ON by default; an older
    /// config file without this key loads as ON via the struct-level
    /// `#[serde(default)]`, since the feature is beneficial for essentially
    /// every mic and safe (boost-only, never attenuates) for the rest.
    pub auto_gain_enabled: bool,

    /// Whether the one-time DPDFNet-2 default migration (D-10) has already
    /// run on this config. Sticky once `true` — the migration must never
    /// repeat, even if the user later deliberately switches back to
    /// DeepFilterNet. Only meaningful once
    /// [`dpdfnet_policy::DPDFNET2_DEFAULT_DECISION`] is `Pass`; while
    /// `Pending`/`Fail`, this field may still be observed but is never used
    /// to gate a migration attempt that could not happen anyway (D-12).
    pub dpdfnet_default_migration_complete: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            input_device: None,
            // DeepFilterNet ships bundled in the AppImage and produces
            // noticeably cleaner output than RNNoise with no audible
            // artefacts across Low/Medium/High — verified by A/B tests on
            // fan + keyboard + mouse noise. The fallback chain in
            // `create_engine_with_fallback` drops back to RNNoise
            // automatically if the LADSPA library is missing, so users on
            // systems without the DF plugin still get noise suppression.
            engine: EngineType::DeepFilterNet,
            strengths: default_strengths_map(),
            legacy_strength: None,
            mode: ProcessingMode::Balanced,
            monitor_enabled: false,
            enabled: true,
            autostart: false,
            khip_library_path: None,
            tray_hint_shown: false,
            tray_absent_notified: false,
            autostart_hidden_notified: false,
            last_seen_update_version: None,
            auto_gain_enabled: true,
            dpdfnet_default_migration_complete: false,
        }
    }
}

impl Config {
    /// Return the path to the config file (`~/.config/cleanmic/config.toml`).
    pub fn config_path() -> Result<PathBuf> {
        let config_dir = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                PathBuf::from(home).join(".config")
            });
        Ok(config_dir.join("cleanmic").join("config.toml"))
    }

    /// Load configuration from the default path on disk.
    ///
    /// Returns [`Config::default()`] when the file does not exist or is
    /// corrupt (with a logged warning for the corrupt case). Missing fields
    /// in the TOML file are filled from defaults via serde.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::config_path()?)
    }

    /// Load configuration from a specific path.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;

        match toml::from_str::<Self>(&contents) {
            Ok(mut config) => {
                config.seed_missing_strengths();
                config.apply_dpdfnet2_migration(dpdfnet_policy::DPDFNET2_DEFAULT_DECISION);
                Ok(config)
            }
            Err(err) => {
                log::warn!(
                    "corrupt or invalid config at {}: {}; using defaults",
                    path.display(),
                    err
                );
                Ok(Self::default())
            }
        }
    }

    /// Return the remembered normalized strength for `engine` (0.0..=1.0).
    ///
    /// Always returns a finite, clamped value: falls back to this engine's
    /// own default (never another engine's, never a blind 0.5) if somehow
    /// still absent — this should not happen for a `Config` produced by
    /// `load`/`load_from` (whose seeding pass fills every engine), but keeps
    /// this accessor infallible for `Config`s built directly (e.g. tests,
    /// `Config::default()` callers who mutate `strengths` by hand).
    pub fn strength_for(&self, engine: EngineType) -> f32 {
        self.strengths
            .get(&engine)
            .copied()
            .map(sanitize_strength)
            .unwrap_or_else(|| default_strength_for(engine))
    }

    /// Store a clamped, finite normalized strength for `engine` (D-13).
    /// Switching to a different engine later restores this exact value via
    /// [`Config::strength_for`] rather than sharing one global strength.
    pub fn set_strength_for(&mut self, engine: EngineType, value: f32) {
        self.strengths.insert(engine, sanitize_strength(value));
    }

    /// Fill in every `EngineType`'s strength after deserialization,
    /// preferring already-persisted new-format values and falling back to
    /// the legacy global scalar only for a config that had none at all.
    ///
    /// Two cases:
    /// - `strengths` is empty (legacy pre-D-13 file, or a config with no
    ///   per-engine table whatsoever): seed RNNoise/DeepFilterNet/Khip from
    ///   the legacy scalar (if the file had one) so an upgrade doesn't
    ///   silently reset a deliberately-tuned strength; DPDFNet-2/DPDFNet-8
    ///   ALWAYS get their own separately-validated constants (D-14), never
    ///   the legacy scalar and never copied from another engine.
    /// - `strengths` is non-empty (new-format file, possibly hand-edited or
    ///   from a partial/older build with fewer engines): fill only
    ///   genuinely-missing engines with THAT engine's own default; never
    ///   copy another already-present engine's persisted value.
    ///
    /// Every value is defensively clamped/sanitized afterward regardless of
    /// which branch ran, since a hand-edited TOML file could still contain a
    /// NaN/inf/out-of-range float.
    fn seed_missing_strengths(&mut self) {
        if self.strengths.is_empty() {
            let legacy = self.legacy_strength.map(sanitize_strength);
            for engine in EngineType::all() {
                let seeded = match engine {
                    EngineType::RNNoise | EngineType::DeepFilterNet | EngineType::Khip => {
                        legacy.unwrap_or(0.5)
                    }
                    EngineType::Dpdfnet2 => DPDFNET2_DEFAULT_STRENGTH,
                    EngineType::Dpdfnet8 => DPDFNET8_DEFAULT_STRENGTH,
                };
                self.strengths.insert(engine, seeded);
            }
        } else {
            for engine in EngineType::all() {
                self.strengths
                    .entry(engine)
                    .or_insert_with(|| default_strength_for(engine));
            }
        }
        for value in self.strengths.values_mut() {
            *value = sanitize_strength(*value);
        }
        self.legacy_strength = None;
    }

    /// Apply the D-10 one-time conditional DPDFNet-2 default migration using
    /// the given `decision`. `load_from` always calls this with the real
    /// [`dpdfnet_policy::DPDFNET2_DEFAULT_DECISION`] constant (`Pending`
    /// until a future plan approves default evidence, per D-12); exposed
    /// with an explicit `decision` parameter — rather than hardcoding the
    /// constant internally — so tests can exercise the
    /// `Pass`/`Fail`/`Pending` branches without flipping the real product
    /// constant.
    pub fn apply_dpdfnet2_migration(&mut self, decision: Dpdfnet2DefaultDecision) {
        let (engine, complete) = dpdfnet_policy::migrate_engine_for_decision(
            self.engine,
            self.dpdfnet_default_migration_complete,
            decision,
        );
        self.engine = engine;
        self.dpdfnet_default_migration_complete = complete;
    }

    /// Returns `true` if no config file exists on disk (first run).
    pub fn is_first_run() -> Result<bool> {
        Ok(!Self::config_path()?.exists())
    }

    /// Check if this is the first run using a specific config path.
    pub fn is_first_run_at(path: &Path) -> bool {
        !path.exists()
    }

    /// Persist the current configuration to the default path on disk.
    ///
    /// Creates the parent directory (`~/.config/cleanmic/`) if it does not
    /// exist.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::config_path()?)
    }

    /// Persist the current configuration to a specific path.
    ///
    /// Creates the parent directory if it does not exist.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config dir {}", parent.display()))?;
        }

        let contents =
            toml::to_string_pretty(self).context("failed to serialize config to TOML")?;

        fs::write(path, contents)
            .with_context(|| format!("failed to write config to {}", path.display()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Return a path to `config.toml` inside a fresh temp directory.
    /// The returned `TempDir` handle keeps the directory alive until dropped.
    fn temp_config_path() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let path = tmp.path().join("cleanmic").join("config.toml");
        (tmp, path)
    }

    #[test]
    fn default_has_expected_values() {
        let cfg = Config::default();
        assert_eq!(cfg.input_device, None);
        assert_eq!(cfg.engine, EngineType::DeepFilterNet);
        assert!((cfg.strength_for(EngineType::DeepFilterNet) - 0.5).abs() < f32::EPSILON);
        assert_eq!(cfg.mode, ProcessingMode::Balanced);
        assert!(!cfg.monitor_enabled);
        assert!(cfg.enabled);
        assert!(!cfg.autostart);
        assert!(cfg.auto_gain_enabled);
        assert!(!cfg.dpdfnet_default_migration_complete);
    }

    #[test]
    fn roundtrip_save_then_load() {
        let (_tmp, path) = temp_config_path();
        let mut original = Config {
            input_device: Some("alsa_input.usb-Blue_Yeti".into()),
            engine: EngineType::RNNoise,
            mode: ProcessingMode::MaxQuality,
            monitor_enabled: true,
            enabled: false,
            autostart: true,
            ..Config::default()
        };
        original.set_strength_for(EngineType::RNNoise, 0.8);
        original.save_to(&path).expect("save failed");
        let loaded = Config::load_from(&path).expect("load failed");
        assert_eq!(original, loaded);
    }

    #[test]
    fn loading_missing_file_returns_defaults() {
        let (_tmp, path) = temp_config_path();
        let cfg = Config::load_from(&path).expect("load failed");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn loading_corrupt_file_returns_defaults() {
        let (_tmp, path) = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "this is not valid {{{{ toml").unwrap();

        let cfg = Config::load_from(&path).expect("load failed");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn loading_partial_file_fills_defaults() {
        let (_tmp, path) = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Only set engine and the legacy scalar strength; all other fields
        // should come from defaults.
        fs::write(&path, "engine = \"RNNoise\"\nstrength = 0.9\n").unwrap();

        let cfg = Config::load_from(&path).expect("load failed");
        assert_eq!(cfg.engine, EngineType::RNNoise);
        // Legacy scalar seeds RNNoise/DeepFilterNet/Khip (D-14).
        assert!((cfg.strength_for(EngineType::RNNoise) - 0.9).abs() < f32::EPSILON);
        assert!((cfg.strength_for(EngineType::DeepFilterNet) - 0.9).abs() < f32::EPSILON);
        assert!((cfg.strength_for(EngineType::Khip) - 0.9).abs() < f32::EPSILON);
        // DPDFNet-2/8 must NEVER inherit the legacy scalar (D-14) — they get
        // their own distinct constants, neither of which is 0.9 or 0.5.
        assert!((cfg.strength_for(EngineType::Dpdfnet2) - 0.9).abs() > f32::EPSILON);
        assert!((cfg.strength_for(EngineType::Dpdfnet8) - 0.9).abs() > f32::EPSILON);
        // Remaining fields should be defaults.
        assert_eq!(cfg.mode, ProcessingMode::Balanced);
        assert!(!cfg.monitor_enabled);
        assert!(cfg.enabled);
        assert!(!cfg.autostart);
    }

    #[test]
    fn last_seen_update_version_roundtrips() {
        let (_tmp, path) = temp_config_path();
        let original = Config {
            last_seen_update_version: Some("v1.2.0".to_owned()),
            ..Config::default()
        };
        original.save_to(&path).expect("save failed");
        let loaded = Config::load_from(&path).expect("load failed");
        assert_eq!(loaded.last_seen_update_version, Some("v1.2.0".to_owned()));
    }

    #[test]
    fn last_seen_update_version_defaults_to_none_from_partial_toml() {
        let (_tmp, path) = temp_config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Old config file without this field — should load as None.
        std::fs::write(&path, "engine = \"RNNoise\"\nstrength = 0.5\n").unwrap();
        let cfg = Config::load_from(&path).expect("load failed");
        assert_eq!(cfg.last_seen_update_version, None);
    }

    #[test]
    fn autostart_hidden_notified_defaults_to_false_from_partial_toml() {
        let (_tmp, path) = temp_config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Old config file (pre-260508-k7q) without this field — should load as false.
        std::fs::write(&path, "engine = \"RNNoise\"\nstrength = 0.5\n").unwrap();
        let cfg = Config::load_from(&path).expect("load failed");
        assert!(!cfg.autostart_hidden_notified);
    }

    #[test]
    fn auto_gain_enabled_defaults_to_true_from_partial_toml() {
        let (_tmp, path) = temp_config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Old config file (pre-260923-x24) without this field — should load as true.
        std::fs::write(&path, "engine = \"RNNoise\"\nstrength = 0.5\n").unwrap();
        let cfg = Config::load_from(&path).expect("load failed");
        assert!(cfg.auto_gain_enabled);
    }

    #[test]
    fn auto_gain_enabled_false_roundtrips() {
        let (_tmp, path) = temp_config_path();
        let original = Config {
            auto_gain_enabled: false,
            ..Config::default()
        };
        original.save_to(&path).expect("save failed");
        let loaded = Config::load_from(&path).expect("load failed");
        assert!(!loaded.auto_gain_enabled);
    }

    #[test]
    fn config_directory_created_if_absent() {
        let (_tmp, path) = temp_config_path();
        let dir = path.parent().unwrap();
        assert!(!dir.exists());

        Config::default().save_to(&path).expect("save failed");

        assert!(dir.exists());
        assert!(path.exists());
    }

    // ── Per-engine strength (D-13/D-14, Task 1) ─────────────────────────────

    #[test]
    fn all_five_engines_have_finite_clamped_default_strengths() {
        let cfg = Config::default();
        for engine in EngineType::all() {
            let s = cfg.strength_for(engine);
            assert!(s.is_finite(), "{engine:?} default strength not finite");
            assert!((0.0..=1.0).contains(&s), "{engine:?} default out of range");
        }
    }

    #[test]
    fn dpdfnet_default_strengths_are_distinct_from_legacy_and_each_other() {
        let cfg = Config::default();
        let dpdfnet2 = cfg.strength_for(EngineType::Dpdfnet2);
        let dpdfnet8 = cfg.strength_for(EngineType::Dpdfnet8);
        // D-14: never automatically 0.5, never copied from another engine.
        assert!((dpdfnet2 - 0.5).abs() > f32::EPSILON);
        assert!((dpdfnet8 - 0.5).abs() > f32::EPSILON);
        assert!((dpdfnet2 - dpdfnet8).abs() > f32::EPSILON);
    }

    #[test]
    fn set_strength_for_then_strength_for_round_trips_per_engine() {
        let mut cfg = Config::default();
        cfg.set_strength_for(EngineType::RNNoise, 0.2);
        cfg.set_strength_for(EngineType::DeepFilterNet, 0.9);
        cfg.set_strength_for(EngineType::Dpdfnet2, 0.33);
        cfg.set_strength_for(EngineType::Dpdfnet8, 0.77);
        cfg.set_strength_for(EngineType::Khip, 0.11);

        assert!((cfg.strength_for(EngineType::RNNoise) - 0.2).abs() < f32::EPSILON);
        assert!((cfg.strength_for(EngineType::DeepFilterNet) - 0.9).abs() < f32::EPSILON);
        assert!((cfg.strength_for(EngineType::Dpdfnet2) - 0.33).abs() < f32::EPSILON);
        assert!((cfg.strength_for(EngineType::Dpdfnet8) - 0.77).abs() < f32::EPSILON);
        assert!((cfg.strength_for(EngineType::Khip) - 0.11).abs() < f32::EPSILON);
    }

    #[test]
    fn set_strength_for_clamps_out_of_range_and_sanitizes_non_finite() {
        let mut cfg = Config::default();
        cfg.set_strength_for(EngineType::RNNoise, 5.0);
        assert!((cfg.strength_for(EngineType::RNNoise) - 1.0).abs() < f32::EPSILON);

        cfg.set_strength_for(EngineType::RNNoise, -5.0);
        assert!((cfg.strength_for(EngineType::RNNoise) - 0.0).abs() < f32::EPSILON);

        cfg.set_strength_for(EngineType::RNNoise, f32::NAN);
        assert!((cfg.strength_for(EngineType::RNNoise) - 0.5).abs() < f32::EPSILON);

        cfg.set_strength_for(EngineType::RNNoise, f32::INFINITY);
        assert!((cfg.strength_for(EngineType::RNNoise) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn new_format_strengths_table_round_trips_all_five_engines() {
        let (_tmp, path) = temp_config_path();
        let mut original = Config::default();
        for (i, engine) in EngineType::all().enumerate() {
            original.set_strength_for(engine, i as f32 / 10.0);
        }
        original.save_to(&path).expect("save failed");

        let loaded = Config::load_from(&path).expect("load failed");
        for engine in EngineType::all() {
            assert!(
                (loaded.strength_for(engine) - original.strength_for(engine)).abs() < f32::EPSILON,
                "{engine:?} strength did not round-trip"
            );
        }
    }

    #[test]
    fn partial_new_format_table_fills_missing_engines_from_own_default_not_a_copy() {
        let (_tmp, path) = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // New-format table present but only RNNoise is set — simulates a
        // hand-edited or partially-upgraded file. DeepFilterNet/Dpdfnet2/
        // Dpdfnet8/Khip must be filled from THEIR OWN defaults, not RNNoise's
        // persisted 0.95.
        fs::write(&path, "engine = \"RNNoise\"\n[strengths]\nRNNoise = 0.95\n").unwrap();

        let cfg = Config::load_from(&path).expect("load failed");
        assert!((cfg.strength_for(EngineType::RNNoise) - 0.95).abs() < f32::EPSILON);
        assert!((cfg.strength_for(EngineType::DeepFilterNet) - 0.5).abs() < f32::EPSILON);
        assert!(
            (cfg.strength_for(EngineType::Dpdfnet2) - DPDFNET2_DEFAULT_STRENGTH).abs()
                < f32::EPSILON
        );
        assert!(
            (cfg.strength_for(EngineType::Dpdfnet8) - DPDFNET8_DEFAULT_STRENGTH).abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn hand_edited_out_of_range_new_format_values_are_sanitized_on_load() {
        let (_tmp, path) = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "engine = \"RNNoise\"\n[strengths]\nRNNoise = 42.0\nDeepFilterNet = -3.0\n",
        )
        .unwrap();

        let cfg = Config::load_from(&path).expect("load failed");
        assert!((cfg.strength_for(EngineType::RNNoise) - 1.0).abs() < f32::EPSILON);
        assert!((cfg.strength_for(EngineType::DeepFilterNet) - 0.0).abs() < f32::EPSILON);
    }

    // ── Conditional DPDFNet-2 default migration (D-10/D-12, Task 1) ─────────

    #[test]
    fn pending_decision_never_migrates_on_load() {
        let (_tmp, path) = temp_config_path();
        let original = Config {
            engine: EngineType::DeepFilterNet,
            ..Config::default()
        };
        original.save_to(&path).expect("save failed");

        // `load_from` always uses the real (Pending) product constant.
        let loaded = Config::load_from(&path).expect("load failed");
        assert_eq!(loaded.engine, EngineType::DeepFilterNet);
        assert!(!loaded.dpdfnet_default_migration_complete);
    }

    #[test]
    fn pass_decision_migrates_deepfilternet_selection_exactly_once() {
        let mut cfg = Config {
            engine: EngineType::DeepFilterNet,
            ..Config::default()
        };
        cfg.apply_dpdfnet2_migration(Dpdfnet2DefaultDecision::Pass);
        assert_eq!(cfg.engine, EngineType::Dpdfnet2);
        assert!(cfg.dpdfnet_default_migration_complete);

        // A deliberate switch back to DeepFilterNet after migration must NOT
        // be re-migrated even if `apply_dpdfnet2_migration(Pass)` is called
        // again (D-10's "exactly once" marker).
        cfg.engine = EngineType::DeepFilterNet;
        cfg.apply_dpdfnet2_migration(Dpdfnet2DefaultDecision::Pass);
        assert_eq!(
            cfg.engine,
            EngineType::DeepFilterNet,
            "already-migrated config must not re-migrate a deliberate re-selection"
        );
    }

    #[test]
    fn fail_decision_never_migrates() {
        let mut cfg = Config {
            engine: EngineType::DeepFilterNet,
            ..Config::default()
        };
        cfg.apply_dpdfnet2_migration(Dpdfnet2DefaultDecision::Fail);
        assert_eq!(cfg.engine, EngineType::DeepFilterNet);
        // `Fail`, like `Pending`, never migrates AND never marks the
        // per-config marker complete — only `Pass` ever spends this config's
        // one migration chance (see `dpdfnet_policy::migrate_engine_for_decision`).
        assert!(!cfg.dpdfnet_default_migration_complete);
    }

    #[test]
    fn migration_marker_persists_across_save_and_load() {
        let (_tmp, path) = temp_config_path();
        let mut cfg = Config {
            engine: EngineType::DeepFilterNet,
            ..Config::default()
        };
        cfg.apply_dpdfnet2_migration(Dpdfnet2DefaultDecision::Pass);
        assert_eq!(cfg.engine, EngineType::Dpdfnet2);
        cfg.save_to(&path).expect("save failed");

        let loaded = Config::load_from(&path).expect("load failed");
        assert!(loaded.dpdfnet_default_migration_complete);
        assert_eq!(loaded.engine, EngineType::Dpdfnet2);
    }
}

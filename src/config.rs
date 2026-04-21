//! Settings persistence.
//!
//! Reads and writes user preferences (selected mic, engine, strength, mode,
//! monitor state, autostart) in TOML format under the XDG config directory
//! (`~/.config/cleanmic/config.toml`).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::{EngineType, ProcessingMode};

/// Application configuration, persisted as TOML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// PipeWire node name of the selected input device, or `None` for the
    /// system default.
    pub input_device: Option<String>,

    /// Active noise suppression engine.
    pub engine: EngineType,

    /// Normalized suppression strength (0.0..=1.0).
    pub strength: f32,

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

    /// The most recent update version the user has been notified about.
    ///
    /// Set to the tag string (e.g. "v1.2.0") after the first banner/desktop
    /// notification fires for that version. Prevents repeated notifications
    /// on every launch. Per D-06, D-12.
    pub last_seen_update_version: Option<String>,
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
            strength: 0.5,
            mode: ProcessingMode::Balanced,
            monitor_enabled: false,
            enabled: true,
            autostart: false,
            khip_library_path: None,
            tray_hint_shown: false,
            tray_absent_notified: false,
            last_seen_update_version: None,
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
            Ok(config) => Ok(config),
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
        assert!((cfg.strength - 0.5).abs() < f32::EPSILON);
        assert_eq!(cfg.mode, ProcessingMode::Balanced);
        assert!(!cfg.monitor_enabled);
        assert!(cfg.enabled);
        assert!(!cfg.autostart);
    }

    #[test]
    fn roundtrip_save_then_load() {
        let (_tmp, path) = temp_config_path();
        let original = Config {
            input_device: Some("alsa_input.usb-Blue_Yeti".into()),
            engine: EngineType::RNNoise,
            strength: 0.8,
            mode: ProcessingMode::MaxQuality,
            monitor_enabled: true,
            enabled: false,
            autostart: true,
            khip_library_path: None,
            tray_hint_shown: false,
            tray_absent_notified: false,
            last_seen_update_version: None,
        };
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
        // Only set engine and strength; all other fields should come from defaults.
        fs::write(&path, "engine = \"RNNoise\"\nstrength = 0.9\n").unwrap();

        let cfg = Config::load_from(&path).expect("load failed");
        assert_eq!(cfg.engine, EngineType::RNNoise);
        assert!((cfg.strength - 0.9).abs() < f32::EPSILON);
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
    fn config_directory_created_if_absent() {
        let (_tmp, path) = temp_config_path();
        let dir = path.parent().unwrap();
        assert!(!dir.exists());

        Config::default().save_to(&path).expect("save failed");

        assert!(dir.exists());
        assert!(path.exists());
    }
}

//! GTK4 + libadwaita main window.
//!
//! Provides the control interface: device picker, engine selector with
//! user-friendly labels ("Balanced" / "High Quality" / "Advanced"),
//! mode selector, strength slider, monitor toggle, input/output level
//! meters, enable/disable toggle, and autostart toggle.
//!
//! The window follows GNOME design language and should feel like a
//! system utility (e.g., network manager), not a DAW.
//!
//! # Feature gating
//!
//! The public API types (`UiEvent`, `UiState`, `DeviceInfo`) are always
//! compiled so the audio service can reference them without pulling in GTK.
//! The actual GTK4 window construction lives in `window.rs` and `status.rs`,
//! both gated on `#[cfg(feature = "gui")]`.

#[cfg(feature = "gui")]
pub mod status;
#[cfg(feature = "gui")]
pub mod window;

pub mod meters;
pub mod welcome;

use crate::config::Config;
use crate::engine::{EngineType, ProcessingMode};

// ── Public event type ─────────────────────────────────────────────────────────

/// Events produced by the UI and consumed by the audio service.
///
/// Each variant represents a user action that the audio service must act upon.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    /// The user selected a different noise suppression engine.
    EngineChanged(EngineType),

    /// The user moved the strength slider to a new normalized value (0.0..=1.0).
    StrengthChanged(f32),

    /// The user selected a different input device (PipeWire node name).
    DeviceChanged(String),

    /// The user toggled the enable/disable switch.
    EnableToggled(bool),

    /// The user toggled the monitor (listen-to-processed-mic) switch.
    MonitorToggled(bool),

    /// The user toggled the autostart switch.
    AutostartToggled(bool),

    /// The user requested application exit (e.g., via tray "Quit" or Ctrl+Q).
    Quit,

    /// User requested a manual update check (from tray menu or About dialog). Per D-02, D-04.
    CheckForUpdates,

    /// Background updater detected a newer version. Carries the tag string (e.g. "v1.2.0").
    /// Used to trigger banner, desktop notification, and tray indicator. Per D-05.
    UpdateAvailable(String),
}

// ── Public state type ─────────────────────────────────────────────────────────

/// A description of an available input device, as presented in the device
/// picker dropdown.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    /// PipeWire node name used internally (stable identifier).
    pub name: String,

    /// Human-readable description shown in the UI (e.g., "Built-in Microphone").
    pub description: String,
}

/// Snapshot of all state the UI needs to render itself.
///
/// The audio service pushes a fresh `UiState` whenever anything changes; the
/// UI thread replaces its current state and refreshes all controls.
#[derive(Debug, Clone, PartialEq)]
pub struct UiState {
    /// Whether the audio pipeline is running.
    pub active: bool,

    /// Currently selected engine.
    pub engine: EngineType,

    /// Normalized suppression strength (0.0..=1.0).
    pub strength: f32,

    /// Currently selected processing mode.
    pub mode: ProcessingMode,

    /// PipeWire node name of the selected input device, or `None` when the
    /// system default is in use.
    pub input_device: Option<String>,

    /// Whether the monitor (listen-to-processed-mic) is enabled.
    pub monitor_enabled: bool,

    /// Whether autostart is enabled.
    pub autostart: bool,

    /// Latest RMS level of the raw input signal (linear 0.0..=1.0).
    /// Drives the input level meter.
    pub input_level: f32,

    /// Latest RMS level of the processed output signal (linear 0.0..=1.0).
    /// Drives the output level meter.
    pub output_level: f32,

    /// All input devices currently visible to PipeWire.
    /// Does not include "CleanMic" itself.
    pub available_devices: Vec<DeviceInfo>,

    /// Whether the Khip library was detected on this system.
    /// When `false`, the Khip engine option is grayed out.
    pub khip_available: bool,

    /// If Some, a newer version is available; string is the tag name (e.g. "v1.2.0").
    /// Drives the adw::Banner reveal state. Per D-05, D-07.
    pub update_available: Option<String>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            active: false,
            engine: EngineType::RNNoise,
            strength: 0.5,
            mode: ProcessingMode::Balanced,
            input_device: None,
            monitor_enabled: false,
            autostart: false,
            input_level: 0.0,
            output_level: 0.0,
            available_devices: Vec::new(),
            khip_available: false,
            update_available: None,
        }
    }
}

impl UiState {
    /// Construct a `UiState` from a persisted [`Config`].
    ///
    /// Level meters and device list are left at their zero/empty defaults
    /// because those come from the live audio service, not the config file.
    pub fn from_config(config: &Config) -> Self {
        Self {
            active: config.enabled,
            engine: config.engine,
            strength: config.strength,
            mode: config.mode,
            input_device: config.input_device.clone(),
            monitor_enabled: config.monitor_enabled,
            autostart: config.autostart,
            ..Default::default()
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::engine::{EngineType, ProcessingMode};

    // ── UiState ───────────────────────────────────────────────────────────────

    #[test]
    fn ui_state_default_values() {
        let state = UiState::default();
        assert!(!state.active);
        assert_eq!(state.engine, EngineType::RNNoise);
        assert!((state.strength - 0.5).abs() < f32::EPSILON);
        assert_eq!(state.mode, ProcessingMode::Balanced);
        assert_eq!(state.input_device, None);
        assert!(!state.monitor_enabled);
        assert!(!state.autostart);
        assert!((state.input_level - 0.0).abs() < f32::EPSILON);
        assert!((state.output_level - 0.0).abs() < f32::EPSILON);
        assert!(state.available_devices.is_empty());
        assert!(!state.khip_available);
    }

    #[test]
    fn ui_state_from_config_copies_fields() {
        let config = Config {
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

        let state = UiState::from_config(&config);

        assert!(!state.active, "active maps from config.enabled");
        assert_eq!(state.engine, EngineType::RNNoise);
        assert!((state.strength - 0.8).abs() < f32::EPSILON);
        assert_eq!(state.mode, ProcessingMode::MaxQuality);
        assert_eq!(state.input_device, Some("alsa_input.usb-Blue_Yeti".into()));
        assert!(state.monitor_enabled);
        assert!(state.autostart);
        // live fields are zeroed
        assert!((state.input_level).abs() < f32::EPSILON);
        assert!((state.output_level).abs() < f32::EPSILON);
        assert!(state.available_devices.is_empty());
        assert!(!state.khip_available);
    }

    #[test]
    fn ui_state_from_default_config() {
        let config = Config::default();
        let state = UiState::from_config(&config);
        // Default config has enabled = true
        assert!(state.active);
        assert_eq!(state.engine, EngineType::DeepFilterNet);
        assert!((state.strength - 0.5).abs() < f32::EPSILON);
    }

    // ── UiEvent ───────────────────────────────────────────────────────────────

    #[test]
    fn ui_event_engine_changed_variants() {
        let e = UiEvent::EngineChanged(EngineType::RNNoise);
        assert_eq!(e, UiEvent::EngineChanged(EngineType::RNNoise));

        let e2 = UiEvent::EngineChanged(EngineType::DeepFilterNet);
        assert_eq!(e2, UiEvent::EngineChanged(EngineType::DeepFilterNet));

        let e3 = UiEvent::EngineChanged(EngineType::Khip);
        assert_eq!(e3, UiEvent::EngineChanged(EngineType::Khip));
    }

    #[test]
    fn ui_event_strength_changed() {
        let e = UiEvent::StrengthChanged(0.75);
        assert_eq!(e, UiEvent::StrengthChanged(0.75));
    }

    #[test]
    fn ui_event_device_changed() {
        let e = UiEvent::DeviceChanged("alsa_input.usb-Blue_Yeti".into());
        assert_eq!(e, UiEvent::DeviceChanged("alsa_input.usb-Blue_Yeti".into()));
    }

    #[test]
    fn ui_event_bool_variants() {
        assert_eq!(UiEvent::EnableToggled(true), UiEvent::EnableToggled(true));
        assert_eq!(
            UiEvent::MonitorToggled(false),
            UiEvent::MonitorToggled(false)
        );
        assert_eq!(
            UiEvent::AutostartToggled(true),
            UiEvent::AutostartToggled(true)
        );
    }

    #[test]
    fn ui_event_quit() {
        assert_eq!(UiEvent::Quit, UiEvent::Quit);
    }

    // ── DeviceInfo ────────────────────────────────────────────────────────────

    #[test]
    fn device_info_fields() {
        let d = DeviceInfo {
            name: "alsa_input.pci-0000_00_1f.3-platform-skl_hda_dsp_generic".into(),
            description: "Built-in Microphone".into(),
        };
        assert_eq!(
            d.name,
            "alsa_input.pci-0000_00_1f.3-platform-skl_hda_dsp_generic"
        );
        assert_eq!(d.description, "Built-in Microphone");
    }

    #[test]
    fn available_devices_can_be_populated() {
        let mut state = UiState::default();
        state.available_devices.push(DeviceInfo {
            name: "alsa_input.usb-Blue_Yeti".into(),
            description: "Blue Yeti".into(),
        });
        state.available_devices.push(DeviceInfo {
            name: "alsa_input.pci-builtin".into(),
            description: "Built-in Microphone".into(),
        });
        assert_eq!(state.available_devices.len(), 2);
    }
}

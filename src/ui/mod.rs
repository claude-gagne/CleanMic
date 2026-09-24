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

use std::collections::BTreeMap;

use crate::config::Config;
use crate::engine::{EngineAvailability, EngineType, ProcessingMode};

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

    /// The user selected a different processing mode (CPU/quality trade-off). Per D-03/D-04.
    ModeChanged(ProcessingMode),

    /// The user selected a different input device (PipeWire node name).
    DeviceChanged(String),

    /// The user selected the "Default" entry in the picker, meaning "follow
    /// whatever the OS has set as the default input source". Distinct from
    /// `DeviceChanged` because it must clear `config.input_device` to `None`
    /// rather than pin to a specific name. Per D-06.
    DeviceChangedToDefault,

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

    /// Whether the device's port/route is currently usable (R1). `false`
    /// means the source's port is unavailable (e.g. an unplugged headset
    /// jack) — such a device only reaches the picker at all when it is the
    /// pinned or system-default device (see
    /// [`crate::pipewire::devices::picker_devices`]).
    pub available: bool,
}

/// Snapshot of all state the UI needs to render itself.
///
/// The audio service pushes a fresh `UiState` whenever anything changes; the
/// UI thread replaces its current state and refreshes all controls.
#[derive(Debug, Clone, PartialEq)]
pub struct UiState {
    /// Whether the audio pipeline is running.
    pub active: bool,

    /// The truthful, currently ACTIVE engine (T-15.1-07) — never the
    /// merely-requested one. May differ from `requested_engine` for the
    /// duration of a fallback (e.g. a migrated DPDFNet-2 selection whose
    /// model/runtime failed to initialize, D-11). Existing consumers
    /// (`src/ui/window.rs`, `src/ui/status.rs`) continue to read this as
    /// "the engine to show selected" — that contract is unchanged; it has
    /// simply always meant "active", now made explicit alongside
    /// `requested_engine`.
    pub engine: EngineType,

    /// The engine the user (or a conditional migration) actually asked for,
    /// before any fallback substitution. Equal to `engine` except during an
    /// active fallback. Not yet consumed by `src/ui/window.rs`/`src/tray.rs`
    /// (a later phase wires selector/tray truth); exists so application
    /// logic and tests can assert fallback behavior without losing the
    /// original intent.
    pub requested_engine: EngineType,

    /// Normalized suppression strength (0.0..=1.0) for the currently ACTIVE
    /// engine (`engine`) — i.e. `Config::strength_for(engine)`. Restored
    /// per-engine on switch (D-13), never one value shared across engines.
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

    /// PipeWire node name of the OS-level default input source, if any and
    /// if it is a real mic (not CleanMic). `None` means either (a) the OS
    /// default is CleanMic itself, or (b) the default could not be resolved
    /// from pw-metadata. The picker uses this signal to decide whether to
    /// render the "Default (MicName)" entry or hide it entirely. Per D-01,
    /// D-08.
    pub system_default_name: Option<String>,

    /// Whether the Khip library was detected on this system.
    /// When `false`, the Khip engine option is grayed out.
    ///
    /// Kept for `src/ui/window.rs`/`src/tray.rs` backward compatibility
    /// (unchanged contract); equivalent to
    /// `availability[&EngineType::Khip].available`.
    pub khip_available: bool,

    /// Independent per-engine availability/reason map (T-15.1-07/
    /// T-15.1-08), superseding the Khip-only special case above for any
    /// future consumer — populated from
    /// [`crate::engine::all_engine_availability`].
    pub availability: BTreeMap<EngineType, EngineAvailability>,

    /// Non-empty only when a fallback substitution is currently in effect
    /// (e.g. a migrated-but-uninitializable DPDFNet-2 fell back to
    /// DeepFilterNet, D-11). Carries an already-translated, human-readable
    /// notice built by [`crate::engine::fallback_notice`] — `None` means no
    /// fallback is in effect and `engine == requested_engine`.
    pub fallback_notice: Option<String>,

    /// If Some, a newer version is available; string is the tag name (e.g. "v1.2.0").
    /// Drives the adw::Banner reveal state. Per D-05, D-07.
    pub update_available: Option<String>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            active: false,
            engine: EngineType::RNNoise,
            requested_engine: EngineType::RNNoise,
            strength: 0.5,
            mode: ProcessingMode::Balanced,
            input_device: None,
            monitor_enabled: false,
            autostart: false,
            input_level: 0.0,
            output_level: 0.0,
            available_devices: Vec::new(),
            system_default_name: None,
            khip_available: false,
            availability: BTreeMap::new(),
            fallback_notice: None,
            update_available: None,
        }
    }
}

impl UiState {
    /// Construct a `UiState` from a persisted [`Config`].
    ///
    /// Level meters, device list, and `availability` are left at their
    /// zero/empty defaults because those come from the live audio
    /// service/engine probes, not the config file — callers (`src/app.rs`)
    /// populate `availability` from [`crate::engine::all_engine_availability`]
    /// once real enumeration is available. `requested_engine` defaults to
    /// `config.engine` (no divergence known from the persisted config
    /// alone); a caller that just resolved a startup fallback should
    /// override both `requested_engine` and `fallback_notice` afterward.
    pub fn from_config(config: &Config) -> Self {
        Self {
            active: config.enabled,
            engine: config.engine,
            requested_engine: config.engine,
            strength: config.strength_for(config.engine),
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
        assert_eq!(state.requested_engine, EngineType::RNNoise);
        assert!((state.strength - 0.5).abs() < f32::EPSILON);
        assert_eq!(state.mode, ProcessingMode::Balanced);
        assert_eq!(state.input_device, None);
        assert!(!state.monitor_enabled);
        assert!(!state.autostart);
        assert!((state.input_level - 0.0).abs() < f32::EPSILON);
        assert!((state.output_level - 0.0).abs() < f32::EPSILON);
        assert!(state.available_devices.is_empty());
        assert_eq!(state.system_default_name, None);
        assert!(!state.khip_available);
        assert!(state.availability.is_empty());
        assert_eq!(state.fallback_notice, None);
    }

    #[test]
    fn ui_state_from_config_copies_fields() {
        let mut config = Config {
            input_device: Some("alsa_input.usb-Blue_Yeti".into()),
            engine: EngineType::RNNoise,
            mode: ProcessingMode::MaxQuality,
            monitor_enabled: true,
            enabled: false,
            autostart: true,
            ..Config::default()
        };
        config.set_strength_for(EngineType::RNNoise, 0.8);

        let state = UiState::from_config(&config);

        assert!(!state.active, "active maps from config.enabled");
        assert_eq!(state.engine, EngineType::RNNoise);
        assert_eq!(
            state.requested_engine,
            EngineType::RNNoise,
            "no fallback known from config alone — requested equals active"
        );
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
        assert!(state.availability.is_empty());
        assert_eq!(state.fallback_notice, None);
    }

    #[test]
    fn ui_state_from_config_reads_the_active_engines_own_strength() {
        // D-13: strength must come from the SELECTED engine's remembered
        // value, not a shared global — switching the configured engine must
        // change which value `from_config` surfaces.
        let mut config = Config::default();
        config.set_strength_for(EngineType::RNNoise, 0.15);
        config.set_strength_for(EngineType::Dpdfnet2, 0.85);

        config.engine = EngineType::RNNoise;
        assert!((UiState::from_config(&config).strength - 0.15).abs() < f32::EPSILON);

        config.engine = EngineType::Dpdfnet2;
        assert!((UiState::from_config(&config).strength - 0.85).abs() < f32::EPSILON);
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

    #[test]
    fn ui_state_availability_can_be_populated_with_all_five_engines() {
        let mut state = UiState::default();
        state.availability = crate::engine::all_engine_availability();
        assert_eq!(state.availability.len(), 5);
        for engine in EngineType::all() {
            assert!(state.availability.contains_key(&engine));
        }
    }

    #[test]
    fn ui_state_fallback_notice_reflects_requested_vs_active_divergence() {
        let mut state = UiState::default();
        state.engine = EngineType::DeepFilterNet;
        state.requested_engine = EngineType::Dpdfnet2;
        state.fallback_notice =
            crate::engine::fallback_notice(state.requested_engine, state.engine);

        assert_ne!(state.engine, state.requested_engine);
        let notice = state
            .fallback_notice
            .expect("fallback occurred, must be Some");
        assert!(
            notice.contains("DeepFilterNet"),
            "must name the ACTIVE engine"
        );
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
    fn ui_event_mode_changed() {
        let e = UiEvent::ModeChanged(ProcessingMode::LowCpu);
        assert_eq!(e, UiEvent::ModeChanged(ProcessingMode::LowCpu));
        assert_ne!(
            UiEvent::ModeChanged(ProcessingMode::LowCpu),
            UiEvent::ModeChanged(ProcessingMode::MaxQuality)
        );
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
            available: true,
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
            available: true,
        });
        state.available_devices.push(DeviceInfo {
            name: "alsa_input.pci-builtin".into(),
            description: "Built-in Microphone".into(),
            available: true,
        });
        assert_eq!(state.available_devices.len(), 2);
    }
}

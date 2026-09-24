//! Physical microphone enumeration.
//!
//! Provides [`InputDevice`] and [`list_input_devices()`] to discover available
//! input devices from PipeWire, filtering out the CleanMic virtual source.
//! When the `pipewire` feature is not enabled, a stub implementation returns
//! mock devices for testing and development.

use super::NODE_NAME;

/// A physical input (microphone) device known to PipeWire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    /// PipeWire node ID.
    pub id: u32,
    /// Short node name (e.g. `alsa_input.pci-0000_00_1f.3.analog-stereo`).
    pub name: String,
    /// Human-readable description (e.g. "Built-in Audio Analog Stereo").
    pub description: String,
    /// Whether this device is the system default/preferred microphone.
    pub is_default: bool,
    /// Whether this device's port/route is currently usable (R1).
    ///
    /// `false` only when every input route PipeWire reports for this node's
    /// card profile device is `"no"` (e.g. an unplugged headset jack). `"yes"`,
    /// `"unknown"`, or the absence of any route information at all means
    /// `true` (fail-open, per T-tua-02) — we never want a parsing gap to hide
    /// a device that is actually usable.
    pub available: bool,
}

/// Callback type for device change notifications (additions and removals).
pub type DeviceChangeCallback = Box<dyn Fn(&[InputDevice]) + Send + 'static>;

/// Manages device enumeration and change notification.
pub struct DeviceEnumerator {
    /// Registered listeners for device changes.
    listeners: Vec<DeviceChangeCallback>,
}

impl DeviceEnumerator {
    /// Create a new device enumerator.
    pub fn new() -> Self {
        Self {
            listeners: Vec::new(),
        }
    }

    /// Register a callback that fires when the device list changes.
    pub fn on_device_change(&mut self, callback: DeviceChangeCallback) {
        self.listeners.push(callback);
    }

    /// List available physical input (microphone) devices.
    ///
    /// The "CleanMic" virtual source is always excluded from the returned list.
    pub fn list_input_devices(&self) -> Vec<InputDevice> {
        let raw = raw_input_devices();
        filter_cleanmic(raw)
    }

    /// Notify all listeners with the current device list.
    ///
    /// In a real PipeWire implementation this would be called from the
    /// registry event loop when nodes are added or removed. In stub mode
    /// it can be called manually for testing.
    pub fn notify_listeners(&self) {
        let devices = self.list_input_devices();
        for listener in &self.listeners {
            listener(&devices);
        }
    }
}

impl Default for DeviceEnumerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Filter out any device whose name or description identifies it as the
/// CleanMic virtual source (case-insensitive to handle PipeWire name variations).
fn filter_cleanmic(devices: Vec<InputDevice>) -> Vec<InputDevice> {
    devices
        .into_iter()
        .filter(|d| {
            let name_lc = d.name.to_lowercase();
            let desc_lc = d.description.to_lowercase();
            let node_lc = NODE_NAME.to_lowercase();
            name_lc != node_lc && !desc_lc.contains(&node_lc)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Live implementation (requires running PipeWire daemon)
// ---------------------------------------------------------------------------

#[cfg(feature = "pipewire")]
fn raw_input_devices() -> Vec<InputDevice> {
    match enumerate_real_devices() {
        Ok(devices) if !devices.is_empty() => devices,
        Ok(_) => {
            log::warn!("pw-dump returned no audio source devices — falling back to stubs");
            stub_input_devices()
        }
        Err(e) => {
            log::warn!("Real device enumeration failed ({e}) — falling back to stub devices");
            stub_input_devices()
        }
    }
}

/// Query PipeWire via `pw-dump` and return all physical audio source nodes.
///
/// Falls back gracefully when `pw-dump` is not available. Actual parsing is
/// delegated to [`parse_pw_dump`], which is pure and feature-independent
/// (R4) so it can be unit-tested without a running PipeWire daemon.
#[cfg(feature = "pipewire")]
fn enumerate_real_devices() -> Result<Vec<InputDevice>, String> {
    use std::process::Command;

    let output = Command::new("pw-dump")
        .output()
        .map_err(|e| format!("failed to run pw-dump: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pw-dump exited with {}: {stderr}", output.status));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("failed to parse pw-dump JSON: {e}"))?;

    let entries = json
        .as_array()
        .ok_or_else(|| "pw-dump output is not a JSON array".to_string())?;

    let devices = parse_pw_dump(entries);

    log::debug!(
        "enumerate_real_devices: found {} audio source(s) via pw-dump",
        devices.len()
    );

    Ok(devices)
}

/// Parse a `pw-dump` JSON array (already deserialized) into [`InputDevice`]s.
///
/// Pure and feature-independent (R4): takes the deserialized entries, never
/// touches the filesystem or spawns a process, so it is fully unit-testable
/// without a running PipeWire daemon or the `pipewire` feature.
///
/// `pw-dump` outputs a JSON array of objects, each with an `id`, `type`, and
/// `info` field. Audio nodes have `type = "PipeWire:Interface:Node"` and carry
/// `props` with `media.class`, `node.name`, and `node.description`. Filters
/// out virtual sources (anything not exactly `media.class == "Audio/Source"`)
/// and CleanMic's own node. Availability (R1) is derived per-node from the
/// matching `PipeWire:Interface:Device` object's `Route`/`EnumRoute` params —
/// see [`route_available`].
pub fn parse_pw_dump(entries: &[serde_json::Value]) -> Vec<InputDevice> {
    let mut devices: Vec<InputDevice> = Vec::new();

    for entry in entries {
        // Only care about Node interfaces.
        if entry.get("type").and_then(|t| t.as_str()) != Some("PipeWire:Interface:Node") {
            continue;
        }

        let id = match entry.get("id").and_then(|v| v.as_u64()) {
            Some(v) => v as u32,
            None => continue,
        };

        let props = match entry.get("info").and_then(|i| i.get("props")) {
            Some(p) => p,
            None => continue,
        };

        let media_class = match props.get("media.class").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => continue,
        };

        // Keep only plain "Audio/Source" — not virtual sources.
        if media_class != "Audio/Source" {
            continue;
        }

        let node_name = props
            .get("node.name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if node_name.is_empty() {
            continue;
        }

        // Skip CleanMic's own virtual node (belt-and-suspenders; filter_cleanmic
        // also catches this, but let's not even add it here).
        if node_name == super::NODE_NAME {
            continue;
        }

        let description = props
            .get("node.description")
            .and_then(|v| v.as_str())
            .or_else(|| props.get("node.nick").and_then(|v| v.as_str()))
            .unwrap_or(&node_name)
            .to_string();

        let available = route_available(entries, props);

        devices.push(InputDevice {
            id,
            name: node_name,
            description,
            is_default: false, // filled in below
            available,
        });
    }

    // Second pass: mark the highest-priority device as default.
    // The node with the highest `priority.session` value is PipeWire's
    // preferred default source. If no node carries this property, fall back
    // to marking the first device in the list as the default.
    if !devices.is_empty() {
        let mut best_id: u32 = devices[0].id;
        let mut best_priority: u64 = 0;

        for entry in entries {
            if entry.get("type").and_then(|t| t.as_str()) != Some("PipeWire:Interface:Node") {
                continue;
            }
            let id = match entry.get("id").and_then(|v| v.as_u64()) {
                Some(v) => v as u32,
                None => continue,
            };
            // Only consider IDs that made it into our device list.
            if !devices.iter().any(|d| d.id == id) {
                continue;
            }
            let priority = entry
                .get("info")
                .and_then(|i| i.get("props"))
                .and_then(|p| p.get("priority.session"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if priority > best_priority {
                best_priority = priority;
                best_id = id;
            }
        }

        let mut marked = false;
        for dev in &mut devices {
            if dev.id == best_id {
                dev.is_default = true;
                marked = true;
                break;
            }
        }
        // If nothing had a priority hint, mark the first device as default.
        if !marked {
            devices[0].is_default = true;
        }
    }

    devices
}

/// Determine whether a node's input route is available (R1), by looking up
/// the matching `PipeWire:Interface:Device` object's `Route`/`EnumRoute`
/// params.
///
/// Reads `device.id` and `card.profile.device` from the node's `props`
/// (T-tua-02: every missing or mistyped field fails open to `true`, no
/// unwrap/expect/indexing on JSON values):
///
/// 1. Find the `PipeWire:Interface:Device` entry whose `id` equals the
///    node's `device.id`.
/// 2. Prefer active `Route` entries with `direction == "Input"` whose
///    integer `device` field equals the node's `card.profile.device`. If any
///    exist, available means any of them has `available` != `"no"`.
/// 3. Otherwise fall back to `EnumRoute` entries with `direction == "Input"`
///    whose `devices` array contains the card profile device. If none
///    exist, available (nothing to disqualify it). If some exist, available
///    means any of them is not `"no"`.
fn route_available(entries: &[serde_json::Value], node_props: &serde_json::Value) -> bool {
    let Some(device_id) = node_props.get("device.id").and_then(|v| v.as_u64()) else {
        return true;
    };
    let Some(card_profile_device) = node_props
        .get("card.profile.device")
        .and_then(|v| v.as_u64())
    else {
        return true;
    };

    let Some(device_entry) = entries.iter().find(|e| {
        e.get("type").and_then(|t| t.as_str()) == Some("PipeWire:Interface:Device")
            && e.get("id").and_then(|v| v.as_u64()) == Some(device_id)
    }) else {
        return true;
    };

    let Some(params) = device_entry.get("info").and_then(|i| i.get("params")) else {
        return true;
    };

    // Prefer active Route entries.
    if let Some(routes) = params.get("Route").and_then(|v| v.as_array()) {
        let matching: Vec<&serde_json::Value> = routes
            .iter()
            .filter(|r| {
                r.get("direction").and_then(|v| v.as_str()) == Some("Input")
                    && r.get("device").and_then(|v| v.as_u64()) == Some(card_profile_device)
            })
            .collect();
        if !matching.is_empty() {
            return matching
                .iter()
                .any(|r| r.get("available").and_then(|v| v.as_str()) != Some("no"));
        }
    }

    // Fall back to EnumRoute entries.
    if let Some(enum_routes) = params.get("EnumRoute").and_then(|v| v.as_array()) {
        let matching: Vec<&serde_json::Value> = enum_routes
            .iter()
            .filter(|r| {
                r.get("direction").and_then(|v| v.as_str()) == Some("Input")
                    && r.get("devices")
                        .and_then(|v| v.as_array())
                        .map(|devs| devs.iter().any(|d| d.as_u64() == Some(card_profile_device)))
                        .unwrap_or(false)
            })
            .collect();
        if matching.is_empty() {
            return true;
        }
        return matching
            .iter()
            .any(|r| r.get("available").and_then(|v| v.as_str()) != Some("no"));
    }

    // No route information at all — fail open.
    true
}

/// Filter and label the full device list for presentation in the picker
/// (R1, R2, OWNER-LOCK).
///
/// Presentation-only: this must never be used for capture-target resolution
/// (`resolve_runtime_capture_target`) or D-10 "no input device available"
/// detection, both of which need the UNFILTERED device list so an
/// unavailable-but-pinned/default device is never silently dropped from
/// capture.
///
/// Rules:
/// - Keeps a device if it is available, OR its name equals `pinned`, OR its
///   name equals `system_default` (never hide the persisted pick or the
///   device capture follows).
/// - If nothing survives but `devices` is non-empty, returns all devices
///   unfiltered (the picker never claims "no input" while sources exist).
/// - Exact-duplicate descriptions among the kept devices are made unique by
///   appending " (2)", " (3)", … to the second and later occurrences. The
///   first occurrence is left untouched and full real names are never
///   shortened (R2 / OWNER-LOCK).
pub fn picker_devices(
    devices: &[InputDevice],
    pinned: Option<&str>,
    system_default: Option<&str>,
) -> Vec<InputDevice> {
    let mut kept: Vec<InputDevice> = devices
        .iter()
        .filter(|d| {
            d.available
                || Some(d.name.as_str()) == pinned
                || Some(d.name.as_str()) == system_default
        })
        .cloned()
        .collect();

    if kept.is_empty() && !devices.is_empty() {
        kept = devices.to_vec();
    }

    // Disambiguate exact-duplicate descriptions, preserving order. First
    // occurrence untouched; subsequent ones get " (2)", " (3)", ….
    let mut seen_counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for dev in &mut kept {
        let count = seen_counts.entry(dev.description.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            dev.description = format!("{} ({})", dev.description, count);
        }
    }

    kept
}

// ---------------------------------------------------------------------------
// Stub implementation (no PipeWire daemon needed)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "pipewire"))]
fn raw_input_devices() -> Vec<InputDevice> {
    stub_input_devices()
}

/// Returns a fixed set of mock devices for testing / stub mode.
fn stub_input_devices() -> Vec<InputDevice> {
    vec![
        InputDevice {
            id: 42,
            name: "alsa_input.pci-0000_00_1f.3.analog-stereo".into(),
            description: "Built-in Audio Analog Stereo".into(),
            is_default: true,
            available: true,
        },
        InputDevice {
            id: 57,
            name: "alsa_input.usb-Blue_Yeti-00.analog-stereo".into(),
            description: "Blue Yeti USB Microphone".into(),
            is_default: false,
            available: true,
        },
        // This one should be filtered out by list_input_devices().
        InputDevice {
            id: 99,
            name: NODE_NAME.into(),
            description: "CleanMic Virtual Source".into(),
            is_default: false,
            available: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn list_input_devices_returns_devices() {
        let enumerator = DeviceEnumerator::new();
        let devices = enumerator.list_input_devices();
        assert!(
            !devices.is_empty(),
            "list_input_devices should return at least one device"
        );
    }

    #[test]
    fn cleanmic_is_filtered_out() {
        let enumerator = DeviceEnumerator::new();
        let devices = enumerator.list_input_devices();
        assert!(
            !devices.iter().any(|d| d.name == NODE_NAME),
            "CleanMic virtual source must not appear in the device list"
        );
    }

    #[test]
    fn devices_have_non_empty_name_and_description() {
        let enumerator = DeviceEnumerator::new();
        for device in enumerator.list_input_devices() {
            assert!(!device.name.is_empty(), "device name must not be empty");
            assert!(
                !device.description.is_empty(),
                "device description must not be empty"
            );
        }
    }

    #[test]
    fn default_device_is_identifiable() {
        let enumerator = DeviceEnumerator::new();
        let devices = enumerator.list_input_devices();
        let default_count = devices.iter().filter(|d| d.is_default).count();
        assert_eq!(
            default_count, 1,
            "exactly one device should be marked as default"
        );
    }

    #[test]
    fn device_change_callback_fires() {
        let mut enumerator = DeviceEnumerator::new();
        let received = Arc::new(Mutex::new(false));
        let received_clone = Arc::clone(&received);

        enumerator.on_device_change(Box::new(move |devices| {
            assert!(!devices.is_empty());
            *received_clone.lock().unwrap() = true;
        }));

        enumerator.notify_listeners();
        assert!(
            *received.lock().unwrap(),
            "callback should have been called"
        );
    }

    // -- Integration tests requiring a running PipeWire daemon --

    #[cfg(feature = "pipewire")]
    #[test]
    #[ignore]
    fn integration_list_real_devices() {
        let enumerator = DeviceEnumerator::new();
        let devs = enumerator.list_input_devices();
        for d in &devs {
            println!("device name={} available={}", d.name, d.available);
        }
        let picked = picker_devices(&devs, None, None);
        for d in &picked {
            println!("picker name={}", d.name);
        }
        assert!(!devs.is_empty(), "expected at least one real device");
        assert!(
            !devs.iter().any(|d| d.name == NODE_NAME),
            "CleanMic virtual source must not appear in the device list"
        );
    }

    #[test]
    #[ignore]
    fn integration_device_hotplug_notification() {
        // TODO: Create a PipeWire null source, verify device change callback
        // fires, then destroy it.
    }

    // ── parse_pw_dump / route_available (R1, R4, T-tua-02) ───────────────────

    /// The three node objects plus two device objects from the "Live pw-dump
    /// facts" fixture in the plan context, plus a CleanMic virtual source
    /// node that must be excluded.
    fn live_fixture_entries() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({
                "id": 62,
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "alsa_input.pci-0000_07_00.6.HiFi__Mic2__source",
                    "node.description": "Ryzen HD Audio Controller Stereo Microphone",
                    "device.id": 52,
                    "card.profile.device": 1,
                    "device.profile.description": "Stereo Microphone",
                    "priority.session": 2000
                }}
            }),
            serde_json::json!({
                "id": 63,
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "alsa_input.pci-0000_07_00.6.HiFi__Mic1__source",
                    "node.description": "Ryzen HD Audio Controller Digital Microphone",
                    "device.id": 52,
                    "card.profile.device": 2,
                    "device.profile.description": "Digital Microphone",
                    "priority.session": 2000
                }}
            }),
            serde_json::json!({
                "id": 104,
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "bluez_input.14:28:76:A3:1A:7E",
                    "node.description": "AirPods Pro",
                    "device.id": 64,
                    "card.profile.device": 0,
                    "priority.session": 2010
                }}
            }),
            serde_json::json!({
                "id": 200,
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Audio/Source/Virtual",
                    "node.name": NODE_NAME,
                    "node.description": "CleanMic Virtual Source"
                }}
            }),
            serde_json::json!({
                "id": 52,
                "type": "PipeWire:Interface:Device",
                "info": {"params": {
                    "EnumRoute": [
                        {"index": 0, "direction": "Output", "description": "Speaker", "available": "unknown", "devices": [0]},
                        {"index": 1, "direction": "Input", "description": "Stereo Microphone", "available": "no", "devices": [1]},
                        {"index": 2, "direction": "Input", "description": "Digital Microphone", "available": "unknown", "devices": [2]},
                        {"index": 3, "direction": "Output", "description": "Headphones", "available": "no", "devices": [3]}
                    ],
                    "Route": [
                        {"index": 0, "direction": "Output", "available": "unknown", "device": 0, "devices": [0]},
                        {"index": 2, "direction": "Input", "available": "unknown", "device": 2, "devices": [2]}
                    ]
                }}
            }),
            serde_json::json!({
                "id": 64,
                "type": "PipeWire:Interface:Device",
                "info": {"params": {
                    "EnumRoute": [
                        {"index": 0, "direction": "Input", "description": "Mains-libres", "available": "yes", "devices": [0]}
                    ],
                    "Route": [
                        {"index": 0, "direction": "Input", "available": "yes", "device": 0, "devices": [0]}
                    ]
                }}
            }),
        ]
    }

    #[test]
    fn parse_pw_dump_excludes_cleanmic_and_marks_availability() {
        let entries = live_fixture_entries();
        let devices = parse_pw_dump(&entries);

        assert_eq!(devices.len(), 3, "CleanMic virtual node must be excluded");
        assert!(!devices.iter().any(|d| d.name == NODE_NAME));

        let mic2 = devices
            .iter()
            .find(|d| d.name.contains("Mic2"))
            .expect("Mic2 present");
        assert!(!mic2.available, "unplugged Mic2 must be unavailable");

        let mic1 = devices
            .iter()
            .find(|d| d.name.contains("Mic1"))
            .expect("Mic1 present");
        assert!(mic1.available, "Mic1 must be available");

        let airpods = devices
            .iter()
            .find(|d| d.name.contains("bluez"))
            .expect("AirPods present");
        assert!(airpods.available, "AirPods must be available");
        assert!(
            airpods.is_default,
            "AirPods has the highest priority.session (2010) and must be default"
        );
    }

    #[test]
    fn route_takes_precedence_over_enum_route() {
        // Route says "no", EnumRoute says "unknown" for the same device —
        // the active Route entry wins.
        let entries = vec![
            serde_json::json!({
                "id": 1,
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "test-node",
                    "device.id": 10,
                    "card.profile.device": 5
                }}
            }),
            serde_json::json!({
                "id": 10,
                "type": "PipeWire:Interface:Device",
                "info": {"params": {
                    "EnumRoute": [
                        {"direction": "Input", "available": "unknown", "devices": [5]}
                    ],
                    "Route": [
                        {"direction": "Input", "available": "no", "device": 5}
                    ]
                }}
            }),
        ];
        let devices = parse_pw_dump(&entries);
        assert_eq!(devices.len(), 1);
        assert!(
            !devices[0].available,
            "active Route 'no' must win over EnumRoute 'unknown'"
        );
    }

    #[test]
    fn fail_open_no_device_id() {
        let entries = vec![serde_json::json!({
            "id": 1,
            "type": "PipeWire:Interface:Node",
            "info": {"props": {
                "media.class": "Audio/Source",
                "node.name": "test-node"
            }}
        })];
        let devices = parse_pw_dump(&entries);
        assert_eq!(devices.len(), 1);
        assert!(devices[0].available, "missing device.id must fail open");
    }

    #[test]
    fn fail_open_no_card_profile_device() {
        let entries = vec![serde_json::json!({
            "id": 1,
            "type": "PipeWire:Interface:Node",
            "info": {"props": {
                "media.class": "Audio/Source",
                "node.name": "test-node",
                "device.id": 10
            }}
        })];
        let devices = parse_pw_dump(&entries);
        assert_eq!(devices.len(), 1);
        assert!(
            devices[0].available,
            "missing card.profile.device must fail open"
        );
    }

    #[test]
    fn fail_open_missing_device_object() {
        let entries = vec![serde_json::json!({
            "id": 1,
            "type": "PipeWire:Interface:Node",
            "info": {"props": {
                "media.class": "Audio/Source",
                "node.name": "test-node",
                "device.id": 999,
                "card.profile.device": 1
            }}
        })];
        let devices = parse_pw_dump(&entries);
        assert_eq!(devices.len(), 1);
        assert!(
            devices[0].available,
            "no matching Device object must fail open"
        );
    }

    #[test]
    fn fail_open_device_without_params() {
        let entries = vec![
            serde_json::json!({
                "id": 1,
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "test-node",
                    "device.id": 10,
                    "card.profile.device": 1
                }}
            }),
            serde_json::json!({
                "id": 10,
                "type": "PipeWire:Interface:Device",
                "info": {}
            }),
        ];
        let devices = parse_pw_dump(&entries);
        assert_eq!(devices.len(), 1);
        assert!(
            devices[0].available,
            "Device object without params must fail open"
        );
    }

    #[test]
    fn fail_open_non_string_available_value() {
        let entries = vec![
            serde_json::json!({
                "id": 1,
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "test-node",
                    "device.id": 10,
                    "card.profile.device": 1
                }}
            }),
            serde_json::json!({
                "id": 10,
                "type": "PipeWire:Interface:Device",
                "info": {"params": {
                    "Route": [
                        {"direction": "Input", "available": 123, "device": 1}
                    ]
                }}
            }),
        ];
        let devices = parse_pw_dump(&entries);
        assert_eq!(devices.len(), 1);
        assert!(
            devices[0].available,
            "non-string available value must fail open, not panic"
        );
    }

    #[test]
    fn all_no_input_routes_gives_unavailable() {
        let entries = vec![
            serde_json::json!({
                "id": 1,
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "test-node",
                    "device.id": 10,
                    "card.profile.device": 5
                }}
            }),
            serde_json::json!({
                "id": 10,
                "type": "PipeWire:Interface:Device",
                "info": {"params": {
                    "EnumRoute": [
                        {"direction": "Input", "available": "no", "devices": [5]},
                        {"direction": "Input", "available": "no", "devices": [5]}
                    ]
                }}
            }),
        ];
        let devices = parse_pw_dump(&entries);
        assert!(!devices[0].available);
    }

    #[test]
    fn any_non_no_input_route_gives_available() {
        let entries = vec![
            serde_json::json!({
                "id": 1,
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "test-node",
                    "device.id": 10,
                    "card.profile.device": 5
                }}
            }),
            serde_json::json!({
                "id": 10,
                "type": "PipeWire:Interface:Device",
                "info": {"params": {
                    "EnumRoute": [
                        {"direction": "Input", "available": "no", "devices": [5]},
                        {"direction": "Input", "available": "unknown", "devices": [5]}
                    ]
                }}
            }),
        ];
        let devices = parse_pw_dump(&entries);
        assert!(devices[0].available);
    }

    #[test]
    fn output_direction_routes_are_ignored() {
        // The only Output route claims availability, but the only Input
        // route says "no" — Output must be ignored, so the result is false.
        let entries = vec![
            serde_json::json!({
                "id": 1,
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "test-node",
                    "device.id": 10,
                    "card.profile.device": 5
                }}
            }),
            serde_json::json!({
                "id": 10,
                "type": "PipeWire:Interface:Device",
                "info": {"params": {
                    "EnumRoute": [
                        {"direction": "Output", "available": "yes", "devices": [5]},
                        {"direction": "Input", "available": "no", "devices": [5]}
                    ]
                }}
            }),
        ];
        let devices = parse_pw_dump(&entries);
        assert!(
            !devices[0].available,
            "Output-direction routes must not count toward availability"
        );
    }

    // ── picker_devices (R1, R2, OWNER-LOCK) ───────────────────────────────────

    fn picker_fixture_devices() -> Vec<InputDevice> {
        vec![
            InputDevice {
                id: 62,
                name: "alsa_input.pci-0000_07_00.6.HiFi__Mic2__source".into(),
                description: "Ryzen HD Audio Controller Stereo Microphone".into(),
                is_default: false,
                available: false,
            },
            InputDevice {
                id: 63,
                name: "alsa_input.pci-0000_07_00.6.HiFi__Mic1__source".into(),
                description: "Ryzen HD Audio Controller Digital Microphone".into(),
                is_default: false,
                available: true,
            },
            InputDevice {
                id: 104,
                name: "bluez_input.14:28:76:A3:1A:7E".into(),
                description: "AirPods Pro".into(),
                is_default: true,
                available: true,
            },
        ]
    }

    #[test]
    fn picker_devices_excludes_unavailable_by_default() {
        let devs = picker_fixture_devices();
        let picked = picker_devices(&devs, None, None);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].name, devs[1].name);
        assert_eq!(picked[1].name, devs[2].name);
        assert!(!picked.iter().any(|d| d.name == devs[0].name));
    }

    #[test]
    fn picker_devices_keeps_pinned_unavailable_device_at_its_position() {
        let devs = picker_fixture_devices();
        let picked = picker_devices(&devs, Some(devs[0].name.as_str()), None);
        assert_eq!(picked.len(), 3);
        assert_eq!(picked[0].name, devs[0].name);
        assert!(!picked[0].available);
    }

    #[test]
    fn picker_devices_keeps_default_unavailable_device() {
        let devs = picker_fixture_devices();
        let picked = picker_devices(&devs, None, Some(devs[0].name.as_str()));
        assert_eq!(picked.len(), 3);
        assert!(picked.iter().any(|d| d.name == devs[0].name));
    }

    #[test]
    fn picker_devices_fails_open_when_nothing_survives() {
        let all_unavailable = vec![
            InputDevice {
                id: 1,
                name: "a".into(),
                description: "A Mic".into(),
                is_default: false,
                available: false,
            },
            InputDevice {
                id: 2,
                name: "b".into(),
                description: "B Mic".into(),
                is_default: false,
                available: false,
            },
        ];
        let picked = picker_devices(&all_unavailable, None, None);
        assert_eq!(
            picked.len(),
            2,
            "picker must never claim no input while sources exist"
        );
    }

    #[test]
    fn picker_devices_disambiguates_duplicate_descriptions() {
        let devs = vec![
            InputDevice {
                id: 1,
                name: "a".into(),
                description: "Same Name".into(),
                is_default: false,
                available: true,
            },
            InputDevice {
                id: 2,
                name: "b".into(),
                description: "Same Name".into(),
                is_default: false,
                available: true,
            },
            InputDevice {
                id: 3,
                name: "c".into(),
                description: "Same Name".into(),
                is_default: false,
                available: true,
            },
        ];
        let picked = picker_devices(&devs, None, None);
        assert_eq!(picked[0].description, "Same Name");
        assert_eq!(picked[1].description, "Same Name (2)");
        assert_eq!(picked[2].description, "Same Name (3)");
    }
}

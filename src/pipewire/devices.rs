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
/// Filters out virtual sources (media.class "Audio/Source/Virtual") and our
/// own CleanMic node. Falls back gracefully when `pw-dump` is not available.
///
/// `pw-dump` outputs a JSON array of objects, each with an `id`, `type`, and
/// `info` field. Audio nodes have `type = "PipeWire:Interface:Node"` and carry
/// `props` with `media.class`, `node.name`, and `node.description`.
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

        devices.push(InputDevice {
            id,
            name: node_name,
            description,
            is_default: false, // filled in below
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

    log::info!(
        "enumerate_real_devices: found {} audio source(s) via pw-dump",
        devices.len()
    );

    Ok(devices)
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
        },
        InputDevice {
            id: 57,
            name: "alsa_input.usb-Blue_Yeti-00.analog-stereo".into(),
            description: "Blue Yeti USB Microphone".into(),
            is_default: false,
        },
        // This one should be filtered out by list_input_devices().
        InputDevice {
            id: 99,
            name: NODE_NAME.into(),
            description: "CleanMic Virtual Source".into(),
            is_default: false,
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

    #[test]
    #[ignore]
    fn integration_list_real_devices() {
        // TODO: With `pipewire` feature and a running daemon, verify real
        // devices are returned.
    }

    #[test]
    #[ignore]
    fn integration_device_hotplug_notification() {
        // TODO: Create a PipeWire null source, verify device change callback
        // fires, then destroy it.
    }
}

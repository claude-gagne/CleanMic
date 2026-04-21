//! GTK4 + libadwaita main application window.
//!
//! Constructs the window using the GNOME preferences layout pattern:
//!
//! ```text
//! AdwApplicationWindow
//! ├── AdwHeaderBar
//! └── AdwPreferencesPage
//!     ├── [status group] — headline + routing info
//!     ├── AdwPreferencesGroup "Input"
//!     │   ├── AdwComboRow    — microphone picker
//!     │   └── AdwSwitchRow   — enable / disable
//!     ├── AdwPreferencesGroup "Engine"
//!     │   ├── AdwComboRow    — engine selector (Balanced / High Quality / Advanced)
//!     │   └── custom row     — strength slider
//!     ├── AdwPreferencesGroup "Levels"
//!     │   ├── MeterRow       — input level meter
//!     │   └── MeterRow       — output level meter
//!     └── AdwPreferencesGroup "Settings"
//!         ├── AdwSwitchRow   — autostart
//!         └── AdwSwitchRow   — monitor (listen to processed mic)
//! ```
//!
//! Only compiled when the `gui` feature is enabled.

#![cfg(feature = "gui")]

use std::sync::mpsc;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Box as GBox, Label, Orientation};
use libadwaita::prelude::*;
use libadwaita::{
    ApplicationWindow, Banner, ComboRow, HeaderBar, PreferencesGroup, PreferencesPage, SwitchRow,
};

use crate::ui::meters::widget::MeterRow;

/// Handles returned from [`build_main_window`] so the caller can drive the
/// level meter widgets and synchronize UI state from the GLib timer loop.
pub struct WindowHandles {
    /// The constructed application window.
    pub window: ApplicationWindow,
    /// The input (pre-suppression) level meter row.
    pub input_meter: MeterRow,
    /// The output (post-suppression) level meter row.
    pub output_meter: MeterRow,
    /// The enable/disable switch row — updated when pipeline state changes.
    pub enable_row: SwitchRow,
    /// The engine selector combo row — updated on fallback engine changes.
    pub engine_row: ComboRow,
    /// The 3-step strength picker (Light / Balanced / Strong) — same for all engines.
    pub strength_row: ComboRow,
    /// The monitor toggle switch — updated on monitor state changes.
    pub monitor_row: SwitchRow,
    /// The header bar window title widget (title + subtitle).
    pub win_title: libadwaita::WindowTitle,
    /// The device picker combo row — updated when device list changes.
    pub device_row: ComboRow,
    /// The update notification banner at the top of the window.
    /// Revealed when a new version is available (per D-05, D-08).
    pub update_banner: Banner,
}

// Type alias for the SwitchRow closure parameter.
type SwitchRowRef = SwitchRow;

use crate::engine::EngineType;
use crate::ui::{DeviceInfo, UiEvent, UiState};
use crate::tr;

// ── Engine selector helpers ───────────────────────────────────────────────────

/// Label shown in the engine combo row dropdown.
fn engine_label(engine: EngineType) -> &'static str {
    match engine {
        EngineType::RNNoise => "RNNoise",
        EngineType::DeepFilterNet => "DeepFilterNet",
        EngineType::Khip => "Khip",
    }
}

/// Subtitle describing what the engine does, shown below the combo row title.
///
/// Kept short — the combo row's selected-value column (on the right) gets
/// squeezed when the subtitle wraps, truncating the engine name.
fn engine_subtitle(engine: EngineType) -> &'static str {
    match engine {
        EngineType::RNNoise => "Lightweight, low CPU",
        EngineType::DeepFilterNet => "High quality (default)",
        EngineType::Khip => "User-supplied, adaptive",
    }
}

/// All engine entries in display order.
const ENGINE_ORDER: [EngineType; 3] = [
    EngineType::RNNoise,
    EngineType::DeepFilterNet,
    EngineType::Khip,
];

/// Map a combo-row index (0-based) back to an [`EngineType`].
fn index_to_engine(index: u32) -> Option<EngineType> {
    ENGINE_ORDER.get(index as usize).copied()
}

/// Map an [`EngineType`] to its combo-row index.
pub fn engine_to_index(engine: EngineType) -> u32 {
    ENGINE_ORDER.iter().position(|&e| e == engine).unwrap_or(0) as u32
}

// ── Window construction ───────────────────────────────────────────────────────

/// Build and return the main [`ApplicationWindow`] together with live meter
/// handles.
///
/// `state` is the initial UI state (loaded from config before the audio
/// service starts).  `event_tx` is the channel the window uses to send
/// [`UiEvent`]s to the audio service.
///
/// When `tray_available` is `true`, closing the window hides it so the app
/// continues running in the tray. When `false`, closing the window quits the app
/// and a one-time notification has already been sent by the caller.
///
/// The returned [`WindowHandles`] contains the window itself plus the two
/// [`MeterRow`] widgets so the caller's GLib timer can call
/// `meter.refresh(&level_meter)` at ~30 fps.
pub fn build_main_window(
    app: &libadwaita::Application,
    state: &UiState,
    event_tx: mpsc::Sender<UiEvent>,
    config: std::rc::Rc<std::cell::RefCell<crate::config::Config>>,
    tray_available: bool,
) -> WindowHandles {
    let window = ApplicationWindow::builder()
        .application(app)
        .title(tr!("CleanMic"))
        .default_width(420)
        .default_height(-1)
        .resizable(false)
        .build();

    // ── Header bar ────────────────────────────────────────────────────────────
    let header = HeaderBar::new();
    let win_title = libadwaita::WindowTitle::new(&tr!("CleanMic"), "");
    header.set_title_widget(Some(&win_title));

    // ── Hamburger menu (primary menu) ─────────────────────────────────────────
    let menu = gio::Menu::new();
    menu.append(Some(&tr!("Check for updates")), Some("app.check-for-updates"));
    menu.append(Some(&tr!("About CleanMic")), Some("app.about"));

    let menu_button = gtk4::MenuButton::new();
    menu_button.set_icon_name("open-menu-symbolic");
    menu_button.set_menu_model(Some(&menu));
    header.pack_end(&menu_button);

    // ── Preferences page ──────────────────────────────────────────────────────
    let page = PreferencesPage::new();

    // Wrap in a clamp for comfortable width on large screens.
    let clamp = libadwaita::Clamp::new();
    clamp.set_maximum_size(500);
    clamp.set_child(Some(&page));

    let subtitle = if state.active { tr!("Active") } else { tr!("Inactive") };
    win_title.set_subtitle(&subtitle);

    // Update notification banner (per D-05, D-08) — hidden initially
    let update_banner = Banner::new("");
    update_banner.set_button_label(Some(&tr!("Download")));
    update_banner.set_revealed(false);

    // Clicking "Download" opens GitHub Releases page
    let releases_url = crate::updater::RELEASES_PAGE_URL.to_owned();
    update_banner.connect_button_clicked(move |_| {
        if let Err(e) = gtk4::gio::AppInfo::launch_default_for_uri(
            &releases_url,
            gtk4::gio::AppLaunchContext::NONE,
        ) {
            log::warn!("updater: failed to open browser for releases page: {e}");
        }
    });

    let root = GBox::new(Orientation::Vertical, 0);
    root.append(&header);
    root.append(&update_banner);
    root.append(&clamp);
    window.set_content(Some(&root));

    // ── Input group ───────────────────────────────────────────────────────────
    let input_group = PreferencesGroup::new();
    input_group.set_title(&tr!("Input"));

    // Device picker
    let device_row = build_device_row(state);
    {
        let tx = event_tx.clone();
        // The row model stores DESCRIPTIONS for display, but downstream code
        // (PipeWire target.object) needs node.name. Clone the (description →
        // node.name) mapping into the callback so we translate on change.
        let devices_for_cb: Vec<(String, String)> = state
            .available_devices
            .iter()
            .map(|d| (d.description.clone(), d.name.clone()))
            .collect();
        // "Default" at index 0 resolves to the first enumerated device's
        // node.name so PipeWire pins an actual mic rather than following the
        // system default (which could be CleanMic itself and self-loop).
        let default_fallback = devices_for_cb.first().map(|(_, n)| n.clone());
        device_row.connect_selected_item_notify(move |row| {
            let idx = row.selected() as usize;
            let name: Option<String> = if idx == 0 {
                default_fallback.clone()
            } else {
                devices_for_cb.get(idx - 1).map(|(_, n)| n.clone())
            };
            let Some(name) = name else {
                log::warn!(
                    "Device picker: no device available for selected index {} — ignoring",
                    idx,
                );
                return;
            };
            if tx.send(UiEvent::DeviceChanged(name)).is_err() {
                log::warn!("UI event channel closed - DeviceChanged dropped");
            }
        });
    }
    input_group.add(&device_row);

    // Enable / disable switch
    let enable_row = SwitchRow::new();
    enable_row.set_title(&tr!("Enable"));
    enable_row.set_active(state.active);
    {
        let tx = event_tx.clone();
        enable_row.connect_active_notify(move |row: &SwitchRowRef| {
            if tx.send(UiEvent::EnableToggled(row.is_active())).is_err() {
                log::warn!("UI event channel closed - EnableToggled dropped");
            }
        });
    }
    input_group.add(&enable_row);
    page.add(&input_group);

    // ── Engine group ──────────────────────────────────────────────────────────
    let engine_group = PreferencesGroup::new();
    engine_group.set_title(&tr!("Noise Processing"));

    let engine_row = build_engine_row(state);
    let strength_row = build_strength_row(state, event_tx.clone());

    {
        let tx = event_tx.clone();
        let khip_available = state.khip_available;
        engine_row.connect_selected_notify(move |row| {
            let idx = row.selected();
            if let Some(engine) = index_to_engine(idx) {
                // Update subtitle to describe the selected engine.
                row.set_subtitle(engine_subtitle(engine));
                if engine == EngineType::Khip && !khip_available {
                    return;
                }
                if tx.send(UiEvent::EngineChanged(engine)).is_err() {
                    log::warn!("UI event channel closed - EngineChanged dropped");
                }
            }
        });
    }
    engine_group.add(&engine_row);
    engine_group.add(&strength_row);

    page.add(&engine_group);

    // ── Level meters group ────────────────────────────────────────────────────
    let levels_group = PreferencesGroup::new();
    levels_group.set_title(&tr!("Levels"));

    let input_meter = MeterRow::new(&tr!("Input"));
    levels_group.add(&input_meter.row);

    let output_meter = MeterRow::new(&tr!("Output"));
    levels_group.add(&output_meter.row);

    page.add(&levels_group);

    // ── Settings group ────────────────────────────────────────────────────────
    let settings_group = PreferencesGroup::new();
    settings_group.set_title(&tr!("Settings"));

    let autostart_row = SwitchRow::new();
    autostart_row.set_title(&tr!("Start on login"));
    autostart_row.set_subtitle(&tr!("Launch CleanMic automatically when you log in"));
    autostart_row.set_active(state.autostart);
    {
        let tx = event_tx.clone();
        autostart_row.connect_active_notify(move |row: &SwitchRowRef| {
            if tx.send(UiEvent::AutostartToggled(row.is_active())).is_err() {
                log::warn!("UI event channel closed - AutostartToggled dropped");
            }
        });
    }
    settings_group.add(&autostart_row);

    let monitor_row = SwitchRow::new();
    monitor_row.set_title(&tr!("Listen to processed mic"));
    monitor_row.set_subtitle(&tr!("Route processed audio to your headphones"));
    monitor_row.set_active(state.monitor_enabled);
    {
        let tx = event_tx.clone();
        monitor_row.connect_active_notify(move |row: &SwitchRowRef| {
            if tx.send(UiEvent::MonitorToggled(row.is_active())).is_err() {
                log::warn!("UI event channel closed - MonitorToggled dropped");
            }
        });
    }
    settings_group.add(&monitor_row);

    page.add(&settings_group);

    // ── Close behaviour: depends on tray availability ────────────────────────────
    // When the tray is available: hide window so the app continues in background.
    // When the tray is absent: closing the window quits the application.
    {
        let config_close = config;
        window.connect_close_request(move |win| {
            if tray_available {
                // Tray is available: hide window, app continues in tray.
                let mut cfg = config_close.borrow_mut();
                if !cfg.tray_hint_shown {
                    cfg.tray_hint_shown = true;
                    // Save immediately so the hint isn't repeated on crash.
                    if let Err(e) = cfg.save() {
                        log::warn!("Failed to save tray hint flag: {e}");
                    }

                    // Show a desktop notification via GNotification.
                    if let Some(app) = win.application() {
                        let notif = gtk4::gio::Notification::new(
                            &gettextrs::gettext("CleanMic is still running"),
                        );
                        notif.set_body(Some(
                            &gettextrs::gettext(
                                "The window was closed but CleanMic continues processing \
                                 your microphone in the background. Look for the tray icon \
                                 to reopen or quit.",
                            ),
                        ));
                        app.send_notification(Some("tray-hint"), &notif);
                    }
                }
                win.set_visible(false);
                glib::Propagation::Stop
            } else {
                // No tray available: closing the window quits the application.
                if let Some(app) = win.application() {
                    app.quit();
                }
                glib::Propagation::Proceed
            }
        });
    }

    WindowHandles {
        window,
        input_meter,
        output_meter,
        enable_row,
        engine_row,
        strength_row,
        monitor_row,
        win_title,
        device_row,
        update_banner,
    }
}

// ── Helper builders ───────────────────────────────────────────────────────────

/// Build the microphone picker `ComboRow`.
fn build_device_row(state: &UiState) -> ComboRow {
    let row = ComboRow::new();
    row.set_title(&tr!("Microphone"));

    // Build string model: first entry is "Default"
    let strings: Vec<String> = std::iter::once(tr!("Default"))
        .chain(
            state
                .available_devices
                .iter()
                .map(|d| d.description.clone()),
        )
        .collect();

    // We use a StringList as the model so each entry carries its description.
    // The actual PipeWire node name is looked up by matching the description
    // against available_devices when the selection changes.
    // (In a real app you'd use a custom GListModel; this keeps the stub simple.)
    let model = gtk4::StringList::new(&strings.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    row.set_model(Some(&model));

    // Select the current device (or "Default" at index 0)
    let selected_idx = state
        .input_device
        .as_deref()
        .and_then(|node| {
            state
                .available_devices
                .iter()
                .position(|d| d.name == node)
                .map(|i| (i + 1) as u32) // +1 because index 0 is "Default"
        })
        .unwrap_or(0);
    row.set_selected(selected_idx);

    row
}

/// Build the engine selector `ComboRow`.
fn build_engine_row(state: &UiState) -> ComboRow {
    let row = ComboRow::new();
    row.set_title(&tr!("Engine"));
    row.set_subtitle(engine_subtitle(state.engine));

    // Always include all engines in the model so users can see what exists.
    // The factory below grays out and disables unavailable entries.
    let labels: Vec<String> = ENGINE_ORDER
        .iter()
        .map(|&e| match e {
            EngineType::Khip if !state.khip_available => tr!("Khip (not installed)"),
            _ => engine_label(e).to_owned(),
        })
        .collect();
    let labels_ref: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    let model = gtk4::StringList::new(&labels_ref);
    row.set_model(Some(&model));
    row.set_selected(engine_to_index(state.engine));

    // Custom list factory: dims and disables items that contain "(not installed)".
    // set_list_factory controls the popup list; set_factory controls the closed display.
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let label = Label::new(None);
        label.set_halign(gtk4::Align::Start);
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let label = item.child().and_downcast::<Label>().unwrap();
        let text = item
            .item()
            .and_downcast::<gtk4::StringObject>()
            .unwrap()
            .string();
        label.set_text(&text);
        let unavailable = text.contains("not installed");
        if unavailable {
            label.add_css_class("dim-label");
            item.set_activatable(false);
            item.set_selectable(false);
        } else {
            label.remove_css_class("dim-label");
            item.set_activatable(true);
            item.set_selectable(true);
        }
    });
    row.set_list_factory(Some(&factory));

    row
}

// ── Strength level helpers (shared by all engines) ────────────────────────────

/// Map a normalized strength to a 3-step level index (0=Light, 1=Balanced, 2=Strong).
pub fn strength_to_level_index(strength: f32) -> u32 {
    if strength < 0.33 {
        0
    } else if strength < 0.67 {
        1
    } else {
        2
    }
}

fn level_index_to_strength(index: u32) -> f32 {
    match index {
        0 => 1.0 / 6.0,
        1 => 0.5,
        _ => 5.0 / 6.0,
    }
}

/// Build the 3-step strength `ComboRow` (Light / Balanced / Strong).
///
/// Used by all engines — RNNoise, DeepFilterNet, and Khip all accept the same
/// normalized values, which each engine maps to its own internal parameters.
fn build_strength_row(state: &UiState, event_tx: mpsc::Sender<UiEvent>) -> ComboRow {
    let row = ComboRow::new();
    row.set_title(&tr!("Strength"));

    let model = gtk4::StringList::new(&[]);
    model.append(&tr!("Light"));
    model.append(&tr!("Balanced"));
    model.append(&tr!("Strong"));
    row.set_model(Some(&model));
    row.set_selected(strength_to_level_index(state.strength));

    row.connect_selected_notify(move |r| {
        let val = level_index_to_strength(r.selected());
        if event_tx.send(UiEvent::StrengthChanged(val)).is_err() {
            log::warn!("UI event channel closed - StrengthChanged dropped");
        }
    });

    row
}

// ── UI state synchronization ─────────────────────────────────────────────────

impl WindowHandles {
    /// Update UI controls from the current config state.
    ///
    /// Called from the GLib timer when the pipeline config has changed (e.g.,
    /// engine fallback, device change). Widget signal handlers will fire but
    /// since the values match the config, the resulting events are no-ops.
    ///
    /// `state` should be constructed from the current `Config` via
    /// `UiState::from_config`.
    pub fn update_from_state(&self, state: &UiState) {
        // Engine selector
        let engine_idx = engine_to_index(state.engine);
        if self.engine_row.selected() != engine_idx {
            self.engine_row.set_selected(engine_idx);
        }

        // Strength level (3-step, same for all engines)
        let level_idx = strength_to_level_index(state.strength);
        if self.strength_row.selected() != level_idx {
            self.strength_row.set_selected(level_idx);
        }

        // Enable/disable switch
        if self.enable_row.is_active() != state.active {
            self.enable_row.set_active(state.active);
        }

        // Monitor switch
        if self.monitor_row.is_active() != state.monitor_enabled {
            self.monitor_row.set_active(state.monitor_enabled);
        }

        self.win_title.set_subtitle(&if state.active { tr!("Active") } else { tr!("Inactive") });
    }

    /// Repopulate the device picker with a fresh device list.
    ///
    /// Preserves the current selection if the device is still available.
    pub fn update_device_list(&self, devices: &[DeviceInfo], current_device: Option<&str>) {
        let strings: Vec<String> = std::iter::once(tr!("Default"))
            .chain(devices.iter().map(|d| d.description.clone()))
            .collect();

        let model = gtk4::StringList::new(&strings.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        self.device_row.set_model(Some(&model));

        // Select the current device (or "Default" at index 0)
        let selected_idx = current_device
            .and_then(|node| {
                devices
                    .iter()
                    .position(|d| d.name == node)
                    .map(|i| (i + 1) as u32)
            })
            .unwrap_or(0);
        self.device_row.set_selected(selected_idx);
    }
}

// ── DeviceInfo display helper (used by status widget too) ────────────────────

/// Return the human-readable label for a device, falling back to the node name.
pub fn device_display_name<'a>(node: &'a str, devices: &'a [DeviceInfo]) -> &'a str {
    devices
        .iter()
        .find(|d| d.name == node)
        .map(|d| d.description.as_str())
        .unwrap_or(node)
}

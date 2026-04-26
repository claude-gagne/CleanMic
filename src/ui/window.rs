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
//!     ├── AdwPreferencesGroup "Noise Processing"
//!     │   └── AdwActionRow×3 — engine selector (radio-grouped: RNNoise / DeepFilterNet / Khip)
//!     ├── AdwPreferencesGroup "Strength"
//!     │   └── AdwComboRow    — strength picker (Light / Balanced / Strong)
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

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Box as GBox, Orientation};
use libadwaita::prelude::*;
use libadwaita::{
    ApplicationWindow, Banner, ComboRow, HeaderBar, PreferencesGroup, PreferencesPage, SwitchRow,
};

use crate::ui::meters::widget::MeterRow;

/// Handles returned from [`build_main_window`] so the caller can drive the
/// level meter widgets and synchronize UI state from the GLib timer loop.
///
/// Derives `Clone` because Plan 03's 1500ms default-source polling timer
/// clones the entire `handles` struct into its closure (Option B scaffolding
/// choice — avoids wrapping in `Rc<RefCell<...>>`). Every field is a
/// GObject-refcounted widget; cloning bumps a refcount rather than
/// deep-copying, so the clones share the same underlying widgets as the
/// originals (standard gtk4 widget-sharing semantics).
#[derive(Clone)]
pub struct WindowHandles {
    /// The constructed application window.
    pub window: ApplicationWindow,
    /// The input (pre-suppression) level meter row.
    pub input_meter: MeterRow,
    /// The output (post-suppression) level meter row.
    pub output_meter: MeterRow,
    /// The enable/disable switch row — updated when pipeline state changes.
    pub enable_row: SwitchRow,
    /// The engine selector — a list of radio-grouped rows. Programmatic
    /// engine changes (e.g., from the tray or audio fallback) must go through
    /// `EngineSelector::set_engine()`, which uses an internal guard flag to
    /// prevent feedback loops.
    pub engine_selector: EngineSelector,
    /// The 3-step strength picker (Light / Balanced / Strong) — same for all engines.
    pub strength_row: ComboRow,
    /// The monitor toggle switch — updated on monitor state changes.
    pub monitor_row: SwitchRow,
    /// The header bar window title widget (title + subtitle).
    pub win_title: libadwaita::WindowTitle,
    /// The device picker combo row — updated when device list changes.
    pub device_row: ComboRow,
    /// Flag set while [`update_device_list`] is mutating the picker model so
    /// the selected-item-notify handler can skip spurious events emitted by
    /// `set_model` / `set_selected`. Without this guard, every programmatic
    /// refresh would fire `UiEvent::DeviceChanged`, overwriting the user's
    /// explicit pick in `config.input_device` and breaking D-06 + D-03.
    pub device_updating: Rc<Cell<bool>>,
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

/// Display name for an engine (used as the row title in the selector).
fn engine_label(engine: EngineType) -> &'static str {
    match engine {
        EngineType::RNNoise => "RNNoise",
        EngineType::DeepFilterNet => "DeepFilterNet",
        EngineType::Khip => "Khip",
    }
}

/// Subtitle describing what the engine does, shown below the row title.
///
/// Kept short so it fits in a single AdwActionRow subtitle line.
fn engine_subtitle(engine: EngineType) -> String {
    // Per 08.3 D-01: wrap engine subtitles in tr!() for i18n coverage.
    match engine {
        EngineType::RNNoise => tr!("Lightweight, low CPU"),
        EngineType::DeepFilterNet => tr!("High quality (default)"),
        EngineType::Khip => tr!("User-supplied, adaptive"),
    }
}

/// Engine selector built from a list of AdwActionRow + radio-grouped CheckButton
/// pairs, mirroring the GNOME Sound Settings output-device pattern.
///
/// Replaces the previous AdwComboRow which could not enforce row-level
/// disabling — `set_activatable(false)` on a `gtk4::ListItem` only affects
/// rendering, not GtkDropDown's selection model, so users could still pick
/// "Khip (not installed)" with no effect (silent early-return in the handler).
///
/// This selector uses `set_sensitive(false)` on the Khip row when
/// `khip_available=false`, matching the tray's `enabled` flag semantics in
/// `src/tray.rs:240`.
#[derive(Clone)]
pub struct EngineSelector {
    /// The AdwPreferencesGroup that holds all engine rows. Add this to the page.
    pub group: PreferencesGroup,
    /// (engine, row, check_button) tuples in display order. Cloning is cheap
    /// (GObject refcount bumps, plus an Rc clone via the embedding struct).
    rows: Vec<(EngineType, libadwaita::ActionRow, gtk4::CheckButton)>,
    /// Guard flag — set to `true` while `set_engine()` is mutating the active
    /// row programmatically, so the per-row toggled handler returns early
    /// instead of emitting a spurious `UiEvent::EngineChanged`. Mirror of the
    /// existing `device_updating` pattern (window.rs:216-219, 235-237, 702-705).
    updating: Rc<Cell<bool>>,
}

impl EngineSelector {
    /// Programmatically set the active engine without firing UiEvent::EngineChanged.
    /// Used by the audio→UI sync path (e.g., tray-initiated changes, engine
    /// fallback). Sets the guard flag, mutates the matching CheckButton's
    /// `active` state, then clears the guard.
    pub fn set_engine(&self, engine: EngineType) {
        self.updating.set(true);
        for (e, _row, check) in &self.rows {
            if *e == engine && !check.is_active() {
                check.set_active(true);
            }
        }
        self.updating.set(false);
    }

    /// Return the currently active engine (the one whose CheckButton is
    /// active). Returns `None` only in the impossible state where no row is
    /// active — callers should treat that as "no change".
    #[allow(dead_code)]
    pub fn active_engine(&self) -> Option<EngineType> {
        self.rows
            .iter()
            .find(|(_, _, c)| c.is_active())
            .map(|(e, _, _)| *e)
    }

    /// Set sensitivity on every row at once. Used by the health-check path
    /// (`src/app.rs`) to disable engine selection when the audio thread
    /// dies (D-15). Per-engine availability (Khip) is set at construction
    /// time and is **independent** of this — calling `set_all_sensitive(true)`
    /// after the audio thread is restored does NOT undo Khip's
    /// per-row disabled state, because we read each CheckButton's current
    /// sensitivity (the construction-time source of truth) before re-enabling.
    pub fn set_all_sensitive(&self, sensitive: bool) {
        for (_engine, row, check) in &self.rows {
            // Khip row stays disabled if it was disabled at construction time
            // (khip_available=false). Re-enabling the row here would let the
            // user pick an engine that cannot init — bug we're fixing.
            let allow = if sensitive {
                check.is_sensitive()
            } else {
                false
            };
            row.set_sensitive(allow);
        }
    }
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
    let device_updating: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    {
        let tx = event_tx.clone();
        let device_updating_cb = device_updating.clone();
        // Clone the (description → node.name) mapping for real devices.
        // Used to translate a real-device pick back to a PipeWire node name.
        let devices_for_cb: Vec<(String, String)> = state
            .available_devices
            .iter()
            .map(|d| (d.description.clone(), d.name.clone()))
            .collect();
        // Precompute the "Default " prefix so we can detect synthetic Default entries
        // in the current model at selection time. Format matches build_device_model:
        // tr!("Default") + " (" + desc + ")".
        let default_prefix = format!("{} (", tr!("Default"));
        device_row.connect_selected_item_notify(move |row| {
            // G-05 guard: skip events fired by programmatic model refreshes
            // in update_device_list. Only real user clicks should emit
            // UiEvent::DeviceChanged / DeviceChangedToDefault.
            if device_updating_cb.get() {
                return;
            }
            let idx = row.selected() as usize;
            // Read the string at the selected index AND at index 0 in one model access.
            let model = row
                .model()
                .and_then(|m| m.downcast::<gtk4::StringList>().ok());
            let item_text: Option<String> = model
                .as_ref()
                .and_then(|sl| sl.string(idx as u32).map(|s| s.to_string()));
            // Direct model[0] read for "is Default present?" — simpler than the
            // nested item_text.as_ref().map(...) pattern (planner revision w7).
            let has_default = model
                .as_ref()
                .and_then(|sl| sl.string(0))
                .map(|first| first.to_string().starts_with(&default_prefix))
                .unwrap_or(false);

            // D-10: "No input device available" is the only string — picker is
            // insensitive, but guard anyway.
            if let Some(ref text) = item_text
                && text == &tr!("No input device available")
            {
                return;
            }

            // D-01/D-06: index 0 is the Default entry when it starts with
            // "Default (" — emit DeviceChangedToDefault. Otherwise it's a
            // real device (Default is hidden).
            if idx == 0 && has_default {
                if tx.send(UiEvent::DeviceChangedToDefault).is_err() {
                    log::warn!("UI event channel closed - DeviceChangedToDefault dropped");
                }
                return;
            }

            // Real-device pick. Figure out the offset: if index 0 was the
            // Default entry, real devices start at index 1. Otherwise at 0.
            let real_idx = if has_default { idx.saturating_sub(1) } else { idx };
            let name: Option<String> = devices_for_cb.get(real_idx).map(|(_, n)| n.clone());
            let Some(name) = name else {
                log::warn!(
                    "Device picker: no device available for selected index {idx} (real_idx {real_idx}) — ignoring"
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
    // EngineSelector owns its own AdwPreferencesGroup with one ActionRow per
    // engine plus radio-grouped CheckButtons. Replaces a ComboRow whose
    // list-factory disabling didn't actually prevent selection (UAT bug #3).
    let engine_selector = build_engine_selector(state, event_tx.clone());
    let strength_row = build_strength_row(state, event_tx.clone());

    // The strength row stays in its own group so the radio-row group reads
    // cleanly as "pick one engine" (matches GNOME Sound Settings output-device
    // styling — the volume slider is in a separate group from the device list).
    let strength_group = PreferencesGroup::new();
    strength_group.set_title(&tr!("Strength"));
    strength_group.add(&strength_row);

    page.add(&engine_selector.group);
    page.add(&strength_group);

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
        engine_selector,
        strength_row,
        monitor_row,
        win_title,
        device_row,
        device_updating,
        update_banner,
    }
}

// ── Helper builders ───────────────────────────────────────────────────────────

/// Result of computing the picker's string model and current selection.
///
/// `strings` is the list shown in the dropdown.
/// `selected_idx` is the index the combo row should mark as active.
/// `default_present` indicates whether index 0 is the synthetic "Default (Mic)"
/// entry (true) or the first real device (false). The selection closure uses
/// this to decide which UiEvent variant to emit when index 0 is picked.
/// `no_input` indicates the D-10 "No input device available" state.
struct DevicePickerModel {
    strings: Vec<String>,
    selected_idx: u32,
    /// Retained as part of the helper's contract even though the selection
    /// closure inspects the StringList directly (via `has_default` prefix
    /// match). Plan 03 or future consumers that render the picker from the
    /// computed model without re-reading the widget can read this flag.
    #[allow(dead_code)]
    default_present: bool,
    no_input: bool,
}

/// Compute the picker's string list and selection state from the current
/// device list, the OS default name, and the user's persisted input_device.
///
/// Rules (per D-01, D-02, D-10):
/// - `system_default_name = Some(name)` + `name` resolves to a real device in `devices`
///   → prepend `"Default (description)"` as index 0; real devices follow at index 1..N.
/// - `system_default_name = None` OR the default name is not in `devices`
///   → no Default entry; real devices start at index 0.
/// - `devices` empty AND `system_default_name` is None
///   → single entry `"No input device available"` (D-10). `no_input = true`.
fn build_device_model(
    devices: &[DeviceInfo],
    system_default_name: Option<&str>,
    current_device: Option<&str>,
) -> DevicePickerModel {
    // D-10 no-input branch.
    if devices.is_empty() && system_default_name.is_none() {
        return DevicePickerModel {
            strings: vec![tr!("No input device available")],
            selected_idx: 0,
            default_present: false,
            no_input: true,
        };
    }

    // Resolve the default's description, if present and in the device list.
    let default_description: Option<String> = system_default_name
        .and_then(|name| devices.iter().find(|d| d.name == name))
        .map(|d| d.description.clone());

    let mut strings: Vec<String> = Vec::with_capacity(devices.len() + 1);
    let default_present = if let Some(ref desc) = default_description {
        // D-02 label format: tr!("Default") + " (" + description + ")"
        strings.push(format!("{} ({})", tr!("Default"), desc));
        true
    } else {
        false
    };
    for d in devices {
        strings.push(d.description.clone());
    }

    // Compute selected_idx:
    // - If current_device is None AND Default is present → index 0 (following OS default).
    // - If current_device is Some(name) AND name matches a real device → its position + (1 if default_present else 0).
    // - Else → 0 (fall back to first entry, which is either Default or the first real mic).
    let offset: u32 = if default_present { 1 } else { 0 };
    let selected_idx = match current_device {
        None if default_present => 0,
        Some(name) => devices
            .iter()
            .position(|d| d.name == name)
            .map(|i| i as u32 + offset)
            .unwrap_or(0),
        None => 0,
    };

    DevicePickerModel {
        strings,
        selected_idx,
        default_present,
        no_input: false,
    }
}

/// Build the microphone picker `ComboRow`.
fn build_device_row(state: &UiState) -> ComboRow {
    let row = ComboRow::new();
    row.set_title(&tr!("Microphone"));

    let model = build_device_model(
        &state.available_devices,
        state.system_default_name.as_deref(),
        state.input_device.as_deref(),
    );

    let list = gtk4::StringList::new(
        &model.strings.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
    );
    row.set_model(Some(&list));
    row.set_selected(model.selected_idx);
    row.set_sensitive(!model.no_input);

    row
}

/// Build the engine selector as an AdwPreferencesGroup containing one
/// AdwActionRow per engine, each with a radio-grouped CheckButton suffix.
///
/// The previous ComboRow-based approach could not enforce row-level disabling:
/// `set_activatable(false)` on a `gtk4::ListItem` only affects rendering, not
/// GtkDropDown's selection model, so users could still pick "Khip (not
/// installed)" with no effect. Mirrors the tray's enabled-flag semantics.
fn build_engine_selector(
    state: &UiState,
    event_tx: mpsc::Sender<UiEvent>,
) -> EngineSelector {
    let group = PreferencesGroup::new();
    group.set_title(&tr!("Noise Processing"));

    let updating: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let mut rows: Vec<(EngineType, libadwaita::ActionRow, gtk4::CheckButton)> =
        Vec::with_capacity(3);

    let engines = [
        EngineType::RNNoise,
        EngineType::DeepFilterNet,
        EngineType::Khip,
    ];

    // Build the radio group: first CheckButton is the group leader; subsequent
    // ones are joined via set_group(Some(&group_leader)).
    let mut group_leader: Option<gtk4::CheckButton> = None;

    for engine in engines {
        let row = libadwaita::ActionRow::new();

        // Title: "Khip (not installed)" when unavailable, else the plain label.
        // Subtitle: the per-engine description (already i18n-wrapped via tr!()
        // in engine_subtitle()).
        let title = if engine == EngineType::Khip && !state.khip_available {
            tr!("Khip (not installed)")
        } else {
            engine_label(engine).to_owned()
        };
        row.set_title(&title);
        row.set_subtitle(&engine_subtitle(engine));

        let check = gtk4::CheckButton::new();
        check.set_valign(gtk4::Align::Center);
        if let Some(ref leader) = group_leader {
            check.set_group(Some(leader));
        } else {
            group_leader = Some(check.clone());
        }

        // Initial selection: this row's CheckButton is active iff it matches
        // state.engine.
        check.set_active(engine == state.engine);

        // Khip row: sensitive=false when not installed. Setting on the row
        // makes the entire row visually disabled and unclickable; setting on
        // the CheckButton too is belt-and-suspenders so a programmatic
        // `set_active(true)` from a future bug also no-ops cleanly.
        if engine == EngineType::Khip && !state.khip_available {
            row.set_sensitive(false);
            check.set_sensitive(false);
        }

        // Make the whole row clickable to toggle the CheckButton (standard
        // AdwActionRow + radio pattern). When row.set_sensitive(false), this
        // does nothing — the row swallows clicks. That's the fix.
        row.add_prefix(&check);
        row.set_activatable_widget(Some(&check));

        // Per-row toggled handler. Fires for BOTH the row going inactive and
        // the row going active in a radio group, so we filter on is_active().
        {
            let tx = event_tx.clone();
            let updating_cb = updating.clone();
            let khip_available = state.khip_available;
            check.connect_toggled(move |btn| {
                // Skip the "deactivating" half of the radio toggle — only the
                // row gaining selection should send an event.
                if !btn.is_active() {
                    return;
                }
                // Guard against programmatic mutations from set_engine().
                if updating_cb.get() {
                    return;
                }
                // Belt-and-suspenders: even if some future code path makes
                // the unavailable Khip row sensitive, refuse to dispatch.
                if engine == EngineType::Khip && !khip_available {
                    return;
                }
                if tx.send(UiEvent::EngineChanged(engine)).is_err() {
                    log::warn!("UI event channel closed - EngineChanged dropped");
                }
            });
        }

        group.add(&row);
        rows.push((engine, row, check));
    }

    EngineSelector { group, rows, updating }
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
        // Engine selector — set_engine is a no-op if the matching row is already
        // active, and uses a guard flag internally so it never re-emits EngineChanged.
        self.engine_selector.set_engine(state.engine);

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
    /// When `system_default_name` is `Some` and resolves to a device in
    /// `devices`, a "Default (MicName)" entry is prepended. When `None`,
    /// no Default entry is shown. When `devices` is empty AND the default
    /// is unresolvable, shows "No input device available" and marks the
    /// picker insensitive. Per D-01, D-02, D-10.
    pub fn update_device_list(
        &self,
        devices: &[DeviceInfo],
        current_device: Option<&str>,
        system_default_name: Option<&str>,
    ) {
        let model = build_device_model(devices, system_default_name, current_device);
        let list = gtk4::StringList::new(
            &model.strings.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        );
        // G-05: guard the selected-item-notify handler so the programmatic
        // set_model / set_selected calls below don't emit a spurious
        // UiEvent::DeviceChanged. Resetting to false after both calls complete
        // ensures user clicks captured after this update still fire normally.
        self.device_updating.set(true);
        self.device_row.set_model(Some(&list));
        self.device_row.set_selected(model.selected_idx);
        self.device_updating.set(false);
        self.device_row.set_sensitive(!model.no_input);
    }

    /// Control the "input available" UI state for D-10.
    ///
    /// When `available = false`: disables the enable toggle (sensitive = false)
    /// and forces the switch off so the pipeline doesn't try to capture.
    /// When `available = true`: re-enables the toggle (sensitive = true); the
    /// caller is responsible for restoring the toggle's active state from
    /// config if desired.
    ///
    /// Called by the app layer when the device list + system default resolution
    /// confirms no usable physical mic is available (or becomes usable again
    /// after a hot-plug or OS default flip). Per D-10.
    pub fn set_input_available(&self, available: bool) {
        self.enable_row.set_sensitive(available);
        if !available {
            // Force the switch off so AudioPipeline::start isn't re-entered.
            // The app layer is responsible for calling pipeline.stop() as
            // well to match this UI state (see Plan 03 handler).
            if self.enable_row.is_active() {
                self.enable_row.set_active(false);
            }
        }
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

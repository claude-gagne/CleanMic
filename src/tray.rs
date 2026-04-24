//! System tray icon.
//!
//! Provides a StatusNotifierItem (via the `ksni` crate) with quick actions:
//! enable/disable toggle, engine switch, mode switch, monitor toggle,
//! open main window, and quit.
//!
//! Note: GNOME does not natively show StatusNotifierItem icons without a
//! shell extension (e.g., AppIndicator/KStatusNotifierItem).
//!
//! The public types (`TrayCommand`, `TrayState`, `MenuItem`) are always
//! compiled so the rest of the crate can reference them without the `tray`
//! feature.  The actual `ksni::Tray` implementation lives behind
//! `#[cfg(feature = "tray")]`.

use crate::engine::{EngineType, ProcessingMode};
use gettextrs::gettext;

// ── Commands ──────────────────────────────────────────────────────────────────

/// Commands that the tray icon can send to the audio service or application.
#[derive(Debug, Clone, PartialEq)]
pub enum TrayCommand {
    /// Toggle the audio pipeline on or off.
    Toggle,
    /// Switch to the given engine.
    SetEngine(EngineType),
    /// Toggle the monitor (listen-to-processed-mic) output.
    ToggleMonitor,
    /// Bring the main window to the front (or show it if hidden).
    OpenWindow,
    /// Quit the application gracefully.
    Quit,
    /// User clicked "Check for updates" in the tray menu. Per D-02.
    CheckForUpdates,
    /// Open the GitHub Releases page in the default browser. Per 08.3 D-04.
    OpenReleasesPage,
}

// ── State ─────────────────────────────────────────────────────────────────────

/// Snapshot of the state reflected in the tray icon and its menu.
#[derive(Debug, Clone, PartialEq)]
pub struct TrayState {
    /// Whether the audio pipeline is currently active.
    pub active: bool,
    /// Currently selected engine.
    pub engine: EngineType,
    /// Currently selected processing mode.
    pub mode: ProcessingMode,
    /// Whether the monitor output is enabled.
    pub monitor_enabled: bool,
    /// Whether the Khip library is installed on this system.
    /// When `false` the "Advanced" engine entry is grayed out.
    pub khip_available: bool,
    /// Whether the audio thread is alive and processing.
    /// When `false` (dead thread), menu items that require audio are grayed out.
    pub audio_available: bool,
    /// If Some, a newer version is available; displayed as a persistent menu item. Per D-07.
    pub update_available: Option<String>,
}

impl Default for TrayState {
    fn default() -> Self {
        Self {
            active: true,
            engine: EngineType::DeepFilterNet,
            mode: ProcessingMode::Balanced,
            monitor_enabled: false,
            khip_available: false,
            audio_available: true,
            update_available: None,
        }
    }
}

impl TrayState {
    /// Create a new `TrayState` with explicit values.
    pub fn new(
        active: bool,
        engine: EngineType,
        mode: ProcessingMode,
        monitor_enabled: bool,
        khip_available: bool,
    ) -> Self {
        Self {
            active,
            engine,
            mode,
            monitor_enabled,
            khip_available,
            audio_available: true,
            update_available: None,
        }
    }

    /// Toggle the `active` flag and return `&mut self` for chaining.
    pub fn set_active(&mut self, active: bool) -> &mut Self {
        self.active = active;
        self
    }

    /// Update the engine and return `&mut self` for chaining.
    pub fn set_engine(&mut self, engine: EngineType) -> &mut Self {
        self.engine = engine;
        self
    }

    /// Update the processing mode and return `&mut self` for chaining.
    pub fn set_mode(&mut self, mode: ProcessingMode) -> &mut Self {
        self.mode = mode;
        self
    }

    /// Toggle the monitor flag and return `&mut self` for chaining.
    pub fn set_monitor_enabled(&mut self, enabled: bool) -> &mut Self {
        self.monitor_enabled = enabled;
        self
    }

    /// Update Khip availability and return `&mut self` for chaining.
    pub fn set_khip_available(&mut self, available: bool) -> &mut Self {
        self.khip_available = available;
        self
    }

    /// Update the available version indicator.
    pub fn set_update_available(&mut self, version: Option<String>) -> &mut Self {
        self.update_available = version;
        self
    }

    /// Return the icon name that should be used for the current state.
    ///
    /// The caller is responsible for resolving the name to an actual path.
    pub fn icon_name(&self) -> &'static str {
        if self.active {
            "cleanmic-active"
        } else {
            "cleanmic-disabled"
        }
    }
}

// ── Menu model ────────────────────────────────────────────────────────────────

/// A single entry in the tray context menu, suitable for building the real
/// menu in both the `ksni` backend and tests.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuItem {
    /// A checkable action item (toggle / radio).
    Check {
        label: String,
        checked: bool,
        enabled: bool,
        command: TrayCommand,
    },
    /// A plain action item (no check state).
    Action {
        label: String,
        enabled: bool,
        command: TrayCommand,
    },
    /// A visual separator between groups.
    Separator,
    /// A submenu with a label and child items.
    Submenu {
        label: String,
        children: Vec<MenuItem>,
    },
}

impl MenuItem {
    /// Convenience: create an enabled plain action item.
    pub fn action(label: impl Into<String>, command: TrayCommand) -> Self {
        Self::Action {
            label: label.into(),
            enabled: true,
            command,
        }
    }

    /// Convenience: create a check item.
    pub fn check(
        label: impl Into<String>,
        checked: bool,
        enabled: bool,
        command: TrayCommand,
    ) -> Self {
        Self::Check {
            label: label.into(),
            checked,
            enabled,
            command,
        }
    }
}

/// Build the context-menu model for the given `TrayState`.
///
/// This is pure data — no GTK or D-Bus types — so it can be called in tests
/// without the `tray` feature.
pub fn build_menu(state: &TrayState) -> Vec<MenuItem> {
    // ── Enable / Disable ──────────────────────────────────────────────────
    // Fixed label with checkmark reflecting current active state.
    // Checked = pipeline is running; clicking toggles it.
    // Grayed out when audio thread is dead.
    let toggle_label = if state.audio_available {
        gettext("CleanMic active")
    } else {
        gettext("CleanMic (unavailable)")
    };
    let toggle_item = MenuItem::check(
        toggle_label,
        state.active,
        state.audio_available,
        TrayCommand::Toggle,
    );

    // ── Engine submenu ────────────────────────────────────────────────────
    let engine_children = vec![
        MenuItem::check(
            "RNNoise",
            state.engine == EngineType::RNNoise,
            state.audio_available,
            TrayCommand::SetEngine(EngineType::RNNoise),
        ),
        MenuItem::check(
            "DeepFilterNet",
            state.engine == EngineType::DeepFilterNet,
            state.audio_available,
            TrayCommand::SetEngine(EngineType::DeepFilterNet),
        ),
        MenuItem::check(
            if state.khip_available {
                "Khip".to_owned()
            } else {
                gettext("Khip (not installed)")
            },
            state.engine == EngineType::Khip,
            state.khip_available && state.audio_available,
            TrayCommand::SetEngine(EngineType::Khip),
        ),
    ];
    let engine_submenu = MenuItem::Submenu {
        label: gettext("Engine"),
        children: engine_children,
    };

    // ── Monitor ───────────────────────────────────────────────────────────
    let monitor_item = MenuItem::check(
        gettext("Monitor"),
        state.monitor_enabled,
        state.audio_available,
        TrayCommand::ToggleMonitor,
    );

    // ── Window + Quit ─────────────────────────────────────────────────────
    let open_item = MenuItem::action(gettext("Open CleanMic"), TrayCommand::OpenWindow);
    let quit_item = MenuItem::action(gettext("Quit"), TrayCommand::Quit);

    let mut items = vec![toggle_item, engine_submenu, monitor_item, open_item];

    // Persistent update indicator — clicking opens Releases page (per 08.3 D-04).
    if let Some(ref version) = state.update_available {
        items.push(MenuItem::action(
            format!("Update available: {}", version),
            TrayCommand::OpenReleasesPage,
        ));
    }
    // Always show "Check for updates" item (per D-02).
    items.push(MenuItem::action(
        gettext("Check for updates"),
        TrayCommand::CheckForUpdates,
    ));

    items.push(MenuItem::Separator);
    items.push(quit_item);
    items
}

// ── ksni integration ──────────────────────────────────────────────────────────

#[cfg(feature = "tray")]
pub mod icon {
    //! `ksni::Tray` implementation for CleanMic.

    use super::{build_menu, TrayCommand, TrayState};
    use ksni::{self, MenuItem as KsniItem};
    use std::sync::{Arc, Mutex};

    /// Indicator dot color: green (#33d17a) for active state.
    const ACTIVE_COLOR: [u8; 4] = [0xFF, 0x33, 0xD1, 0x7A]; // ARGB
    /// Indicator dot color: red (#E01B24) for disabled state.
    const DISABLED_COLOR: [u8; 4] = [0xFF, 0xE0, 0x1B, 0x24]; // ARGB
    /// Panel foreground (white works on both light/dark panels at this size).
    const FG: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF]; // ARGB
    const TRANSPARENT: [u8; 4] = [0x00, 0x00, 0x00, 0x00];

    /// Size of the tray icon in pixels.
    const ICON_SIZE: i32 = 32;

    /// Build a 32x32 ARGB32 tray icon: microphone silhouette + colored indicator dot.
    fn make_tray_icon(indicator_color: [u8; 4]) -> ksni::Icon {
        let sz = ICON_SIZE as usize;
        let mut pixels = vec![TRANSPARENT; sz * sz];

        // Helper: set pixel if in bounds
        let mut set = |x: usize, y: usize, color: [u8; 4]| {
            if x < sz && y < sz {
                pixels[y * sz + x] = color;
            }
        };

        // Mic capsule body: ~14px wide, centered at x=16
        // Rounded top
        for x in 12..20 {
            set(x, 2, FG);
        }
        for x in 11..21 {
            set(x, 3, FG);
        }
        for x in 10..22 {
            set(x, 4, FG);
        }
        // Main body rows 5..15
        for y in 5..15 {
            for x in 9..23 {
                set(x, y, FG);
            }
        }
        // Rounded bottom
        for x in 10..22 {
            set(x, 15, FG);
        }
        for x in 11..21 {
            set(x, 16, FG);
        }
        for x in 12..20 {
            set(x, 17, FG);
        }

        // Stand arms curving from sides
        for y in 17..19 {
            set(7, y, FG);
            set(8, y, FG);
            set(23, y, FG);
            set(24, y, FG);
        }
        set(7, 19, FG);
        set(24, 19, FG);
        set(8, 20, FG);
        set(9, 20, FG);
        set(22, 20, FG);
        set(23, 20, FG);
        set(9, 21, FG);
        set(10, 21, FG);
        set(21, 21, FG);
        set(22, 21, FG);
        for x in 11..21 {
            set(x, 22, FG);
        }

        // Vertical post: columns 15..17, rows 23..26
        for y in 23..27 {
            for x in 14..18 {
                set(x, y, FG);
            }
        }

        // Base foot: columns 10..22, rows 27..28
        for x in 10..22 {
            set(x, 27, FG);
            set(x, 28, FG);
        }

        // Indicator dot: radius 4 circle at bottom-right (centered at 26,26)
        for dy in -4i32..=4 {
            for dx in -4i32..=4 {
                if dx * dx + dy * dy <= 16 {
                    set((26 + dx) as usize, (26 + dy) as usize, indicator_color);
                }
            }
        }

        // Convert to ARGB32 network byte order (big-endian: A, R, G, B)
        let data: Vec<u8> = pixels
            .iter()
            .flat_map(|&[a, r, g, b]| [a, r, g, b])
            .collect();

        ksni::Icon {
            width: ICON_SIZE,
            height: ICON_SIZE,
            data,
        }
    }

    /// Shared tray state guarded by a mutex so the ksni background thread and
    /// the main thread can both update it.
    pub type SharedTrayState = Arc<Mutex<TrayState>>;

    /// The ksni tray object.
    pub struct CleanMicTray {
        pub state: SharedTrayState,
        pub sender: std::sync::mpsc::Sender<TrayCommand>,
    }

    impl ksni::Tray for CleanMicTray {
        fn id(&self) -> String {
            "com.cleanmic.CleanMic".into()
        }

        fn title(&self) -> String {
            "CleanMic".into()
        }

        fn icon_name(&self) -> String {
            // Return empty string to force the panel to use icon_pixmap.
            // Icon theme lookup fails inside AppImages because the desktop
            // environment cannot see the AppImage's internal filesystem.
            String::new()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.active {
                vec![make_tray_icon(ACTIVE_COLOR)]
            } else {
                vec![make_tray_icon(DISABLED_COLOR)]
            }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            // Left-click: open main window.
            if self.sender.send(TrayCommand::OpenWindow).is_err() {
                log::warn!("tray command channel closed - OpenWindow dropped");
            }
        }

        fn menu(&self) -> Vec<KsniItem<Self>> {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            convert_menu(&build_menu(&state), &self.sender)
        }
    }

    /// Recursively convert our `MenuItem` tree into `ksni::MenuItem` items.
    fn convert_menu(
        items: &[super::MenuItem],
        sender: &std::sync::mpsc::Sender<TrayCommand>,
    ) -> Vec<KsniItem<CleanMicTray>> {
        items
            .iter()
            .map(|item| match item {
                super::MenuItem::Separator => KsniItem::Separator,
                super::MenuItem::Action {
                    label,
                    enabled,
                    command,
                } => {
                    let command = command.clone();
                    let sender = sender.clone();
                    KsniItem::Standard(ksni::menu::StandardItem {
                        label: label.clone(),
                        enabled: *enabled,
                        activate: Box::new(move |_tray: &mut CleanMicTray| {
                            if sender.send(command.clone()).is_err() {
                                log::warn!("tray command channel closed - menu action dropped");
                            }
                        }),
                        ..Default::default()
                    })
                }
                super::MenuItem::Check {
                    label,
                    checked,
                    enabled,
                    command,
                } => {
                    let command = command.clone();
                    let sender = sender.clone();
                    KsniItem::Checkmark(ksni::menu::CheckmarkItem {
                        label: label.clone(),
                        enabled: *enabled,
                        checked: *checked,
                        activate: Box::new(move |_tray: &mut CleanMicTray| {
                            if sender.send(command.clone()).is_err() {
                                log::warn!("tray command channel closed - menu action dropped");
                            }
                        }),
                        ..Default::default()
                    })
                }
                super::MenuItem::Submenu { label, children } => {
                    KsniItem::SubMenu(ksni::menu::SubMenu {
                        label: label.clone(),
                        submenu: convert_menu(children, sender),
                        ..Default::default()
                    })
                }
            })
            .collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineType, ProcessingMode};

    // ── TrayState ─────────────────────────────────────────────────────────────

    #[test]
    fn tray_state_default_values() {
        let state = TrayState::default();
        assert!(state.active);
        assert_eq!(state.engine, EngineType::DeepFilterNet);
        assert_eq!(state.mode, ProcessingMode::Balanced);
        assert!(!state.monitor_enabled);
        assert!(!state.khip_available);
    }

    #[test]
    fn tray_state_new_explicit() {
        let state = TrayState::new(
            false,
            EngineType::RNNoise,
            ProcessingMode::LowCpu,
            true,
            true,
        );
        assert!(!state.active);
        assert_eq!(state.engine, EngineType::RNNoise);
        assert_eq!(state.mode, ProcessingMode::LowCpu);
        assert!(state.monitor_enabled);
        assert!(state.khip_available);
    }

    #[test]
    fn tray_state_set_active() {
        let mut state = TrayState::default();
        state.set_active(false);
        assert!(!state.active);
        state.set_active(true);
        assert!(state.active);
    }

    #[test]
    fn tray_state_set_engine() {
        let mut state = TrayState::default();
        state.set_engine(EngineType::RNNoise);
        assert_eq!(state.engine, EngineType::RNNoise);
        state.set_engine(EngineType::Khip);
        assert_eq!(state.engine, EngineType::Khip);
    }

    #[test]
    fn tray_state_set_mode() {
        let mut state = TrayState::default();
        state.set_mode(ProcessingMode::MaxQuality);
        assert_eq!(state.mode, ProcessingMode::MaxQuality);
        state.set_mode(ProcessingMode::LowCpu);
        assert_eq!(state.mode, ProcessingMode::LowCpu);
    }

    #[test]
    fn tray_state_set_monitor_enabled() {
        let mut state = TrayState::default();
        assert!(!state.monitor_enabled);
        state.set_monitor_enabled(true);
        assert!(state.monitor_enabled);
    }

    #[test]
    fn tray_state_set_khip_available() {
        let mut state = TrayState::default();
        assert!(!state.khip_available);
        state.set_khip_available(true);
        assert!(state.khip_available);
    }

    #[test]
    fn tray_state_icon_name_reflects_active() {
        let mut state = TrayState::default();
        state.set_active(true);
        assert_eq!(state.icon_name(), "cleanmic-active");
        state.set_active(false);
        assert_eq!(state.icon_name(), "cleanmic-disabled");
    }

    #[test]
    fn tray_state_update_reflects_changes() {
        let mut state = TrayState::default();
        // Chain several updates and check all are reflected.
        state
            .set_active(false)
            .set_engine(EngineType::RNNoise)
            .set_mode(ProcessingMode::MaxQuality)
            .set_monitor_enabled(true)
            .set_khip_available(true);

        assert!(!state.active);
        assert_eq!(state.engine, EngineType::RNNoise);
        assert_eq!(state.mode, ProcessingMode::MaxQuality);
        assert!(state.monitor_enabled);
        assert!(state.khip_available);
    }

    // ── TrayCommand ───────────────────────────────────────────────────────────

    #[test]
    fn tray_command_variants_constructible() {
        let cmds = vec![
            TrayCommand::Toggle,
            TrayCommand::SetEngine(EngineType::RNNoise),
            TrayCommand::SetEngine(EngineType::DeepFilterNet),
            TrayCommand::SetEngine(EngineType::Khip),
            TrayCommand::ToggleMonitor,
            TrayCommand::OpenWindow,
            TrayCommand::Quit,
            TrayCommand::OpenReleasesPage,
        ];
        // Just verify they can be constructed and compared.
        assert_eq!(cmds[0], TrayCommand::Toggle);
        assert_eq!(cmds[4], TrayCommand::ToggleMonitor);
        assert_eq!(cmds[5], TrayCommand::OpenWindow);
        assert_eq!(cmds[6], TrayCommand::Quit);
    }

    #[test]
    fn tray_command_open_releases_page_constructible() {
        let cmd = TrayCommand::OpenReleasesPage;
        assert_eq!(cmd, TrayCommand::OpenReleasesPage);
        assert_ne!(cmd, TrayCommand::CheckForUpdates);
    }

    // ── Menu model ────────────────────────────────────────────────────────────

    #[test]
    fn menu_has_expected_top_level_items() {
        let state = TrayState::default();
        let menu = build_menu(&state);

        // Expected order: toggle, engine submenu, monitor, open, check-for-updates,
        // separator, quit.
        assert_eq!(menu.len(), 7);

        // Toggle item is a Check.
        assert!(matches!(menu[0], MenuItem::Check { .. }));

        // Engine submenu.
        assert!(matches!(&menu[1], MenuItem::Submenu { label, .. } if label == "Engine"));

        // Monitor item.
        assert!(matches!(&menu[2], MenuItem::Check { .. }));

        // Open window.
        assert!(matches!(
            &menu[3],
            MenuItem::Action {
                command: TrayCommand::OpenWindow,
                ..
            }
        ));

        // Check for updates (always present).
        assert!(matches!(
            &menu[4],
            MenuItem::Action {
                command: TrayCommand::CheckForUpdates,
                ..
            }
        ));

        // Separator.
        assert!(matches!(menu[5], MenuItem::Separator));

        // Quit.
        assert!(matches!(
            &menu[6],
            MenuItem::Action {
                command: TrayCommand::Quit,
                ..
            }
        ));
    }

    #[test]
    fn menu_engine_submenu_has_three_items() {
        let state = TrayState::default();
        let menu = build_menu(&state);

        if let MenuItem::Submenu { children, .. } = &menu[1] {
            assert_eq!(children.len(), 3, "engine submenu should have 3 entries");
        } else {
            panic!("expected engine submenu at index 1");
        }
    }

    #[test]
    fn menu_engine_checkmarks_follow_state() {
        let mut state = TrayState::default();
        state.set_engine(EngineType::RNNoise);
        let menu = build_menu(&state);

        if let MenuItem::Submenu { children, .. } = &menu[1] {
            // RNNoise entry (index 0) should be checked.
            assert!(
                matches!(&children[0], MenuItem::Check { checked: true, .. }),
                "RNNoise should be checked"
            );
            // DeepFilterNet (index 1) should not be checked.
            assert!(
                matches!(&children[1], MenuItem::Check { checked: false, .. }),
                "DeepFilterNet should not be checked"
            );
        } else {
            panic!("expected engine submenu");
        }
    }

    #[test]
    fn menu_khip_grayed_out_when_unavailable() {
        let mut state = TrayState::default();
        state.set_khip_available(false);
        let menu = build_menu(&state);

        if let MenuItem::Submenu { children, .. } = &menu[1] {
            // Khip is the third child.
            assert!(
                matches!(&children[2], MenuItem::Check { enabled: false, .. }),
                "Khip should be disabled when unavailable"
            );
        } else {
            panic!("expected engine submenu");
        }
    }

    #[test]
    fn menu_khip_enabled_when_available() {
        let mut state = TrayState::default();
        state.set_khip_available(true);
        let menu = build_menu(&state);

        if let MenuItem::Submenu { children, .. } = &menu[1] {
            assert!(
                matches!(&children[2], MenuItem::Check { enabled: true, .. }),
                "Khip should be enabled when available"
            );
        } else {
            panic!("expected engine submenu");
        }
    }

    #[test]
    fn menu_toggle_label_reflects_active_state() {
        let mut state = TrayState::default();
        state.set_active(true);
        let active_menu = build_menu(&state);
        state.set_active(false);
        let disabled_menu = build_menu(&state);

        if let MenuItem::Check { label, checked, .. } = &active_menu[0] {
            assert!(
                label.contains("active"),
                "toggle label should contain 'active'"
            );
            assert!(checked, "active state toggle should be checked");
        } else {
            panic!("expected check item at index 0");
        }

        if let MenuItem::Check { label, checked, .. } = &disabled_menu[0] {
            assert!(
                label.contains("active"),
                "toggle label should contain 'active'"
            );
            assert!(!checked, "inactive state toggle should not be checked");
        } else {
            panic!("expected check item at index 0");
        }
    }

    #[test]
    fn menu_monitor_toggle_reflects_state() {
        let mut state = TrayState::default();
        state.set_monitor_enabled(true);
        let menu = build_menu(&state);

        assert!(
            matches!(
                &menu[2],
                MenuItem::Check {
                    checked: true,
                    command: TrayCommand::ToggleMonitor,
                    ..
                }
            ),
            "monitor item should be checked when enabled"
        );
    }

    #[test]
    fn tray_command_set_engine_carries_type() {
        let cmd = TrayCommand::SetEngine(EngineType::Khip);
        assert_eq!(cmd, TrayCommand::SetEngine(EngineType::Khip));
        assert_ne!(cmd, TrayCommand::SetEngine(EngineType::RNNoise));
    }

    #[test]
    fn menu_update_indicator_shown_when_update_available() {
        let mut state = TrayState::default();
        state.set_update_available(Some("v1.2.0".to_owned()));
        let menu = build_menu(&state);
        // update-indicator + check-for-updates = 2 extra items → total 8.
        assert_eq!(menu.len(), 8);
        // Verify update indicator label contains "v1.2.0" and binds to OpenReleasesPage
        // (per 08.3 D-04 — clicking the indicator opens the Releases page directly,
        // not re-runs the update check).
        let has_indicator = menu.iter().any(|item| {
            matches!(item, MenuItem::Action { label, command: TrayCommand::OpenReleasesPage, .. }
                if label.contains("v1.2.0"))
        });
        assert!(
            has_indicator,
            "expected update indicator with version v1.2.0"
        );
    }

    #[test]
    fn menu_update_indicator_binds_to_open_releases_page() {
        let mut state = TrayState::default();
        state.set_update_available(Some("v1.2.3".to_owned()));
        let menu = build_menu(&state);
        let has_open_releases = menu.iter().any(|item| {
            matches!(item, MenuItem::Action { command: TrayCommand::OpenReleasesPage, label, .. }
                if label.contains("v1.2.3"))
        });
        assert!(
            has_open_releases,
            "update indicator should bind to OpenReleasesPage, not CheckForUpdates"
        );
    }

    #[test]
    fn menu_no_update_indicator_when_none() {
        let state = TrayState::default();
        let menu = build_menu(&state);
        // None of the action items should contain "Update available".
        let has_update = menu.iter().any(|item| {
            matches!(item, MenuItem::Action { label, .. } if label.contains("Update available"))
        });
        assert!(
            !has_update,
            "no update indicator expected when update_available is None"
        );
    }

    #[test]
    fn tray_command_check_for_updates_constructible() {
        let cmd = TrayCommand::CheckForUpdates;
        assert_eq!(cmd, TrayCommand::CheckForUpdates);
    }
}

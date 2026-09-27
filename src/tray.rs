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

use std::collections::BTreeMap;

use crate::engine::{AvailabilityReason, EngineAvailability, EngineType, ProcessingMode};
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
    /// Toggle automatic mic volume (speech-gated input boost for too-quiet
    /// mics — see [`crate::dsp::AutoGain`]). Quick task 260923-x24.
    ToggleAutoGain,
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

/// Default per-engine availability seed for [`TrayState::default`] — RNNoise
/// and DeepFilterNet are treated as normally compiled-in/available (matching
/// this crate's historical assumption before per-engine availability
/// tracking existed); Khip and both DPDFNet variants require an external
/// runtime probe, so they default to unavailable until real construction
/// (`src/app.rs`) overwrites this with [`crate::engine::all_engine_availability`].
fn default_tray_availability() -> BTreeMap<EngineType, EngineAvailability> {
    use AvailabilityReason::{Available, RuntimeMissing};
    EngineType::all()
        .map(|engine| {
            let available = matches!(engine, EngineType::RNNoise | EngineType::DeepFilterNet);
            (
                engine,
                EngineAvailability {
                    available,
                    reason: if available { Available } else { RuntimeMissing },
                },
            )
        })
        .collect()
}

/// Snapshot of the state reflected in the tray icon and its menu.
#[derive(Debug, Clone, PartialEq)]
pub struct TrayState {
    /// Whether the audio pipeline is currently active.
    pub active: bool,
    /// Currently selected engine — the truthful ACTIVE engine (T-15.1-07),
    /// never a merely-requested one that failed to initialize (D-11).
    pub engine: EngineType,
    /// Currently selected processing mode.
    pub mode: ProcessingMode,
    /// Whether the monitor output is enabled.
    pub monitor_enabled: bool,
    /// Whether automatic mic volume (speech-gated input boost for too-quiet
    /// mics) is enabled. Mirrors the Paramètres toggle (quick task
    /// 260923-x24). `TrayState::new`'s signature is kept at 5 args for
    /// compatibility; callers set this field via [`set_auto_gain_enabled`](Self::set_auto_gain_enabled).
    pub auto_gain_enabled: bool,
    /// Independent per-engine availability/reason map (D-02/D-08),
    /// superseding the previous Khip-only `khip_available: bool` special
    /// case. Populated from [`crate::engine::all_engine_availability`].
    /// Each unavailable entry keeps its submenu row visible but disabled
    /// (T-15.1-10) — never hidden, never silently interchangeable with
    /// another engine's result (one variant's unavailability never implies
    /// anything about another's, D-02).
    pub availability: BTreeMap<EngineType, EngineAvailability>,
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
            auto_gain_enabled: true,
            availability: default_tray_availability(),
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
        availability: BTreeMap<EngineType, EngineAvailability>,
    ) -> Self {
        Self {
            active,
            engine,
            mode,
            monitor_enabled,
            auto_gain_enabled: true,
            availability,
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

    /// Toggle the automatic-mic-volume flag and return `&mut self` for
    /// chaining.
    pub fn set_auto_gain_enabled(&mut self, enabled: bool) -> &mut Self {
        self.auto_gain_enabled = enabled;
        self
    }

    /// Replace the per-engine availability map wholesale and return `&mut
    /// self` for chaining. Generalizes the previous Khip-only
    /// `set_khip_available` (D-02/D-08) — callers rebuild the map from
    /// [`crate::engine::all_engine_availability`] rather than flipping a
    /// single boolean.
    pub fn set_availability(
        &mut self,
        availability: BTreeMap<EngineType, EngineAvailability>,
    ) -> &mut Self {
        self.availability = availability;
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

/// Resolve `engine`'s truthful availability from `state.availability`,
/// falling back to "available" when the map has no entry for it (mirrors
/// `src/ui/window.rs`'s `resolve_availability` — kept as an independent copy
/// here since `window` is gated behind the `gui` feature and this module
/// must stay buildable/testable without it).
fn resolve_tray_availability(state: &TrayState, engine: EngineType) -> EngineAvailability {
    state
        .availability
        .get(&engine)
        .copied()
        .unwrap_or(EngineAvailability {
            available: true,
            reason: AvailabilityReason::Available,
        })
}

/// Compute the tray submenu label for `engine`, given its truthful
/// availability (D-02/D-08). When available, returns the untranslated
/// proper-noun product name (`EngineType::short_name`). When unavailable,
/// the entry stays visible with a translated unavailable label — never
/// hidden. Mirrors `src/ui/window.rs`'s `engine_row_text` wording so the two
/// surfaces read consistently.
fn engine_menu_label(engine: EngineType, availability: EngineAvailability) -> String {
    if availability.available {
        return engine.short_name().to_owned();
    }
    if engine == EngineType::Khip {
        // Phase 15.4 Plan 01 Task 2: reworded from "Khip (not installed)" —
        // that title tripped the AppImageHub catalog's OCR hard-phrase list
        // (research Pitfall 2); same meaning, matches src/ui/window.rs.
        return gettext("Khip (not detected)");
    }
    format!("{} {}", engine.short_name(), gettext("(unavailable)"))
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
    // Fixed D-07 order (RNNoise, DeepFilterNet, DPDFNet-2, DPDFNet-8, Khip),
    // built from EngineType::ALL so the tray can never drift out of sync
    // with the window selector's row order. Each engine's checked/enabled
    // state is independently derived from its own availability entry
    // (D-02) — one variant's unavailability never disables another's row.
    let engine_children: Vec<MenuItem> = EngineType::all()
        .map(|engine| {
            let availability = resolve_tray_availability(state, engine);
            MenuItem::check(
                engine_menu_label(engine, availability),
                state.engine == engine,
                availability.available && state.audio_available,
                TrayCommand::SetEngine(engine),
            )
        })
        .collect();
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

    // ── Automatic mic volume ──────────────────────────────────────────────
    // Reuses Task 2's msgid — the tray already mirrors the Paramètres
    // Monitor toggle, so auto-gain is mirrored too (quick task 260923-x24).
    let auto_gain_item = MenuItem::check(
        gettext("Automatic mic volume"),
        state.auto_gain_enabled,
        state.audio_available,
        TrayCommand::ToggleAutoGain,
    );

    // ── Window + Quit ─────────────────────────────────────────────────────
    let open_item = MenuItem::action(gettext("Open CleanMic"), TrayCommand::OpenWindow);
    let quit_item = MenuItem::action(gettext("Quit"), TrayCommand::Quit);

    let mut items = vec![
        toggle_item,
        engine_submenu,
        monitor_item,
        auto_gain_item,
        open_item,
    ];

    // Persistent update indicator — clicking opens Releases page (per 08.3 D-04).
    if let Some(ref version) = state.update_available {
        items.push(MenuItem::action(
            format!("{}: {}", gettext("Update available"), version),
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

    use super::{TrayCommand, TrayState, build_menu};
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

    /// Build an availability map with every engine available, except the
    /// ones explicitly listed as unavailable — convenience for tests that
    /// only care about one or two engines' state.
    fn availability_with_unavailable(
        unavailable: &[EngineType],
    ) -> BTreeMap<EngineType, EngineAvailability> {
        EngineType::all()
            .map(|engine| {
                let available = !unavailable.contains(&engine);
                (
                    engine,
                    EngineAvailability {
                        available,
                        reason: if available {
                            AvailabilityReason::Available
                        } else {
                            AvailabilityReason::RuntimeMissing
                        },
                    },
                )
            })
            .collect()
    }

    fn all_available() -> BTreeMap<EngineType, EngineAvailability> {
        availability_with_unavailable(&[])
    }

    // ── TrayState ─────────────────────────────────────────────────────────────

    #[test]
    fn tray_state_default_values() {
        let state = TrayState::default();
        assert!(state.active);
        assert_eq!(state.engine, EngineType::DeepFilterNet);
        assert_eq!(state.mode, ProcessingMode::Balanced);
        assert!(!state.monitor_enabled);
        assert_eq!(state.availability.len(), 5);
        assert!(!state.availability[&EngineType::Khip].available);
        assert!(state.availability[&EngineType::RNNoise].available);
        assert!(state.availability[&EngineType::DeepFilterNet].available);
    }

    #[test]
    fn tray_state_new_explicit() {
        let state = TrayState::new(
            false,
            EngineType::RNNoise,
            ProcessingMode::LowCpu,
            true,
            all_available(),
        );
        assert!(!state.active);
        assert_eq!(state.engine, EngineType::RNNoise);
        assert_eq!(state.mode, ProcessingMode::LowCpu);
        assert!(state.monitor_enabled);
        assert!(state.availability[&EngineType::Khip].available);
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
    fn tray_state_set_availability() {
        let mut state = TrayState::default();
        assert!(!state.availability[&EngineType::Khip].available);
        state.set_availability(all_available());
        assert!(state.availability[&EngineType::Khip].available);
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
            .set_availability(all_available());

        assert!(!state.active);
        assert_eq!(state.engine, EngineType::RNNoise);
        assert_eq!(state.mode, ProcessingMode::MaxQuality);
        assert!(state.monitor_enabled);
        assert!(state.availability[&EngineType::Khip].available);
    }

    // ── TrayCommand ───────────────────────────────────────────────────────────

    #[test]
    fn tray_command_variants_constructible() {
        let cmds = [
            TrayCommand::Toggle,
            TrayCommand::SetEngine(EngineType::RNNoise),
            TrayCommand::SetEngine(EngineType::DeepFilterNet),
            TrayCommand::SetEngine(EngineType::Dpdfnet2),
            TrayCommand::SetEngine(EngineType::Dpdfnet8),
            TrayCommand::SetEngine(EngineType::Khip),
            TrayCommand::ToggleMonitor,
            TrayCommand::ToggleAutoGain,
            TrayCommand::OpenWindow,
            TrayCommand::Quit,
            TrayCommand::OpenReleasesPage,
        ];
        // Just verify they can be constructed and compared.
        assert_eq!(cmds[0], TrayCommand::Toggle);
        assert_eq!(cmds[6], TrayCommand::ToggleMonitor);
        assert_eq!(cmds[7], TrayCommand::ToggleAutoGain);
        assert_eq!(cmds[8], TrayCommand::OpenWindow);
        assert_eq!(cmds[9], TrayCommand::Quit);
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

        // Expected order: toggle, engine submenu, monitor, auto-gain, open,
        // check-for-updates, separator, quit.
        assert_eq!(menu.len(), 8);

        // Toggle item is a Check.
        assert!(matches!(menu[0], MenuItem::Check { .. }));

        // Engine submenu.
        assert!(matches!(&menu[1], MenuItem::Submenu { label, .. } if label == "Engine"));

        // Monitor item.
        assert!(matches!(&menu[2], MenuItem::Check { .. }));

        // Automatic mic volume item.
        assert!(matches!(
            &menu[3],
            MenuItem::Check {
                command: TrayCommand::ToggleAutoGain,
                ..
            }
        ));

        // Open window.
        assert!(matches!(
            &menu[4],
            MenuItem::Action {
                command: TrayCommand::OpenWindow,
                ..
            }
        ));

        // Check for updates (always present).
        assert!(matches!(
            &menu[5],
            MenuItem::Action {
                command: TrayCommand::CheckForUpdates,
                ..
            }
        ));

        // Separator.
        assert!(matches!(menu[6], MenuItem::Separator));

        // Quit.
        assert!(matches!(
            &menu[7],
            MenuItem::Action {
                command: TrayCommand::Quit,
                ..
            }
        ));
    }

    #[test]
    fn menu_engine_submenu_has_five_items_in_d07_order() {
        let mut state = TrayState::default();
        state.set_availability(all_available());
        let menu = build_menu(&state);

        if let MenuItem::Submenu { children, .. } = &menu[1] {
            assert_eq!(children.len(), 5, "engine submenu should have 5 entries");
            let commands: Vec<&TrayCommand> = children
                .iter()
                .map(|c| match c {
                    MenuItem::Check { command, .. } => command,
                    other => panic!("expected Check item, got {other:?}"),
                })
                .collect();
            assert_eq!(
                commands,
                vec![
                    &TrayCommand::SetEngine(EngineType::RNNoise),
                    &TrayCommand::SetEngine(EngineType::DeepFilterNet),
                    &TrayCommand::SetEngine(EngineType::Dpdfnet2),
                    &TrayCommand::SetEngine(EngineType::Dpdfnet8),
                    &TrayCommand::SetEngine(EngineType::Khip),
                ],
                "engine submenu must mirror the window selector's fixed D-07 order"
            );
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
    fn menu_checkmark_reflects_actual_active_engine_not_requested() {
        // T-15.1-09: the checkmark must follow `state.engine` (the
        // truthful ACTIVE engine) — this test picks an engine that would
        // be a plausible "requested" value to make sure nothing in
        // build_menu re-derives the checkmark from anything else.
        let mut state = TrayState::default();
        state.set_engine(EngineType::DeepFilterNet); // fallback landed here
        let menu = build_menu(&state);

        if let MenuItem::Submenu { children, .. } = &menu[1] {
            assert!(
                matches!(&children[1], MenuItem::Check { checked: true, .. }),
                "DeepFilterNet (the active engine) should be checked" // i18n-ignore
            );
            assert!(
                matches!(&children[2], MenuItem::Check { checked: false, .. }),
                "DPDFNet-2 (the merely-requested, failed engine) must not be checked" // i18n-ignore
            );
        } else {
            panic!("expected engine submenu");
        }
    }

    #[test]
    fn menu_khip_grayed_out_when_unavailable() {
        let mut state = TrayState::default();
        state.set_availability(availability_with_unavailable(&[EngineType::Khip]));
        let menu = build_menu(&state);

        if let MenuItem::Submenu { children, .. } = &menu[1] {
            // Khip is the fifth (last) child per D-07 order.
            assert!(
                matches!(&children[4], MenuItem::Check { enabled: false, .. }),
                "Khip should be disabled when unavailable"
            );
        } else {
            panic!("expected engine submenu");
        }
    }

    #[test]
    fn menu_khip_enabled_when_available() {
        let mut state = TrayState::default();
        state.set_availability(all_available());
        let menu = build_menu(&state);

        if let MenuItem::Submenu { children, .. } = &menu[1] {
            assert!(
                matches!(&children[4], MenuItem::Check { enabled: true, .. }),
                "Khip should be enabled when available"
            );
        } else {
            panic!("expected engine submenu");
        }
    }

    #[test]
    fn menu_dpdfnet_variants_are_independently_disabled_per_d02() {
        let mut state = TrayState::default();
        // Only DPDFNet-2 unavailable; DPDFNet-8 stays available — proves
        // one variant's disablement never leaks into the other's row.
        state.set_availability(availability_with_unavailable(&[EngineType::Dpdfnet2]));
        let menu = build_menu(&state);

        if let MenuItem::Submenu { children, .. } = &menu[1] {
            assert!(
                matches!(&children[2], MenuItem::Check { enabled: false, .. }),
                "DPDFNet-2 should be disabled"
            );
            assert!(
                matches!(&children[3], MenuItem::Check { enabled: true, .. }),
                "DPDFNet-8 must remain enabled independently of DPDFNet-2 (D-02)"
            );
        } else {
            panic!("expected engine submenu");
        }
    }

    #[test]
    fn menu_dpdfnet_unavailable_label_is_translated_and_visible() {
        let mut state = TrayState::default();
        state.set_availability(availability_with_unavailable(&[EngineType::Dpdfnet8]));
        let menu = build_menu(&state);

        if let MenuItem::Submenu { children, .. } = &menu[1] {
            if let MenuItem::Check { label, enabled, .. } = &children[3] {
                assert!(!enabled);
                assert!(
                    label.contains("DPDFNet-8"),
                    "unavailable entry must keep the product name, never hide the row: {label}"
                );
                assert_ne!(label, "DPDFNet-8", "must carry an unavailable marker");
            } else {
                panic!("expected Check item for DPDFNet-8");
            }
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
    fn menu_auto_gain_toggle_reflects_state() {
        let mut state = TrayState::default();
        state.set_auto_gain_enabled(false);
        let menu = build_menu(&state);

        assert!(
            matches!(
                &menu[3],
                MenuItem::Check {
                    checked: false,
                    command: TrayCommand::ToggleAutoGain,
                    label,
                    ..
                } if label == "Automatic mic volume" // i18n-ignore
            ),
            "auto-gain item should be unchecked when disabled"
        );

        state.set_auto_gain_enabled(true);
        let menu = build_menu(&state);
        assert!(
            matches!(
                &menu[3],
                MenuItem::Check {
                    checked: true,
                    command: TrayCommand::ToggleAutoGain,
                    ..
                }
            ),
            "auto-gain item should be checked when enabled"
        );
    }

    #[test]
    fn tray_state_set_auto_gain_enabled() {
        let mut state = TrayState::default();
        assert!(state.auto_gain_enabled, "TrayState::default starts ON");
        state.set_auto_gain_enabled(false);
        assert!(!state.auto_gain_enabled);
        state.set_auto_gain_enabled(true);
        assert!(state.auto_gain_enabled);
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
        // Base menu (8 items, including auto-gain) + the update indicator = 9.
        assert_eq!(menu.len(), 9);
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

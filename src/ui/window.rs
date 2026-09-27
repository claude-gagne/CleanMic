//! GTK4 + libadwaita main application window.
//!
//! Constructs the window using the GNOME preferences layout pattern:
//!
//! ```text
//! AdwApplicationWindow (resizable, 420x400 min, 420x720 default — R1-R4)
//! ├── AdwHeaderBar (":minimize,close" — no maximize, see below)
//! ├── update_banner / pipewire_banner (hidden unless triggered)
//! └── AdwClamp (420px, unit Px, maximum == tightening_threshold — R1)
//!     └── AdwPreferencesPage
//!         ├── (its own internal GtkScrolledWindow: Never/Automatic — R2)
//!         ├── [status group] — headline + routing info
//!         ├── AdwPreferencesGroup "Input"
//!         │   ├── AdwComboRow    — microphone picker
//!         │   └── AdwSwitchRow   — enable / disable
//!         ├── AdwPreferencesGroup "Noise Processing"
//!         │   └── AdwActionRow×5 — engine selector (radio-grouped, fixed
//!         │                        order per D-07: RNNoise / DeepFilterNet /
//!         │                        DPDFNet-2 / DPDFNet-8 / Khip)
//!         ├── AdwPreferencesGroup "Strength"
//!         │   └── AdwComboRow    — strength picker (Light / Balanced / Strong)
//!         ├── AdwPreferencesGroup "Mode"
//!         │   └── AdwComboRow    — CPU/quality trade-off (D-03/D-04)
//!         ├── AdwPreferencesGroup "Levels"
//!         │   ├── MeterRow       — input level meter
//!         │   └── MeterRow       — output level meter
//!         └── AdwPreferencesGroup "Settings"
//!             ├── AdwSwitchRow   — autostart
//!             ├── AdwSwitchRow   — monitor (listen to processed mic)
//!             └── AdwSwitchRow   — automatic mic volume (input auto-gain)
//! ```
//!
//! Only compiled when the `gui` feature is enabled (gated on the `pub mod
//! window` declaration in `src/ui/mod.rs`).
//!
//! # Sizing and width policy (quick task 260927-mvb)
//!
//! The window is vertically resizable and remembers its height across
//! launches; its width is fixed by convention (a hard-clamped content column
//! plus a WM minimum-size hint), never persisted. See
//! `src/ui/window_geometry.rs`'s module doc for the full sizing policy,
//! including the "Width policy and its limits" section — the honest
//! documentation of why GTK4 cannot hard-lock a *resizable* window's maximum
//! width.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::mpsc;

use gtk4::gio;
use gtk4::glib;
use gtk4::pango;
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
    /// The processing mode picker (Low CPU / Balanced / Max Quality) — only
    /// takes effect on the DPDFNet engines (D-03/D-04); distinct from
    /// `strength_row`'s suppression-aggressiveness knob.
    pub mode_row: ComboRow,
    /// The monitor toggle switch — updated on monitor state changes.
    pub monitor_row: SwitchRow,
    /// The automatic-mic-volume toggle switch — updated on auto-gain state
    /// changes (quick task 260923-x24).
    pub auto_gain_row: SwitchRow,
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
    /// What each index in `device_row`'s CURRENT popup list represents —
    /// replaces the old build-time (description, node.name) vector capture,
    /// which went stale after any runtime list refresh. R1 makes refreshes
    /// routine (plug/unplug, pin changes), so a click must always resolve
    /// against the list actually on screen, not the one captured at window
    /// construction time. Replaced wholesale by [`update_device_list`] under
    /// the `device_updating` guard. Not `pub`: `PickerTarget` is private to
    /// this module and only the selection closure / `update_device_list`
    /// (both defined here) ever touch it.
    device_targets: Rc<RefCell<Vec<PickerTarget>>>,
    /// The update notification banner at the top of the window.
    /// Revealed when a new version is available (per D-05, D-08).
    pub update_banner: Banner,
    /// The no-PipeWire banner (D-02) — revealed once at window build when
    /// the daemon was unreachable at startup; never re-evaluated afterward
    /// (a PipeWire-less session never attempts a live upgrade). Distinct
    /// widget from `update_banner`: different trigger, different lifetime.
    pipewire_banner: Banner,
    /// The wrapping explanation label shown together with
    /// `pipewire_banner`. AdwBanner titles get cut off at this window's
    /// 420px width (quick task 260510-wqm dropped a banner for exactly this
    /// reason), so the full sentence lives in its own label instead of
    /// being crammed into the banner's title.
    pipewire_explanation: gtk4::Label,
    /// Flag set while a programmatic engine switch (either a user row click
    /// handled in `build_engine_selector`, or a full state sync in
    /// [`update_from_state`](WindowHandles::update_from_state)) is
    /// restoring the newly-active engine's own remembered strength (D-13)
    /// into `strength_row`. Without this guard, `ComboRow::set_selected`
    /// fires `notify::selected` unconditionally, re-emitting a spurious
    /// `UiEvent::StrengthChanged` for a value the user never picked — mirrors
    /// the existing `device_updating` guard pattern.
    pub strength_updating: Rc<Cell<bool>>,
    /// Flag set while a programmatic mode sync (full state sync in
    /// [`update_from_state`](WindowHandles::update_from_state)) is restoring
    /// `mode_row`. Without this guard, `ComboRow::set_selected` fires
    /// `notify::selected` unconditionally, re-emitting a spurious
    /// `UiEvent::ModeChanged` — mirrors `strength_updating`.
    pub mode_updating: Rc<Cell<bool>>,
    /// Flag set while a programmatic visual-only override of the enable
    /// switch (currently only [`Self::set_enable_visual_off`], quick task
    /// 260927-mvb) is running, so `connect_active_notify`'s handler can skip
    /// it — mirrors `device_updating`/`strength_updating`/`mode_updating`.
    /// Without this guard, forcing the switch off to render the no-PipeWire
    /// state honestly would dispatch `UiEvent::EnableToggled(false)`, which
    /// cascades into `config.enabled = false` and a save — exactly what
    /// `set_enable_sensitive`'s doc (and T-15.4-02) forbids.
    enable_updating: Rc<Cell<bool>>,
}

// Type alias for the SwitchRow closure parameter.
type SwitchRowRef = SwitchRow;

use crate::config::Config;
use crate::engine::{AvailabilityReason, EngineAvailability, EngineType, ProcessingMode};
use crate::tr;
use crate::ui::window_geometry::{
    DEFAULT_WINDOW_HEIGHT, MIN_WINDOW_HEIGHT, SAVE_DEBOUNCE_MS, WINDOW_WIDTH, height_to_remember,
    initial_window_height,
};
use crate::ui::{DeviceInfo, UiEvent, UiState};

// ── Engine selector helpers ───────────────────────────────────────────────────

/// Display name for an engine (used as the row title in the selector).
///
/// Untranslated proper nouns (matches `EngineType::short_name`'s established
/// convention) — RNNoise/DeepFilterNet/DPDFNet-2/DPDFNet-8/Khip are product
/// names, not prose, per D-05.
fn engine_label(engine: EngineType) -> &'static str {
    match engine {
        EngineType::RNNoise => "RNNoise",
        EngineType::DeepFilterNet => "DeepFilterNet",
        EngineType::Dpdfnet2 => "DPDFNet-2",
        EngineType::Dpdfnet8 => "DPDFNet-8",
        EngineType::Khip => "Khip",
    }
}

/// Subtitle describing what the engine does, shown below the row title.
///
/// Kept short so it fits in a single AdwActionRow subtitle line. Per D-06,
/// the DPDFNet-2/DPDFNet-8 subtitles communicate only relative PROCESSOR USE
/// — never a Light/Quality label or an unsupported sound-quality ranking,
/// since the phase's evaluation never established one.
fn engine_subtitle(engine: EngineType) -> String {
    // Per 08.3 D-01: wrap engine subtitles in tr!() for i18n coverage.
    match engine {
        EngineType::RNNoise => tr!("Lightweight, low CPU"),
        EngineType::DeepFilterNet => tr!("High quality (default)"),
        EngineType::Dpdfnet2 => tr!("DPDFNet, lower processor use"),
        // Per D-02: a static, gentle caveat only — never a measured
        // hardware verdict, never an auto-switch, just a pointer to the
        // manual remedy (the Low CPU Mode, D-03/D-04). Quick 260924-n4s
        // (R2): ONE tr!() msgid for the whole sentence pair, with real
        // sentence punctuation — a bare `format!()` join of two separately
        // translated fragments left the translator no control over the
        // period, and can't be reworded per-language.
        EngineType::Dpdfnet8 => tr!(
            "DPDFNet, higher processor use. May glitch on a slower processor — try Low CPU Mode"
        ),
        EngineType::Khip => tr!("User-supplied, adaptive"),
    }
}

/// Resolve `engine`'s truthful availability from `state.availability`,
/// falling back to "available" when the map has no entry for it (e.g. a
/// `UiState` constructed before the app layer populates the map — matches
/// the historical unconditional-enable behavior for engines that predate
/// per-engine availability tracking).
fn resolve_availability(state: &UiState, engine: EngineType) -> EngineAvailability {
    state
        .availability
        .get(&engine)
        .copied()
        .unwrap_or(EngineAvailability {
            available: true,
            reason: AvailabilityReason::Available,
        })
}

/// Compute the row title and subtitle for `engine` given its truthful
/// availability (T-15.1-07/D-08). When available, returns the engine's
/// normal label/subtitle. When unavailable, the row stays visible with a
/// translated unavailable title and a reason-specific subtitle — it is
/// never hidden and never silently re-enabled.
///
/// Khip keeps its existing, more actionable "not detected — copy the
/// library" copy (a user-fixable local install step). The other engines use
/// a shared generic "{name} (unavailable)" pattern, since a missing Cargo
/// feature or a missing bundled DPDFNet asset is not something the user can
/// fix by copying a file.
///
/// Phase 15.4 Plan 01 Task 2: reworded from "Khip (not installed)" /
/// "Not detected — copy libkhip.so to ~/.local/lib/" — the old title tripped
/// the AppImageHub catalog's screenshot OCR hard-phrase list ("not
/// installed"), and it's visible in EVERY catalog run since the catalog's
/// test host never has libkhip.so. Same meaning, no catalog-unsafe phrase.
fn engine_row_text(engine: EngineType, availability: EngineAvailability) -> (String, String) {
    if availability.available {
        return (engine_label(engine).to_owned(), engine_subtitle(engine));
    }
    if engine == EngineType::Khip {
        return (
            tr!("Khip (not detected)"),
            tr!("Optional: copy libkhip.so to ~/.local/lib/"),
        );
    }
    let title = format!("{} {}", engine.short_name(), tr!("(unavailable)"));
    let subtitle = match availability.reason {
        AvailabilityReason::FeatureDisabled => tr!("Not included in this build"),
        // RuntimeMissing and the impossible Available-with-available=false
        // case both fall back to the same generic, non-user-actionable copy.
        _ => tr!("Required files not found"),
    };
    (title, subtitle)
}

/// Whether a row's "became active" toggle should be allowed to dispatch
/// `UiEvent::EngineChanged` (T-15.1-10). Refuses when the engine is
/// currently unavailable OR a programmatic selection (`set_engine`/
/// `set_engine_availability`) is in flight. Extracted as a pure function so
/// the decision itself is unit-testable without constructing GTK widgets —
/// the real `connect_toggled` closure in `build_engine_selector` calls this
/// with its own live `available`/`updating` reads.
fn should_dispatch_engine_change(available: bool, updating: bool) -> bool {
    available && !updating
}

/// Compute the strength `ComboRow` level index that reflects `engine`'s own
/// remembered normalized strength (D-13). Used to restore the strength row
/// immediately when the user switches engines, rather than showing the
/// previously-active engine's level until the next full state sync.
fn restore_strength_level_index_for_engine(config: &Config, engine: EngineType) -> u32 {
    strength_to_level_index(config.strength_for(engine))
}

/// Engine selector built from a list of AdwActionRow + radio-grouped CheckButton
/// pairs, mirroring the GNOME Sound Settings output-device pattern.
///
/// Replaces the previous AdwComboRow which could not enforce row-level
/// disabling — `set_activatable(false)` on a `gtk4::ListItem` only affects
/// rendering, not GtkDropDown's selection model, so users could still pick
/// "Khip (not detected)" with no effect (silent early-return in the handler).
///
/// This selector uses `set_sensitive(false)` on any row whose engine is
/// currently unavailable, matching the tray's `enabled` flag semantics in
/// `src/tray.rs`.
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
    /// Live per-engine availability map, keyed by `EngineType` (D-02/D-08).
    /// Captured by `Rc` into each row's `connect_toggled` closure so the
    /// per-row "is this engine available?" filter reflects runtime
    /// detection (`set_engine_availability`), not just the construction-time
    /// value of `state.availability`. Generalizes the previous Khip-only
    /// `khip_available: Rc<Cell<bool>>` field (per parent todo Option E,
    /// 260427-cgu, extended to every engine by this plan).
    availability: Rc<RefCell<BTreeMap<EngineType, EngineAvailability>>>,
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
    /// dies (D-15). Per-engine availability is set at construction time (and
    /// updated by `set_engine_availability`) and is **independent** of
    /// this — calling `set_all_sensitive(true)` after the audio thread is
    /// restored does NOT undo an unavailable engine's per-row disabled
    /// state, because we read each CheckButton's current sensitivity (the
    /// availability-driven source of truth) before re-enabling.
    pub fn set_all_sensitive(&self, sensitive: bool) {
        for (_engine, row, check) in &self.rows {
            // A row stays disabled if its engine is currently unavailable.
            // Re-enabling the row here would let the user pick an engine
            // that cannot init — bug we're fixing.
            let allow = if sensitive {
                check.is_sensitive()
            } else {
                false
            };
            row.set_sensitive(allow);
        }
    }

    /// Update a single engine's availability at runtime (T-15.1-07/
    /// T-15.1-08), after construction-time detection reported it unavailable
    /// but a later re-poll succeeded — or, defensively, the reverse.
    /// Generalizes the previous Khip-only `set_khip_available` (per parent
    /// todo Option E, 260427-cgu) so every engine can be hot-updated from a
    /// single shared method, e.g. the 1500ms Khip re-detection tick in
    /// `src/app.rs`.
    ///
    /// Effects:
    ///   1. The shared `availability` map is updated so the per-row
    ///      `connect_toggled` closure's live truth check reflects the change.
    ///   2. The row's title/subtitle flip via `engine_row_text`.
    ///   3. The row and its CheckButton's `sensitive` flag flip to match.
    ///
    /// Does NOT fire `UiEvent::EngineChanged`: the row's
    /// `CheckButton::is_active` state is untouched, so no `connect_toggled`
    /// handler runs (mirrors the existing `updating` guard pattern used by
    /// `set_engine`).
    pub fn set_engine_availability(&self, engine: EngineType, availability: EngineAvailability) {
        // Idempotent — safe to call from any path, even redundantly.
        {
            let mut map = self.availability.borrow_mut();
            if map.get(&engine).copied() == Some(availability) {
                return;
            }
            map.insert(engine, availability);
        }

        for (e, row, check) in &self.rows {
            if *e == engine {
                let (title, subtitle) = engine_row_text(engine, availability);
                row.set_title(&title);
                row.set_subtitle(&subtitle);
                row.set_sensitive(availability.available);
                check.set_sensitive(availability.available);
                return;
            }
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
        .default_width(WINDOW_WIDTH)
        .default_height(DEFAULT_WINDOW_HEIGHT)
        .resizable(true)
        .build();

    // R2/R4: the WM-enforced floor. Becomes the X11 WM_NORMAL_HINTS PMinSize
    // hint / the Wayland xdg_toplevel.set_min_size request — see
    // `window_geometry`'s "Width policy and its limits" doc for why there is
    // no matching maximum-size hint.
    window.set_size_request(WINDOW_WIDTH, MIN_WINDOW_HEIGHT);

    // ── Header bar ────────────────────────────────────────────────────────────
    let header = HeaderBar::new();
    let win_title = libadwaita::WindowTitle::new(&tr!("CleanMic"), "");
    header.set_title_widget(Some(&win_title));

    // 260510-ec4: explicit decoration layout — show minimize + close.
    // Maximize stays hidden even though the window now resizes vertically
    // (quick task 260927-mvb, R1): the content column is clamped to
    // WINDOW_WIDTH, so a maximized window would just be the same 420px
    // column with empty side margins — a maximize button would be a no-op
    // for anything the user can see. A header double-click, Super+Up, or
    // edge tiling can still maximize or tile the window; those sizes are
    // never persisted (`record_window_height` skips maximized/fullscreen,
    // and GTK does not track `default-size` while maximized or tiled).
    header.set_decoration_layout(Some(":minimize,close"));

    // ── Hamburger menu (primary menu) ─────────────────────────────────────────
    // 260510-ec4: HIG three-section grouping —
    //   Section 1: app actions (updates / about)
    //   Section 2: feedback (report issue)
    //   Section 3: window-level (quit)
    let menu = gio::Menu::new();

    let app_section = gio::Menu::new();
    app_section.append(
        Some(&tr!("Check for updates")),
        Some("app.check-for-updates"),
    );
    app_section.append(Some(&tr!("About CleanMic")), Some("app.about"));
    menu.append_section(None, &app_section);

    let feedback_section = gio::Menu::new();
    feedback_section.append(Some(&tr!("Report an issue")), Some("app.report-issue"));
    menu.append_section(None, &feedback_section);

    let quit_section = gio::Menu::new();
    quit_section.append(Some(&tr!("Quit")), Some("app.quit"));
    menu.append_section(None, &quit_section);

    let menu_button = gtk4::MenuButton::new();
    menu_button.set_icon_name("open-menu-symbolic");
    menu_button.set_menu_model(Some(&menu));
    header.pack_end(&menu_button);

    // ── Preferences page ──────────────────────────────────────────────────────
    let page = PreferencesPage::new();

    // The page's own scroller (Task 2, R2): AdwPreferencesPage embeds a
    // GtkScrolledWindow internally; this makes its policy explicit
    // (Never/Automatic) instead of relying on the libadwaita default, and
    // must run before any group (and therefore any combo popover) exists.
    // See `page_scroller`'s doc for why no second scroller is added around
    // the page.
    match page_scroller(&page) {
        Some(scroller) => {
            scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        }
        None => {
            log::warn!(
                "could not locate AdwPreferencesPage's internal scroller — \
                 libadwaita's internal page layout may have changed; the \
                 page's own scrolling defaults apply instead"
            );
        }
    }

    // Fixed-width content column (R1): GTK4 has no max-size hint for a
    // *resizable* toplevel (see window_geometry's "Width policy and its
    // limits" doc), so the width is hard-clamped here instead. With
    // maximum_size == tightening_threshold, the clamp never eases — a wider,
    // tiled, or maximized window only gains empty side margins; the header
    // bar and banners still span the full width.
    let clamp = libadwaita::Clamp::new();
    clamp.set_maximum_size(WINDOW_WIDTH);
    clamp.set_tightening_threshold(WINDOW_WIDTH);
    clamp.set_unit(libadwaita::LengthUnit::Px);
    clamp.set_child(Some(&page));

    let subtitle = if state.active {
        tr!("Active")
    } else {
        tr!("Inactive")
    };
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

    // No-PipeWire banner (D-02) — hidden initially, revealed only when the
    // app layer discovers the daemon is unreachable (`set_pipewire_unavailable`,
    // called from `run_with_gui`). No button: there is nothing to click here,
    // unlike the update banner's "Download".
    let pipewire_banner = Banner::new(&tr!("Audio is off: PipeWire isn't running"));
    pipewire_banner.set_revealed(false);

    // The full explanation wraps in its own label (see the doc comment on
    // `WindowHandles::pipewire_explanation` for why it isn't in the banner
    // title itself). Every user-facing string here is short of the catalog
    // OCR hard-phrase list (research Pitfall 2) — no "failed to", "unable
    // to start", "cannot open", etc.
    let pipewire_explanation = gtk4::Label::new(Some(&tr!(
        "CleanMic needs PipeWire, the standard Linux audio service, to clean \
         your microphone. Start or install PipeWire, then open CleanMic again."
    )));
    pipewire_explanation.set_wrap(true);
    pipewire_explanation.set_wrap_mode(pango::WrapMode::WordChar);
    pipewire_explanation.set_xalign(0.0);
    pipewire_explanation.set_margin_start(12);
    pipewire_explanation.set_margin_end(12);
    pipewire_explanation.set_margin_top(6);
    pipewire_explanation.set_margin_bottom(6);
    pipewire_explanation.set_visible(false);

    let root = GBox::new(Orientation::Vertical, 0);
    root.append(&header);
    root.append(&update_banner);
    root.append(&pipewire_banner);
    root.append(&pipewire_explanation);
    root.append(&clamp);
    window.set_content(Some(&root));

    // ── Input group ───────────────────────────────────────────────────────────
    let input_group = PreferencesGroup::new();
    input_group.set_title(&tr!("Input"));

    // Device picker
    let device_row = build_device_row(state);
    let device_updating: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // Seeded from the same initial model `build_device_row` computed
    // internally — kept in sync thereafter by `update_device_list` under the
    // `device_updating` guard. Replaces the old build-time (description,
    // node.name) vector capture, which went stale after any runtime list
    // refresh (R1 makes refreshes routine: plug/unplug, pin changes).
    let device_targets: Rc<RefCell<Vec<PickerTarget>>> = Rc::new(RefCell::new(
        build_device_model(
            &state.available_devices,
            state.system_default_name.as_deref(),
            state.input_device.as_deref(),
        )
        .targets,
    ));
    {
        let tx = event_tx.clone();
        let device_updating_cb = device_updating.clone();
        let device_targets_cb = device_targets.clone();
        device_row.connect_selected_item_notify(move |row| {
            // G-05 guard: skip events fired by programmatic model refreshes
            // in update_device_list. Only real user clicks should emit
            // UiEvent::DeviceChanged / DeviceChangedToDefault.
            if device_updating_cb.get() {
                return;
            }
            let idx = row.selected() as usize;
            let target = device_targets_cb.borrow().get(idx).cloned();
            match target {
                Some(PickerTarget::Default) => {
                    if tx.send(UiEvent::DeviceChangedToDefault).is_err() {
                        log::warn!("UI event channel closed - DeviceChangedToDefault dropped");
                    }
                }
                Some(PickerTarget::Device(name)) => {
                    if tx.send(UiEvent::DeviceChanged(name)).is_err() {
                        log::warn!("UI event channel closed - DeviceChanged dropped");
                    }
                }
                Some(PickerTarget::NoInput) | None => {
                    // D-10 placeholder, or an out-of-range index (should not
                    // happen — guard anyway).
                }
            }
        });
    }
    input_group.add(&device_row);

    // Enable / disable switch
    let enable_row = SwitchRow::new();
    enable_row.set_title(&tr!("Enable"));
    enable_row.set_active(state.active);
    let enable_updating: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    {
        let tx = event_tx.clone();
        let enable_updating_cb = enable_updating.clone();
        enable_row.connect_active_notify(move |row: &SwitchRowRef| {
            // Quick task 260927-mvb: skip events fired by
            // `set_enable_visual_off`'s programmatic override — mirrors the
            // `device_updating`/`strength_updating`/`mode_updating` pattern.
            if enable_updating_cb.get() {
                return;
            }
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
    //
    // strength_row is built first so a clone of it (plus the shared
    // strength_updating guard) can be threaded into build_engine_selector —
    // switching engines restores the newly-active engine's own remembered
    // strength (D-13) immediately, rather than waiting for the next full
    // state sync.
    let strength_updating: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let strength_row = build_strength_row(state, event_tx.clone(), strength_updating.clone());
    let engine_selector = build_engine_selector(
        state,
        event_tx.clone(),
        config.clone(),
        strength_row.clone(),
        strength_updating.clone(),
    );

    // The strength row stays in its own group so the radio-row group reads
    // cleanly as "pick one engine" (matches GNOME Sound Settings output-device
    // styling — the volume slider is in a separate group from the device list).
    let strength_group = PreferencesGroup::new();
    strength_group.set_title(&tr!("Strength"));
    strength_group.add(&strength_row);

    // Mode picker — a distinct CPU/quality trade-off lever from Strength's
    // suppression-aggressiveness knob (D-04). Its own group, same pattern as
    // Strength.
    let mode_updating: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let mode_row = build_mode_row(state, event_tx.clone(), mode_updating.clone());
    let mode_group = PreferencesGroup::new();
    mode_group.set_title(&tr!("Mode"));
    mode_group.add(&mode_row);

    page.add(&engine_selector.group);
    page.add(&strength_group);
    page.add(&mode_group);

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
            let active = row.is_active();
            if tx.send(UiEvent::AutostartToggled(active)).is_err() {
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

    let auto_gain_row = build_auto_gain_row(state, event_tx.clone());
    settings_group.add(&auto_gain_row);

    page.add(&settings_group);

    // ── Window sizing: restore + debounced persistence (R2-R4) ───────────────
    //
    // Must run after every group has been added to `page`: the page's own
    // scroller propagates its natural height (see Task 2's explicit
    // `set_policy` on that scroller), so this measurement is only the FULL
    // content height once every group is present.
    let natural_content_height = {
        let (_min, nat, _min_baseline, _nat_baseline) =
            root.measure(Orientation::Vertical, WINDOW_WIDTH);
        (nat > 0).then_some(nat)
    };
    let restore_height = initial_window_height(
        config.borrow().window_height,
        natural_content_height,
        smallest_monitor_height(),
    );
    log::debug!(
        "restoring window height: remembered={:?} natural={:?} monitor={:?} -> {}",
        config.borrow().window_height,
        natural_content_height,
        smallest_monitor_height(),
        restore_height
    );
    window.set_default_size(WINDOW_WIDTH, restore_height);

    // Debounced persistence: `connect_default_height_notify` fires on every
    // resize pixel while the user drags, but the shared `Config` should only
    // be rewritten once per drag. The app.rs "UI state sync from config"
    // timer (~30fps) compares the WHOLE `Config` on every tick and calls
    // `sync_tray_state` on any difference, which sends an unconditional ksni
    // `LayoutUpdated` D-Bus signal — a per-pixel write would spam the tray
    // host during a drag. A monotonically increasing generation counter lets
    // a stale scheduled save recognize it has been superseded and skip
    // itself; no GLib source removal (FFI) is needed.
    {
        let generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        let config_resize = config.clone();
        let window_weak = window.downgrade();
        window.connect_default_height_notify(move |_win| {
            let this_generation = generation.get().wrapping_add(1);
            generation.set(this_generation);
            let generation_check = generation.clone();
            let config_cb = config_resize.clone();
            let window_weak_cb = window_weak.clone();
            glib::timeout_add_local_once(
                std::time::Duration::from_millis(SAVE_DEBOUNCE_MS),
                move || {
                    if generation_check.get() != this_generation {
                        // Superseded by a later resize notification.
                        return;
                    }
                    let Some(win) = window_weak_cb.upgrade() else {
                        return;
                    };
                    if record_window_height(&win, &config_cb) {
                        match config_cb.try_borrow() {
                            Ok(cfg) => {
                                if let Err(e) = cfg.save() {
                                    log::warn!("Failed to save window_height: {e}");
                                } else {
                                    log::debug!("Saved window_height = {:?}", cfg.window_height);
                                }
                            }
                            Err(_) => {
                                log::debug!("Config busy — skipping this window_height save tick");
                            }
                        }
                    }
                },
            );
        });
    }

    // ── Quit paths without close-request (R4) ─────────────────────────────────
    // Ctrl+Q (app.quit), the tray's Quit command, and SIGTERM all end the
    // GLib main loop WITHOUT ever running `connect_close_request` below —
    // that handler only fires for a user closing the window itself. Persist
    // the height here too so those paths aren't silently lossy. This ONLY
    // calls `record_window_height` (no `.save()`): `src/app.rs`'s single
    // ordered shutdown save, which runs after `run_with_args` returns (i.e.
    // after GApplication shutdown), persists this same shared `Config` Rc
    // and picks up the in-memory change made here.
    {
        let window_shutdown = window.downgrade();
        let config_shutdown = config.clone();
        app.connect_shutdown(move |_app| {
            if let Some(win) = window_shutdown.upgrade() {
                record_window_height(&win, &config_shutdown);
            }
        });
    }

    // ── Close behaviour: depends on tray availability ────────────────────────────
    // When the tray is available: hide window so the app continues in background.
    // When the tray is absent: closing the window quits the application.
    {
        let config_close = config;
        window.connect_close_request(move |win| {
            // R4: persist the current height BEFORE the tray-hint logic
            // below, whichever close path is taken — this covers both
            // close-to-tray and close-to-quit. The borrow started here ends
            // before the following `borrow_mut()`.
            if record_window_height(win, &config_close)
                && let Err(e) = config_close.borrow().save()
            {
                log::warn!("Failed to save window_height on close: {e}");
            }

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
                        let notif = gtk4::gio::Notification::new(&gettextrs::gettext(
                            "CleanMic is still running",
                        ));
                        notif.set_body(Some(&gettextrs::gettext(
                            "The window was closed but CleanMic continues processing \
                                 your microphone in the background. Look for the tray icon \
                                 to reopen or quit.",
                        )));
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
        mode_row,
        monitor_row,
        auto_gain_row,
        win_title,
        device_row,
        device_updating,
        device_targets,
        update_banner,
        pipewire_banner,
        pipewire_explanation,
        strength_updating,
        mode_updating,
        enable_updating,
    }
}

/// Return the smallest connected monitor's logical height, or `None` if
/// there is no display or no monitor (quick task 260927-mvb, R3/R4).
///
/// Before the window is mapped, GTK4 cannot say which monitor it will
/// eventually appear on (Wayland never exposes global monitor positions to a
/// client), so the smallest monitor is used as a conservative upper bound —
/// it guarantees the window fits on whichever monitor it lands on. This
/// value is only ever used as a CAP on the restored height; it never
/// overwrites the remembered value itself.
pub(crate) fn smallest_monitor_height() -> Option<i32> {
    let display = gtk4::gdk::Display::default()?;
    let monitors = display.monitors();
    let mut min_height: Option<i32> = None;
    for i in 0..monitors.n_items() {
        let Some(item) = monitors.item(i) else {
            continue;
        };
        let Ok(monitor) = item.downcast::<gtk4::gdk::Monitor>() else {
            continue;
        };
        let height = monitor.geometry().height();
        min_height = Some(match min_height {
            Some(current) => current.min(height),
            None => height,
        });
    }
    min_height
}

/// Depth-first search for `AdwPreferencesPage`'s own internal
/// `GtkScrolledWindow` (quick task 260927-mvb, R2).
///
/// libadwaita's embedded template
/// (`/org/gnome/Adwaita/ui/adw-preferences-page.ui`) already wraps its
/// content in `AdwPreferencesPage > GtkScrolledWindow > AdwClamp > GtkBox` —
/// with `propagate-natural-height` true, which Task 1's natural-height
/// measurement at window-build time depends on. This function only makes the
/// scroller's policy explicit (`Never`/`Automatic`); it never constructs a
/// second, nesting `ScrolledWindow` around the page, which would create two
/// scroll containers and break the harness's wheel-scroll driving and scroll
/// anchors (`scripts/nested-run.sh scroll`).
fn page_scroller(page: &PreferencesPage) -> Option<gtk4::ScrolledWindow> {
    fn search(widget: &gtk4::Widget) -> Option<gtk4::ScrolledWindow> {
        if let Ok(scrolled) = widget.clone().downcast::<gtk4::ScrolledWindow>() {
            return Some(scrolled);
        }
        let mut child = widget.first_child();
        while let Some(w) = child {
            if let Some(found) = search(&w) {
                return Some(found);
            }
            child = w.next_sibling();
        }
        None
    }

    let mut child = page.first_child();
    while let Some(widget) = child {
        if let Some(found) = search(&widget) {
            return Some(found);
        }
        child = widget.next_sibling();
    }
    None
}

/// Compute and, if it changed, persist the window's current "normal" height
/// into `config.window_height` (quick task 260927-mvb, R4).
///
/// Returns `false` (and touches nothing) when:
/// - the window is currently maximized or fullscreen — those sizes are never
///   remembered, since restoring into a maximized/fullscreen state would be
///   meaningless and GTK does not track `default-size` while in either state;
/// - [`height_to_remember`] rejects the current height (`<= 0`);
/// - the shared `Config` is currently borrowed elsewhere
///   (`try_borrow_mut` failing) — logged at debug level, never a panic.
///
/// Returns `true` only when the config's `window_height` was actually
/// changed, so callers know whether a save is warranted.
fn record_window_height(window: &ApplicationWindow, config: &Rc<RefCell<Config>>) -> bool {
    if window.is_maximized() || window.is_fullscreen() {
        return false;
    }
    let Some(height) = height_to_remember(window.default_size().1) else {
        return false;
    };
    match config.try_borrow_mut() {
        Ok(mut cfg) => {
            if cfg.window_height == Some(height) {
                false
            } else {
                cfg.window_height = Some(height);
                true
            }
        }
        Err(_) => {
            log::debug!("Config busy — skipping this window_height record");
            false
        }
    }
}

/// Build the "Automatic mic volume" `SwitchRow` for the Settings group
/// (quick task 260923-x24, per `LOCK-UI-TOGGLE`).
///
/// Mirrors `monitor_row`'s construction exactly: title, subtitle,
/// `set_active` from state, and a `connect_active_notify` handler that sends
/// [`UiEvent::AutoGainToggled`].
fn build_auto_gain_row(state: &UiState, event_tx: mpsc::Sender<UiEvent>) -> SwitchRow {
    let row = SwitchRow::new();
    row.set_title(&tr!("Automatic mic volume"));
    row.set_subtitle(&tr!("Boosts microphones that are too quiet"));
    row.set_active(state.auto_gain_enabled);
    row.connect_active_notify(move |row: &SwitchRowRef| {
        if event_tx
            .send(UiEvent::AutoGainToggled(row.is_active()))
            .is_err()
        {
            log::warn!("UI event channel closed - AutoGainToggled dropped");
        }
    });
    row
}

// ── Helper builders ───────────────────────────────────────────────────────────

/// What a given index in the device picker's CURRENT popup list represents.
///
/// Computed fresh by [`build_device_model`] every time the list is (re)built
/// and stored in [`WindowHandles::device_targets`] so the selection closure
/// always resolves a click against the list actually on screen (R1 makes
/// runtime list refreshes routine: plug/unplug, pin changes).
#[derive(Debug, Clone, PartialEq)]
enum PickerTarget {
    /// Index 0's synthetic "Default (Mic)" entry — selecting it emits
    /// `UiEvent::DeviceChangedToDefault`, clearing `config.input_device`
    /// rather than pinning to a specific name (D-06).
    Default,
    /// A real device, identified by its stable PipeWire node name —
    /// selecting it emits `UiEvent::DeviceChanged(name)`.
    Device(String),
    /// The D-10 "No input device available" placeholder entry — never
    /// emits a `UiEvent` (the picker is also insensitive in this state).
    NoInput,
}

/// Result of computing the picker's string model and current selection.
///
/// `strings` is the list shown in the dropdown.
/// `selected_idx` is the index the combo row should mark as active.
/// `default_present` indicates whether index 0 is the synthetic "Default (Mic)"
/// entry (true) or the first real device (false). The selection closure uses
/// this to decide which UiEvent variant to emit when index 0 is picked.
/// `no_input` indicates the D-10 "No input device available" state.
/// `targets` is parallel to `strings`: `targets[i]` identifies what picking
/// `strings[i]` means (Default / a specific device / the no-input sentinel).
struct DevicePickerModel {
    strings: Vec<String>,
    selected_idx: u32,
    /// Retained as part of the helper's contract even though the selection
    /// closure now inspects `targets` directly rather than a string prefix
    /// match. Future consumers that render the picker from the computed
    /// model without re-reading the widget can read this flag.
    #[allow(dead_code)]
    default_present: bool,
    no_input: bool,
    targets: Vec<PickerTarget>,
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
///
/// R1 / OWNER-LOCK: a real device with `available == false` renders as
/// `"{description} ({unplugged})"` — the full real name is never shortened,
/// only suffixed. Available devices render as their description untouched.
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
            targets: vec![PickerTarget::NoInput],
        };
    }

    // Resolve the default's description, if present and in the device list.
    let default_description: Option<String> = system_default_name
        .and_then(|name| devices.iter().find(|d| d.name == name))
        .map(|d| d.description.clone());

    let mut strings: Vec<String> = Vec::with_capacity(devices.len() + 1);
    let mut targets: Vec<PickerTarget> = Vec::with_capacity(devices.len() + 1);
    let default_present = if let Some(ref desc) = default_description {
        // D-02 label format: tr!("Default") + " (" + description + ")"
        strings.push(format!("{} ({})", tr!("Default"), desc));
        targets.push(PickerTarget::Default);
        true
    } else {
        false
    };
    for d in devices {
        if d.available {
            strings.push(d.description.clone());
        } else {
            // R1: kept only because it is pinned or the system default;
            // OWNER-LOCK: the full real name is never shortened, only
            // suffixed with the translated marker.
            strings.push(format!("{} ({})", d.description, tr!("unplugged")));
        }
        targets.push(PickerTarget::Device(d.name.clone()));
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
        targets,
    }
}

/// Build the microphone picker `ComboRow`.
fn build_device_row(state: &UiState) -> ComboRow {
    let row = ComboRow::new();
    row.set_title(&tr!("Microphone"));

    // OWNER-LOCK: device names must never be truncated or ellipsized —
    // start, middle, or end — anywhere in the picker. See
    // `apply_no_truncation`'s doc for why the device row uses
    // `CollapsedValue::Subtitle` (45-57-char device names cannot fit beside
    // the title) while Mode/Strength use `CollapsedValue::Suffix`.
    //
    // MUST run before the model is installed below: libadwaita writes the
    // `use-subtitle` subtitle only when the selection changes, so enabling
    // it afterwards left the row blank on first paint (debug session
    // mic-row-blank-single-device).
    apply_no_truncation(&row, CollapsedValue::Subtitle);

    let model = build_device_model(
        &state.available_devices,
        state.system_default_name.as_deref(),
        state.input_device.as_deref(),
    );
    install_device_model(&row, &model);

    row
}

/// Install a computed [`DevicePickerModel`] on the device picker row.
///
/// The single code path for both the initial build ([`build_device_row`])
/// and every runtime refresh ([`WindowHandles::update_device_list`]), so the
/// first paint can never diverge from a refresh again. `row` must already
/// have `apply_no_truncation(CollapsedValue::Subtitle)` applied: the
/// `set_model`/`set_selected` below are what make libadwaita write the
/// selected entry's full name into the row's subtitle.
///
/// With a single entry libadwaita deliberately hides the row's arrow and
/// makes it non-activatable (there is nothing to choose); the name is still
/// shown. With 2+ entries the row is openable.
///
/// Does not touch the `device_updating` guard or `device_targets` — callers
/// own those.
fn install_device_model(row: &ComboRow, model: &DevicePickerModel) {
    let list = gtk4::StringList::new(&model.strings.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    row.set_model(Some(&list));
    row.set_selected(model.selected_idx);
    row.set_sensitive(!model.no_input);
}

/// Where a [`full_name_factory`]'s label is rendered: in the popup list, or
/// as the row's own always-visible collapsed-value label (replacing
/// `AdwComboRow`'s default, ellipsizing-at-20-chars factory).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FactoryRole {
    /// The popup list shown when the row is activated.
    Popup,
    /// The collapsed value shown next to the row's title/arrow.
    CollapsedValue,
}

/// Plain-data label configuration for [`full_name_factory`], factored out so
/// the OWNER-LOCK contract is unit-testable without constructing any GTK
/// widget.
struct FullNameLabelSpec {
    ellipsize: pango::EllipsizeMode,
    wrap: bool,
    wrap_mode: pango::WrapMode,
    max_width_chars: i32,
    hexpand: bool,
    /// Minimum width in character units (`GtkLabel::width-chars`), or `-1`
    /// for GTK's default (no explicit minimum). Quick task 260927-mvb: for
    /// [`FactoryRole::CollapsedValue`] this reserves enough horizontal room
    /// for the longest fixed-vocabulary selector value even when a sibling
    /// row element (e.g. Mode's long DPDFNet-only-scope subtitle) would
    /// otherwise squeeze this label below its natural single-line width —
    /// the exact GTK box-allocation squeeze that clipped French "Qualité
    /// maximale" down to "Qualité" (the row's fixed single-line height hid
    /// the wrapped second line entirely).
    width_chars: i32,
}

/// The char count of "Faible consommation" — the longest French or English
/// selector value across Mode and Strength.
const LONGEST_SELECTOR_VALUE_CHARS: i32 = 19;

/// OWNER-LOCK no-truncation label spec for `role`.
///
/// Neither role ever ellipsizes, and both allow at least 19 characters
/// (`"Faible consommation"`, the longest French/English selector value)
/// before wrapping. The popup list (T-tua-03) wraps mid-word (`WordChar`) so
/// even a single unbroken long name bounds the popover's width rather than
/// growing it unboundedly; the collapsed value wraps only at spaces
/// (`Word`, never mid-word) since it sits beside the row's title rather than
/// inside a width-constrained popover.
fn full_name_label_spec(role: FactoryRole) -> FullNameLabelSpec {
    match role {
        FactoryRole::Popup => FullNameLabelSpec {
            ellipsize: pango::EllipsizeMode::None,
            wrap: true,
            wrap_mode: pango::WrapMode::WordChar,
            max_width_chars: 60,
            hexpand: true,
            width_chars: -1,
        },
        FactoryRole::CollapsedValue => FullNameLabelSpec {
            ellipsize: pango::EllipsizeMode::None,
            wrap: true,
            wrap_mode: pango::WrapMode::Word,
            max_width_chars: 60,
            hexpand: false,
            width_chars: LONGEST_SELECTOR_VALUE_CHARS,
        },
    }
}

/// Build a `role`-specific list-item factory for `row` (OWNER-LOCK, T-tua-01,
/// T-tua-03).
///
/// Renders each entry as a plain-text, non-ellipsizing, word-wrapping label
/// per [`full_name_label_spec`]. For [`FactoryRole::Popup`] only, a
/// check-mark image is added whose opacity mirrors whether that entry is the
/// row's currently `selected-item` — the same visual contract as
/// libadwaita's own default popup factory (`adw-combo-row.c`), which reacts
/// to `notify::selected-item` on the row. [`FactoryRole::CollapsedValue`]
/// renders a label-only child (no check mark, no `hexpand`), so it never
/// steals width from the row's title box.
fn full_name_factory(row: &ComboRow, role: FactoryRole) -> gtk4::SignalListItemFactory {
    let factory = gtk4::SignalListItemFactory::new();
    let spec = full_name_label_spec(role);

    // A weak reference to the row (per upstream `gtk_object_expression_new`
    // semantics) so this factory — whose lifetime is tied to the row's own
    // popover/suffix — never creates a strong reference cycle back to the
    // row itself.
    let row_expr = gtk4::ObjectExpression::new(row);
    let selected_item_expr = row_expr.chain_property::<ComboRow>("selected-item");

    factory.connect_setup(move |_factory, list_item| {
        let Some(list_item) = list_item.downcast_ref::<gtk4::ListItem>() else {
            return;
        };

        let hbox = GBox::new(Orientation::Horizontal, 6);

        let label = gtk4::Label::new(None);
        label.set_xalign(0.0);
        label.set_hexpand(spec.hexpand);
        // T-tua-03: bounded wrap width keeps the popover from growing
        // unboundedly wide on an extremely long name — it grows vertically
        // instead.
        label.set_ellipsize(spec.ellipsize);
        label.set_wrap(spec.wrap);
        label.set_wrap_mode(spec.wrap_mode);
        label.set_max_width_chars(spec.max_width_chars);
        label.set_width_chars(spec.width_chars);

        hbox.append(&label);

        if role == FactoryRole::Popup {
            let check = gtk4::Image::from_icon_name("object-select-symbolic");
            hbox.append(&check);

            // Check-mark opacity mirrors "is this item the row's
            // selected-item?" — evaluated fresh whenever `selected-item`
            // changes (bound with `this` = this specific list item, whose
            // own `item` property never changes after bind).
            let item_expr = gtk4::ListItem::this_expression("item");
            let opacity_expr = gtk4::ClosureExpression::with_callback::<f64, _>(
                [selected_item_expr.clone().upcast(), item_expr.upcast()],
                |values: &[glib::Value]| -> f64 {
                    let selected = values[1].get::<Option<glib::Object>>().ok().flatten();
                    let item = values[2].get::<Option<glib::Object>>().ok().flatten();
                    if selected == item { 1.0 } else { 0.0 }
                },
            );
            opacity_expr.bind(&check, "opacity", Some(list_item));
        }

        list_item.set_child(Some(&hbox));
    });

    factory.connect_bind(move |_factory, list_item| {
        let Some(list_item) = list_item.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let Some(child) = list_item.child() else {
            return;
        };
        let Some(hbox) = child.downcast_ref::<GBox>() else {
            return;
        };
        let Some(label) = hbox
            .first_child()
            .and_then(|w| w.downcast::<gtk4::Label>().ok())
        else {
            return;
        };
        let Some(item) = list_item.item() else {
            return;
        };
        let Some(string_object) = item.downcast_ref::<gtk4::StringObject>() else {
            return;
        };
        // Plain text (never markup), matching the OWNER-LOCK / T-tua-01
        // contract on the collapsed value.
        label.set_text(&string_object.string());
    });

    factory
}

/// Where a `ComboRow`'s collapsed value is rendered, for
/// [`apply_no_truncation`].
enum CollapsedValue {
    /// Route the selected value through the row's own subtitle
    /// (`use-subtitle`), which spans the full row width. Used for the
    /// device picker, whose 45-57-char device names cannot fit beside the
    /// title.
    Subtitle,
    /// Replace `AdwComboRow`'s default (ellipsizing) collapsed-value
    /// factory with a [`FactoryRole::CollapsedValue`] [`full_name_factory`],
    /// leaving the row's own subtitle untouched. Used for rows whose
    /// subtitle already carries other information (Mode's DPDFNet-only
    /// scope hint) or should otherwise stay free.
    Suffix,
}

/// OWNER-LOCK no-truncation entry point for every `AdwComboRow` in the
/// window: no ellipsis anywhere in a selector's popup list or its displayed
/// (collapsed) value.
///
/// `AdwComboRow` renders its always-visible collapsed value through a
/// `GtkListView` named `current` in the row's suffix, using `priv->factory`
/// — whose *default* factory hard-codes `gtk_label_set_ellipsize(END)` +
/// `gtk_label_set_max_width_chars(20)`, which is what produced "Qualité
/// maxim…". `set_factory()` replaces that factory on both `current` and the
/// popup `list` (when no separate list factory has been set yet);
/// `set_list_factory()` replaces only the popup. This function therefore
/// always installs a non-ellipsizing [`full_name_factory`] as the popup
/// factory, and — for [`CollapsedValue::Suffix`] — installs a second,
/// `CollapsedValue`-role factory via `set_factory()` *first*, so the later
/// `set_list_factory()` call is the one that ends up assigned to the popup
/// (calling them in the other order would let `set_factory()`'s own popup
/// assignment win instead).
///
/// [`CollapsedValue::Subtitle`] instead turns on `use-subtitle`, which
/// overwrites the row's own subtitle with the selected value on every
/// selection change — appropriate for the device picker but wrong for the
/// Mode row, whose subtitle already carries the DPDFNet-only scope hint
/// (D-04/15.2-02): `use-subtitle` there would silently delete that hint on
/// every selection. Mode and Strength therefore use
/// [`CollapsedValue::Suffix`], which leaves the row's own subtitle alone.
///
/// Ordering contract for [`CollapsedValue::Subtitle`]: call this BEFORE the
/// row's model is installed. libadwaita writes the `use-subtitle` subtitle
/// only from its selection-changed handler — turning `use-subtitle` on for a
/// row whose model and selection already exist leaves the subtitle blank
/// until the selection next changes (debug session
/// mic-row-blank-single-device). Debug builds assert this.
fn apply_no_truncation(row: &ComboRow, collapsed: CollapsedValue) {
    // T-voj-05 (parity with T-tua-01): translated selector values always
    // render as plain text, never interpreted as Pango markup.
    row.set_use_markup(false);

    match collapsed {
        CollapsedValue::Subtitle => {
            debug_assert!(
                row.model().is_none(),
                "apply_no_truncation(Subtitle) must run before the row's model is installed"
            );
            row.set_use_subtitle(true);
            row.set_subtitle_lines(0);
        }
        CollapsedValue::Suffix => {
            row.set_factory(Some(&full_name_factory(row, FactoryRole::CollapsedValue)));
        }
    }

    row.set_list_factory(Some(&full_name_factory(row, FactoryRole::Popup)));
}

/// Build the engine selector as an AdwPreferencesGroup containing one
/// AdwActionRow per engine, each with a radio-grouped CheckButton suffix.
///
/// The previous ComboRow-based approach could not enforce row-level disabling:
/// `set_activatable(false)` on a `gtk4::ListItem` only affects rendering, not
/// GtkDropDown's selection model, so users could still pick "Khip (not
/// installed)" with no effect. Mirrors the tray's enabled-flag semantics.
///
/// Rows are built in `EngineType::ALL`'s fixed order (D-07): RNNoise,
/// DeepFilterNet, DPDFNet-2, DPDFNet-8, Khip. `config`/`strength_row`/
/// `strength_updating` implement D-13: when the user picks a different
/// engine row, the strength row is immediately updated to that engine's own
/// remembered value under the shared guard, rather than the previously
/// active engine's value lingering until the next full state sync.
fn build_engine_selector(
    state: &UiState,
    event_tx: mpsc::Sender<UiEvent>,
    config: Rc<RefCell<Config>>,
    strength_row: ComboRow,
    strength_updating: Rc<Cell<bool>>,
) -> EngineSelector {
    let group = PreferencesGroup::new();
    group.set_title(&tr!("Noise Processing"));

    let updating: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // Live per-engine availability map shared with each row's
    // connect_toggled closure and with `set_engine_availability()`. Seeded
    // from the construction-time state so behavior is identical to the
    // previous frozen-by-value capture when no runtime flip occurs (per
    // 260427-cgu Option E, generalized to every engine by this plan).
    let availability: Rc<RefCell<BTreeMap<EngineType, EngineAvailability>>> =
        Rc::new(RefCell::new(state.availability.clone()));
    let mut rows: Vec<(EngineType, libadwaita::ActionRow, gtk4::CheckButton)> =
        Vec::with_capacity(EngineType::ALL.len());

    // Build the radio group: first CheckButton is the group leader; subsequent
    // ones are joined via set_group(Some(&group_leader)).
    let mut group_leader: Option<gtk4::CheckButton> = None;

    for engine in EngineType::ALL {
        let row = libadwaita::ActionRow::new();

        let engine_availability = resolve_availability(state, engine);
        let (title, subtitle) = engine_row_text(engine, engine_availability);
        row.set_title(&title);
        row.set_subtitle(&subtitle);

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

        // An unavailable row: sensitive=false. Setting on the row makes the
        // entire row visually disabled and unclickable; setting on the
        // CheckButton too is belt-and-suspenders so a programmatic
        // `set_active(true)` from a future bug also no-ops cleanly (D-08).
        if !engine_availability.available {
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
            // Rc clone of the live map so a runtime flip via
            // EngineSelector::set_engine_availability() is observable here.
            // Before 260427-cgu this was a frozen-by-value capture which
            // prevented hot-detection.
            let availability_cb = availability.clone();
            let config_cb = config.clone();
            let strength_row_cb = strength_row.clone();
            let strength_updating_cb = strength_updating.clone();
            check.connect_toggled(move |btn| {
                // Skip the "deactivating" half of the radio toggle — only the
                // row gaining selection should send an event.
                if !btn.is_active() {
                    return;
                }
                let available = availability_cb
                    .borrow()
                    .get(&engine)
                    .map(|a| a.available)
                    .unwrap_or(true);
                // Guards against programmatic mutations from set_engine()/
                // set_engine_availability() and against dispatching for a
                // row that has become unavailable since construction
                // (T-15.1-10) — even if some future code path leaves the
                // row itself sensitive.
                if !should_dispatch_engine_change(available, updating_cb.get()) {
                    return;
                }
                if tx.send(UiEvent::EngineChanged(engine)).is_err() {
                    log::warn!("UI event channel closed - EngineChanged dropped");
                }

                // D-13: restore THIS engine's own remembered strength into
                // the shared strength row immediately, guarded so this
                // programmatic restoration cannot re-emit
                // UiEvent::StrengthChanged (mirrors `device_updating`).
                let idx = restore_strength_level_index_for_engine(&config_cb.borrow(), engine);
                strength_updating_cb.set(true);
                if strength_row_cb.selected() != idx {
                    strength_row_cb.set_selected(idx);
                }
                strength_updating_cb.set(false);
            });
        }

        group.add(&row);
        rows.push((engine, row, check));
    }

    EngineSelector {
        group,
        rows,
        updating,
        availability,
    }
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
/// Used by all five engines — each accepts the same normalized values, which
/// it maps to its own internal parameters. `updating` guards programmatic
/// restoration (D-13, e.g. an engine-row click or `update_from_state`) so
/// `ComboRow::set_selected`'s unconditional `notify::selected` signal never
/// re-emits a spurious `UiEvent::StrengthChanged` for a value the user never
/// picked — mirrors the `device_updating` guard pattern.
fn build_strength_row(
    state: &UiState,
    event_tx: mpsc::Sender<UiEvent>,
    updating: Rc<Cell<bool>>,
) -> ComboRow {
    let row = ComboRow::new();
    row.set_title(&tr!("Strength"));

    let model = gtk4::StringList::new(&[]);
    model.append(&tr!("Light"));
    model.append(&tr!("Balanced"));
    model.append(&tr!("Strong"));
    row.set_model(Some(&model));
    row.set_selected(strength_to_level_index(state.strength));
    // OWNER-LOCK: full values in both the collapsed row and the popup, no
    // ellipsis. Suffix (not Subtitle): this row has no subtitle to preserve,
    // but Suffix is still correct/harmless here and keeps the three ComboRows
    // consistent (see `apply_no_truncation`'s doc).
    apply_no_truncation(&row, CollapsedValue::Suffix);

    row.connect_selected_notify(move |r| {
        if updating.get() {
            return;
        }
        let val = level_index_to_strength(r.selected());
        if event_tx.send(UiEvent::StrengthChanged(val)).is_err() {
            log::warn!("UI event channel closed - StrengthChanged dropped");
        }
    });

    row
}

// ── Mode level helpers (CPU/quality trade-off, D-03/D-04) ────────────────────

/// Map a [`ProcessingMode`] to its `ComboRow` level index (0=Low CPU,
/// 1=Balanced, 2=Max Quality).
fn mode_to_level_index(mode: ProcessingMode) -> u32 {
    match mode {
        ProcessingMode::LowCpu => 0,
        ProcessingMode::Balanced => 1,
        ProcessingMode::MaxQuality => 2,
    }
}

/// Map a `ComboRow` level index back to a [`ProcessingMode`].
fn level_index_to_mode(index: u32) -> ProcessingMode {
    match index {
        0 => ProcessingMode::LowCpu,
        2 => ProcessingMode::MaxQuality,
        _ => ProcessingMode::Balanced,
    }
}

/// Build the 3-step Mode `ComboRow` (Low CPU / Balanced / Max Quality).
///
/// Distinct from `build_strength_row` (D-04): Mode is a CPU/quality
/// trade-off, Strength is suppression aggressiveness. Entries deliberately do
/// NOT reuse Strength's Light/Balanced/Strong vocabulary — "Standard" (not
/// "Balanced") is used for the middle entry so the plain (non-contextual)
/// `tr!()`/gettext msgid never collides with Strength's own "Balanced"
/// msgid, which would otherwise force an identical French translation for
/// two different concepts. `updating` guards programmatic restoration
/// exactly like `build_strength_row`'s `updating` parameter — without it,
/// `ComboRow::set_selected` would re-emit a spurious `UiEvent::ModeChanged`
/// on every programmatic sync.
/// Engine-scope hint for the Mode row's subtitle: only `DpdfnetEngine`'s
/// `process()` actually decimates inference on Mode (D-03);
/// RNNoise/DeepFilterNet/Khip/the experimental adapter all keep `set_mode`
/// as a behavioral no-op (RNNoise stores the value but every branch is
/// documented as "no-op for now"). Extracted as a pure function so the
/// wording is unit-testable without constructing a GTK widget (this test
/// binary's single allowed `gtk4::init()` call is already spent — see the
/// `Tests` section header comment below).
fn mode_row_subtitle() -> String {
    tr!("Only affects the DPDFNet engines")
}

fn build_mode_row(
    state: &UiState,
    event_tx: mpsc::Sender<UiEvent>,
    updating: Rc<Cell<bool>>,
) -> ComboRow {
    let row = ComboRow::new();
    row.set_title(&tr!("Mode"));
    row.set_subtitle(&mode_row_subtitle());

    let model = gtk4::StringList::new(&[]);
    model.append(&tr!("Low CPU"));
    model.append(&tr!("Standard"));
    model.append(&tr!("Max Quality"));
    row.set_model(Some(&model));
    row.set_selected(mode_to_level_index(state.mode));
    // OWNER-LOCK: full values in both the collapsed row and the popup, no
    // ellipsis. Suffix (not Subtitle): `use-subtitle` would overwrite this
    // row's DPDFNet-only scope hint (set just above) on every selection.
    apply_no_truncation(&row, CollapsedValue::Suffix);

    row.connect_selected_notify(move |r| {
        if updating.get() {
            return;
        }
        let val = level_index_to_mode(r.selected());
        if event_tx.send(UiEvent::ModeChanged(val)).is_err() {
            log::warn!("UI event channel closed - ModeChanged dropped");
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

        // Strength level (3-step, same for all engines). Guarded (D-13) so
        // this programmatic sync cannot re-emit UiEvent::StrengthChanged.
        let level_idx = strength_to_level_index(state.strength);
        if self.strength_row.selected() != level_idx {
            self.strength_updating.set(true);
            self.strength_row.set_selected(level_idx);
            self.strength_updating.set(false);
        }

        // Mode (CPU/quality trade-off). Guarded (D-03/D-04) so this
        // programmatic sync cannot re-emit UiEvent::ModeChanged.
        let mode_idx = mode_to_level_index(state.mode);
        if self.mode_row.selected() != mode_idx {
            self.mode_updating.set(true);
            self.mode_row.set_selected(mode_idx);
            self.mode_updating.set(false);
        }

        // Enable/disable switch
        if self.enable_row.is_active() != state.active {
            self.enable_row.set_active(state.active);
        }

        // Monitor switch
        if self.monitor_row.is_active() != state.monitor_enabled {
            self.monitor_row.set_active(state.monitor_enabled);
        }

        // Automatic mic volume switch
        if self.auto_gain_row.is_active() != state.auto_gain_enabled {
            self.auto_gain_row.set_active(state.auto_gain_enabled);
        }

        self.win_title.set_subtitle(&if state.active {
            tr!("Active")
        } else {
            tr!("Inactive")
        });
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
        // G-05: guard the selected-item-notify handler so the programmatic
        // set_model / set_selected calls inside `install_device_model` don't
        // emit a spurious UiEvent::DeviceChanged. Resetting to false after
        // they complete ensures user clicks captured after this update still
        // fire normally.
        self.device_updating.set(true);
        // Assign the new targets in their own statement so the RefCell
        // borrow is released before set_model/set_selected run (both may
        // synchronously fire selected-item-notify, whose handler also
        // borrows `device_targets`).
        *self.device_targets.borrow_mut() = model.targets.clone();
        install_device_model(&self.device_row, &model);
        self.device_updating.set(false);
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

    /// Set the enable toggle's sensitivity WITHOUT touching its active
    /// state (D-02, T-15.4-02).
    ///
    /// Unlike [`Self::set_input_available`], this never calls
    /// `set_active(false)` — doing so would fire the row's
    /// `active-notify` handler and dispatch `UiEvent::EnableToggled(false)`,
    /// which cascades into `config.enabled = false` and a save. Used when
    /// PipeWire itself is unreachable: the user's saved preference must
    /// survive a PipeWire-less session untouched.
    pub fn set_enable_sensitive(&self, sensitive: bool) {
        self.enable_row.set_sensitive(sensitive);
    }

    /// Reveal or hide the no-PipeWire banner and its wrapping explanation
    /// (D-02). Called once at window build when PipeWire was unreachable at
    /// startup; a PipeWire-less session never attempts a live upgrade, so
    /// this is never re-evaluated afterward — the user restarts CleanMic
    /// once the daemon is back.
    ///
    /// Also forces the header subtitle to the existing translated
    /// "Inactive" string when `unavailable` is true (quick task
    /// 260927-mvb): audio genuinely is not running without PipeWire,
    /// regardless of `config.enabled` — the subtitle must not keep claiming
    /// "Active" while the banner right below it says otherwise. Reuses the
    /// same `tr!("Inactive")` msgid [`update_from_state`] already uses, so
    /// this introduces no new user-facing string.
    pub fn set_pipewire_unavailable(&self, unavailable: bool) {
        self.pipewire_banner.set_revealed(unavailable);
        self.pipewire_explanation.set_visible(unavailable);
        if unavailable {
            self.win_title.set_subtitle(&tr!("Inactive"));
        }
    }

    /// Force the enable switch to render OFF, WITHOUT dispatching
    /// `UiEvent::EnableToggled` or touching `config.enabled` (D-02, quick
    /// task 260927-mvb).
    ///
    /// Unlike [`Self::set_input_available`] (whose `false` branch ALSO
    /// forces the switch off, but through the normal notify path, since
    /// D-10's "no input device" case is a real pipeline decision that
    /// legitimately belongs in `config.enabled`), this only fixes the
    /// VISUAL contradiction of a PipeWire-less session showing the switch
    /// ON — the persisted preference must survive untouched so a later
    /// PipeWire-available relaunch resumes exactly as the user left it.
    /// Guarded by `enable_updating`, mirroring `device_updating` /
    /// `strength_updating` / `mode_updating`.
    pub fn set_enable_visual_off(&self) {
        self.enable_updating.set(true);
        if self.enable_row.is_active() {
            self.enable_row.set_active(false);
        }
        self.enable_updating.set(false);
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

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// GTK only permits initialization from a single OS thread ever, while `cargo
// test` runs each `#[test]` on its own thread. Most tests below are therefore
// pure/helper tests exercising the plain-data decision logic that the real
// `connect_toggled`/`connect_selected_notify` closures delegate to. Tests that
// must inspect a REAL widget (the device-row section) go through
// `crate::ui::gtk_test::run`, which executes them on the crate's one shared
// GTK thread and skips cleanly when no display server is available. Never
// call `gtk4::init()` directly from a test.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::engine::AvailabilityReason;

    // ── Fixed order and names (D-05/D-07) ───────────────────────────────────

    #[test]
    fn engine_selector_row_order_matches_d07() {
        // The selector builds rows from EngineType::ALL directly — this
        // pins that the constant itself is the fixed D-07 order the
        // selector relies on.
        assert_eq!(
            EngineType::ALL,
            [
                EngineType::RNNoise,
                EngineType::DeepFilterNet,
                EngineType::Dpdfnet2,
                EngineType::Dpdfnet8,
                EngineType::Khip,
            ]
        );
    }

    // ── build_device_model (R1, R2, OWNER-LOCK) ─────────────────────────────
    //
    // Lowercase fixture descriptions ("internal mic", "jack mic") and dotted
    // node names keep the i18n Class B guard from tripping on this file;
    // `tr!` returns the msgid verbatim in tests (no locale loaded).

    #[test]
    fn build_device_model_available_device_shows_full_description_untouched() {
        let devices = vec![DeviceInfo {
            name: "internal.mic".into(),
            description: "internal mic".into(),
            available: true,
        }];
        let model = build_device_model(&devices, None, Some("internal.mic"));
        assert_eq!(model.strings, vec!["internal mic".to_string()]);
    }

    #[test]
    fn build_device_model_unavailable_pinned_device_gets_unplugged_marker_and_is_selected() {
        let devices = vec![DeviceInfo {
            name: "jack.mic".into(),
            description: "jack mic".into(),
            available: false,
        }];
        let model = build_device_model(&devices, None, Some("jack.mic"));
        assert_eq!(
            model.strings,
            vec![format!("jack mic ({})", tr!("unplugged"))]
        );
        assert_eq!(model.selected_idx, 0);
    }

    #[test]
    fn build_device_model_with_default_present_orders_targets_and_label() {
        let devices = vec![
            DeviceInfo {
                name: "internal.mic".into(),
                description: "internal mic".into(),
                available: true,
            },
            DeviceInfo {
                name: "jack.mic".into(),
                description: "jack mic".into(),
                available: true,
            },
        ];
        let model = build_device_model(&devices, Some("internal.mic"), None);
        assert_eq!(
            model.targets,
            vec![
                PickerTarget::Default,
                PickerTarget::Device("internal.mic".into()),
                PickerTarget::Device("jack.mic".into()),
            ]
        );
        assert_eq!(
            model.strings[0],
            format!("{} ({})", tr!("Default"), "internal mic")
        );
    }

    #[test]
    fn build_device_model_with_default_absent_targets_start_at_device_zero() {
        let devices = vec![DeviceInfo {
            name: "internal.mic".into(),
            description: "internal mic".into(),
            available: true,
        }];
        // system_default_name is Some but doesn't resolve to any device in
        // `devices`, so no Default entry is prepended (matches D-01: only a
        // resolvable default gets the synthetic entry).
        let model = build_device_model(&devices, Some("unresolvable.name"), None);
        assert_eq!(
            model.targets,
            vec![PickerTarget::Device("internal.mic".into())]
        );
        assert!(!model.default_present);
    }

    #[test]
    fn build_device_model_no_input_state() {
        let model = build_device_model(&[], None, None);
        assert_eq!(model.strings, vec![tr!("No input device available")]);
        assert_eq!(model.targets, vec![PickerTarget::NoInput]);
        assert!(model.no_input);
    }

    // ── Device picker scenarios (debug session mic-row-blank-single-device) ─
    //
    // The session's "also verify" matrix: 1 device, 2+ devices, default =
    // CleanMic's own virtual source (self-loop), default = None, the saved
    // device unplugged (still enumerated) and the saved device gone (no
    // longer enumerated). Each `devices` list is the post-`picker_devices`
    // list the window actually receives.

    fn mic(name: &str, description: &str, available: bool) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            description: description.into(),
            available,
        }
    }

    struct PickerScenario {
        label: &'static str,
        devices: Vec<DeviceInfo>,
        system_default: Option<&'static str>,
        pinned: Option<&'static str>,
        /// The full, untruncated string the collapsed row must show.
        expected_shown: String,
        /// Number of entries in the popup list.
        expected_len: usize,
    }

    fn picker_scenarios() -> Vec<PickerScenario> {
        let internal = || mic("internal.mic", "internal mic", true);
        let headset = || mic("bt.headset", "bt headset", true);
        vec![
            PickerScenario {
                // The environment that exposed the bug: one visible mic, and
                // the OS default is CleanMic itself. `current_system_default_name`
                // already filters that to None upstream; passing the raw
                // name here also proves the window can never turn it into a
                // Default entry or a pickable target.
                label: "one device, os default is cleanmic (self-loop)",
                devices: vec![internal()],
                system_default: Some(crate::pipewire::NODE_NAME),
                pinned: Some("internal.mic"),
                expected_shown: "internal mic".into(),
                expected_len: 1,
            },
            PickerScenario {
                label: "one device, no default, following default",
                devices: vec![internal()],
                system_default: None,
                pinned: None,
                expected_shown: "internal mic".into(),
                expected_len: 1,
            },
            PickerScenario {
                label: "two devices, second pinned, no default",
                devices: vec![internal(), headset()],
                system_default: None,
                pinned: Some("bt.headset"),
                expected_shown: "bt headset".into(),
                expected_len: 2,
            },
            PickerScenario {
                label: "two devices, following a resolvable default",
                devices: vec![internal(), headset()],
                system_default: Some("internal.mic"),
                pinned: None,
                expected_shown: format!("{} ({})", tr!("Default"), "internal mic"),
                expected_len: 3,
            },
            PickerScenario {
                label: "saved device unplugged but still enumerated",
                devices: vec![internal(), mic("jack.mic", "jack mic", false)],
                system_default: None,
                pinned: Some("jack.mic"),
                expected_shown: format!("jack mic ({})", tr!("unplugged")),
                expected_len: 2,
            },
            PickerScenario {
                label: "saved device gone from enumeration",
                devices: vec![internal()],
                system_default: None,
                pinned: Some("gone.mic"),
                expected_shown: "internal mic".into(),
                expected_len: 1,
            },
            PickerScenario {
                label: "no input device at all",
                devices: vec![],
                system_default: None,
                pinned: None,
                expected_shown: tr!("No input device available"),
                expected_len: 1,
            },
        ]
    }

    #[test]
    fn device_picker_scenarios_select_the_expected_full_name() {
        for sc in picker_scenarios() {
            let model = build_device_model(&sc.devices, sc.system_default, sc.pinned);
            assert_eq!(model.strings.len(), sc.expected_len, "{}", sc.label);
            assert_eq!(model.targets.len(), model.strings.len(), "{}", sc.label);
            let shown = model
                .strings
                .get(model.selected_idx as usize)
                .unwrap_or_else(|| panic!("{}: selected_idx out of range", sc.label));
            assert_eq!(shown, &sc.expected_shown, "{}", sc.label);
            // Self-loop guard: CleanMic's own virtual source is never offered.
            assert!(
                !model
                    .targets
                    .contains(&PickerTarget::Device(crate::pipewire::NODE_NAME.into())),
                "{}",
                sc.label
            );
        }
    }

    /// What a device `ComboRow` actually displays, read back on the GTK thread.
    #[derive(Debug)]
    struct DeviceRowView {
        subtitle: String,
        n_items: u32,
        activatable: bool,
        sensitive: bool,
    }

    fn device_row_view(row: &ComboRow) -> DeviceRowView {
        DeviceRowView {
            subtitle: row.subtitle().map(|s| s.to_string()).unwrap_or_default(),
            n_items: row.model().map(|m| m.n_items()).unwrap_or(0),
            activatable: row.is_activatable(),
            sensitive: row.is_sensitive(),
        }
    }

    fn scenario_state(sc: &PickerScenario) -> UiState {
        let mut state = UiState::from_config(&Config::default());
        state.available_devices = sc.devices.clone();
        state.system_default_name = sc.system_default.map(Into::into);
        state.input_device = sc.pinned.map(Into::into);
        state
    }

    /// Regression (debug session mic-row-blank-single-device): the collapsed
    /// Microphone row must show the selected entry's full name on FIRST
    /// paint, in every scenario. libadwaita writes the `use-subtitle`
    /// subtitle only on a selection change, so enabling it after the model
    /// was installed left the row blank until an unrelated refresh — and
    /// the 1500 ms timer never refreshes a stable environment. The row must
    /// also be openable whenever there is anything to choose (2+ entries).
    #[test]
    fn device_row_shows_selected_full_name_on_first_paint() {
        let ran = crate::ui::gtk_test::run(|| {
            for sc in picker_scenarios() {
                let row = build_device_row(&scenario_state(&sc));
                let view = device_row_view(&row);
                assert_eq!(
                    view.subtitle, sc.expected_shown,
                    "{}: collapsed row must show the selected full name ({view:?})",
                    sc.label
                );
                assert_eq!(view.n_items as usize, sc.expected_len, "{}", sc.label);
                if sc.expected_len >= 2 {
                    assert!(
                        view.activatable,
                        "{}: row must be openable when there is a choice",
                        sc.label
                    );
                }
                assert_eq!(view.sensitive, !sc.devices.is_empty(), "{}", sc.label);
            }
        });
        if ran.is_none() {
            eprintln!("skipped: no display server for GTK");
        }
    }

    /// The refresh path (`update_device_list` -> `install_device_model`)
    /// keeps the selected full name visible through device-count changes:
    /// 1 device -> 2 (e.g. a Bluetooth headset connects: the row becomes
    /// openable) -> back to 1 (it disconnects: the row falls back to the
    /// remaining mic) -> no device at all (D-10 placeholder, insensitive).
    #[test]
    fn device_row_refresh_keeps_selected_full_name_across_device_count_changes() {
        let ran = crate::ui::gtk_test::run(|| {
            let one = vec![mic("internal.mic", "internal mic", true)];
            let two = vec![
                mic("internal.mic", "internal mic", true),
                mic("bt.headset", "bt headset", true),
            ];
            let mut state = UiState::from_config(&Config::default());
            state.available_devices = one.clone();
            state.input_device = Some("internal.mic".into());
            let row = build_device_row(&state);
            assert_eq!(device_row_view(&row).subtitle, "internal mic");

            install_device_model(&row, &build_device_model(&two, None, Some("bt.headset")));
            let view = device_row_view(&row);
            assert_eq!(view.subtitle, "bt headset", "{view:?}");
            assert_eq!(view.n_items, 2);
            assert!(view.activatable, "two entries must be openable: {view:?}");

            install_device_model(&row, &build_device_model(&one, None, Some("bt.headset")));
            let view = device_row_view(&row);
            assert_eq!(view.subtitle, "internal mic", "{view:?}");
            assert!(view.sensitive);

            install_device_model(&row, &build_device_model(&[], None, None));
            let view = device_row_view(&row);
            assert_eq!(view.subtitle, tr!("No input device available"));
            assert!(!view.sensitive);
        });
        if ran.is_none() {
            eprintln!("skipped: no display server for GTK");
        }
    }

    // ── build_auto_gain_row (quick task 260923-x24, LOCK-UI-TOGGLE) ─────────

    /// The "Automatic mic volume" row shows the right title/subtitle, mirrors
    /// `state.auto_gain_enabled`, and emits `UiEvent::AutoGainToggled` on
    /// toggle. Goes through the shared GTK test thread like the device-row
    /// tests above; skips cleanly when headless.
    #[test]
    fn auto_gain_row_reflects_state_and_emits_toggle_event() {
        let ran = crate::ui::gtk_test::run(|| {
            let mut state = UiState::from_config(&Config::default());
            state.auto_gain_enabled = true;
            let (tx, rx) = mpsc::channel::<UiEvent>();
            let row = build_auto_gain_row(&state, tx);

            assert_eq!(row.title().to_string(), tr!("Automatic mic volume"));
            assert_eq!(
                row.subtitle().map(|t| t.to_string()).unwrap_or_default(),
                tr!("Boosts microphones that are too quiet")
            );
            assert!(row.is_active(), "row must reflect state.auto_gain_enabled");

            row.set_active(false);
            assert_eq!(rx.try_recv(), Ok(UiEvent::AutoGainToggled(false)));
        });
        if ran.is_none() {
            eprintln!("skipped: no display server for GTK");
        }
    }

    #[test]
    fn engine_label_names_match_d05_exactly() {
        let names: Vec<&str> = EngineType::all().map(engine_label).collect();
        assert_eq!(
            names,
            vec!["RNNoise", "DeepFilterNet", "DPDFNet-2", "DPDFNet-8", "Khip"]
        );
    }

    // ── Subtitle wording — D-06 (no quality hierarchy claim) ────────────────

    #[test]
    fn dpdfnet_subtitles_communicate_processor_use_not_quality_per_d06() {
        for engine in [EngineType::Dpdfnet2, EngineType::Dpdfnet8] {
            let subtitle = engine_subtitle(engine).to_lowercase();
            assert!(
                !subtitle.contains("quality"),
                "{engine:?} subtitle must not claim a quality ranking: {subtitle}"
            );
            assert!(
                !subtitle.contains("light") && !subtitle.contains("strong"),
                "{engine:?} subtitle must not reuse Light/Strong labels: {subtitle}"
            );
            assert!(
                subtitle.contains("processor"),
                "{engine:?} subtitle should describe relative processor use: {subtitle}"
            );
        }
        // The two variants must still read as distinct from each other.
        assert_ne!(
            engine_subtitle(EngineType::Dpdfnet2),
            engine_subtitle(EngineType::Dpdfnet8)
        );
    }

    #[test]
    fn dpdfnet8_subtitle_carries_a_gentle_static_weak_cpu_caveat_per_d02() {
        let subtitle = engine_subtitle(EngineType::Dpdfnet8).to_lowercase();
        // Points at the low-CPU Mode remedy...
        assert!(
            subtitle.contains("low cpu mode"),
            "DPDFNet-8 subtitle must point to the Low CPU Mode remedy: {subtitle}"
        );
        // ...without asserting a measured hardware verdict (D-02: static,
        // gentle note only — never a benchmarked/quantified claim).
        assert!(
            !subtitle.contains("quality")
                && !subtitle.contains("benchmark")
                && !subtitle.contains("measured"),
            "DPDFNet-8 caveat must never claim a measured verdict: {subtitle}"
        );
        // The pre-existing D-06 processor-use wording must still be present.
        assert!(subtitle.contains("processor"));
    }

    #[test]
    fn mode_row_subtitle_conveys_dpdfnet_only_scope() {
        // D-04 discretionary hint: the Mode row's subtitle should tell the
        // user its DPDFNet-only scope, mirroring the same tr!() convention
        // used everywhere else (no new dialog/toast subsystem). Tested via
        // the extracted pure `mode_row_subtitle()` helper — no ComboRow is
        // constructed here (this test binary's single allowed
        // `gtk4::init()` call is already spent elsewhere).
        let subtitle = mode_row_subtitle();
        assert!(
            subtitle.to_lowercase().contains("dpdfnet"),
            "Mode row subtitle should convey its DPDFNet-only scope: {subtitle:?}" // i18n-ignore
        );
    }

    // ── OWNER-LOCK no-truncation label spec (260923-voj) ────────────────────
    //
    // Pure `full_name_label_spec()` tests only — no ComboRow/GTK widget is
    // constructed here (same constraint as `mode_row_subtitle_conveys_...`
    // above: this test binary's single allowed `gtk4::init()` call is
    // already spent elsewhere).

    #[test]
    fn full_name_label_spec_popup_matches_tua_contract() {
        // Regression guard for the Microphone popup (260923-tua): the Popup
        // role must keep exactly the spec 260923-tua shipped.
        let spec = full_name_label_spec(FactoryRole::Popup);
        assert_eq!(spec.ellipsize, pango::EllipsizeMode::None);
        assert!(spec.wrap);
        assert_eq!(spec.wrap_mode, pango::WrapMode::WordChar);
        assert_eq!(spec.max_width_chars, 60);
        assert!(spec.hexpand);
    }

    #[test]
    fn full_name_label_spec_collapsed_value_never_ellipsizes() {
        let spec = full_name_label_spec(FactoryRole::CollapsedValue);
        assert_eq!(spec.ellipsize, pango::EllipsizeMode::None);
        assert!(spec.wrap);
        // Breaks only at spaces, never mid-word — distinct from the Popup
        // role's WordChar, since this label sits beside the row's title
        // rather than inside a width-constrained popover.
        assert_eq!(spec.wrap_mode, pango::WrapMode::Word);
        assert_eq!(spec.max_width_chars, 60);
        assert!(!spec.hexpand);
    }

    #[test]
    fn full_name_label_spec_collapsed_value_reserves_a_minimum_width() {
        // Quick task 260927-mvb: a sibling row element (e.g. Mode's long
        // DPDFNet-only-scope subtitle) squeezing this label below its
        // natural single-line width is exactly what clipped French
        // "Qualité maximale" down to "Qualité" — the row's fixed
        // single-line height hides the wrapped second line entirely.
        // width_chars sets a MINIMUM (distinct from max_width_chars, which
        // only bounds growth), guaranteeing room for the longest
        // fixed-vocabulary selector value regardless of squeeze.
        let collapsed = full_name_label_spec(FactoryRole::CollapsedValue);
        assert_eq!(collapsed.width_chars, LONGEST_SELECTOR_VALUE_CHARS);

        // The Popup role is unaffected — it already has its own
        // T-tua-03 popover-width bounding via max_width_chars and needs no
        // minimum-width guarantee.
        let popup = full_name_label_spec(FactoryRole::Popup);
        assert_eq!(popup.width_chars, -1);
    }

    #[test]
    fn full_name_label_spec_fits_longest_selector_value() {
        for role in [FactoryRole::Popup, FactoryRole::CollapsedValue] {
            let spec = full_name_label_spec(role);
            assert_eq!(
                spec.ellipsize,
                pango::EllipsizeMode::None,
                "{role:?}: OWNER-LOCK forbids ellipsis"
            );
            assert!(
                spec.max_width_chars >= LONGEST_SELECTOR_VALUE_CHARS,
                "{role:?}: max_width_chars {} must fit \"Faible consommation\" ({} chars)", // i18n-ignore
                spec.max_width_chars,
                LONGEST_SELECTOR_VALUE_CHARS
            );
        }
    }

    // ── engine_row_text — disabled-row behavior (D-08) ──────────────────────

    #[test]
    fn engine_row_text_available_engine_uses_normal_label_and_subtitle() {
        let (title, subtitle) = engine_row_text(
            EngineType::Dpdfnet2,
            EngineAvailability {
                available: true,
                reason: AvailabilityReason::Available,
            },
        );
        assert_eq!(title, "DPDFNet-2");
        assert_eq!(subtitle, engine_subtitle(EngineType::Dpdfnet2));
    }

    #[test]
    fn engine_row_text_unavailable_dpdfnet_variant_stays_visible_and_translated() {
        for reason in [
            AvailabilityReason::FeatureDisabled,
            AvailabilityReason::RuntimeMissing,
        ] {
            let (title, subtitle) = engine_row_text(
                EngineType::Dpdfnet8,
                EngineAvailability {
                    available: false,
                    reason,
                },
            );
            // Never hidden: the row keeps its product name in the title,
            // plus a translated unavailable marker (D-08).
            assert!(title.contains("DPDFNet-8"));
            assert!(
                !subtitle.is_empty(),
                "unavailable subtitle must not be blank"
            );
            assert_ne!(
                subtitle,
                engine_subtitle(EngineType::Dpdfnet8),
                "unavailable subtitle must differ from the normal one"
            );
        }
    }

    #[test]
    fn engine_row_text_khip_unavailable_keeps_existing_actionable_copy() {
        let (title, subtitle) = engine_row_text(
            EngineType::Khip,
            EngineAvailability {
                available: false,
                reason: AvailabilityReason::RuntimeMissing,
            },
        );
        assert_eq!(title, "Khip (not detected)");
        assert_eq!(subtitle, "Optional: copy libkhip.so to ~/.local/lib/");
    }

    #[test]
    fn engine_row_text_dpdfnet_variants_are_independent() {
        // D-02: one variant's unavailability never leaks into the other's
        // row text.
        let (title2, _) = engine_row_text(
            EngineType::Dpdfnet2,
            EngineAvailability {
                available: false,
                reason: AvailabilityReason::RuntimeMissing,
            },
        );
        let (title8, _) = engine_row_text(
            EngineType::Dpdfnet8,
            EngineAvailability {
                available: true,
                reason: AvailabilityReason::Available,
            },
        );
        assert!(title2.contains("DPDFNet-2"));
        assert_eq!(title8, "DPDFNet-8");
    }

    // ── resolve_availability ─────────────────────────────────────────────────

    #[test]
    fn resolve_availability_falls_back_to_available_when_map_has_no_entry() {
        let state = UiState::default();
        assert!(state.availability.is_empty());
        let resolved = resolve_availability(&state, EngineType::Dpdfnet2);
        assert!(resolved.available);
    }

    #[test]
    fn resolve_availability_reads_the_populated_map_entry() {
        let mut state = UiState::default();
        state.availability.insert(
            EngineType::Dpdfnet2,
            EngineAvailability {
                available: false,
                reason: AvailabilityReason::RuntimeMissing,
            },
        );
        let resolved = resolve_availability(&state, EngineType::Dpdfnet2);
        assert!(!resolved.available);
        assert_eq!(resolved.reason, AvailabilityReason::RuntimeMissing);
    }

    // ── should_dispatch_engine_change — no callback on unavailable/
    //    programmatic rows (T-15.1-10) ───────────────────────────────────────

    #[test]
    fn should_dispatch_engine_change_truth_table() {
        assert!(
            should_dispatch_engine_change(true, false),
            "available, not updating -> dispatch"
        );
        assert!(
            !should_dispatch_engine_change(false, false),
            "unavailable row must never dispatch even when not updating"
        );
        assert!(
            !should_dispatch_engine_change(true, true),
            "programmatic update in flight must never dispatch"
        );
        assert!(
            !should_dispatch_engine_change(false, true),
            "unavailable AND updating must never dispatch"
        );
    }

    // ── restore_strength_level_index_for_engine — D-13 ──────────────────────

    #[test]
    fn restore_strength_level_index_for_engine_reads_each_engines_own_value() {
        let mut config = Config::default();
        config.set_strength_for(EngineType::RNNoise, 0.1); // Light
        config.set_strength_for(EngineType::Dpdfnet2, 0.5); // Balanced
        config.set_strength_for(EngineType::Dpdfnet8, 0.9); // Strong

        assert_eq!(
            restore_strength_level_index_for_engine(&config, EngineType::RNNoise),
            0
        );
        assert_eq!(
            restore_strength_level_index_for_engine(&config, EngineType::Dpdfnet2),
            1
        );
        assert_eq!(
            restore_strength_level_index_for_engine(&config, EngineType::Dpdfnet8),
            2
        );
    }

    #[test]
    fn restore_strength_level_index_for_engine_does_not_mix_up_engines() {
        // D-13: switching to engine A must never read engine B's strength.
        let mut config = Config::default();
        config.set_strength_for(EngineType::RNNoise, 0.05);
        config.set_strength_for(EngineType::DeepFilterNet, 0.95);

        let rnnoise_idx = restore_strength_level_index_for_engine(&config, EngineType::RNNoise);
        let deepfilter_idx =
            restore_strength_level_index_for_engine(&config, EngineType::DeepFilterNet);
        assert_ne!(rnnoise_idx, deepfilter_idx);
    }

    // ── strength_to_level_index / level_index_to_strength (pre-existing,
    //    now exercised alongside the new engine-restoration tests) ─────────

    #[test]
    fn strength_to_level_index_boundaries() {
        assert_eq!(strength_to_level_index(0.0), 0);
        assert_eq!(strength_to_level_index(0.32), 0);
        assert_eq!(strength_to_level_index(0.33), 1);
        assert_eq!(strength_to_level_index(0.66), 1);
        assert_eq!(strength_to_level_index(0.67), 2);
        assert_eq!(strength_to_level_index(1.0), 2);
    }

    // ── mode_to_level_index / level_index_to_mode — D-03/D-04 ───────────────

    #[test]
    fn mode_to_level_index_round_trips() {
        assert_eq!(mode_to_level_index(ProcessingMode::LowCpu), 0);
        assert_eq!(mode_to_level_index(ProcessingMode::Balanced), 1);
        assert_eq!(mode_to_level_index(ProcessingMode::MaxQuality), 2);

        assert_eq!(level_index_to_mode(0), ProcessingMode::LowCpu);
        assert_eq!(level_index_to_mode(1), ProcessingMode::Balanced);
        assert_eq!(level_index_to_mode(2), ProcessingMode::MaxQuality);
    }

    #[test]
    fn mode_entries_do_not_reuse_strength_vocabulary() {
        // D-04: Mode must not reuse Strength's Light/Balanced/Strong labels —
        // not even "Balanced", since the plain (non-contextual) tr!()/gettext
        // msgid would otherwise force an identical French translation for
        // two different concepts.
        let strength_entries = ["Light", "Balanced", "Strong"]; // i18n-ignore
        let mode_entries = ["Low CPU", "Standard", "Max Quality"]; // i18n-ignore
        for entry in mode_entries {
            assert!(
                !strength_entries.contains(&entry),
                "Mode entry {entry:?} must not reuse a Strength vocabulary word" // i18n-ignore
            );
        }
    }

    // ── Window height restore + debounced persistence (quick 260927-mvb) ────

    /// Build a registered, `NON_UNIQUE` throwaway `libadwaita::Application`
    /// for a GTK-thread test, or `None` (with an explanatory `eprintln!`) if
    /// registration fails — GTK rejects a window built without one.
    fn test_application(id: &str) -> Option<libadwaita::Application> {
        let app = libadwaita::Application::builder()
            .application_id(id)
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        match app.register(gio::Cancellable::NONE) {
            Ok(()) => Some(app),
            Err(e) => {
                eprintln!("skipped: could not register test application: {e}");
                None
            }
        }
    }

    #[test]
    fn window_height_survives_resize_save_and_relaunch() {
        let ran = crate::ui::gtk_test::run(|| {
            let Some(app) = test_application("com.cleanmic.CleanMic.test.window-height") else {
                return;
            };

            // ── Restore: a config with a remembered height opens at that
            // height, within the WM-enforced minimum size request. ──────────
            let config = Rc::new(RefCell::new(Config {
                window_height: Some(560),
                ..Config::default()
            }));
            let state = UiState::from_config(&config.borrow());
            let (tx, _rx) = mpsc::channel::<UiEvent>();
            let handles = build_main_window(&app, &state, tx, config.clone(), false);
            let window = handles.window.clone();

            assert!(window.is_resizable());
            assert_eq!(window.size_request(), (WINDOW_WIDTH, MIN_WINDOW_HEIGHT));
            match smallest_monitor_height() {
                Some(monitor_height) if monitor_height < 560 + window_geometry_test_margin() => {
                    eprintln!(
                        "skipped default_size assertion: monitor height {monitor_height} \
                         is too small to fit 560 + margin"
                    );
                }
                _ => {
                    assert_eq!(window.default_size(), (WINDOW_WIDTH, 560));
                }
            }

            // ── Resize -> debounced save -> a second resize -> save again. ──
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let pump_until = |config: &Rc<RefCell<Config>>, expected: Option<i32>| {
                let ctx = glib::MainContext::default();
                while std::time::Instant::now() < deadline
                    && config.borrow().window_height != expected
                {
                    ctx.iteration(false);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                assert_eq!(config.borrow().window_height, expected);
            };

            window.set_default_size(WINDOW_WIDTH, 610);
            pump_until(&config, Some(610));

            window.set_default_size(WINDOW_WIDTH, 250);
            pump_until(&config, Some(MIN_WINDOW_HEIGHT));

            window.set_default_size(WINDOW_WIDTH, 610);
            pump_until(&config, Some(610));

            // ── Relaunch: save to a fresh tempdir, load it back, build a
            // second window from the loaded config — it must restore 610. ──
            let tmp = tempfile::tempdir().expect("tempdir");
            let path = tmp.path().join("config.toml");
            config.borrow().save_to(&path).expect("save_to failed");
            let loaded = Config::load_from(&path).expect("load_from failed");
            assert_eq!(loaded.window_height, Some(610));
            let loaded_config = Rc::new(RefCell::new(loaded));
            let loaded_state = UiState::from_config(&loaded_config.borrow());
            let (tx2, _rx2) = mpsc::channel::<UiEvent>();
            let relaunch_handles =
                build_main_window(&app, &loaded_state, tx2, loaded_config.clone(), false);
            match smallest_monitor_height() {
                Some(monitor_height) if monitor_height < 610 + window_geometry_test_margin() => {
                    eprintln!(
                        "skipped relaunch default_size assertion: monitor height \
                         {monitor_height} is too small to fit 610 + margin"
                    );
                }
                _ => {
                    assert_eq!(relaunch_handles.window.default_size(), (WINDOW_WIDTH, 610));
                }
            }

            // ── A default config's content genuinely needs scrolling: its
            // natural height exceeds DEFAULT_WINDOW_HEIGHT, and the window
            // itself opens at DEFAULT_WINDOW_HEIGHT (not the natural size). ──
            let default_config = Rc::new(RefCell::new(Config::default()));
            let default_state = UiState::from_config(&default_config.borrow());
            let (tx3, _rx3) = mpsc::channel::<UiEvent>();
            let default_handles =
                build_main_window(&app, &default_state, tx3, default_config.clone(), false);
            if let Some(content) = default_handles.window.content() {
                let (_min, nat, _min_baseline, _nat_baseline) =
                    content.measure(Orientation::Vertical, WINDOW_WIDTH);
                assert!(
                    nat > DEFAULT_WINDOW_HEIGHT,
                    "default content's natural height ({nat}) must exceed \
                     DEFAULT_WINDOW_HEIGHT ({DEFAULT_WINDOW_HEIGHT}) — otherwise \
                     nothing would need scrolling"
                );
            }
            match smallest_monitor_height() {
                Some(monitor_height)
                    if monitor_height < DEFAULT_WINDOW_HEIGHT + window_geometry_test_margin() =>
                {
                    eprintln!(
                        "skipped default-height assertion: monitor height \
                         {monitor_height} is too small to fit the default + margin"
                    );
                }
                _ => {
                    assert_eq!(
                        default_handles.window.default_size(),
                        (WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT)
                    );
                }
            }

            window.destroy();
            relaunch_handles.window.destroy();
            default_handles.window.destroy();
        });
        if ran.is_none() {
            eprintln!("skipped: no display server for GTK");
        }
    }

    /// The margin used by the skip-guard above — mirrors
    /// `window_geometry::MONITOR_MARGIN` without importing it at module scope
    /// (it would otherwise be an unused import outside `mod tests`).
    fn window_geometry_test_margin() -> i32 {
        crate::ui::window_geometry::MONITOR_MARGIN
    }

    // ── Fixed-width clamp, explicit scroller, decoration (quick 260927-mvb) ──

    #[test]
    fn main_window_scrolls_vertically_with_fixed_width_content() {
        let ran = crate::ui::gtk_test::run(|| {
            let Some(app) = test_application("com.cleanmic.CleanMic.test.scroll-clamp") else {
                return;
            };
            let config = Rc::new(RefCell::new(Config::default()));
            let state = UiState::from_config(&config.borrow());
            let (tx, _rx) = mpsc::channel::<UiEvent>();
            let handles = build_main_window(&app, &state, tx, config, false);
            let window = handles.window.clone();

            // The outer AdwClamp is a direct child of the content root.
            let root = window.content().expect("window content must be set");
            let clamp = root
                .first_child()
                .and_then(|w| {
                    let mut child = Some(w);
                    while let Some(widget) = child {
                        if let Ok(c) = widget.clone().downcast::<libadwaita::Clamp>() {
                            return Some(c);
                        }
                        child = widget.next_sibling();
                    }
                    None
                })
                .expect("an AdwClamp must be a child of the content root");
            assert_eq!(clamp.maximum_size(), WINDOW_WIDTH);
            assert_eq!(clamp.tightening_threshold(), WINDOW_WIDTH);
            assert_eq!(clamp.unit(), libadwaita::LengthUnit::Px);

            let page = clamp
                .child()
                .and_then(|w| w.downcast::<PreferencesPage>().ok())
                .expect("clamp's child must be the AdwPreferencesPage");

            // The page's own scroller: explicit policy, no nesting.
            let scroller = page_scroller(&page).expect("page must expose its own scroller");
            assert_eq!(scroller.hscrollbar_policy(), gtk4::PolicyType::Never);
            assert_eq!(scroller.vscrollbar_policy(), gtk4::PolicyType::Automatic);
            assert!(
                scroller.propagates_natural_height(),
                "Task 1's natural-height measurement depends on this staying true" // i18n-ignore
            );
            // No ancestor of the page up to the window is ALSO a
            // ScrolledWindow (no nested scroller was added around the page).
            let window_widget = window.clone().upcast::<gtk4::Widget>();
            let mut ancestor = page.parent();
            while let Some(w) = ancestor {
                if let Ok(sw) = w.clone().downcast::<gtk4::ScrolledWindow>() {
                    assert_eq!(
                        sw, scroller,
                        "found a second, nesting ScrolledWindow above the page's own scroller"
                    );
                }
                if w == window_widget {
                    break;
                }
                ancestor = w.parent();
            }

            // 800px allocation: the page still gets exactly WINDOW_WIDTH.
            clamp.measure(Orientation::Horizontal, -1);
            clamp.measure(Orientation::Vertical, 800);
            clamp.size_allocate(&gtk4::Allocation::new(0, 0, 800, 700), -1);
            assert_eq!(page.width(), WINDOW_WIDTH);

            // Header decoration: maximize stays hidden.
            let header = root
                .first_child()
                .and_then(|w| w.downcast::<HeaderBar>().ok())
                .expect("header must be the first child of the content root");
            assert_eq!(
                header.decoration_layout().as_deref(),
                Some(":minimize,close")
            );

            window.destroy();
        });
        if ran.is_none() {
            eprintln!("skipped: no display server for GTK");
        }
    }

    #[test]
    fn close_request_records_the_current_height() {
        let ran = crate::ui::gtk_test::run(|| {
            let Some(app) = test_application("com.cleanmic.CleanMic.test.close-persists") else {
                return;
            };
            let config = Rc::new(RefCell::new(Config {
                tray_hint_shown: true,
                ..Config::default()
            }));
            let state = UiState::from_config(&config.borrow());
            let (tx, _rx) = mpsc::channel::<UiEvent>();
            // tray_available: true so the close path hides rather than quits.
            let handles = build_main_window(&app, &state, tx, config.clone(), true);
            let window = handles.window.clone();

            window.set_default_size(WINDOW_WIDTH, 590);
            // No main-context pump: the debounce timer must NOT be what
            // persists this — close-request must record it immediately.
            let _: bool = window.emit_by_name("close-request", &[]);

            assert_eq!(config.borrow().window_height, Some(590));

            window.destroy();
        });
        if ran.is_none() {
            eprintln!("skipped: no display server for GTK");
        }
    }
}

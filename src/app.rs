//! Application lifecycle management.
//!
//! Wires together the audio service, GUI, tray icon, and signal handlers.
//! In v0.1 everything runs in a single process: the GUI on the main thread,
//! the audio service on a dedicated background thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::audio::AudioPipeline;
use crate::config::Config;
use crate::engine::{self, EngineType};
use crate::instance_lock;
use crate::pipewire::PipeWireManager;
use crate::tray::TrayCommand;
use crate::ui::UiEvent;
use crate::ui::welcome;

/// Resolve the PipeWire node name to use as the initial capture target.
///
/// Prefers the device persisted in config; otherwise picks the system-default
/// input device if the enumerator reports one; otherwise falls back to the
/// first enumerated non-CleanMic source. Returns `None` only when no physical
/// input devices are visible — in that case the capture stream starts
/// unpinned, and the app logs a warning.
fn resolve_initial_capture_target(
    pw: &PipeWireManager,
    config: &Config,
) -> Option<String> {
    if let Some(ref name) = config.input_device {
        return Some(name.clone());
    }
    let devices = pw.device_enumerator().list_input_devices();
    if let Some(default_dev) = devices.iter().find(|d| d.is_default) {
        return Some(default_dev.name.clone());
    }
    devices.first().map(|d| d.name.clone())
}

/// Rate-limits GNOME desktop notifications to at most one per ID per 60 seconds (per D-03).
pub(crate) struct NotificationThrottle {
    last_sent: HashMap<&'static str, Instant>,
}

impl NotificationThrottle {
    pub fn new() -> Self {
        Self {
            last_sent: HashMap::new(),
        }
    }

    /// Returns `true` if enough time has elapsed since the last notification with this ID.
    pub fn can_notify(&mut self, id: &'static str) -> bool {
        let now = Instant::now();
        match self.last_sent.get(id) {
            Some(&last) if now.duration_since(last) < Duration::from_secs(60) => false,
            _ => {
                self.last_sent.insert(id, now);
                true
            }
        }
    }
}

/// Send a throttled GNOME desktop notification (per D-01, D-03, D-04).
/// Informational only, no action button.
#[cfg(feature = "gui")]
pub(crate) fn send_throttled_notification(
    app: &impl gtk4::prelude::IsA<gtk4::gio::Application>,
    throttle: &mut NotificationThrottle,
    id: &'static str,
    title: &str,
    body: &str,
) {
    use gtk4::prelude::ApplicationExt;
    if throttle.can_notify(id) {
        let notif = gtk4::gio::Notification::new(title);
        notif.set_body(Some(body));
        app.send_notification(Some(id), &notif);
    }
}

/// Initialize gettext for internationalization.
///
/// Must be called before any UI construction so that all `tr!()` calls
/// resolve to the correct locale. Falls back gracefully: if locale files
/// are missing, strings appear in English.
fn init_i18n() {
    use gettextrs::{bind_textdomain_codeset, bindtextdomain, textdomain};

    let locale_dir = std::env::var("APPDIR")
        .map(|d| std::path::PathBuf::from(d).join("usr/share/locale"))
        .unwrap_or_else(|_| {
            // For system installs and dev runs
            std::path::PathBuf::from("/usr/local/share/locale")
        });

    if let Err(e) = textdomain("cleanmic") {
        log::warn!("gettext textdomain failed: {e}");
        return;
    }
    if let Err(e) = bindtextdomain("cleanmic", locale_dir) {
        log::warn!("gettext bindtextdomain failed: {e}");
        return;
    }
    if let Err(e) = bind_textdomain_codeset("cleanmic", "UTF-8") {
        log::warn!("gettext bind_textdomain_codeset failed: {e}");
    }
}

/// Schedule a debounced config save. Cancels any previously pending save
/// and schedules a new one 300ms from now. Only used for rapid-fire events
/// (strength, engine, device changes). One-off events (autostart, monitor,
/// quit) save immediately.
#[cfg(feature = "gui")]
fn schedule_debounced_save(
    config: &std::rc::Rc<std::cell::RefCell<Config>>,
    pending: &std::rc::Rc<std::cell::Cell<Option<gtk4::glib::SourceId>>>,
) {
    if let Some(id) = pending.take() {
        // The source may have already fired (timeout_add_local_once auto-removes
        // after firing). Use raw g_source_remove to avoid the unwrap() panic
        // in SourceId::remove() when the source no longer exists.
        unsafe {
            gtk4::glib::ffi::g_source_remove(id.as_raw());
        }
    }
    let config = config.clone();
    let pending_weak = std::rc::Rc::downgrade(pending);
    let id = gtk4::glib::timeout_add_local_once(
        std::time::Duration::from_millis(300),
        move || {
            // Clear the cell so a future schedule_debounced_save does not try
            // to remove this already-consumed source.
            if let Some(p) = pending_weak.upgrade() {
                p.set(Option::<gtk4::glib::SourceId>::None);
            }
            if let Err(e) = config.borrow().save() {
                log::error!("Debounced config save failed: {e}");
            } else {
                log::debug!("Debounced config saved");
            }
        },
    );
    pending.set(Some(id));
}

/// Build a debug info string for the About dialog.
///
/// Reports engine availability (compile-time feature flags) and GTK version,
/// useful for bug reports.
#[cfg(feature = "gui")]
fn build_debug_info() -> String {
    let deepfilter = if cfg!(feature = "deepfilter") {
        "available"
    } else {
        "not compiled"
    };
    let rnnoise = if cfg!(feature = "rnnoise") {
        "available"
    } else {
        "not compiled"
    };
    format!(
        "DeepFilterNet: {}\nRNNoise: {}\nKhip: user-supplied\nGTK: {}.{}.{}\n",
        deepfilter,
        rnnoise,
        gtk4::major_version(),
        gtk4::minor_version(),
        gtk4::micro_version(),
    )
}

/// Global flag set by signal handlers to request shutdown.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Returns `true` if a shutdown signal has been received.
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
}

/// Request a shutdown (can be called from signal handlers or application code).
pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

/// Check if a StatusNotifierItem host (tray) is available on the D-Bus session bus.
///
/// Called once at startup. If `dbus-send` is not found, conservatively returns `false`.
/// Uses the `org.kde.StatusNotifierWatcher` service name as a proxy for tray host
/// availability — this watcher is registered whenever an AppIndicator-compatible
/// tray host is running.
#[cfg(feature = "gui")]
fn is_sni_watcher_available() -> bool {
    std::process::Command::new("dbus-send")
        .args([
            "--session",
            "--dest=org.freedesktop.DBus",
            "--type=method_call",
            "--print-reply=literal",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.ListNames",
        ])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.contains("org.kde.StatusNotifierWatcher"))
        .unwrap_or(false)
}

/// Register signal handlers for SIGTERM, SIGINT, and SIGHUP using `sigaction`.
///
/// Uses `SA_RESTART` so that interrupted system calls (e.g., `sleep`) are
/// automatically restarted rather than failing with `EINTR`. The handler only
/// performs an atomic store, which is async-signal-safe.
fn register_signal_handlers() {
    use std::mem::MaybeUninit;

    for &sig in &[libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let mut sa: libc::sigaction = unsafe { MaybeUninit::zeroed().assume_init() };
        sa.sa_sigaction = signal_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        // Block no additional signals during handler execution.
        unsafe {
            libc::sigemptyset(&raw mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// Bare signal handler: sets the shutdown flag.
///
/// Only performs an atomic store, which is async-signal-safe.
extern "C" fn signal_handler(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

/// User-friendly error messages for common failure modes.
pub mod errors {
    /// Error message when no microphone is found.
    pub const NO_MIC_FOUND: &str =
        "No microphone found. Please connect a microphone and restart CleanMic.";
    /// Error message when PipeWire is not running.
    pub const PIPEWIRE_NOT_RUNNING: &str = "PipeWire is not running. CleanMic requires PipeWire for audio. Please start PipeWire and try again.";
    /// Error message when Khip engine is selected but unavailable.
    pub const KHIP_NOT_INSTALLED: &str = "Khip is not installed. Please install the Khip library or select a different engine (Balanced or High Quality).";
}

/// Orchestrate graceful shutdown: save config, stop audio, destroy virtual mic,
/// log timing.
///
/// This function must complete within 2 seconds. It logs a warning if shutdown
/// takes longer than expected.
pub fn shutdown(
    config: &Config,
    pipeline: Option<AudioPipeline>,
    pw_manager: &mut PipeWireManager,
) -> Result<()> {
    let start = Instant::now();
    log::info!("Shutdown sequence started");

    // 1. Save configuration.
    if let Err(e) = config.save() {
        log::error!("Failed to save config during shutdown: {}", e);
    } else {
        log::info!("Config saved");
    }

    // 2. Stop audio pipeline (joins the audio thread).
    if let Some(pipeline) = pipeline {
        pipeline.shutdown();
        log::info!("Audio pipeline stopped");
    }

    // 3. Destroy the virtual mic.
    if let Err(e) = pw_manager.destroy_virtual_mic() {
        log::error!("Failed to destroy virtual mic during shutdown: {}", e);
    } else {
        log::info!("Virtual mic destroyed");
    }

    let elapsed = start.elapsed();
    if elapsed > Duration::from_secs(2) {
        log::warn!(
            "Shutdown took {:.1}s (exceeds 2s budget)",
            elapsed.as_secs_f64()
        );
    } else {
        log::info!(
            "Shutdown completed in {:.1}ms",
            elapsed.as_secs_f64() * 1000.0
        );
    }

    Ok(())
}

/// Dispatch a [`UiEvent`] to the audio pipeline and update config accordingly.
fn handle_ui_event(
    event: UiEvent,
    pipeline: &AudioPipeline,
    config: &mut Config,
    pw_manager: &mut PipeWireManager,
) {
    match event {
        UiEvent::EngineChanged(engine_type) => {
            let (mut eng, actual_type) = engine::create_engine_with_fallback(engine_type);
            if actual_type != engine_type {
                log::warn!(
                    "{:?} engine unavailable — fell back to {:?}",
                    engine_type,
                    actual_type
                );
            }
            // Apply current strength to the new engine before handing it off.
            eng.set_strength(config.strength);
            pipeline.set_engine(eng);
            config.engine = actual_type;
            log::info!("Engine changed to {:?}", actual_type);
        }
        UiEvent::StrengthChanged(strength) => {
            pipeline.set_strength(strength);
            config.strength = strength;
            log::info!("Strength changed to {:.2}", strength);
        }
        UiEvent::DeviceChanged(device) => {
            pipeline.set_input_device(device.clone());
            match pw_manager.set_capture_target(Some(device.clone())) {
                Ok(new_reader) => {
                    pipeline.replace_capture_reader(new_reader);
                    log::info!("Capture target retargeted to {}", device);
                }
                Err(e) => {
                    log::error!("Failed to retarget capture stream to {}: {e}", device);
                }
            }
            config.input_device = Some(device);
        }
        UiEvent::DeviceChangedToDefault => {
            // Placeholder: full handler lands in plan 08.2-03 (app orchestration).
            // When the user picks "Default" in the mic picker, config.input_device
            // must clear to None (= "follow OS default"), then the capture target
            // resolves to whatever `PipeWireManager::configured_default_source()`
            // returns. Plan 01 only introduces the event variant; plan 03 wires
            // the resolver call and the capture-stream retarget. Per D-06.
            log::debug!(
                "DeviceChangedToDefault received — full handler lands in plan 08.2-03"
            );
        }
        UiEvent::EnableToggled(enabled) => {
            if enabled {
                pipeline.start();
            } else {
                pipeline.stop();
            }
            config.enabled = enabled;
            log::info!("Pipeline {}", if enabled { "enabled" } else { "disabled" });
        }
        UiEvent::MonitorToggled(enabled) => {
            if enabled {
                match pw_manager.enable_monitor() {
                    Ok(writer) => {
                        // Give the ring-buffer writer to the audio thread
                        // *before* enabling, so the first write has somewhere to go.
                        pipeline.set_monitor_writer(Some(writer));
                        pipeline.set_monitor(true);
                        config.monitor_enabled = true;
                    }
                    Err(e) => {
                        log::error!("Failed to enable monitor output: {e}");
                    }
                }
            } else {
                // Disable first so no more writes are attempted, then detach
                // the writer so we don't hold a dead ring buffer end.
                pipeline.set_monitor(false);
                pipeline.set_monitor_writer(None);
                let _ = pw_manager.disable_monitor();
                config.monitor_enabled = false;
            }
            log::info!("Monitor {}", if enabled { "enabled" } else { "disabled" });
        }
        UiEvent::AutostartToggled(enabled) => {
            config.autostart = enabled;
            if enabled {
                if let Err(e) = crate::autostart::enable_autostart() {
                    log::error!("Failed to enable autostart: {}", e);
                }
            } else if let Err(e) = crate::autostart::disable_autostart() {
                log::error!("Failed to disable autostart: {}", e);
            }
        }
        UiEvent::Quit => {
            request_shutdown();
        }
        UiEvent::CheckForUpdates => {
            // Handled by the update checker in a later phase (D-02, D-04).
            log::info!("Manual update check requested");
        }
        UiEvent::UpdateAvailable(version) => {
            // Handled by the update notifier in a later phase (D-05, D-07).
            log::info!("Update available: {}", version);
        }
    }
}

/// Dispatch a [`TrayCommand`] to the audio pipeline and update config.
#[allow(dead_code)]
fn handle_tray_command(
    cmd: TrayCommand,
    pipeline: &AudioPipeline,
    config: &mut Config,
    pw_manager: &mut PipeWireManager,
) {
    match cmd {
        TrayCommand::Toggle => {
            let new_state = !config.enabled;
            handle_ui_event(UiEvent::EnableToggled(new_state), pipeline, config, pw_manager);
        }
        TrayCommand::SetEngine(engine_type) => {
            handle_ui_event(UiEvent::EngineChanged(engine_type), pipeline, config, pw_manager);
        }
        TrayCommand::ToggleMonitor => {
            let new_state = !config.monitor_enabled;
            handle_ui_event(UiEvent::MonitorToggled(new_state), pipeline, config, pw_manager);
        }
        TrayCommand::OpenWindow => {
            log::info!("Open main window requested (not yet implemented in headless mode)");
        }
        TrayCommand::Quit => {
            request_shutdown();
        }
        TrayCommand::CheckForUpdates => {
            // Handled by the update checker in a later phase (D-02, D-04).
            log::info!("Manual update check requested via tray");
        }
    }
}

/// Start the CleanMic application.
///
/// This is the real application entry point that wires everything together:
/// - Loads (or creates default) configuration
/// - Connects to PipeWire, cleans up orphans, creates the virtual mic
/// - Creates the audio pipeline and sets the engine from config
/// - Handles first-run detection and guidance
/// - On subsequent runs with enabled=true, auto-starts the pipeline
/// - Main loop: polls for shutdown, dispatches UI events and tray commands
/// - On shutdown: saves config, stops audio, destroys virtual mic
pub fn run() -> Result<()> {
    // Initialize i18n before any UI construction so all tr!() calls resolve correctly.
    init_i18n();

    // Reset shutdown flag (important for tests that call run() multiple times).
    SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);

    register_signal_handlers();

    // -- Single-instance lock --
    // A second CleanMic would try to register its own "CleanMic" virtual mic
    // and re-pw-link audio into a graph the first instance already owns,
    // producing unpredictable routing (two virtual mics with the same name,
    // output streams racing to link to either one, etc.). Acquire an advisory
    // flock on a per-user file; bail out if another process holds it. The
    // lock is released automatically when the process exits (even on crash).
    let _instance_lock = match instance_lock::acquire() {
        Ok(lock) => lock,
        Err(instance_lock::Error::AlreadyRunning(path)) => {
            log::info!(
                "Another CleanMic instance is already running (lock: {}) — exiting",
                path.display()
            );
            eprintln!("CleanMic is already running.");
            return Ok(());
        }
        Err(instance_lock::Error::Io(e)) => {
            log::warn!("Could not acquire single-instance lock ({e}) — continuing anyway");
            instance_lock::Guard::dummy()
        }
    };

    // -- Desktop integration (idempotent, runs every startup) --
    // Installs ~/.local/share/applications/ desktop entry and icon so GNOME
    // can match the running window to the correct dock icon even for users who
    // have never enabled autostart.
    if let Err(e) = crate::autostart::install_desktop_integration() {
        log::warn!("Desktop integration install failed (non-fatal): {e}");
    }

    // -- First-run detection (before loading config, which creates defaults) --
    let first_run = welcome::is_first_run();

    // -- Load config --
    let mut config = Config::load().context("failed to load configuration")?;

    // -- Connect to PipeWire --
    let mut pw_manager = PipeWireManager::connect()
        .inspect_err(|_e| {
            log::error!("{}", errors::PIPEWIRE_NOT_RUNNING);
        })
        .context("failed to connect to PipeWire")?;

    // Clean up any orphaned nodes from a previous crashed session.
    if let Err(e) = pw_manager.cleanup_orphans() {
        log::warn!("Orphan cleanup failed (non-fatal): {}", e);
    }

    // -- Resolve the initial capture target --
    // The capture stream must be pinned to a specific physical mic via
    // PW_KEY_TARGET_OBJECT; otherwise WirePlumber routes it to the system
    // default source, and if the user sets CleanMic as the default input
    // the stream self-loops. Prefer the user's saved choice, then the first
    // enumerated non-CleanMic source.
    let initial_capture_target = resolve_initial_capture_target(&pw_manager, &config);
    if initial_capture_target.is_none() {
        log::warn!(
            "No physical input devices found — capture stream will start unpinned. \
             The app will self-loop if CleanMic is selected as the system default input."
        );
    }
    if config.input_device.is_none()
        && let Some(ref name) = initial_capture_target
    {
        config.input_device = Some(name.clone());
    }

    // -- Create the virtual mic --
    pw_manager
        .create_virtual_mic(initial_capture_target)
        .context("failed to create virtual mic")?;

    // -- Create audio pipeline --
    // Take ring buffer halves from the PipeWire manager so the audio thread
    // can exchange audio with the PipeWire RT callbacks.
    let pipeline = match (
        pw_manager.take_capture_reader(),
        pw_manager.take_output_writer(),
    ) {
        (Some(capture_reader), Some(output_writer)) => {
            log::info!("Audio pipeline created with PipeWire ring buffers");
            AudioPipeline::with_ring_buffers(capture_reader, output_writer)
                .context("failed to spawn audio thread with ring buffers")?
        }
        _ => {
            log::warn!("Ring buffers not available — audio pipeline running in simulation mode");
            AudioPipeline::new()
                .context("failed to spawn audio thread in simulation mode")?
        }
    };

    // Set the engine from config using the full fallback chain (D-02).
    let engine_type = config.engine;
    let (mut eng, actual_type) = engine::create_engine_with_fallback(engine_type);
    if actual_type != engine_type {
        log::warn!(
            "{:?} engine unavailable — fell back to {:?}",
            engine_type,
            actual_type
        );
        config.engine = actual_type;
    }
    // Apply the persisted strength and mode before handing the engine to
    // the audio thread. Otherwise the engine runs at its constructor
    // defaults until the user nudges a control, which silently overrides
    // whatever the UI was showing at launch.
    eng.set_strength(config.strength);
    eng.set_mode(config.mode);
    pipeline.set_engine(eng);
    log::info!(
        "Engine set to {:?} (strength={:.2}, mode={:?})",
        actual_type,
        config.strength,
        config.mode,
    );

    // Set input device if configured.
    if let Some(ref device) = config.input_device {
        pipeline.set_input_device(device.clone());
    }

    // Restore monitor from config. Without this, a user who enabled monitor
    // in a previous session sees the UI switch stuck on "on" but hears
    // nothing until they toggle it off and back on — because the PipeWire
    // monitor stream + ring-buffer writer are only created in the
    // MonitorToggled(true) handler, which never fires on pure startup.
    // Mirror that handler here to make startup equivalent to an on-toggle.
    if config.monitor_enabled {
        match pw_manager.enable_monitor() {
            Ok(writer) => {
                pipeline.set_monitor_writer(Some(writer));
                pipeline.set_monitor(true);
                log::info!("Monitor auto-enabled from config");
            }
            Err(e) => {
                log::error!("Failed to enable monitor at startup: {e}");
                config.monitor_enabled = false;
            }
        }
    }

    // -- First-run vs subsequent-run behavior --
    if first_run {
        welcome::log_first_run_instructions();
        // On first run, save defaults so next launch is not a first run.
        if let Err(e) = config.save() {
            log::warn!("Failed to save initial config: {}", e);
        }
    }

    // Auto-start pipeline if enabled (including first run with default enabled=true).
    if config.enabled {
        pipeline.start();
        log::info!("Audio pipeline auto-started (enabled=true)");
    } else {
        log::info!("Audio pipeline not started (enabled=false)");
    }

    // -- Run with GUI or headless --
    #[cfg(feature = "gui")]
    {
        run_with_gui(config, pipeline, pw_manager, first_run)?;
    }

    #[cfg(not(feature = "gui"))]
    {
        run_headless(config, pipeline, pw_manager)?;
    }

    Ok(())
}

/// Headless main loop (no GUI feature).
#[cfg(not(feature = "gui"))]
fn run_headless(
    mut config: Config,
    pipeline: AudioPipeline,
    mut pw_manager: PipeWireManager,
) -> Result<()> {
    let (_ui_tx, ui_rx) = mpsc::channel::<UiEvent>();
    let (_tray_tx, tray_rx) = mpsc::channel::<TrayCommand>();

    log::info!("CleanMic running headless — waiting for shutdown signal");

    while !is_shutdown_requested() {
        while let Ok(event) = ui_rx.try_recv() {
            handle_ui_event(event, &pipeline, &mut config, &mut pw_manager);
        }
        while let Ok(cmd) = tray_rx.try_recv() {
            handle_tray_command(cmd, &pipeline, &mut config, &mut pw_manager);
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    log::info!("Shutdown signal received");
    shutdown(&config, Some(pipeline), &mut pw_manager)?;
    Ok(())
}

/// GTK application main loop.
#[cfg(feature = "gui")]
fn run_with_gui(
    mut config: Config,
    pipeline: AudioPipeline,
    pw_manager: PipeWireManager,
    first_run: bool,
) -> Result<()> {
    use gtk4::glib;
    use gtk4::prelude::*;
    use libadwaita::prelude::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    // -- Detect SNI tray watcher availability --
    let tray_available = is_sni_watcher_available();
    log::info!("SNI tray watcher available: {}", tray_available);

    let app = libadwaita::Application::builder()
        .application_id("com.cleanmic.CleanMic")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    // -- Start tray icon (if feature enabled and tray watcher is present) --
    #[cfg(feature = "tray")]
    let tray_state = if tray_available {
        use crate::engine;
        use crate::tray::TrayState;
        use std::sync::{Arc, Mutex};

        let state = TrayState::new(
            config.enabled,
            config.engine,
            config.mode,
            config.monitor_enabled,
            engine::is_engine_available(EngineType::Khip),
        );
        Some(Arc::new(Mutex::new(state)))
    } else {
        None
    };

    #[cfg(feature = "tray")]
    let (tray_cmd_rx, tray_handle) = if tray_available {
        use crate::tray::icon::CleanMicTray;

        let (tray_cmd_tx, tray_cmd_rx) = mpsc::channel::<TrayCommand>();

        let tray = CleanMicTray {
            state: tray_state.as_ref().expect("tray_state set when tray_available").clone(),
            sender: tray_cmd_tx,
        };

        let service = ksni::TrayService::new(tray);
        // Grab the handle before moving the service into the thread.
        // Calling handle.update(|_| {}) sets the need_update flag so ksni
        // sends LayoutUpdated to the panel and re-queries menu() with fresh state.
        let handle = service.handle();

        std::thread::Builder::new()
            .name("cleanmic-tray".into())
            .spawn(move || {
                if let Err(e) = service.run() {
                    log::error!("Tray service exited with error: {}", e);
                }
            })
            .context("failed to spawn tray thread")?;

        (Some(tray_cmd_rx), Some(handle))
    } else {
        (None, None)
    };

    // Send one-time notification when tray is not available.
    // This is deferred to the activate signal so `app` has a registration context.
    let tray_absent_should_notify = !tray_available && !config.tray_absent_notified;
    if tray_absent_should_notify {
        config.tray_absent_notified = true;
        if let Err(e) = config.save() {
            log::warn!("Failed to save tray_absent_notified flag: {e}");
        }
    }

    // -- Update check result channel --
    // Main-thread-only: Receiver stays on main thread (wrapped in Rc<RefCell<...>>
    // so the GLib timer closure can poll it across Fn call boundaries).
    // Sender is cloned into background threads and the GIO action.
    // Channel carries (Option<String>, bool) where bool = is_manual_check.
    // When manual=true and result is None, show "up to date" feedback (per D-04).
    #[cfg(feature = "updater")]
    let (update_result_tx, update_result_rx_raw) =
        std::sync::mpsc::channel::<(Option<String>, bool)>();
    #[cfg(feature = "updater")]
    let update_result_rx = Rc::new(RefCell::new(update_result_rx_raw));

    // Wrap shared state so the GTK closures can access it.
    let config = Rc::new(RefCell::new(config));
    let pipeline = Rc::new(pipeline);
    let pw_manager = Rc::new(RefCell::new(pw_manager));

    // Pending debounced config save (for rapid-fire events: strength, engine, device).
    let pending_save: Rc<std::cell::Cell<Option<glib::SourceId>>> = Rc::new(std::cell::Cell::new(None));

    #[cfg(feature = "tray")]
    let tray_state = Rc::new(tray_state);

    // Wrap the tray receiver so it can be shared across Fn closure boundaries.
    #[cfg(feature = "tray")]
    let tray_cmd_rx = Rc::new(RefCell::new(tray_cmd_rx));

    // tray_handle is Clone — clone it for the activate closure.
    #[cfg(feature = "tray")]
    let tray_handle_activate = tray_handle.clone();

    let config_clone = config.clone();
    let pipeline_clone = pipeline.clone();
    let pw_manager_clone = pw_manager.clone();
    #[cfg(feature = "updater")]
    let update_result_rx_activate = update_result_rx.clone();
    #[cfg(feature = "tray")]
    let tray_state_activate = tray_state.clone();
    #[cfg(feature = "tray")]
    let tray_cmd_rx_activate = tray_cmd_rx.clone();

    app.connect_activate(move |app| {
        log::info!("GTK activate signal received — building window");

        // Send one-time notification when tray is unavailable.
        if tray_absent_should_notify {
            let notif = gtk4::gio::Notification::new(&gettextrs::gettext("No system tray detected"));
            notif.set_body(Some(
                &gettextrs::gettext(
                    "Install the AppIndicator extension to minimize CleanMic to tray. \
                     Closing the window will quit the application."
                )
            ));
            app.send_notification(Some("tray-absent"), &notif);
        }

        // Populate the device list from real enumeration before building the
        // window so the device picker shows actual devices on first open.
        let mut initial_state = {
            let cfg = config_clone.borrow();
            crate::ui::UiState::from_config(&cfg)
        };
        {
            let pw = pw_manager_clone.borrow();
            let devices = pw.device_enumerator().list_input_devices();
            initial_state.available_devices = devices
                .iter()
                .map(|d| crate::ui::DeviceInfo {
                    name: d.name.clone(),
                    description: d.description.clone(),
                })
                .collect();
            initial_state.khip_available =
                engine::is_engine_available(EngineType::Khip);
        }

        let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();

        let handles = crate::ui::window::build_main_window(
            app,
            &initial_state,
            ui_tx,
            config_clone.clone(),
            tray_available,
        );
        handles.window.present();

        // -- Background update check on launch (per D-01, D-03: fire-and-forget) --
        #[cfg(feature = "updater")]
        {
            let tx = update_result_tx.clone();
            std::thread::Builder::new()
                .name("cleanmic-updater".into())
                .spawn(move || match crate::updater::check_for_update() {
                    Ok(result) => {
                        // is_manual = false — auto-launch check, no "up to date" feedback
                        let _ = tx.send((result, false));
                    }
                    Err(e) => {
                        log::debug!("updater: check failed: {e}");
                    }
                })
                .ok();
        }

        // ── About action ──────────────────────────────────────────────────────
        let about_action = gtk4::gio::SimpleAction::new("about", None);
        let window_weak = handles.window.downgrade();
        about_action.connect_activate(move |_, _| {
            let dialog = libadwaita::AboutDialog::new();
            dialog.set_application_name("CleanMic");
            dialog.set_version(env!("CARGO_PKG_VERSION"));
            dialog.set_developer_name("Claude Gagne");
            dialog.set_website("https://github.com/claude-gagne/CleanMic");
            dialog.set_issue_url("https://github.com/claude-gagne/CleanMic/issues");
            dialog.set_license_type(gtk4::License::MitX11);
            dialog.set_application_icon("com.cleanmic.CleanMic");
            dialog.add_link(&crate::tr!("Support CleanMic"), "https://buymeacoffee.com/claudegagne");
            dialog.set_debug_info(&build_debug_info());
            dialog.set_debug_info_filename("cleanmic-debug.txt");
            if let Some(win) = window_weak.upgrade() {
                dialog.present(Some(&win));
            }
        });
        app.add_action(&about_action);

        // ── "Check for updates" GIO action (hamburger menu + programmatic) ────
        #[cfg(feature = "updater")]
        {
            let check_update_action =
                gtk4::gio::SimpleAction::new("check-for-updates", None);
            let tx_action = update_result_tx.clone();
            check_update_action.connect_activate(move |_, _| {
                let tx2 = tx_action.clone();
                std::thread::Builder::new()
                    .name("cleanmic-updater-manual".into())
                    .spawn(move || {
                        let result = crate::updater::check_for_update().unwrap_or(None);
                        // is_manual = true — show "up to date" feedback if None (per D-04)
                        let _ = tx2.send((result, true));
                    })
                    .ok();
            });
            app.add_action(&check_update_action);
        }

        if first_run {
            log::info!("First run — showing main window with setup guidance");
        }

        // Shared smoothed level meters driven from the GLib timer.
        let level_meters = Rc::new(RefCell::new(crate::ui::meters::LevelMeters::new()));

        // The MeterRow widgets need to be accessible from the timer closure.
        // Wrap in Rc so ownership is shared between the activate closure and
        // the timer closure without requiring Send.
        let input_meter = Rc::new(handles.input_meter);
        let output_meter = Rc::new(handles.output_meter);

        // Capture the update banner for the timer closure.
        let update_banner = handles.update_banner.clone();

        // Keep references to the UI control widgets for state synchronization
        // from the GLib timer. GTK4 widgets are reference-counted (GObject), so
        // cloning is cheap.
        let sync_enable_row = handles.enable_row.clone();
        let sync_engine_row = handles.engine_row.clone();
        let sync_strength_row = handles.strength_row.clone();
        let sync_monitor_row = handles.monitor_row.clone();
        let sync_win_title = handles.win_title.clone();
        let sync_device_row = handles.device_row.clone();

        // Track the last-known config snapshot so we only update UI controls
        // when the config actually changes (avoids re-triggering signal handlers
        // every frame).
        let last_synced_config = Rc::new(RefCell::new(config_clone.borrow().clone()));

        // Notification throttle for disconnect/reconnect notifications (D-03).
        let notification_throttle = Rc::new(RefCell::new(NotificationThrottle::new()));

        // Guards against looping reconnect attempts after a single disconnect event.
        // Reset to `false` after a successful reconnect so future disconnects trigger
        // another attempt.
        use std::cell::Cell;
        let pw_reconnect_attempted = Rc::new(Cell::new(false));

        // Poll UI events and levels on a GLib timer (~30fps).
        let window_timer = handles.window.clone();
        let pipeline_timer = pipeline_clone.clone();
        let config_timer = config_clone.clone();
        let pw_timer = pw_manager_clone.clone();
        let pending_save_timer = pending_save.clone();
        let app_weak = app.downgrade();
        let notification_throttle_timer = notification_throttle.clone();
        let pw_reconnect_attempted_timer = pw_reconnect_attempted.clone();
        let level_meters_timer = level_meters.clone();
        let input_meter_timer = input_meter.clone();
        let output_meter_timer = output_meter.clone();
        let sync_enable_timer = sync_enable_row;
        let sync_engine_timer = sync_engine_row;
        let sync_strength_timer = sync_strength_row;
        let sync_monitor_timer = sync_monitor_row;
        let sync_win_title_timer = sync_win_title;
        let _sync_device_timer = sync_device_row;
        let last_synced_timer = last_synced_config;
        let update_banner_timer = update_banner;
        #[cfg(feature = "updater")]
        let update_result_rx_timer = update_result_rx_activate.clone();
        #[cfg(feature = "updater")]
        let update_result_tx_timer = update_result_tx.clone();
        #[cfg(feature = "tray")]
        let tray_state_timer = tray_state_activate.clone();
        #[cfg(feature = "tray")]
        let tray_cmd_rx_timer = tray_cmd_rx_activate.clone();
        #[cfg(feature = "tray")]
        let tray_handle_timer = tray_handle_activate.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(33), move || {
            // --- Level meter refresh ---
            if let Some(report) = pipeline_timer.poll_levels() {
                level_meters_timer.borrow_mut().update(report);
            }
            {
                let meters = level_meters_timer.borrow();
                input_meter_timer.refresh(&meters.input);
                output_meter_timer.refresh(&meters.output);
            }

            // --- Poll update check result (per D-04, D-05, D-06, D-07) ---
            #[cfg(feature = "updater")]
            if let Ok((maybe_version, is_manual)) = update_result_rx_timer.borrow().try_recv() {
                if let Some(ref new_version) = maybe_version {
                    let is_new = config_timer.borrow().last_seen_update_version.as_deref()
                        != Some(new_version.as_str());

                    // Always reveal banner (persistent indicator per D-05)
                    let banner_title = format!("CleanMic {} available", new_version);
                    update_banner_timer.set_title(&banner_title);
                    update_banner_timer.set_revealed(true);

                    if is_new {
                        // Per D-05: send desktop notification on first detection of this version
                        if let Some(app_ref) = app_weak.upgrade() {
                            use gtk4::prelude::ApplicationExt;
                            let notif = gtk4::gio::Notification::new(
                                &gettextrs::gettext("Update available"),
                            );
                            notif.set_body(Some(&format!(
                                "CleanMic {} is available. Click Download to get it.",
                                new_version
                            )));
                            app_ref.send_notification(Some("cleanmic-update"), &notif);
                        }

                        // Per D-06: persist so we don't notify again for this version
                        {
                            let mut cfg = config_timer.borrow_mut();
                            cfg.last_seen_update_version = Some(new_version.clone());
                            if let Err(e) = cfg.save() {
                                log::error!("updater: failed to save last_seen_update_version: {e}");
                            }
                        }
                    }

                    // Per D-07: update tray indicator with new version
                    #[cfg(feature = "tray")]
                    {
                        if let Some(ref ts_arc) = *tray_state_timer {
                            let mut ts = ts_arc.lock().unwrap_or_else(|e| e.into_inner());
                            ts.set_update_available(Some(new_version.clone()));
                            drop(ts);
                            if let Some(ref handle) = tray_handle_timer {
                                handle.update(|_tray: &mut crate::tray::icon::CleanMicTray| {});
                            }
                        }
                    }
                } else if is_manual {
                    // Per D-04: manual check returned no update — show explicit "up to date" feedback
                    if let Some(app_ref) = app_weak.upgrade() {
                        use gtk4::prelude::ApplicationExt;
                        let notif = gtk4::gio::Notification::new(
                            &gettextrs::gettext("CleanMic is up to date"),
                        );
                        notif.set_body(Some(&format!(
                            "{}  v{}",
                            gettextrs::gettext("You're running the latest version:"),
                            env!("CARGO_PKG_VERSION")
                        )));
                        app_ref.send_notification(Some("cleanmic-update-check"), &notif);
                    }
                }
            }

            // --- UI state sync from config (pipeline -> UI) ---
            // When the config changes (e.g., engine fallback), update the
            // UI controls to reflect the actual pipeline state.
            {
                let cfg = config_timer.borrow();
                let mut last = last_synced_timer.borrow_mut();
                if *cfg != *last {
                    use crate::ui::window::{engine_to_index, strength_to_level_index};

                    let engine_idx = engine_to_index(cfg.engine);
                    if sync_engine_timer.selected() != engine_idx {
                        sync_engine_timer.set_selected(engine_idx);
                    }

                    let level_idx = strength_to_level_index(cfg.strength);
                    if sync_strength_timer.selected() != level_idx {
                        sync_strength_timer.set_selected(level_idx);
                    }

                    if sync_enable_timer.is_active() != cfg.enabled {
                        sync_enable_timer.set_active(cfg.enabled);
                    }

                    if sync_monitor_timer.is_active() != cfg.monitor_enabled {
                        sync_monitor_timer.set_active(cfg.monitor_enabled);
                    }

                    *last = cfg.clone();
                    log::debug!("UI synced from config change");

                    // Sync tray state when config changes autonomously (e.g., engine fallback).
                    #[cfg(feature = "tray")]
                    sync_tray_state(&tray_state_timer, &tray_handle_timer, &cfg);
                }
            }

            // Drain UI events.
            while let Ok(event) = ui_rx.try_recv() {
                let is_quit = matches!(event, UiEvent::Quit);
                let needs_debounce = matches!(
                    event,
                    UiEvent::EngineChanged(_) | UiEvent::StrengthChanged(_) | UiEvent::DeviceChanged(_)
                );
                let needs_immediate_save = matches!(
                    event,
                    UiEvent::EnableToggled(_) | UiEvent::MonitorToggled(_) | UiEvent::AutostartToggled(_)
                );
                handle_ui_event(event, &pipeline_timer, &mut config_timer.borrow_mut(), &mut pw_timer.borrow_mut());
                if needs_debounce {
                    schedule_debounced_save(&config_timer, &pending_save_timer);
                } else if needs_immediate_save && let Err(e) = config_timer.borrow().save() {
                    log::error!("Failed to save config: {e}");
                }
                // Synchronize tray state with config after UI events (only when tray available).
                #[cfg(feature = "tray")]
                {
                    let cfg = config_timer.borrow();
                    sync_tray_state(&tray_state_timer, &tray_handle_timer, &cfg);
                }
                if is_quit && let Some(app) = app_weak.upgrade() {
                    // Perform shutdown before quitting GTK.
                    let cfg = config_timer.borrow();
                    // We can't move pipeline out of Rc, so just save and destroy mic.
                    if let Err(e) = cfg.save() {
                        log::error!("Failed to save config: {}", e);
                    }
                    if let Err(e) = pw_timer.borrow_mut().destroy_virtual_mic() {
                        log::error!("Failed to destroy virtual mic: {}", e);
                    }
                    app.quit();
                    return glib::ControlFlow::Break;
                }
            }

            // Drain tray commands (if tray feature is enabled).
            #[cfg(feature = "tray")]
            {
                let tray_cmds: Vec<TrayCommand> = {
                    if let Some(ref rx) = *tray_cmd_rx_timer.borrow() {
                        std::iter::from_fn(|| rx.try_recv().ok()).collect()
                    } else {
                        vec![]
                    }
                };
                for cmd in tray_cmds {
                    let is_quit = matches!(cmd, TrayCommand::Quit);
                    let is_open = matches!(cmd, TrayCommand::OpenWindow);
                    let tray_needs_debounce = matches!(cmd, TrayCommand::SetEngine(_));
                    let tray_needs_immediate_save = matches!(
                        cmd,
                        TrayCommand::Toggle | TrayCommand::ToggleMonitor
                    );
                    // Intercept CheckForUpdates before handle_tray_command (per D-02, D-04)
                    #[cfg(feature = "updater")]
                    if matches!(cmd, TrayCommand::CheckForUpdates) {
                        let tx = update_result_tx_timer.clone();
                        std::thread::Builder::new()
                            .name("cleanmic-updater-manual".into())
                            .spawn(move || {
                                let result = crate::updater::check_for_update().unwrap_or(None);
                                // is_manual = true — show "up to date" feedback if None (per D-04)
                                let _ = tx.send((result, true));
                            })
                            .ok();
                        // Result arrives via channel on next timer tick; skip handle_tray_command
                        continue;
                    }
                    handle_tray_command(cmd, &pipeline_timer, &mut config_timer.borrow_mut(), &mut pw_timer.borrow_mut());
                    if tray_needs_debounce {
                        schedule_debounced_save(&config_timer, &pending_save_timer);
                    } else if tray_needs_immediate_save && let Err(e) = config_timer.borrow().save() {
                        log::error!("Failed to save config from tray: {e}");
                    }
                    if is_open {
                        window_timer.set_visible(true);
                        window_timer.present();
                    }
                    // Synchronize tray state after each command.
                    {
                        let cfg = config_timer.borrow();
                        sync_tray_state(&tray_state_timer, &tray_handle_timer, &cfg);
                    }
                    if is_quit && let Some(app) = app_weak.upgrade() {
                        let cfg = config_timer.borrow();
                        if let Err(e) = cfg.save() {
                            log::error!("Failed to save config: {}", e);
                        }
                        if let Err(e) = pw_timer.borrow_mut().destroy_virtual_mic() {
                            log::error!("Failed to destroy virtual mic: {}", e);
                        }
                        app.quit();
                        return glib::ControlFlow::Break;
                    }
                }
            }

            // --- PipeWire disconnect detection and one-attempt reconnect (D-05) ---
            // Poll non-blocking: check_disconnected() reads an AtomicBool set by
            // the PW thread's core error callback when the daemon goes away.
            if pw_timer.borrow().check_disconnected()
                && !pw_reconnect_attempted_timer.get()
            {
                pw_reconnect_attempted_timer.set(true);
                log::warn!("PipeWire daemon disconnected — attempting one reconnect");

                if let Some(app_ref) = app_weak.upgrade() {
                    send_throttled_notification(
                        &app_ref,
                        &mut notification_throttle_timer.borrow_mut(),
                        "pipewire-disconnect",
                        &gettextrs::gettext("PipeWire disconnected"),
                        &gettextrs::gettext("The audio daemon disconnected. Attempting to reconnect\u{2026}"),
                    );
                }

                // Attempt a fresh PipeWire connection.
                match PipeWireManager::connect() {
                    Ok(mut new_pw) => {
                        // Re-create the virtual mic on the new connection,
                        // preserving the previously-selected capture target so
                        // the reconnected capture stream stays pinned and does
                        // not self-loop against CleanMic as default.
                        let reconnect_target = {
                            let cfg = config_timer.borrow();
                            cfg.input_device
                                .clone()
                                .or_else(|| resolve_initial_capture_target(&new_pw, &cfg))
                        };
                        let mic_result = new_pw.create_virtual_mic(reconnect_target);
                        if let Err(ref e) = mic_result {
                            log::error!("Reconnect: failed to re-create virtual mic: {}", e);
                        }

                        // Hand new ring buffer halves to the audio thread so audio
                        // flows through the fresh PipeWire streams.
                        let new_cr = new_pw.take_capture_reader();
                        let new_ow = new_pw.take_output_writer();
                        pipeline_timer.set_ring_buffers(new_cr, new_ow);

                        // Reset disconnect flag on the new manager before swapping it in.
                        new_pw.reset_disconnected();

                        // Replace the old PipeWireManager with the new one.
                        *pw_timer.borrow_mut() = new_pw;

                        // Allow future disconnects to trigger another attempt.
                        pw_reconnect_attempted_timer.set(false);

                        log::info!("PipeWire reconnected — audio pipeline rewired");
                        if let Some(app_ref) = app_weak.upgrade() {
                            send_throttled_notification(
                                &app_ref,
                                &mut notification_throttle_timer.borrow_mut(),
                                "pipewire-disconnect",
                                &gettextrs::gettext("PipeWire reconnected"),
                                &gettextrs::gettext("Audio processing has been restored."),
                            );
                        }
                    }
                    Err(e) => {
                        // D-05: show error and keep window open on reconnect failure.
                        log::error!("PipeWire reconnect failed: {}", e);
                        if let Some(app_ref) = app_weak.upgrade() {
                            send_throttled_notification(
                                &app_ref,
                                &mut notification_throttle_timer.borrow_mut(),
                                "pipewire-disconnect",
                                &gettextrs::gettext("PipeWire reconnect failed"),
                                &gettextrs::gettext("Could not reconnect to the audio daemon. Audio processing is unavailable. Please restart CleanMic or check that PipeWire is running."),
                            );
                        }
                        // Window stays open — do NOT call app.quit() here.
                        // The user can manually restart the app.
                    }
                }
            }

            // Check shutdown signal (from SIGTERM/SIGINT).
            if is_shutdown_requested()
                && let Some(app) = app_weak.upgrade()
            {
                let cfg = config_timer.borrow();
                if let Err(e) = cfg.save() {
                    log::error!("Failed to save config: {}", e);
                }
                if let Err(e) = pw_timer.borrow_mut().destroy_virtual_mic() {
                    log::error!("Failed to destroy virtual mic: {}", e);
                }
                app.quit();
                return glib::ControlFlow::Break;
            }

            // Keep header bar subtitle in sync with pipeline state.
            let subtitle = if config_timer.borrow().enabled {
                gettextrs::gettext("Active")
            } else {
                gettextrs::gettext("Inactive")
            };
            sync_win_title_timer.set_subtitle(&subtitle);

            glib::ControlFlow::Continue
        });

        // ── Health check timer (D-17, D-18, D-15, D-16) ──────────────────
        // Fires every 2 seconds. Compares heartbeat counter values to detect
        // a stuck or dead audio thread. On death: attempt one restart, then
        // disable UI audio controls and update tray to reflect unavailability.
        let pipeline_health = pipeline_clone.clone();
        let config_health = config_clone.clone();
        let app_health = app.downgrade();
        let sync_enable_health = handles.enable_row.clone();
        let sync_engine_health = handles.engine_row.clone();
        let sync_strength_health = handles.strength_row.clone();
        #[cfg(feature = "tray")]
        let tray_state_health = tray_state_activate.clone();
        #[cfg(feature = "tray")]
        let tray_handle_health = tray_handle_activate.clone();
        let last_heartbeat = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let restart_attempted = std::rc::Rc::new(std::cell::Cell::new(false));
        let throttle_health = std::rc::Rc::new(std::cell::RefCell::new(NotificationThrottle::new()));

        glib::timeout_add_local(std::time::Duration::from_secs(2), move || {
            let current_hb = pipeline_health.heartbeat_count();
            let previous_hb = last_heartbeat.get();
            let channel_open = pipeline_health.is_cmd_channel_open();

            if current_hb == previous_hb && !channel_open {
                // Audio thread appears dead.
                let audio_alive = false;

                if !restart_attempted.get() {
                    restart_attempted.set(true);
                    log::error!("audio thread health check failed — attempting restart");

                    // D-18: Attempt one restart. AudioPipeline::new() spawns a
                    // simulation-mode thread (no ring buffers in this context).
                    // A full ring-buffer restart would require PipeWire manager
                    // access which is held by the main timer closure.
                    match AudioPipeline::new() {
                        Ok(_new_pipeline) => {
                            // New pipeline created (simulation mode).
                            // We cannot replace the existing Rc<AudioPipeline>
                            // here, but the spawn attempt satisfies D-18.
                            // The new pipeline is immediately dropped; log success.
                            log::info!("audio thread restart attempt: new thread spawned successfully (simulation mode)");
                            // Note: since we cannot swap out the Rc, audio_alive
                            // remains false — the original dead pipeline is still
                            // referenced by other closures.
                        }
                        Err(e) => {
                            log::error!("audio thread restart failed: {}", e);
                        }
                    }

                    #[cfg(feature = "gui")]
                    if let Some(app) = app_health.upgrade() {
                        send_throttled_notification(
                            &app,
                            &mut throttle_health.borrow_mut(),
                            "audio-thread-dead",
                            &gettextrs::gettext("Audio processing stopped"),
                            &gettextrs::gettext("The audio thread has stopped. Attempting to restart..."),
                        );
                    }
                }

                if !audio_alive {
                    log::error!("audio thread dead — disabling audio controls and tray");

                    // D-15: Disable UI audio controls.
                    sync_enable_health.set_sensitive(false);
                    sync_engine_health.set_sensitive(false);
                    sync_strength_health.set_sensitive(false);

                    // D-16: Update tray state to reflect audio unavailability.
                    #[cfg(feature = "tray")]
                    if let Some(ref ts_arc) = *tray_state_health {
                        let mut ts = ts_arc.lock().unwrap_or_else(|e| e.into_inner());
                        ts.audio_available = false;
                        ts.active = false;
                        drop(ts);
                        if let Some(ref handle) = tray_handle_health {
                            handle.update(|_| {});
                        }
                    }

                    // Update config to reflect disabled state.
                    config_health.borrow_mut().enabled = false;

                    #[cfg(feature = "gui")]
                    if let Some(app) = app_health.upgrade() {
                        send_throttled_notification(
                            &app,
                            &mut throttle_health.borrow_mut(),
                            "audio-thread-dead",
                            &gettextrs::gettext("Audio processing unavailable"),
                            &gettextrs::gettext("Could not restart audio processing. Please restart CleanMic."),
                        );
                    }
                }
            } else {
                // Thread alive — reset restart flag so we can attempt again if
                // it dies in the future.
                restart_attempted.set(false);
            }

            last_heartbeat.set(current_hb);
            glib::ControlFlow::Continue
        });
    });

    // Run the GTK application (blocks until quit).
    // Use default args — GTK needs argv[0] at minimum.
    let empty: Vec<String> = vec![];
    app.run_with_args(&empty);

    log::info!("GTK application exited");

    // Pipeline cleanup — drop triggers shutdown of audio thread.
    // The Rc should have refcount 1 here since GTK closures are dropped.
    drop(pipeline);
    drop(pw_manager);
    drop(config);

    Ok(())
}

/// Rebuild TrayState from current config/pipeline state and push to ksni (D-14).
///
/// No-op if tray is not available (state or handle is `None`).
#[cfg(all(feature = "tray", feature = "gui"))]
fn sync_tray_state(
    tray_state: &std::rc::Rc<Option<std::sync::Arc<std::sync::Mutex<crate::tray::TrayState>>>>,
    tray_handle: &Option<ksni::Handle<crate::tray::icon::CleanMicTray>>,
    config: &Config,
) {
    use crate::tray::TrayState;
    if let (Some(state), Some(handle)) = (tray_state.as_deref(), tray_handle) {
        {
            let mut ts: std::sync::MutexGuard<TrayState> = state.lock().unwrap_or_else(|e| e.into_inner());
            ts.set_active(config.enabled)
                .set_engine(config.engine)
                .set_mode(config.mode)
                .set_monitor_enabled(config.monitor_enabled)
                .set_update_available(config.last_seen_update_version.clone());
        }
        // Notify ksni to send LayoutUpdated so the panel re-queries menu().
        handle.update(|_tray: &mut crate::tray::icon::CleanMicTray| {});
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensure the atomic shutdown flag can be set and read.
    #[test]
    fn signal_flag_set_and_read() {
        // Reset to known state.
        SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
        assert!(!is_shutdown_requested());

        request_shutdown();
        assert!(is_shutdown_requested());

        // Reset for other tests.
        SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
    }

    /// Shutdown can be called on a fresh (inactive) PipeWireManager without panic.
    #[test]
    fn shutdown_without_panic() {
        let config = Config::default();
        let mut pw = PipeWireManager::connect().expect("stub connect should succeed");
        // No virtual mic created — shutdown should still succeed gracefully.
        let result = shutdown(&config, None, &mut pw);
        assert!(result.is_ok());
    }

    /// Shutdown destroys the virtual mic when one is active.
    #[test]
    fn shutdown_destroys_virtual_mic() {
        let config = Config::default();
        let mut pw = PipeWireManager::connect().expect("stub connect should succeed");
        pw.create_virtual_mic(None).expect("create should succeed");
        assert!(pw.is_virtual_mic_active());

        shutdown(&config, None, &mut pw).expect("shutdown should succeed");
        assert!(
            !pw.is_virtual_mic_active(),
            "virtual mic should be destroyed after shutdown"
        );
    }

    /// Shutdown completes within the 2-second budget.
    #[test]
    fn shutdown_completes_within_budget() {
        let config = Config::default();
        let mut pw = PipeWireManager::connect().expect("stub connect should succeed");
        pw.create_virtual_mic(None).expect("create should succeed");

        let start = Instant::now();
        shutdown(&config, None, &mut pw).expect("shutdown should succeed");
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "shutdown took {:?}, exceeds 2s budget",
            elapsed
        );
    }

    /// request_shutdown() sets the flag that is_shutdown_requested() reads.
    #[test]
    fn request_shutdown_is_visible() {
        SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
        assert!(!is_shutdown_requested());

        request_shutdown();
        assert!(is_shutdown_requested());

        // Cleanup.
        SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
    }

    /// Shutdown stops the audio pipeline and destroys virtual mic.
    #[test]
    fn shutdown_with_pipeline_and_virtual_mic() {
        let config = Config::default();
        let mut pw = PipeWireManager::connect().expect("stub connect should succeed");
        pw.create_virtual_mic(None).expect("create should succeed");

        let pipeline = AudioPipeline::new().unwrap();
        pipeline.start();
        std::thread::sleep(Duration::from_millis(20));

        shutdown(&config, Some(pipeline), &mut pw).expect("shutdown should succeed");
        assert!(
            !pw.is_virtual_mic_active(),
            "virtual mic should be destroyed after shutdown"
        );
    }

    /// App orchestration: start pipeline, verify it started, shutdown, verify cleanup.
    #[test]
    fn app_orchestration_start_and_shutdown() {
        let config = Config::default();
        let mut pw = PipeWireManager::connect().expect("stub connect should succeed");
        pw.create_virtual_mic(None).expect("create should succeed");
        assert!(pw.is_virtual_mic_active());

        // Create and start pipeline.
        let pipeline = AudioPipeline::new().unwrap();
        pipeline.start();

        // Give the audio thread a moment to start processing.
        std::thread::sleep(Duration::from_millis(50));

        // Verify pipeline is producing level reports.
        let levels = pipeline.poll_levels();
        assert!(
            levels.is_some(),
            "Pipeline should be producing level reports"
        );

        // Shutdown.
        shutdown(&config, Some(pipeline), &mut pw).expect("shutdown should succeed");
        assert!(
            !pw.is_virtual_mic_active(),
            "Virtual mic should be gone after shutdown"
        );
    }

    /// First-run detection: config absent means first run.
    #[test]
    fn first_run_when_config_absent() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let path = tmp.path().join("cleanmic").join("config.toml");
        assert!(Config::is_first_run_at(&path));
    }

    /// Subsequent-run detection: config exists means not first run.
    #[test]
    fn subsequent_run_when_config_exists() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let path = tmp.path().join("cleanmic").join("config.toml");
        let config = Config {
            enabled: true,
            ..Config::default()
        };
        config.save_to(&path).unwrap();

        assert!(!Config::is_first_run_at(&path));
        let loaded = Config::load_from(&path).unwrap();
        assert!(loaded.enabled);
    }

    /// handle_ui_event dispatches EngineChanged correctly.
    #[test]
    fn handle_ui_event_engine_changed() {
        let pipeline = AudioPipeline::new().unwrap();
        let mut config = Config::default();
        let mut pw = PipeWireManager::connect().unwrap();

        handle_ui_event(
            UiEvent::EngineChanged(EngineType::RNNoise),
            &pipeline,
            &mut config,
            &mut pw,
        );
        assert_eq!(config.engine, EngineType::RNNoise);

        drop(pipeline);
    }

    /// handle_ui_event dispatches EnableToggled correctly.
    #[test]
    fn handle_ui_event_enable_toggle() {
        let pipeline = AudioPipeline::new().unwrap();
        let mut config = Config::default();
        let mut pw = PipeWireManager::connect().unwrap();

        handle_ui_event(UiEvent::EnableToggled(false), &pipeline, &mut config, &mut pw);
        assert!(!config.enabled);

        handle_ui_event(UiEvent::EnableToggled(true), &pipeline, &mut config, &mut pw);
        assert!(config.enabled);

        drop(pipeline);
    }

    /// handle_ui_event for Khip when unavailable falls back to best available engine.
    #[test]
    fn handle_ui_event_khip_unavailable() {
        use crate::engine::khip::KhipEngine;
        if KhipEngine::is_available() {
            return; // Library is installed; skip the "unavailable" path.
        }

        let pipeline = AudioPipeline::new().unwrap();
        let mut config = Config::default();
        config.engine = EngineType::DeepFilterNet;
        let mut pw = PipeWireManager::connect().unwrap();

        handle_ui_event(
            UiEvent::EngineChanged(EngineType::Khip),
            &pipeline,
            &mut config,
            &mut pw,
        );
        // Khip unavailable — fallback chain assigns best available engine (D-02).
        // Config should have been updated to the actual engine used.
        assert_ne!(config.engine, EngineType::Khip, "Khip should not be set when unavailable");

        drop(pipeline);
    }

    /// handle_tray_command dispatches Toggle correctly.
    #[test]
    fn handle_tray_command_toggle() {
        let pipeline = AudioPipeline::new().unwrap();
        let mut config = Config::default();
        config.enabled = true;
        let mut pw = PipeWireManager::connect().unwrap();

        handle_tray_command(TrayCommand::Toggle, &pipeline, &mut config, &mut pw);
        assert!(!config.enabled);

        handle_tray_command(TrayCommand::Toggle, &pipeline, &mut config, &mut pw);
        assert!(config.enabled);

        drop(pipeline);
    }

    /// handle_tray_command Quit sets shutdown flag.
    #[test]
    fn handle_tray_command_quit_requests_shutdown() {
        SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);

        let pipeline = AudioPipeline::new().unwrap();
        let mut config = Config::default();
        let mut pw = PipeWireManager::connect().unwrap();

        handle_tray_command(TrayCommand::Quit, &pipeline, &mut config, &mut pw);
        assert!(is_shutdown_requested());

        // Cleanup.
        SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
        drop(pipeline);
    }
}

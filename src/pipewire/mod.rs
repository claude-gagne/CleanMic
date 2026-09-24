//! PipeWire integration.
//!
//! Manages the PipeWire client connection, creates and destroys the "CleanMic"
//! virtual source node, enumerates physical microphones, wires the audio graph,
//! detects orphaned nodes from previous crashed sessions, and optionally routes
//! processed audio to a monitor output.
//!
//! When compiled without the `pipewire` feature, a stub implementation is
//! provided that logs warnings but does not crash. This allows the public API
//! types to always be available for downstream code that needs to reference them.

pub mod devices;
pub mod monitor;
pub mod ringbuf;

#[cfg(feature = "pipewire")]
mod live;

use devices::DeviceEnumerator;
use monitor::MonitorOutput;
use thiserror::Error;

use ringbuf::{RingBufReader, RingBufWriter};

/// Properties of the CleanMic virtual source node.
pub const NODE_NAME: &str = "CleanMic";
pub const NODE_MEDIA_CLASS: &str = "Audio/Source";
pub const NODE_CHANNELS: u32 = 1;
pub const NODE_SAMPLE_RATE: u32 = 48_000;

/// Errors specific to PipeWire operations.
#[derive(Debug, Error)]
pub enum PipeWireError {
    #[error("failed to connect to PipeWire: {0}")]
    ConnectionFailed(String),

    #[error("failed to create virtual mic node: {0}")]
    NodeCreationFailed(String),

    #[error("failed to destroy virtual mic node: {0}")]
    NodeDestructionFailed(String),

    #[error("PipeWire feature not enabled — running in stub mode")]
    NotAvailable,
}

/// Ring buffer capacity in samples. The null-audio-sink node may request up to
/// 16K samples per callback (~340ms at 48kHz). The buffer must hold at least
/// one full request to avoid silence padding in the output.
const RING_BUF_CAPACITY: usize = 48000; // 1 second, rounds to 65536

/// Environment variable that opts a test in to touching the real PipeWire
/// daemon (creating/destroying a real "CleanMic" node, spawning `pw-dump` /
/// `pw-metadata`). Unit tests must never reach any of these paths by
/// default — see [`LivePipeWireManager::connect`](live::LivePipeWireManager::connect)'s
/// cfg(test) guard, this module's `offline()`, and `make test-live`.
///
/// Only used from test code (the guard, the opt-in tests, and this module's
/// own tests), so this whole block is `#[cfg(test)]`.
#[cfg(test)]
pub(crate) const LIVE_PW_TESTS_ENV: &str = "CLEANMIC_LIVE_PW_TESTS";

/// Pure: `true` only when `value` is exactly `Some("1")`. Never trips on
/// `None`, `""`, `"0"` or `"true"` — the env var must be exactly `"1"`.
#[cfg(test)]
pub(crate) fn live_pw_opt_in(value: Option<&str>) -> bool {
    value == Some("1")
}

/// Whether the current test process has opted in to live PipeWire tests via
/// `CLEANMIC_LIVE_PW_TESTS=1`.
#[cfg(test)]
pub(crate) fn live_pw_tests_opted_in() -> bool {
    live_pw_opt_in(std::env::var(LIVE_PW_TESTS_ENV).ok().as_deref())
}

/// Manages the PipeWire client connection and the "CleanMic" virtual source.
///
/// When the `pipewire` feature is enabled, this connects to a real PipeWire
/// daemon. Otherwise, it operates as a no-op stub that logs warnings.
///
/// Two ring buffers bridge the PipeWire RT callbacks and the audio processing
/// thread:
/// - **capture ring**: PipeWire capture callback (writer) -> audio thread (reader)
/// - **output ring**: audio thread (writer) -> PipeWire source callback (reader)
///
/// An additional **monitor ring** buffer bridges the audio thread and a
/// PipeWire playback stream used for listen-back (hear your own processed mic).
pub struct PipeWireManager {
    virtual_mic_active: bool,
    device_enumerator: DeviceEnumerator,
    monitor_output: MonitorOutput,
    /// Writer for the capture ring buffer (given to PipeWire capture callback).
    /// Kept here so we can hand it off when creating the capture stream.
    /// Only consumed when the `pipewire` feature is active.
    #[cfg_attr(not(feature = "pipewire"), allow(dead_code))]
    capture_writer: Option<RingBufWriter>,
    /// Reader for the capture ring buffer (taken by the audio thread).
    capture_reader: Option<RingBufReader>,
    /// Writer for the output ring buffer (taken by the audio thread).
    output_writer: Option<RingBufWriter>,
    /// Reader for the output ring buffer (given to PipeWire source callback).
    /// Kept here so we can hand it off when creating the source stream.
    /// Only consumed when the `pipewire` feature is active.
    #[cfg_attr(not(feature = "pipewire"), allow(dead_code))]
    output_reader: Option<RingBufReader>,
    /// `None` only for a test-only [`Self::offline()`] manager. Always
    /// `Some` after a real [`Self::connect()`].
    #[cfg(feature = "pipewire")]
    inner: Option<live::LivePipeWireManager>,
}

impl PipeWireManager {
    /// Connect to the PipeWire daemon and return a new manager.
    ///
    /// Without the `pipewire` feature, returns a stub that logs warnings.
    pub fn connect() -> Result<Self, PipeWireError> {
        let (capture_writer, capture_reader) = ringbuf::ring_buffer(RING_BUF_CAPACITY);
        let (output_writer, output_reader) = ringbuf::ring_buffer(RING_BUF_CAPACITY);

        #[cfg(feature = "pipewire")]
        {
            let inner = live::LivePipeWireManager::connect()?;
            Ok(Self {
                virtual_mic_active: false,
                device_enumerator: DeviceEnumerator::new(),
                monitor_output: MonitorOutput::new(),
                capture_writer: Some(capture_writer),
                capture_reader: Some(capture_reader),
                output_writer: Some(output_writer),
                output_reader: Some(output_reader),
                inner: Some(inner),
            })
        }

        #[cfg(not(feature = "pipewire"))]
        {
            log::warn!(
                "PipeWire feature not enabled — PipeWireManager running in stub mode. \
                 No audio devices will be created."
            );
            Ok(Self {
                virtual_mic_active: false,
                device_enumerator: DeviceEnumerator::new(),
                monitor_output: MonitorOutput::new(),
                capture_writer: Some(capture_writer),
                capture_reader: Some(capture_reader),
                output_writer: Some(output_writer),
                output_reader: Some(output_reader),
            })
        }
    }

    /// Build a manager that never contacts the PipeWire daemon.
    ///
    /// Every unit test that does not specifically exercise the live path
    /// (see the `live_pw_*` tests, opt-in via `CLEANMIC_LIVE_PW_TESTS=1` /
    /// `make test-live`) must use this instead of [`Self::connect()`], which
    /// under the `pipewire` feature reaches the real daemon and would create
    /// a real "CleanMic" node in the developer's session on every
    /// `cargo test --all-features` run.
    ///
    /// Builds the same ring buffers, device enumerator and monitor output as
    /// [`Self::connect()`]; under the `pipewire` feature `inner` is `None`
    /// (no daemon connection), so every delegating method behaves exactly
    /// like the non-pipewire stub branch of that method (create/destroy flip
    /// only `virtual_mic_active`, `cleanup_orphans` is `Ok`, `enable_monitor`
    /// returns the writer without touching PipeWire, etc).
    #[cfg(test)]
    pub(crate) fn offline() -> Self {
        let (capture_writer, capture_reader) = ringbuf::ring_buffer(RING_BUF_CAPACITY);
        let (output_writer, output_reader) = ringbuf::ring_buffer(RING_BUF_CAPACITY);

        Self {
            virtual_mic_active: false,
            device_enumerator: DeviceEnumerator::new(),
            monitor_output: MonitorOutput::new(),
            capture_writer: Some(capture_writer),
            capture_reader: Some(capture_reader),
            output_writer: Some(output_writer),
            output_reader: Some(output_reader),
            #[cfg(feature = "pipewire")]
            inner: None,
        }
    }

    /// Create the "CleanMic" virtual source node.
    ///
    /// This is idempotent: calling it when the node already exists is a no-op.
    /// The node is created with media.class = "Audio/Source", mono, 48 kHz, f32.
    ///
    /// `capture_target` is the PipeWire node name of the physical mic to pin
    /// the capture stream to (set via `PW_KEY_TARGET_OBJECT`). Always pin so
    /// WirePlumber's default-source policy cannot re-route the capture stream
    /// to whatever source the user has selected in GNOME Sound Settings. Pass
    /// `None` only when no physical mic is available on the system (the picker
    /// will surface the "No input device available" state).
    pub fn create_virtual_mic(
        &mut self,
        capture_target: Option<String>,
    ) -> Result<(), PipeWireError> {
        if self.virtual_mic_active {
            log::debug!("create_virtual_mic called but virtual mic already active — no-op");
            return Ok(());
        }

        #[cfg(feature = "pipewire")]
        {
            // If ring-buffer halves were consumed by a previous create/destroy
            // cycle, allocate fresh ones. The audio thread will need to obtain
            // the new reader/writer halves via take_capture_reader() /
            // take_output_writer() after this call.
            if self.capture_writer.is_none() || self.output_reader.is_none() {
                let (cw, cr) = ringbuf::ring_buffer(RING_BUF_CAPACITY);
                let (ow, or_) = ringbuf::ring_buffer(RING_BUF_CAPACITY);
                self.capture_writer = Some(cw);
                self.capture_reader = Some(cr);
                self.output_writer = Some(ow);
                self.output_reader = Some(or_);
            }
            let capture_writer = self.capture_writer.take().expect("just ensured Some above");
            let output_reader = self.output_reader.take().expect("just ensured Some above");
            if let Some(inner) = self.inner.as_mut() {
                inner.create_virtual_mic(capture_writer, output_reader, capture_target)?;
            } else {
                log::debug!(
                    "offline PipeWireManager: skipping real create_virtual_mic (no daemon connection)"
                );
            }
        }

        #[cfg(not(feature = "pipewire"))]
        {
            let _ = capture_target;
            log::warn!(
                "Stub: would create virtual mic '{}' (media.class={}, {}ch, {} Hz, f32)",
                NODE_NAME,
                NODE_MEDIA_CLASS,
                NODE_CHANNELS,
                NODE_SAMPLE_RATE,
            );
        }

        self.virtual_mic_active = true;
        log::info!("Virtual mic '{}' is now active", NODE_NAME);
        Ok(())
    }

    /// Destroy the "CleanMic" virtual source node.
    ///
    /// Safe to call even if no node exists (no-op in that case).
    pub fn destroy_virtual_mic(&mut self) -> Result<(), PipeWireError> {
        if !self.virtual_mic_active {
            log::debug!("destroy_virtual_mic called but no virtual mic active — no-op");
            return Ok(());
        }

        #[cfg(feature = "pipewire")]
        {
            if let Some(inner) = self.inner.as_mut() {
                inner.destroy_virtual_mic()?;
            }
        }

        #[cfg(not(feature = "pipewire"))]
        {
            log::warn!("Stub: would destroy virtual mic '{}'", NODE_NAME);
        }

        self.virtual_mic_active = false;
        log::info!("Virtual mic '{}' destroyed", NODE_NAME);
        Ok(())
    }

    /// Detect and clean up orphaned "CleanMic" nodes from a previous crash.
    ///
    /// PipeWire normally cleans up nodes when the owning client dies, so in
    /// practice this is usually a no-op. It exists as defense-in-depth.
    pub fn cleanup_orphans(&self) -> Result<(), PipeWireError> {
        #[cfg(feature = "pipewire")]
        {
            match self.inner.as_ref() {
                Some(inner) => inner.cleanup_orphans(),
                None => Ok(()),
            }
        }

        #[cfg(not(feature = "pipewire"))]
        {
            log::warn!("Stub: would scan for orphaned '{}' nodes", NODE_NAME);
            Ok(())
        }
    }

    /// Returns `true` if the virtual mic node is currently active.
    pub fn is_virtual_mic_active(&self) -> bool {
        self.virtual_mic_active
    }

    /// Enable monitor output — routes processed audio to the playback device.
    ///
    /// Creates a monitor ring buffer pair: sends the reader to the PipeWire
    /// thread (to create a playback stream) and returns the writer to the
    /// caller, who must hand it to the audio thread via
    /// `AudioPipeline::set_monitor_writer`.
    pub fn enable_monitor(&mut self) -> Result<RingBufWriter, PipeWireError> {
        let (monitor_writer, monitor_reader) = ringbuf::ring_buffer(RING_BUF_CAPACITY);

        #[cfg(feature = "pipewire")]
        {
            if let Some(inner) = self.inner.as_mut() {
                inner.enable_monitor(monitor_reader)?;
            }
        }

        #[cfg(not(feature = "pipewire"))]
        {
            let _ = monitor_reader;
        }

        Ok(monitor_writer)
    }

    /// Disable monitor output — stops routing audio to the playback device.
    ///
    /// The caller should send `None` to `AudioPipeline::set_monitor_writer`
    /// after calling this so the audio thread stops writing to the destroyed
    /// stream.
    pub fn disable_monitor(&mut self) -> Result<(), PipeWireError> {
        #[cfg(feature = "pipewire")]
        {
            if let Some(inner) = self.inner.as_mut() {
                inner.disable_monitor()?;
            }
        }

        Ok(())
    }

    /// Returns `true` if monitor output is currently enabled.
    pub fn is_monitor_enabled(&self) -> bool {
        self.monitor_output.is_enabled()
    }

    /// Returns a mutable reference to the monitor output (e.g. for writing
    /// processed audio from the audio pipeline).
    pub fn monitor_output_mut(&mut self) -> &mut MonitorOutput {
        &mut self.monitor_output
    }

    /// Take the capture ring buffer reader (audio thread reads mic input from here).
    ///
    /// Returns `None` if already taken. Must be called before starting the audio
    /// pipeline.
    pub fn take_capture_reader(&mut self) -> Option<RingBufReader> {
        self.capture_reader.take()
    }

    /// Query the user's default audio source from PipeWire metadata.
    ///
    /// Returns the node name listed in `default.configured.audio.source` (the
    /// user's explicit GNOME Sound Settings choice) when available, otherwise
    /// falls back to `default.audio.source` (the runtime auto-resolved
    /// default). The fallback matters on fresh GNOME installs where the
    /// configured key is never populated until the user touches the input
    /// picker. Returns `None` if pw-metadata is unreachable, both keys are
    /// unset, or the `pipewire` feature is disabled.
    pub fn configured_default_source(&self) -> Option<String> {
        #[cfg(feature = "pipewire")]
        {
            crate::pipewire::live::configured_default_source()
        }

        #[cfg(not(feature = "pipewire"))]
        {
            None
        }
    }

    /// Retarget the CleanMic capture stream to a different physical mic.
    ///
    /// Creates a fresh capture ring-buffer pair, hands the writer to the
    /// PipeWire thread (which re-creates the capture stream with
    /// `PW_KEY_TARGET_OBJECT` set to `target_name`), and returns the matching
    /// reader. The caller must pass that reader to the audio thread — typically
    /// via `AudioPipeline::replace_capture_reader` — so audio continues to flow
    /// through the new stream.
    ///
    /// Without the `pipewire` feature this is a no-op that returns `None`.
    pub fn set_capture_target(
        &mut self,
        target_name: Option<String>,
    ) -> Result<Option<RingBufReader>, PipeWireError> {
        #[cfg(feature = "pipewire")]
        {
            match self.inner.as_mut() {
                Some(inner) => {
                    let (new_writer, new_reader) = ringbuf::ring_buffer(RING_BUF_CAPACITY);
                    inner.set_capture_target(target_name, new_writer)?;
                    // Track the reader here so `take_capture_reader()` sees
                    // the latest half if the audio thread has not yet
                    // claimed one. When the audio thread is already running,
                    // the caller owns the reader lifecycle.
                    self.capture_reader = Some(new_reader);
                    Ok(self.capture_reader.take())
                }
                None => Ok(None),
            }
        }

        #[cfg(not(feature = "pipewire"))]
        {
            let _ = target_name;
            log::debug!("Stub: set_capture_target is a no-op without the `pipewire` feature");
            Ok(None)
        }
    }

    /// Take the output ring buffer writer (audio thread writes processed audio here).
    ///
    /// Returns `None` if already taken. Must be called before starting the audio
    /// pipeline.
    pub fn take_output_writer(&mut self) -> Option<RingBufWriter> {
        self.output_writer.take()
    }

    /// Check if the PipeWire daemon connection has been lost.
    ///
    /// Returns `true` if a daemon disconnect was signaled by the PipeWire thread
    /// since the last call to [`reset_disconnected`]. Non-blocking.
    ///
    /// Always returns `false` when the `pipewire` feature is not enabled.
    pub fn check_disconnected(&self) -> bool {
        #[cfg(feature = "pipewire")]
        {
            self.inner
                .as_ref()
                .map(|inner| inner.check_disconnected())
                .unwrap_or(false)
        }
        #[cfg(not(feature = "pipewire"))]
        {
            false
        }
    }

    /// Reset the disconnected flag after a successful reconnect.
    ///
    /// Call this after reconnect succeeds so future disconnects can be detected.
    /// No-op when the `pipewire` feature is not enabled.
    pub fn reset_disconnected(&self) {
        #[cfg(feature = "pipewire")]
        {
            if let Some(inner) = self.inner.as_ref() {
                inner.reset_disconnected();
            }
        }
    }

    /// Returns a reference to the device enumerator for querying input devices.
    pub fn device_enumerator(&self) -> &DeviceEnumerator {
        &self.device_enumerator
    }

    /// Returns a mutable reference to the device enumerator (e.g. to register
    /// change callbacks).
    pub fn device_enumerator_mut(&mut self) -> &mut DeviceEnumerator {
        &mut self.device_enumerator
    }
}

impl Drop for PipeWireManager {
    fn drop(&mut self) {
        if self.virtual_mic_active {
            log::info!("PipeWireManager dropping — cleaning up virtual mic");
            if let Err(e) = self.destroy_virtual_mic() {
                log::error!("Failed to destroy virtual mic during drop: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the stub manager connects successfully without PipeWire.
    /// Only meaningful without the `pipewire` feature — that is the only
    /// configuration where `connect()` is the non-live stub. With the
    /// feature enabled, `connect()` reaches the real daemon and every other
    /// test below uses [`PipeWireManager::offline()`] instead.
    #[cfg(not(feature = "pipewire"))]
    #[test]
    fn stub_connect_succeeds() {
        let manager = PipeWireManager::connect();
        assert!(manager.is_ok());
        assert!(!manager.unwrap().is_virtual_mic_active());
    }

    /// An `offline()` manager starts inactive, in every feature combination.
    #[test]
    fn offline_manager_starts_inactive() {
        let manager = PipeWireManager::offline();
        assert!(!manager.is_virtual_mic_active());
    }

    /// `live_pw_opt_in` accepts only the literal string `"1"` — feature-
    /// independent (pure function, no PipeWire dependency).
    #[test]
    fn live_pw_opt_in_accepts_only_exactly_1() {
        assert!(!live_pw_opt_in(None));
        assert!(!live_pw_opt_in(Some("")));
        assert!(!live_pw_opt_in(Some("0")));
        assert!(!live_pw_opt_in(Some("true")));
        assert!(!live_pw_opt_in(Some("yes")));
        assert!(live_pw_opt_in(Some("1")));
    }

    /// Verify that creating the virtual mic twice is idempotent.
    #[test]
    fn create_twice_is_idempotent() {
        let mut manager = PipeWireManager::offline();

        // First create should succeed and activate the mic.
        assert!(manager.create_virtual_mic(None).is_ok());
        assert!(manager.is_virtual_mic_active());

        // Second create should be a no-op (still active, no error).
        assert!(manager.create_virtual_mic(None).is_ok());
        assert!(manager.is_virtual_mic_active());
    }

    /// Verify that Drop cleans up the virtual mic.
    #[test]
    fn drop_cleans_up_virtual_mic() {
        let mut manager = PipeWireManager::offline();
        manager.create_virtual_mic(None).unwrap();
        assert!(manager.is_virtual_mic_active());

        // Explicitly drop and verify no panic.
        drop(manager);
        // If we get here, Drop ran without panicking.
    }

    /// Verify that destroy_virtual_mic on an inactive manager is a no-op.
    #[test]
    fn destroy_when_not_active_is_noop() {
        let mut manager = PipeWireManager::offline();
        assert!(!manager.is_virtual_mic_active());

        // Should be a no-op, not an error.
        assert!(manager.destroy_virtual_mic().is_ok());
        assert!(!manager.is_virtual_mic_active());
    }

    /// Verify that cleanup_orphans on a clean state is a no-op.
    #[test]
    fn cleanup_orphans_on_clean_state_is_noop() {
        let manager = PipeWireManager::offline();
        assert!(manager.cleanup_orphans().is_ok());
    }

    /// Verify create then destroy cycle works.
    #[test]
    fn create_and_destroy_cycle() {
        let mut manager = PipeWireManager::offline();

        manager.create_virtual_mic(None).unwrap();
        assert!(manager.is_virtual_mic_active());

        manager.destroy_virtual_mic().unwrap();
        assert!(!manager.is_virtual_mic_active());

        // Can create again after destroy.
        manager.create_virtual_mic(None).unwrap();
        assert!(manager.is_virtual_mic_active());
    }

    /// An `offline()` manager never touches the daemon for any of the other
    /// delegating methods either (R1, R3).
    #[test]
    fn offline_manager_delegating_methods_never_touch_the_daemon() {
        let mut manager = PipeWireManager::offline();
        assert!(
            manager
                .set_capture_target(Some("mic".into()))
                .unwrap()
                .is_none()
        );
        assert_eq!(manager.configured_default_source(), None);
        assert!(!manager.check_disconnected());
        manager.reset_disconnected(); // must not panic
        let _writer = manager
            .enable_monitor()
            .expect("enable_monitor is Ok offline");
        assert!(manager.disable_monitor().is_ok());
    }

    // -- Integration tests that require a running PipeWire daemon --

    #[test]
    #[ignore]
    fn integration_virtual_mic_appears_in_pipewire() {
        // TODO: With `pipewire` feature, verify the node shows up in pw-cli.
    }

    #[test]
    #[ignore]
    fn integration_virtual_mic_disappears_on_destroy() {
        // TODO: With `pipewire` feature, verify the node is removed.
    }

    #[test]
    #[ignore]
    fn integration_drop_removes_node() {
        // TODO: With `pipewire` feature, verify drop cleans up the node.
    }

    #[test]
    #[ignore]
    fn integration_orphan_detection() {
        // TODO: With `pipewire` feature, create a node, kill the client,
        // then verify cleanup_orphans detects the stale node.
    }

    // -- Live opt-in tests (run via `make test-live`, CLEANMIC_LIVE_PW_TESTS=1) --

    /// Exercises the real daemon path that the facade tests above used to
    /// cover before they moved to `offline()`. Never runs by default:
    /// `#[ignore]` plus an opt-in early return.
    #[cfg(feature = "pipewire")]
    #[test]
    #[ignore = "live PipeWire: creates a real CleanMic node; run via make test-live"]
    fn live_pw_create_destroy_cycle_on_the_real_daemon() {
        if !live_pw_tests_opted_in() {
            eprintln!(
                "skipping live_pw_create_destroy_cycle_on_the_real_daemon: set {}=1 (run via make test-live)",
                LIVE_PW_TESTS_ENV
            );
            return;
        }

        let mut manager = PipeWireManager::connect().expect("connect to the real daemon");

        assert!(manager.create_virtual_mic(None).is_ok());
        assert!(manager.is_virtual_mic_active());

        // Idempotent.
        assert!(manager.create_virtual_mic(None).is_ok());
        assert!(manager.is_virtual_mic_active());

        assert!(manager.destroy_virtual_mic().is_ok());
        assert!(!manager.is_virtual_mic_active());

        assert!(manager.create_virtual_mic(None).is_ok());
        assert!(manager.is_virtual_mic_active());

        assert!(manager.cleanup_orphans().is_ok());

        drop(manager);
    }

    /// `tests/*.rs` integration test files that reach the live PipeWire path
    /// must be structurally opt-in: a `CLEANMIC_LIVE_PW_TESTS` reference plus
    /// `#[ignore]` in code. Integration tests link the crate's non-test lib,
    /// so the `cfg(test)` connect() guard in live.rs does not cover them —
    /// this is the backstop. Feature-independent — scans source text, not
    /// compiled code, so it runs the same in every build.
    #[test]
    fn integration_tests_touching_live_pipewire_are_opt_in() {
        let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
        let Ok(entries) = std::fs::read_dir(&tests_dir) else {
            // No tests/ directory — nothing to scan.
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            // Strip `//` line comments so a comment mentioning these calls
            // doesn't produce a false positive/negative.
            let code: String = contents
                .lines()
                .map(|line| line.split("//").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");

            let touches_live_pipewire = code.contains("PipeWireManager::connect")
                || code.contains("create_virtual_mic")
                || code.contains("Command::new(\"pw-");

            if !touches_live_pipewire {
                continue;
            }

            assert!(
                code.contains("CLEANMIC_LIVE_PW_TESTS"),
                "{} reaches the live PipeWire path but never mentions CLEANMIC_LIVE_PW_TESTS \
                 — it must be opt-in (see make test-live)",
                path.display()
            );
            assert!(
                code.contains("#[ignore"),
                "{} reaches the live PipeWire path but has no #[ignore] — it must not run by \
                 default (see make test-live)",
                path.display()
            );
        }
    }
}

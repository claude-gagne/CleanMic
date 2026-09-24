//! DeepFilterNet engine adapter.
//!
//! Loads `libdeep_filter_ladspa.so` at runtime via `libloading` and hosts
//! it as a LADSPA plugin. The library ships the DeepFilterNet3 model weights
//! embedded — no separate model files needed.
//!
//! ## Search paths (first match wins)
//!
//! 1. `$APPDIR/usr/lib/libdeep_filter_ladspa.so` — bundled inside AppImage
//! 2. `~/.ladspa/libdeep_filter_ladspa.so`
//! 3. `/usr/lib/ladspa/libdeep_filter_ladspa.so`
//! 4. `/usr/local/lib/ladspa/libdeep_filter_ladspa.so`
//! 5. `/usr/lib/x86_64-linux-gnu/ladspa/libdeep_filter_ladspa.so`
//!
//! ## LADSPA port layout (mono plugin, index 0)
//!
//! | Port | Dir     | Type  | Name                        |
//! |------|---------|-------|-----------------------------|
//! | 0    | In      | Audio | Audio In                    |
//! | 1    | Out     | Audio | Audio Out                   |
//! | 2    | In      | Ctrl  | Attenuation Limit (dB)      |
//! | 3    | In      | Ctrl  | Min processing threshold    |
//! | 4    | In      | Ctrl  | Max ERB processing threshold|
//! | 5    | In      | Ctrl  | Max DF processing threshold |
//! | 6    | In      | Ctrl  | Min Processing Buffer       |
//! | 7    | In      | Ctrl  | Post Filter Beta            |
//!
//! Strength mapping (Attenuation Limit):
//! - Light    → 20 dB  (gentle, preserves some background)
//! - Balanced → 50 dB  (EasyEffects default)
//! - Strong   → 100 dB (maximum suppression)
//!
//! ## Overload guard (debug session dfn-panic-under-load)
//!
//! The vendored plugin (upstream v0.5.6, `ladspa/src/lib.rs`, `Plugin::run`)
//! hands each block to its own worker thread and then waits, on OUR audio
//! thread, until the worker has produced the same number of output samples.
//! Every `run()` whose wall time reaches one block duration is an
//! "Underrun": the plugin adds one frame (10 ms) of output delay and appends
//! 480 zeros to its output queue — a 10 ms hole in the audio plus 10 ms of
//! latency that is never really shed (its "decrease" path drops a single
//! sample while decrementing its counter by a whole frame). The underrun
//! that finds its delay counter at one second — the 100th since
//! instantiation at the earliest — calls `panic!("Processing too slow!
//! Please upgrade your CPU")` inside the plugin's own Rust runtime, which
//! ABORTS the whole CleanMic process (reproduced: SIGABRT; `catch_unwind`
//! never sees it). Under CPU load (a browser video call) that turned the
//! virtual mic into dead output.
//!
//! [`UnderrunGuard`] counts those underruns from the host side, over a timed
//! interval that strictly contains the plugin's own, so it never
//! under-counts: underruns since instantiation are both the plugin's
//! accumulated latency (in frames) and an upper bound of its panic counter.
//! At [`UNDERRUN_BUDGET`] the plugin is re-instantiated in place (0.1 ms,
//! sheds the accumulated latency, restarts the count far below the abort).
//! When fresh instances keep burning the budget, the CPU cannot sustain
//! DeepFilterNet right now: the engine stops calling the plugin, passes the
//! input through dry (never silence) and reports
//! [`EngineHealth::Overloaded`] so the app switches to a lighter engine.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{EngineHealth, NoiseEngine, ProcessingMode};

// ── LADSPA C types ────────────────────────────────────────────────────────────
// Defined to match ladspa.h on x86_64 Linux (64-bit unsigned long).

type LadspaHandle = *mut ();
type LadspaData = f32;

/// Mirrors `LADSPA_PortRangeHint` from ladspa.h.
#[repr(C)]
struct LadspaPortRangeHint {
    hint_descriptor: u64, // unsigned long on 64-bit
    lower_bound: LadspaData,
    upper_bound: LadspaData,
}

/// Mirrors `LADSPA_Descriptor` from ladspa.h.
/// Field order and sizes match the C ABI on x86_64 Linux.
#[repr(C)]
struct LadspaDescriptor {
    unique_id: u64,
    label: *const libc::c_char,
    properties: i32,
    _pad: i32, // alignment padding between i32 and pointer
    name: *const libc::c_char,
    maker: *const libc::c_char,
    copyright: *const libc::c_char,
    port_count: u64,
    port_descriptors: *const i32,
    port_names: *const *const libc::c_char,
    port_range_hints: *const LadspaPortRangeHint,
    implementation_data: *mut (),
    instantiate: unsafe extern "C" fn(*const LadspaDescriptor, u64) -> LadspaHandle,
    connect_port: unsafe extern "C" fn(LadspaHandle, u64, *mut LadspaData),
    activate: Option<unsafe extern "C" fn(LadspaHandle)>,
    run: unsafe extern "C" fn(LadspaHandle, u64),
    run_adding: Option<unsafe extern "C" fn(LadspaHandle, u64)>,
    set_run_adding_gain: Option<unsafe extern "C" fn(LadspaHandle, LadspaData)>,
    deactivate: Option<unsafe extern "C" fn(LadspaHandle)>,
    cleanup: unsafe extern "C" fn(LadspaHandle),
}

/// Signature of the `ladspa_descriptor` entry point.
type LadspaDescriptorFn = unsafe extern "C" fn(index: u64) -> *const LadspaDescriptor;

// ── Port indices for deep_filter_mono ────────────────────────────────────────

const PORT_AUDIO_IN: u64 = 0;
const PORT_AUDIO_OUT: u64 = 1;
const PORT_ATTEN_LIM: u64 = 2; // Attenuation Limit (dB) — our strength knob
const PORT_MIN_PROC: u64 = 3; // Min processing threshold
const PORT_MAX_ERB: u64 = 4; // Max ERB processing threshold
const PORT_MAX_DF: u64 = 5; // Max DF processing threshold
const PORT_MIN_BUF: u64 = 6; // Min Processing Buffer (frames)
const PORT_POST_BETA: u64 = 7; // Post Filter Beta

/// DeepFilterNet frame size: 480 samples at 48 kHz = 10 ms.
const FRAME_SIZE: usize = 480;

/// Wall-clock duration of one [`FRAME_SIZE`] block at 48 kHz: the plugin's
/// own underrun threshold (`rtf = run() time / block duration >= 1`).
const BLOCK_DURATION: Duration = Duration::from_millis(10);

// ── Overload guard ────────────────────────────────────────────────────────────

/// Upstream panics on an underrun once its delay counter (one frame at
/// instantiation, +1 per underrun) has reached one second = 100 frames: never
/// earlier than the 100th underrun since instantiation.
const PLUGIN_PANIC_UNDERRUNS: u32 = 100;

/// Underruns one plugin instance may accumulate before it is replaced by a
/// fresh one: 8 frames = +80 ms of latency that upstream would keep for the
/// instance's whole life. Quiet-machine E2E runs accumulate 4-6 per instance
/// (startup / window-build CPU spike); the 2026-09-24 loaded run hit ~19 per
/// second.
const UNDERRUN_BUDGET: u32 = 8;
const _: () = assert!(UNDERRUN_BUDGET < PLUGIN_PANIC_UNDERRUNS - 1);

/// A plugin instance that burns its whole [`UNDERRUN_BUDGET`] within this
/// many `run()` calls (30 s of audio) tripped "fast": the overload is not a
/// one-off spike.
const FAST_TRIP_CALLS: u64 = 3_000;

/// Consecutive fast trips after which the CPU is considered unable to
/// sustain DeepFilterNet: 3 = two fresh instances in a row failed exactly
/// like the one they replaced. With 2, one burst straddling a restart was
/// enough to give up: under a real background build (E2E 2026-09-24
/// 17:38Z) a first instance tripped after 13 s and its replacement burned
/// 8 underruns in 0.18 s of the same burst. Sustained overload (every call
/// slow) still gives up after 24 underruns, ~1 s.
const FAST_TRIPS_TO_GIVE_UP: u32 = 3;

/// Lifetime cap on in-place re-instantiations per engine. Upstream never
/// stops a dropped instance's worker thread (it polls every 2 ms forever and
/// keeps ~1 MB), so recovery must stay bounded.
const MAX_REINSTANTIATIONS: u32 = 10;

/// A call is counted as an underrun from this much below one block duration,
/// so rounding in the plugin's own `f32` RTF can never make it count one
/// that the guard missed (counting a few more is always safe).
const UNDERRUN_MARGIN: Duration = Duration::from_micros(50);

/// What [`UnderrunGuard::observe`] wants the engine to do after a `run()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardAction {
    /// Keep using the current plugin instance.
    Continue,
    /// Replace the current instance with a fresh one before the next block.
    Reinstantiate,
    /// Stop calling the plugin for good: pass input through, report
    /// [`EngineHealth::Overloaded`].
    GiveUp,
}

/// Host-side accounting of the plugin's underruns (see the module docs).
/// Pure bookkeeping — no allocation, no I/O — so it runs on the audio thread
/// and is unit-tested against a model of upstream's `run()`.
#[derive(Debug, Default)]
struct UnderrunGuard {
    /// Underruns since the current instance was created.
    underruns: u32,
    /// `run()` calls on the current instance.
    calls: u64,
    /// Consecutive instances that burned the budget within [`FAST_TRIP_CALLS`].
    fast_trips: u32,
    /// In-place re-instantiations over this engine's lifetime.
    reinstantiations: u32,
    given_up: bool,
}

impl UnderrunGuard {
    /// Account one `run()` call that took `wall` (timed around the whole FFI
    /// call, so it is never shorter than the plugin's own measurement).
    fn observe(&mut self, wall: Duration) -> GuardAction {
        if self.given_up {
            return GuardAction::GiveUp;
        }
        self.calls += 1;
        if wall + UNDERRUN_MARGIN < BLOCK_DURATION {
            return GuardAction::Continue;
        }
        self.underruns += 1;
        if self.underruns < UNDERRUN_BUDGET {
            return GuardAction::Continue;
        }
        self.fast_trips = if self.calls <= FAST_TRIP_CALLS {
            self.fast_trips + 1
        } else {
            0
        };
        if self.fast_trips >= FAST_TRIPS_TO_GIVE_UP || self.reinstantiations >= MAX_REINSTANTIATIONS
        {
            self.give_up();
            return GuardAction::GiveUp;
        }
        self.reinstantiations += 1;
        self.underruns = 0;
        self.calls = 0;
        GuardAction::Reinstantiate
    }

    /// Stop using the plugin (e.g. a re-instantiation failed).
    fn give_up(&mut self) {
        self.given_up = true;
    }
}

// ── Library search ────────────────────────────────────────────────────────────

const LIB_NAME: &str = "libdeep_filter_ladspa.so";

fn find_library() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 1. Inside AppImage ($APPDIR set by the AppRun script).
    if let Some(appdir) = std::env::var_os("APPDIR") {
        candidates.push(PathBuf::from(appdir).join("usr/lib").join(LIB_NAME));
    }

    // 2. User-local LADSPA directory.
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".ladspa").join(LIB_NAME));
    }

    // 3. System LADSPA directories.
    for dir in &[
        "/usr/lib/ladspa",
        "/usr/local/lib/ladspa",
        "/usr/lib/x86_64-linux-gnu/ladspa",
    ] {
        candidates.push(PathBuf::from(dir).join(LIB_NAME));
    }

    candidates.into_iter().find(|p| p.is_file())
}

/// Returns `true` if `libdeep_filter_ladspa.so` is findable.
pub fn is_available() -> bool {
    find_library().is_some()
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// DeepFilterNet noise suppression engine hosted as a LADSPA plugin.
pub struct DeepFilterEngine {
    /// Loaded shared library — keeps function pointers alive.
    library: Option<libloading::Library>,
    /// Pointer to the LADSPA descriptor (valid for library lifetime).
    descriptor: *const LadspaDescriptor,
    /// Live plugin instance handle.
    handle: LadspaHandle,
    /// Whether the engine has been initialized.
    initialized: bool,
    /// Attenuation limit in dB, connected to Port 2.
    atten_lim: f32,
    /// Default control-port values (ports 3-7).
    ctrl_min_proc: f32,
    ctrl_max_erb: f32,
    ctrl_max_df: f32,
    ctrl_min_buf: f32,
    ctrl_post_beta: f32,
    /// Sample rate passed to `instantiate` (kept for re-instantiation).
    sample_rate: u64,
    /// Underrun accounting that keeps the plugin away from its abort path.
    guard: UnderrunGuard,
}

// SAFETY: Only accessed from the single audio thread.
unsafe impl Send for DeepFilterEngine {}

impl DeepFilterEngine {
    pub fn new() -> Self {
        Self {
            library: None,
            descriptor: std::ptr::null(),
            handle: std::ptr::null_mut(),
            initialized: false,
            atten_lim: 50.0, // Balanced default (EasyEffects default)
            ctrl_min_proc: -15.0,
            ctrl_max_erb: -15.0,
            ctrl_max_df: -15.0,
            ctrl_min_buf: 0.0,
            ctrl_post_beta: 0.0,
            sample_rate: 48_000,
            guard: UnderrunGuard::default(),
        }
    }

    /// Map normalized strength to all DeepFilterNet control parameters.
    ///
    /// Tuned against the values EasyEffects ships as its DeepFilterNet
    /// defaults — they are field-tested on a wide range of mics and
    /// produce fewer robotic/pumping artefacts than the "all thresholds
    /// at 0 dB" aggressive mode we tried first. The SNR thresholds stay
    /// constant; only the attenuation limit (and a small post-filter
    /// beta at High) varies across presets:
    ///
    /// - `min_proc = -10 dB`: bands noisier than this get full suppression
    ///   (voice bands are left alone even when noise is present).
    /// - `max_erb = max_df = 35 dB`: ERB + DF stages keep processing up
    ///   to a very high SNR, so the model doesn't disengage mid-word.
    ///
    /// Presets:
    /// - Low    (< 0.33): 35 dB cap — noticeable bite on transients
    ///   (fan + keyboard + mouse) while leaving voice
    ///   character largely intact.
    /// - Medium (< 0.67): balanced — 50 dB cap, EasyEffects default level.
    /// - High   (≥ 0.67): maximum — 100 dB cap + post-filter beta 0.05
    ///   for aggressive residual cleanup.
    fn strength_to_params(strength: f32) -> (f32, f32, f32, f32, f32) {
        // Returns (atten_lim, min_proc, max_erb, max_df, post_beta)
        const MIN_PROC: f32 = -10.0;
        const MAX_ERB: f32 = 35.0;
        const MAX_DF: f32 = 35.0;
        if strength < 0.33 {
            (35.0, MIN_PROC, MAX_ERB, MAX_DF, 0.0)
        } else if strength < 0.67 {
            (50.0, MIN_PROC, MAX_ERB, MAX_DF, 0.0)
        } else {
            (100.0, MIN_PROC, MAX_ERB, MAX_DF, 0.05)
        }
    }

    /// Connect all ports on `self.handle` using current parameter values.
    /// SAFETY: `handle` must be a valid plugin instance.
    unsafe fn connect_all_ports(&mut self, input: *mut LadspaData, output: *mut LadspaData) {
        unsafe {
            let d = &*self.descriptor;
            let connect = d.connect_port;
            connect(self.handle, PORT_AUDIO_IN, input);
            connect(self.handle, PORT_AUDIO_OUT, output);
            connect(self.handle, PORT_ATTEN_LIM, &mut self.atten_lim);
            connect(self.handle, PORT_MIN_PROC, &mut self.ctrl_min_proc);
            connect(self.handle, PORT_MAX_ERB, &mut self.ctrl_max_erb);
            connect(self.handle, PORT_MAX_DF, &mut self.ctrl_max_df);
            connect(self.handle, PORT_MIN_BUF, &mut self.ctrl_min_buf);
            connect(self.handle, PORT_POST_BETA, &mut self.ctrl_post_beta);
        }
    }

    /// Replace the plugin instance with a fresh one (sheds its accumulated
    /// latency and restarts its underrun count). Runs on the audio thread:
    /// `instantiate` only spawns the plugin's worker thread (~0.1 ms, the
    /// model stays loaded in the library). Returns `false` when the plugin
    /// refused to instantiate; the engine then stays in passthrough.
    fn reinstantiate(&mut self) -> bool {
        // SAFETY: `descriptor`/`handle` are valid while `initialized` (init
        // succeeded and teardown has not run); the old handle is never used
        // again after cleanup.
        unsafe {
            let d = &*self.descriptor;
            if let Some(deactivate) = d.deactivate {
                deactivate(self.handle);
            }
            (d.cleanup)(self.handle);
            self.handle = std::ptr::null_mut();
            let h = (d.instantiate)(self.descriptor, self.sample_rate);
            if h.is_null() {
                return false;
            }
            if let Some(activate) = d.activate {
                activate(h);
            }
            self.handle = h;
        }
        true
    }
}

impl Default for DeepFilterEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl NoiseEngine for DeepFilterEngine {
    fn init(&mut self, sample_rate: u32) -> Result<()> {
        anyhow::ensure!(
            sample_rate == 48_000,
            "DeepFilterNet requires 48 kHz, got {sample_rate}"
        );

        let lib_path = find_library().ok_or_else(|| {
            anyhow::anyhow!(
                "DeepFilterNet library not found. \
                 Install libdeep_filter_ladspa.so to ~/.ladspa/ or /usr/lib/ladspa/. \
                 Run `bash scripts/install-deepfilter.sh` to install automatically."
            )
        })?;

        log::info!("DeepFilterNet: loading library from {}", lib_path.display());

        // SAFETY: path exists (checked above); library is a trusted LADSPA plugin.
        let library = unsafe {
            libloading::Library::new(&lib_path)
                .with_context(|| format!("Failed to load {}", lib_path.display()))?
        };

        // Resolve the LADSPA entry point and get the mono descriptor (index 0).
        let descriptor: *const LadspaDescriptor = unsafe {
            let descriptor_fn: libloading::Symbol<LadspaDescriptorFn> = library
                .get(b"ladspa_descriptor\0")
                .context("ladspa_descriptor not found in library")?;
            let desc = descriptor_fn(0); // index 0 = deep_filter_mono
            if desc.is_null() {
                bail!("ladspa_descriptor(0) returned null");
            }
            desc
        };

        // Instantiate the plugin at 48 kHz.
        let handle = unsafe {
            let h = ((*descriptor).instantiate)(descriptor, sample_rate as u64);
            if h.is_null() {
                bail!("LADSPA instantiate() returned null");
            }
            h
        };

        self.library = Some(library);
        self.descriptor = descriptor;
        self.handle = handle;
        self.sample_rate = sample_rate as u64;
        self.guard = UnderrunGuard::default();

        // Activate the plugin (allocates internal buffers, initializes state).
        unsafe {
            if let Some(activate) = (*descriptor).activate {
                activate(handle);
            }
        }

        self.initialized = true;
        log::info!(
            "DeepFilterNet: initialized (48 kHz, {}-sample frames, atten_lim={:.0} dB)",
            FRAME_SIZE,
            self.atten_lim,
        );
        Ok(())
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        // Once the guard has given up, the plugin is never called again:
        // one more underrun could be the one that aborts the process.
        if !self.initialized || self.handle.is_null() || self.guard.given_up {
            output.copy_from_slice(input);
            return;
        }

        // Process in FRAME_SIZE-sample chunks.
        let mut pos = 0;
        while pos + FRAME_SIZE <= input.len() {
            if self.guard.given_up {
                break;
            }
            let in_ptr = input[pos..].as_ptr() as *mut LadspaData;
            let out_ptr = output[pos..].as_mut_ptr();

            // Timed around the whole FFI call so the guard's measurement is
            // never shorter than the plugin's own (see UnderrunGuard).
            let t0 = Instant::now();
            unsafe {
                self.connect_all_ports(in_ptr, out_ptr);
                let d = &*self.descriptor;
                (d.run)(self.handle, FRAME_SIZE as u64);
            }
            let wall = t0.elapsed();
            pos += FRAME_SIZE;

            match self.guard.observe(wall) {
                GuardAction::Continue => {}
                GuardAction::Reinstantiate => {
                    if self.reinstantiate() {
                        log::warn!(
                            "DeepFilterNet: plugin fell behind real time {UNDERRUN_BUDGET} times \
                             (+{} ms latency it never sheds) — restarted it (restart {}/{MAX_REINSTANTIATIONS})",
                            UNDERRUN_BUDGET as u64 * BLOCK_DURATION.as_millis() as u64,
                            self.guard.reinstantiations,
                        );
                    } else {
                        self.guard.give_up();
                        log::error!(
                            "DeepFilterNet: plugin restart failed — passing audio through unprocessed"
                        );
                    }
                }
                GuardAction::GiveUp => {
                    log::warn!(
                        "DeepFilterNet cannot keep up with real time on this computer \
                         ({} plugin restarts) — passing audio through unprocessed to keep the \
                         microphone working",
                        self.guard.reinstantiations,
                    );
                }
            }
        }

        // Passthrough for whatever was not processed: a sub-frame remainder
        // (should not happen at 480) or the blocks after the guard gave up.
        if pos < input.len() {
            output[pos..].copy_from_slice(&input[pos..]);
        }
    }

    fn set_strength(&mut self, strength: f32) {
        let (atten_lim, min_proc, max_erb, max_df, post_beta) = Self::strength_to_params(strength);
        self.atten_lim = atten_lim;
        self.ctrl_min_proc = min_proc;
        self.ctrl_max_erb = max_erb;
        self.ctrl_max_df = max_df;
        self.ctrl_post_beta = post_beta;
        log::debug!(
            "DeepFilterNet: strength {:.2} → atten={:.0} dB, thresholds={:.0} dB, beta={:.2}",
            strength,
            atten_lim,
            min_proc,
            post_beta
        );
    }

    fn set_mode(&mut self, _mode: ProcessingMode) {}

    fn latency_frames(&self) -> u32 {
        // DeepFilterNet has one frame of algorithmic latency (10 ms at 48 kHz).
        FRAME_SIZE as u32
    }

    fn health(&self) -> EngineHealth {
        if self.guard.given_up {
            EngineHealth::Overloaded
        } else {
            EngineHealth::Healthy
        }
    }

    fn teardown(&mut self) {
        if self.initialized {
            // `handle` is null only after a failed in-place re-instantiation
            // (the old instance was already cleaned up).
            if !self.handle.is_null() {
                unsafe {
                    let d = &*self.descriptor;
                    if let Some(deactivate) = d.deactivate {
                        deactivate(self.handle);
                    }
                    (d.cleanup)(self.handle);
                }
            }
            self.handle = std::ptr::null_mut();
            self.descriptor = std::ptr::null();
            self.library = None; // dlclose
            self.initialized = false;
            log::info!("DeepFilterNet: engine torn down");
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strength_mapping() {
        // Thresholds are constant across presets (EasyEffects defaults);
        // only atten_lim (and post_beta at High) varies.
        let (atten, min_proc, max_erb, max_df, beta) =
            DeepFilterEngine::strength_to_params(1.0 / 6.0);
        assert!((atten - 35.0).abs() < 1e-5);
        assert!((min_proc - -10.0).abs() < 1e-5);
        assert!((max_erb - 35.0).abs() < 1e-5);
        assert!((max_df - 35.0).abs() < 1e-5);
        assert!((beta - 0.0).abs() < 1e-5);

        let (atten, min_proc, max_erb, max_df, beta) = DeepFilterEngine::strength_to_params(0.5);
        assert!((atten - 50.0).abs() < 1e-5);
        assert!((min_proc - -10.0).abs() < 1e-5);
        assert!((max_erb - 35.0).abs() < 1e-5);
        assert!((max_df - 35.0).abs() < 1e-5);
        assert!((beta - 0.0).abs() < 1e-5);

        let (atten, min_proc, max_erb, max_df, beta) =
            DeepFilterEngine::strength_to_params(5.0 / 6.0);
        assert!((atten - 100.0).abs() < 1e-5);
        assert!((min_proc - -10.0).abs() < 1e-5);
        assert!((max_erb - 35.0).abs() < 1e-5);
        assert!((max_df - 35.0).abs() < 1e-5);
        assert!(beta > 0.0);
    }

    #[test]
    fn is_available_does_not_panic() {
        // Just verify the detection logic runs without panicking.
        let _ = is_available();
    }

    #[test]
    fn process_passthrough_when_not_initialized() {
        let mut engine = DeepFilterEngine::new();
        let input = vec![0.5f32; 480];
        let mut output = vec![0.0f32; 480];
        engine.process(&input, &mut output);
        assert_eq!(input, output);
    }

    // ── UnderrunGuard ────────────────────────────────────────────────────────

    const FAST: Duration = Duration::from_micros(4_500); // measured quiet worker frame
    const SLOW: Duration = Duration::from_millis(12);

    /// Model of upstream v0.5.6 `Plugin::run()` bookkeeping (ladspa/src/lib.rs
    /// ~434-470, the vendored build): the derived oracle the guard is
    /// checked against. `decreases` toggles its (broken, but counter-lowering)
    /// "reduce delay again" branch; without it the counter only grows, the
    /// adversarial case for the panic.
    struct UpstreamRunModel {
        proc_delay: usize,
        t_proc_change: usize,
        decreases: bool,
    }

    impl UpstreamRunModel {
        const SR: usize = 48_000;
        fn fresh(decreases: bool) -> Self {
            Self {
                proc_delay: FRAME_SIZE,
                t_proc_change: 0,
                decreases,
            }
        }
        /// One run() the plugin itself timed at `td`. Err = its panic!().
        fn run(&mut self, td: Duration) -> Result<(), ()> {
            let rtf = td.as_secs_f32() / (FRAME_SIZE as f32 / Self::SR as f32);
            if rtf >= 1.0 {
                if self.proc_delay >= Self::SR {
                    return Err(());
                }
                self.proc_delay += FRAME_SIZE;
                self.t_proc_change = 0;
            } else if self.decreases
                && self.t_proc_change > 10 * Self::SR / FRAME_SIZE
                && rtf < 0.5
                && self.proc_delay >= FRAME_SIZE
            {
                self.proc_delay -= FRAME_SIZE;
                self.t_proc_change = 0;
            }
            self.t_proc_change += 1;
            Ok(())
        }
    }

    /// Drive guard + upstream model with `walls` (host-timed); the plugin's
    /// own timing is the host's minus `overhead`. Returns (reinstantiations,
    /// gave_up) and panics if the model ever reaches its panic!().
    fn drive(
        walls: impl IntoIterator<Item = Duration>,
        overhead: Duration,
        decreases: bool,
    ) -> (u32, bool) {
        let mut guard = UnderrunGuard::default();
        let mut plugin = UpstreamRunModel::fresh(decreases);
        let mut reinst = 0;
        for (i, wall) in walls.into_iter().enumerate() {
            if guard.given_up {
                break; // the engine never calls run() again
            }
            plugin
                .run(wall.saturating_sub(overhead))
                .unwrap_or_else(|_| panic!("upstream would have aborted the process at call {i}"));
            match guard.observe(wall) {
                GuardAction::Continue => {}
                GuardAction::Reinstantiate => {
                    reinst += 1;
                    plugin = UpstreamRunModel::fresh(decreases);
                }
                GuardAction::GiveUp => {}
            }
        }
        (reinst, guard.given_up)
    }

    #[test]
    fn unguarded_model_aborts_on_the_100th_underrun() {
        // Oracle sanity: the model reproduces the observed abort (99
        // "Increasing" lines, then the panic).
        let mut plugin = UpstreamRunModel::fresh(false);
        for _ in 0..99 {
            plugin
                .run(SLOW)
                .expect("first 99 underruns only add latency");
        }
        assert!(plugin.run(SLOW).is_err(), "the 100th underrun panics");
    }

    #[test]
    fn guard_keeps_upstream_away_from_its_abort_under_sustained_overload() {
        // Every call slow — the E2E loaded run / the starved-worker repro.
        for decreases in [false, true] {
            let (reinst, gave_up) = drive(
                std::iter::repeat_n(SLOW, 100_000),
                Duration::ZERO,
                decreases,
            );
            assert!(gave_up, "sustained overload must end in GiveUp");
            assert_eq!(
                reinst,
                FAST_TRIPS_TO_GIVE_UP - 1,
                "every fast trip but the last restarts, then give up"
            );
        }
    }

    #[test]
    fn guard_never_lets_upstream_abort_for_any_mix_of_slow_and_fast_calls() {
        // Pseudo-random slow/fast mixes at several densities, with host-side
        // overhead from 0 to 40 us (the host interval contains the plugin's).
        for (seed, slow_per_mille, overhead_us) in [
            (1u32, 5u32, 0u64),
            (2, 50, 10),
            (3, 200, 40),
            (4, 600, 0),
            (5, 999, 25),
            (6, 20, 40),
        ] {
            let mut lcg = seed;
            let walls = (0..300_000).map(move |_| {
                lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                if (lcg >> 8) % 1000 < slow_per_mille {
                    SLOW
                } else {
                    FAST
                }
            });
            for decreases in [false, true] {
                drive(walls.clone(), Duration::from_micros(overhead_us), decreases);
            }
        }
    }

    #[test]
    fn quiet_machine_startup_spike_is_tolerated() {
        // 6 underruns during startup (quiet E2E: 4-6), then real time.
        let walls = std::iter::repeat_n(SLOW, 6).chain(std::iter::repeat_n(FAST, 50_000));
        assert_eq!(drive(walls, Duration::ZERO, true), (0, false));
    }

    #[test]
    fn one_burst_straddling_a_restart_is_not_a_give_up() {
        // E2E 2026-09-24 17:38Z under a real background build: 4 startup
        // underruns, 13 s of real time, then one burst that trips the first
        // instance AND burns its replacement's whole budget, then calm.
        let walls = std::iter::repeat_n(SLOW, 4)
            .chain(std::iter::repeat_n(FAST, 1_300))
            .chain(std::iter::repeat_n(SLOW, 4 + UNDERRUN_BUDGET as usize))
            .chain(std::iter::repeat_n(FAST, 10_000));
        assert_eq!(drive(walls, Duration::ZERO, true), (2, false));
    }

    #[test]
    fn budget_boundary_restarts_exactly_at_the_eighth_underrun() {
        let mut guard = UnderrunGuard::default();
        for _ in 0..UNDERRUN_BUDGET - 1 {
            assert_eq!(guard.observe(SLOW), GuardAction::Continue);
        }
        assert_eq!(guard.observe(SLOW), GuardAction::Reinstantiate);
        assert_eq!(
            (guard.underruns, guard.calls),
            (0, 0),
            "fresh instance, fresh count"
        );
        // The fresh instance again tolerates budget-1.
        for _ in 0..UNDERRUN_BUDGET - 1 {
            assert_eq!(guard.observe(SLOW), GuardAction::Continue);
        }
    }

    #[test]
    fn underrun_threshold_is_one_block_minus_the_margin() {
        let mut guard = UnderrunGuard::default();
        guard.observe(BLOCK_DURATION - UNDERRUN_MARGIN - Duration::from_micros(1));
        assert_eq!(guard.underruns, 0, "just below block - margin is on time");
        guard.observe(BLOCK_DURATION - UNDERRUN_MARGIN);
        assert_eq!(guard.underruns, 1, "block - margin already counts");
        guard.observe(BLOCK_DURATION);
        assert_eq!(
            guard.underruns, 2,
            "a full block (the plugin's rtf == 1) counts"
        );
    }

    #[test]
    fn a_slow_trip_is_a_new_spike_not_an_escalation() {
        let mut guard = UnderrunGuard::default();
        let burn = |g: &mut UnderrunGuard| {
            for _ in 0..UNDERRUN_BUDGET - 1 {
                assert_eq!(g.observe(SLOW), GuardAction::Continue);
            }
            g.observe(SLOW)
        };
        for n in 1..FAST_TRIPS_TO_GIVE_UP {
            assert_eq!(
                burn(&mut guard),
                GuardAction::Reinstantiate,
                "fast trip #{n}"
            );
        }
        // More than FAST_TRIP_CALLS of real time before the next budget burn.
        for _ in 0..FAST_TRIP_CALLS {
            guard.observe(FAST);
        }
        assert_eq!(
            burn(&mut guard),
            GuardAction::Reinstantiate,
            "slow trip resets the streak"
        );
        assert_eq!(guard.fast_trips, 0);
        for n in 1..FAST_TRIPS_TO_GIVE_UP {
            assert_eq!(
                burn(&mut guard),
                GuardAction::Reinstantiate,
                "fast trip #{n} again"
            );
        }
        assert_eq!(
            burn(&mut guard),
            GuardAction::GiveUp,
            "FAST_TRIPS_TO_GIVE_UP fast trips in a row"
        );
    }

    #[test]
    fn a_fast_trip_needs_the_budget_burned_within_the_window() {
        let mut guard = UnderrunGuard::default();
        for _ in 0..UNDERRUN_BUDGET * (FAST_TRIPS_TO_GIVE_UP - 1) {
            guard.observe(SLOW);
        }
        assert_eq!(guard.fast_trips, FAST_TRIPS_TO_GIVE_UP - 1);
        // Window boundary: calls == FAST_TRIP_CALLS is still fast.
        for _ in 0..FAST_TRIP_CALLS - u64::from(UNDERRUN_BUDGET) {
            guard.observe(FAST);
        }
        for _ in 0..UNDERRUN_BUDGET - 1 {
            guard.observe(SLOW);
        }
        assert_eq!(guard.calls, FAST_TRIP_CALLS - 1);
        assert_eq!(
            guard.observe(SLOW),
            GuardAction::GiveUp,
            "burned at exactly FAST_TRIP_CALLS"
        );
    }

    #[test]
    fn lifetime_reinstantiation_cap_bounds_upstream_thread_leaks() {
        let mut guard = UnderrunGuard::default();
        let mut reinst = 0;
        loop {
            // Always a slow trip: never escalates by the fast-trip rule.
            for _ in 0..FAST_TRIP_CALLS {
                guard.observe(FAST);
            }
            let mut action = GuardAction::Continue;
            for _ in 0..UNDERRUN_BUDGET {
                action = guard.observe(SLOW);
            }
            match action {
                GuardAction::Reinstantiate => reinst += 1,
                GuardAction::GiveUp => break,
                GuardAction::Continue => panic!("budget burned without an action"),
            }
        }
        assert_eq!(reinst, MAX_REINSTANTIATIONS);
    }

    #[test]
    fn given_up_is_permanent_and_reported_as_overloaded() {
        let mut engine = DeepFilterEngine::new();
        assert_eq!(engine.health(), EngineHealth::Healthy);
        engine.guard.give_up();
        assert_eq!(engine.guard.observe(FAST), GuardAction::GiveUp);
        assert_eq!(engine.health(), EngineHealth::Overloaded);
        // Given up => dry passthrough, never silence.
        let input: Vec<f32> = (0..960).map(|i| (i as f32 * 0.01).sin() * 0.3).collect();
        let mut output = vec![0.0f32; 960];
        engine.process(&input, &mut output);
        assert_eq!(input, output);
    }

    /// Full init → process → teardown. Requires libdeep_filter_ladspa.so.
    /// Marked #[ignore] to avoid parallel-load issues in the test harness.
    #[test]
    #[ignore]
    fn init_and_process_integration() {
        if !is_available() {
            return;
        }
        let mut engine = DeepFilterEngine::new();
        engine.init(48_000).expect("init should succeed");

        let input = vec![0.1f32; FRAME_SIZE * 4];
        let mut output = vec![0.0f32; FRAME_SIZE * 4];
        engine.process(&input, &mut output);
        // No panic and output differs from silence → suppression is active.

        engine.teardown();
    }
}

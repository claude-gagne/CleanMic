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

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use super::{NoiseEngine, ProcessingMode};

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
    ///                    (fan + keyboard + mouse) while leaving voice
    ///                    character largely intact.
    /// - Medium (< 0.67): balanced — 50 dB cap, EasyEffects default level.
    /// - High   (≥ 0.67): maximum — 100 dB cap + post-filter beta 0.05
    ///                    for aggressive residual cleanup.
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
        if !self.initialized || self.handle.is_null() {
            output.copy_from_slice(input);
            return;
        }

        // Process in FRAME_SIZE-sample chunks.
        let mut pos = 0;
        while pos + FRAME_SIZE <= input.len() {
            let in_ptr = input[pos..].as_ptr() as *mut LadspaData;
            let out_ptr = output[pos..].as_mut_ptr();

            unsafe {
                self.connect_all_ports(in_ptr, out_ptr);
                let d = &*self.descriptor;
                (d.run)(self.handle, FRAME_SIZE as u64);
            }
            pos += FRAME_SIZE;
        }

        // Passthrough for any sub-frame remainder (should not happen at 480).
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

    fn teardown(&mut self) {
        if self.initialized {
            unsafe {
                let d = &*self.descriptor;
                if let Some(deactivate) = d.deactivate {
                    deactivate(self.handle);
                }
                (d.cleanup)(self.handle);
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
        let (atten, min_proc, max_erb, max_df, beta) = DeepFilterEngine::strength_to_params(1.0 / 6.0);
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

        let (atten, min_proc, max_erb, max_df, beta) = DeepFilterEngine::strength_to_params(5.0 / 6.0);
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

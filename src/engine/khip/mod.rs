//! Khip engine adapter.
//!
//! Dynamically loads `libkhip.so` at runtime via `libloading`. The library
//! is never bundled — users install it to a system or user-local path.
//!
//! ## Sample-rate conversion
//!
//! The CleanMic pipeline runs at 48 kHz; Khip requires 32 kHz with exactly
//! 480 samples per call. This engine maintains two small ring buffers and
//! performs linear-interpolation SRC on every chunk:
//!
//! ```text
//! pipeline (48 kHz) → accumulate 720 samples
//!   → downsample 720→480 (48→32 kHz)
//!   → khip_process(480, attenuation)
//!   → upsample 480→720 (32→48 kHz)
//!   → emit to pipeline (48 kHz)
//! ```
//!
//! ## Strength mapping
//!
//! The `attenuation` parameter passed to `khip_process` maps from our
//! 3-step strength control:
//!
//! | Level    | strength value | attenuation |
//! |----------|---------------|-------------|
//! | Light    | ~0.17         | 0.4         |
//! | Balanced | 0.5           | 0.7         |
//! | Strong   | ~0.83         | 1.0         |

use super::{NoiseEngine, ProcessingMode};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub mod ffi;

/// The shared library filename to search for.
const KHIP_LIB_NAME: &str = "libkhip.so";

/// Directories where the Khip library is allowed to be loaded from.
const ALLOWED_DIRS: &[&str] = &["/usr/lib", "/usr/lib/x86_64-linux-gnu", "/usr/local/lib"];

/// Validate that a library path is safe to load.
pub fn validate_library_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("Khip library path must be absolute, got: {}", path.display());
    }
    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            bail!("Khip library path contains '..' traversal: {}", path.display());
        }
    }

    let check_path = match path.canonicalize() {
        Ok(resolved) => {
            for component in resolved.components() {
                if let std::path::Component::ParentDir = component {
                    bail!("Khip resolved path contains '..' traversal: {}", resolved.display());
                }
            }
            resolved
        }
        Err(_) => path.to_path_buf(),
    };

    let parent = check_path
        .parent()
        .context("Khip library path has no parent directory")?;

    let user_local_lib = home_local_lib();
    let mut all_allowed: Vec<PathBuf> = ALLOWED_DIRS.iter().map(PathBuf::from).collect();
    if let Some(ref p) = user_local_lib {
        all_allowed.push(p.clone());
    }

    if !all_allowed.iter().any(|a| parent == a.as_path()) {
        bail!(
            "Khip library path is not in an allowed directory. \
             Allowed: {:?}. Got: {}",
            all_allowed.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            parent.display()
        );
    }
    Ok(())
}

fn home_local_lib() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("lib"))
}

/// Search well-known paths for the Khip shared library.
pub fn find_library() -> Option<PathBuf> {
    let user_local = home_local_lib();
    let mut dirs: Vec<PathBuf> = ALLOWED_DIRS.iter().map(PathBuf::from).collect();
    if let Some(p) = user_local {
        dirs.push(p);
    }
    for dir in &dirs {
        let candidate = dir.join(KHIP_LIB_NAME);
        log::debug!("Khip: checking {}", candidate.display());
        if candidate.is_file() && validate_library_path(&candidate).is_ok() {
            log::debug!("Khip: found at {}", candidate.display());
            return Some(candidate);
        }
    }
    log::debug!("Khip: library not found");
    None
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// Khip noise suppression engine backed by `libkhip.so`.
pub struct KhipEngine {
    /// Loaded library (keeps function pointers alive).
    library: Option<libloading::Library>,
    /// Function pointers resolved from the library.
    functions: Option<ffi::KhipFunctions>,
    /// Active session handle (null when not initialized).
    session: *mut ffi::KhipSession,
    /// Whether the engine is ready to process audio.
    initialized: bool,
    /// Optional user-configured library path override.
    custom_library_path: Option<PathBuf>,
    /// Attenuation passed to khip_process (0.0 = bypass, 1.0 = full).
    attenuation: f32,
    /// Accumulated 48 kHz input samples not yet processed.
    input_buf: Vec<f32>,
    /// Processed 48 kHz samples ready for the pipeline.
    output_buf: Vec<f32>,
}

// SAFETY: KhipEngine is only accessed from the single audio thread.
// The raw session pointer is not shared across threads.
unsafe impl Send for KhipEngine {}

impl KhipEngine {
    pub fn new() -> Self {
        Self {
            library: None,
            functions: None,
            session: std::ptr::null_mut(),
            initialized: false,
            custom_library_path: None,
            attenuation: 0.7, // Balanced default
            input_buf: Vec::with_capacity(ffi::BUF_SIZE_48K * 2),
            output_buf: Vec::with_capacity(ffi::BUF_SIZE_48K * 2),
        }
    }

    pub fn with_library_path(path: PathBuf) -> Self {
        Self { custom_library_path: Some(path), ..Self::new() }
    }

    /// Return true if `libkhip.so` can be found in a valid path.
    pub fn is_available() -> bool {
        find_library().is_some()
    }

    fn resolve_library_path(&self) -> Result<PathBuf> {
        if let Some(ref custom) = self.custom_library_path {
            validate_library_path(custom)?;
            anyhow::ensure!(custom.is_file(), "Khip library not found at {}", custom.display());
            Ok(custom.clone())
        } else {
            find_library().ok_or_else(|| {
                anyhow::anyhow!(
                    "Khip library not found."
                )
            })
        }
    }
}

impl Default for KhipEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl NoiseEngine for KhipEngine {
    fn init(&mut self, sample_rate: u32) -> Result<()> {
        anyhow::ensure!(sample_rate == 48_000, "Pipeline must run at 48 kHz, got {sample_rate}");

        let lib_path = self.resolve_library_path()?;
        log::info!("Khip: loading library from {}", lib_path.display());

        // Belt-and-suspenders guard: ensure thread pools are limited even when
        // called from an integration test context where main() did not run first.
        // SAFETY: engine init runs on the audio thread; no other thread is
        // modifying environment variables at this point.
        unsafe {
            std::env::set_var("OPENBLAS_NUM_THREADS", "1");
            std::env::set_var("OMP_NUM_THREADS", "1");
            std::env::set_var("FFTW_NUM_THREADS", "1");
        }
        log::debug!(
            "Khip: thread pools limited before dlopen \
             (OPENBLAS_NUM_THREADS=1 OMP_NUM_THREADS=1 FFTW_NUM_THREADS=1)"
        );

        // SAFETY: path is validated and in an allowed directory.
        let library = unsafe {
            libloading::Library::new(&lib_path).with_context(|| {
                format!("Failed to load Khip library from {}", lib_path.display())
            })?
        };

        // Resolve all function pointers. Dereferencing Symbol<F> copies the
        // function pointer value — it remains valid while `library` is alive.
        let functions = unsafe {
            let create: unsafe extern "C" fn() -> *mut ffi::KhipSession =
                *library.get(b"khip_create\0").context("khip_create not found in library")?;
            let destroy: unsafe extern "C" fn(*mut ffi::KhipSession) =
                *library.get(b"khip_destroy\0").context("khip_destroy not found in library")?;
            let reset: unsafe extern "C" fn(*mut ffi::KhipSession) =
                *library.get(b"khip_reset\0").context("khip_reset not found in library")?;
            let process: unsafe extern "C" fn(*mut ffi::KhipSession, *const f32, *mut f32, f32) =
                *library.get(b"khip_process\0").context("khip_process not found in library")?;

            ffi::KhipFunctions { create, destroy, reset, process }
        };

        let session = unsafe { (functions.create)() };
        if session.is_null() {
            bail!("khip_create() returned null");
        }

        self.session = session;
        self.functions = Some(functions);
        self.library = Some(library);
        self.initialized = true;
        self.input_buf.clear();
        self.output_buf.clear();

        log::info!(
            "Khip: initialized (internal 32 kHz processing, \
             {}-sample chunks, {:.0} ms latency)",
            ffi::BUF_SIZE,
            ffi::BUF_SIZE as f32 / ffi::SAMPLE_RATE as f32 * 1000.0
        );
        Ok(())
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        if !self.initialized || self.session.is_null() {
            output.copy_from_slice(input);
            return;
        }
        let fns = match self.functions.as_ref() {
            Some(f) => f,
            None => {
                output.copy_from_slice(input);
                return;
            }
        };

        // Accumulate incoming 48 kHz samples.
        self.input_buf.extend_from_slice(input);

        // Process in 720-sample chunks (= 480 samples at 32 kHz).
        while self.input_buf.len() >= ffi::BUF_SIZE_48K {
            let mut chunk_48k = [0.0f32; ffi::BUF_SIZE_48K];
            chunk_48k.copy_from_slice(&self.input_buf[..ffi::BUF_SIZE_48K]);
            self.input_buf.drain(..ffi::BUF_SIZE_48K);

            let chunk_32k = downsample_48_to_32(&chunk_48k);
            let mut out_32k = [0.0f32; ffi::BUF_SIZE];

            // SAFETY: session is non-null and library is loaded; buffers are
            // correctly sized for the library's expectations.
            unsafe {
                (fns.process)(self.session, chunk_32k.as_ptr(), out_32k.as_mut_ptr(), self.attenuation);
            }

            let out_48k = upsample_32_to_48(&out_32k);
            self.output_buf.extend_from_slice(&out_48k);
        }

        // Copy buffered output; fill with silence during initial startup latency.
        let n = output.len().min(self.output_buf.len());
        output[..n].copy_from_slice(&self.output_buf[..n]);
        self.output_buf.drain(..n);
        output[n..].fill(0.0);
    }

    fn set_strength(&mut self, strength: f32) {
        // Map our 3-step levels to Khip attenuation values.
        // 0.0 = bypass, 1.0 = maximum cancellation.
        //
        // Previous Low=0.4 was essentially bypass — users reported hearing
        // keyboard/mouse/fan untouched. Range compressed to 0.6–1.0 so Low
        // is audibly different from "engine off", while keeping distinct
        // Low/Medium/High progression. Khip's adaptive stage still needs
        // ~1–2 s to lock onto a new noise profile at any attenuation level;
        // that is a library-level behavior we can't tune from here.
        self.attenuation = if strength < 0.33 {
            0.6 // Light — audible reduction, preserves some ambience
        } else if strength < 0.67 {
            0.85 // Balanced — clean baseline
        } else {
            1.0 // Strong — maximum cancellation
        };
        log::info!("Khip: strength {:.3} → attenuation {:.2}", strength, self.attenuation);
    }

    fn set_mode(&mut self, _mode: ProcessingMode) {
        // Khip has no mode parameter; the call is accepted and silently ignored.
    }

    fn latency_frames(&self) -> u32 {
        // One 32 kHz frame expressed in 48 kHz samples: 480 × (48000/32000) = 720.
        ffi::BUF_SIZE_48K as u32
    }

    fn teardown(&mut self) {
        if self.initialized {
            if let Some(ref fns) = self.functions {
                if !self.session.is_null() {
                    unsafe { (fns.destroy)(self.session) };
                    self.session = std::ptr::null_mut();
                }
            }
            self.functions = None;
            self.library = None; // dlclose
            self.initialized = false;
            self.input_buf.clear();
            self.output_buf.clear();
            log::info!("Khip: engine torn down");
        }
    }
}

// ── Sample-rate conversion ────────────────────────────────────────────────────

/// Downsample 48 kHz → 32 kHz: 720 → 480 samples via linear interpolation.
/// Each output sample i corresponds to input position i × 1.5.
fn downsample_48_to_32(input: &[f32; ffi::BUF_SIZE_48K]) -> [f32; ffi::BUF_SIZE] {
    let mut out = [0.0f32; ffi::BUF_SIZE];
    for (i, s) in out.iter_mut().enumerate() {
        let pos = i as f32 * 1.5; // 720/480
        let idx = pos as usize;
        let frac = pos - idx as f32;
        let a = input[idx];
        let b = if idx + 1 < ffi::BUF_SIZE_48K { input[idx + 1] } else { a };
        *s = a + frac * (b - a);
    }
    out
}

/// Upsample 32 kHz → 48 kHz: 480 → 720 samples via linear interpolation.
/// Each output sample i corresponds to input position i × (2/3).
fn upsample_32_to_48(input: &[f32; ffi::BUF_SIZE]) -> [f32; ffi::BUF_SIZE_48K] {
    let mut out = [0.0f32; ffi::BUF_SIZE_48K];
    for (i, s) in out.iter_mut().enumerate() {
        let pos = i as f32 * (ffi::BUF_SIZE as f32 / ffi::BUF_SIZE_48K as f32); // × 2/3
        let idx = pos as usize;
        let frac = pos - idx as f32;
        let a = input[idx.min(ffi::BUF_SIZE - 1)];
        let b = input[(idx + 1).min(ffi::BUF_SIZE - 1)];
        *s = a + frac * (b - a);
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_relative_path() {
        assert!(validate_library_path(Path::new("lib/libkhip.so")).is_err());
    }

    #[test]
    fn reject_dotdot_traversal() {
        assert!(validate_library_path(Path::new("/usr/lib/../etc/libkhip.so")).is_err());
    }

    #[test]
    fn reject_outside_allowed_dirs() {
        let r = validate_library_path(Path::new("/tmp/libkhip.so"));
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("not in an allowed directory"));
    }

    #[test]
    fn accept_usr_lib() {
        assert!(validate_library_path(Path::new("/usr/lib/libkhip.so")).is_ok());
    }

    #[test]
    fn accept_usr_local_lib() {
        assert!(validate_library_path(Path::new("/usr/local/lib/libkhip.so")).is_ok());
    }

    #[test]
    fn accept_home_local_lib() {
        if let Some(home) = std::env::var_os("HOME") {
            let path = PathBuf::from(home).join(".local/lib/libkhip.so");
            assert!(validate_library_path(&path).is_ok());
        }
    }

    #[test]
    fn downsample_length() {
        let input = [0.0f32; ffi::BUF_SIZE_48K];
        let out = downsample_48_to_32(&input);
        assert_eq!(out.len(), ffi::BUF_SIZE);
    }

    #[test]
    fn upsample_length() {
        let input = [0.0f32; ffi::BUF_SIZE];
        let out = upsample_32_to_48(&input);
        assert_eq!(out.len(), ffi::BUF_SIZE_48K);
    }

    #[test]
    fn src_roundtrip_dc() {
        // A DC signal should survive downsampling + upsampling with no change.
        let input = [0.5f32; ffi::BUF_SIZE_48K];
        let downsampled = downsample_48_to_32(&input);
        let upsampled = upsample_32_to_48(&downsampled);
        for (i, &s) in upsampled.iter().enumerate() {
            assert!((s - 0.5).abs() < 1e-5, "sample {i}: expected 0.5, got {s}");
        }
    }

    #[test]
    fn is_available_reflects_installation() {
        // With a user-supplied libkhip.so on the library search path this is true;
        // in CI without the library it will be false — both are valid.
        let _ = KhipEngine::is_available(); // just confirm it doesn't panic
    }

    #[test]
    fn set_strength_maps_levels() {
        let mut engine = KhipEngine::new();
        engine.set_strength(1.0 / 6.0); // Light
        assert!((engine.attenuation - 0.6).abs() < 1e-5);
        engine.set_strength(0.5); // Balanced
        assert!((engine.attenuation - 0.85).abs() < 1e-5);
        engine.set_strength(5.0 / 6.0); // Strong
        assert!((engine.attenuation - 1.0).abs() < 1e-5);
    }

    #[test]
    fn process_passthrough_when_not_initialized() {
        let mut engine = KhipEngine::new();
        let input = vec![0.25f32; 480];
        let mut output = vec![0.0f32; 480];
        engine.process(&input, &mut output);
        assert_eq!(input, output);
    }

    /// Integration test: loads the real libkhip.so and exercises the full path.
    ///
    /// Marked `#[ignore]` because libkhip.so uses FFTW/OpenBLAS global state
    /// that is not safe to initialize from multiple parallel test threads.
    /// Run explicitly with: `cargo test -- --ignored` (or RUST_TEST_THREADS=1).
    #[test]
    #[ignore]
    fn init_and_process_integration() {
        // Only runs when libkhip.so is installed.
        if !KhipEngine::is_available() {
            return;
        }
        let mut engine = KhipEngine::new();
        engine.init(48_000).expect("init should succeed");

        // Feed several chunks to get past startup latency.
        let input = vec![0.0f32; ffi::BUF_SIZE_48K * 4];
        let mut output = vec![0.0f32; ffi::BUF_SIZE_48K * 4];
        engine.process(&input, &mut output);
        // Should not panic; output may be silence during warmup.

        engine.teardown();
    }
}

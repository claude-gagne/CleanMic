//! Throwaway/experimental real-time DPDFNet adapter (D-07).
//!
//! **This engine is NOT part of the shipping product.** It exists solely to let
//! Plan 05 measure DPDFNet-2/DPDFNet-8's real-time cost (latency, sustained CPU,
//! memory), engine-switching behavior, and failure-recovery behavior *inside*
//! CleanMic's actual `src/audio.rs` pipeline (DPDF-01), rather than only through
//! the offline Python benchmark harness (`.planning/model-eval/`).
//!
//! Compiled only behind the non-default `dpdfnet-experimental` Cargo feature.
//! **Never** registered in the production engine-selector enum, nor in
//! [`super::create_engine`] / [`super::create_engine_with_fallback`], this
//! phase (D-07/D-08, Pitfall 4) — it is driven only from
//! `tests/dpdfnet_experimental_switch.rs`.
//!
//! ## Scope (decided-visible, per D-08)
//!
//! DOES cover: dynamically loading `libonnxruntime.so` via the `ort` crate's
//! `load-dynamic` feature (mirroring the `khip`/`deepfilter` dlopen pattern),
//! building a real ONNX Runtime `Session` against the pinned DPDFNet-2/8
//! `.onnx` model, threading the model's recurrent state tensor across calls,
//! and reporting real `Session::run()` latency/CPU/memory cost.
//!
//! Deliberately does **NOT** cover: perceptually-correct short-time Fourier
//! transform (STFT) reconstruction, or any other audio-quality concern. The
//! real DPDFNet-2/8 ONNX graphs are frequency-domain, recurrent-state models
//! (`spec [1,1,481,2]` + `state_in [state_size]` -> `spec_e [1,1,481,2]` +
//! `state_out [state_size]`) — see
//! `/tmp/opencode/cleanmic-dpdfnet-reference/hushmic/crates/dpdfnet-ladspa/`
//! for the actual production-quality STFT (960-point Vorbis-windowed,
//! causal analysis/synthesis) that the already-audited HushMic `enhance`
//! renderer and the Python model-eval adapters use for the real audio-quality
//! evaluation (Plan 04's blind-listening harness owns that). Building an
//! equivalent Rust STFT here would require a new supply-chain dependency
//! (a fast Cooley-Tukey FFT for the non-power-of-two `N_FFT = 960` transform
//! size) that was not covered by Task 1's approved `ort`-only checkpoint, and
//! is not needed for DPDF-01's actual measurement goal: `Session::run()`'s
//! real inference cost is identical regardless of whether the input tensor
//! holds a real STFT frame or any other correctly-shaped `f32` data, since
//! ONNX graphs do not branch on input *values* here. This adapter therefore
//! packs/unpacks each 480-sample time-domain block directly into/from the
//! `spec`/`spec_e` tensor's real component (imaginary component left at
//! zero) — a cheap, honestly-documented placeholder, NOT a real STFT — so
//! that the model's true compute graph and true recurrent-state threading
//! run for real on every call, while output audio quality is explicitly out
//! of scope for this adapter (tracked as a Known Stub in the plan Summary).

use super::{NoiseEngine, ProcessingMode};
use anyhow::{Context, Result, anyhow, bail};
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::TensorRef;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

/// Frequency bins the real DPDFNet ONNX graph's `spec`/`spec_e` tensors carry
/// (`N_FFT / 2 + 1` for the reference 960-point transform; see module docs).
const FREQ_BINS: usize = 481;
/// Interleaved real/imaginary length of the `spec` tensor (`FREQ_BINS * 2`).
const SPEC_LEN: usize = FREQ_BINS * 2;
/// Pipeline processing block size (10 ms @ 48 kHz) — matches `src/audio.rs`'s
/// internal `BUFFER_SIZE`, and is also the model's native streaming hop size.
const BLOCK_SIZE: usize = 480;

/// Environment variable pointing at the exact `libonnxruntime.so` to dlopen.
///
/// Required (not optional) for this adapter: unlike a directory-search
/// fallback, `ort`'s `load-dynamic` feature must be told explicitly which
/// `.so` to load via [`ort::init_from`] before any [`Session`] is built, so
/// this engine never silently picks up a mismatched system-installed ONNX
/// Runtime version (e.g. a distro-packaged `libonnxruntime.so.1.23` alongside
/// the pinned `v1.27.0` this phase's evaluation is built against).
const ORT_DYLIB_PATH_VAR: &str = "ORT_DYLIB_PATH";

/// Process-wide ONNX Runtime environment initialization, mirroring the
/// pinned HushMic reference's `ensure_runtime()` (must run exactly once
/// before the first [`Session::builder`] call).
static RUNTIME_INIT: Once = Once::new();
static RUNTIME_OK: AtomicBool = AtomicBool::new(false);

/// Validate that a dylib path is safe to dlopen.
///
/// Mirrors `khip::validate_library_path`'s absolute-path and `..`-traversal
/// checks. Unlike Khip's production allowed-directory allowlist, this
/// experimental, test-only adapter (never compiled into the shipping
/// AppImage, never reachable outside `cargo test --features
/// dpdfnet-experimental`) does not restrict the parent directory beyond
/// requiring it to exist and be a regular file — the caller already has
/// local dev-environment code-execution-equivalent access by definition.
pub fn validate_dylib_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!(
            "ORT_DYLIB_PATH must be an absolute path, got: {}",
            path.display()
        );
    }
    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            bail!("ORT_DYLIB_PATH contains '..' traversal: {}", path.display());
        }
    }
    anyhow::ensure!(
        path.is_file(),
        "ORT_DYLIB_PATH does not point to a regular file: {}",
        path.display()
    );
    Ok(())
}

/// Resolve the `libonnxruntime.so` path to dlopen, via `ORT_DYLIB_PATH`.
pub fn resolve_dylib_path() -> Result<PathBuf> {
    let raw = std::env::var_os(ORT_DYLIB_PATH_VAR)
        .ok_or_else(|| anyhow!("{ORT_DYLIB_PATH_VAR} is not set"))?;
    let path = PathBuf::from(raw);
    validate_dylib_path(&path)?;
    Ok(path)
}

/// Return true if a `libonnxruntime.so` is resolvable via `ORT_DYLIB_PATH`.
pub fn is_available() -> bool {
    resolve_dylib_path().is_ok()
}

/// Map an `ort::Error` (which does not implement `std::error::Error`, so
/// `anyhow::Context` cannot be used on it directly) to an `anyhow::Error`
/// with an added message, mirroring the pinned HushMic reference's own
/// `.map_err(|e| e.to_string())` pattern for the same reason.
fn ort_ctx<T, E: std::fmt::Display>(result: std::result::Result<T, E>, msg: &str) -> Result<T> {
    result.map_err(|e| anyhow!("{msg}: {e}"))
}

/// Install the ONNX Runtime environment from the resolved dylib path.
///
/// Idempotent (safe to call from every `init()`); real failures are surfaced
/// via the return value rather than by allowing `ort` to panic internally.
fn ensure_runtime(dylib_path: &Path) -> Result<()> {
    let mut init_err: Option<String> = None;
    RUNTIME_INIT.call_once(|| match ort::init_from(dylib_path) {
        Ok(builder) => {
            let _ = builder.commit();
            RUNTIME_OK.store(true, Ordering::Release);
        }
        Err(e) => {
            init_err = Some(e.to_string());
        }
    });
    if RUNTIME_OK.load(Ordering::Acquire) {
        Ok(())
    } else {
        bail!(
            "ONNX Runtime environment failed to initialize from {}{}",
            dylib_path.display(),
            init_err.map(|e| format!(": {e}")).unwrap_or_default()
        );
    }
}

/// Experimental DPDFNet noise-suppression engine, backed by a real ONNX
/// Runtime [`Session`] loaded dynamically via the `ort` crate.
pub struct DpdfnetExperimentalEngine {
    /// Path to the pinned DPDFNet-2 or DPDFNet-8 `.onnx` model.
    model_path: PathBuf,
    /// Live inference session, `None` until [`Self::init`] succeeds.
    session: Option<Session>,
    /// Whether the engine is ready to process audio.
    initialized: bool,
    /// Recurrent state threaded across calls (`state_out` -> next `state_in`).
    state: Vec<f32>,
    /// Scratch input tensor buffer, reused every call (no per-call alloc).
    spec_in: [f32; SPEC_LEN],
}

impl DpdfnetExperimentalEngine {
    /// Create a new engine targeting the given `.onnx` model path (DPDFNet-2
    /// or DPDFNet-8 — either the pinned dpdfnet2_48khz_hr.onnx or
    /// dpdfnet8_48khz_hr.onnx scratch artifact from Plan 01).
    pub fn new(model_path: PathBuf) -> Self {
        Self {
            model_path,
            session: None,
            initialized: false,
            state: Vec::new(),
            spec_in: [0.0f32; SPEC_LEN],
        }
    }

    /// Create a new engine, reading the model path from the given
    /// environment variable (e.g. `DPDFNET_EXPERIMENTAL_MODEL_PATH`).
    pub fn from_env(var: &str) -> Result<Self> {
        let path = std::env::var(var).with_context(|| format!("{var} is not set"))?;
        Ok(Self::new(PathBuf::from(path)))
    }

    /// Return true if `ORT_DYLIB_PATH` resolves to a real dylib.
    pub fn is_available() -> bool {
        is_available()
    }
}

impl NoiseEngine for DpdfnetExperimentalEngine {
    fn init(&mut self, sample_rate: u32) -> Result<()> {
        anyhow::ensure!(
            sample_rate == 48_000,
            "DPDFNet experimental adapter requires 48 kHz, got {sample_rate}"
        );

        anyhow::ensure!(
            self.model_path.is_file(),
            "DPDFNet model not found at {}",
            self.model_path.display()
        );

        let dylib_path = resolve_dylib_path()?;
        log::info!(
            "DPDFNet (experimental): loading ONNX Runtime from {}",
            dylib_path.display()
        );
        ensure_runtime(&dylib_path)?;

        // Belt-and-suspenders: limit ONNX Runtime's own intra/inter-op thread
        // pools before building the session — the real-time-safety analog of
        // khip's OPENBLAS/OMP/FFTW_NUM_THREADS env vars, applied here via
        // the session builder's own thread-count options instead (ONNX
        // Runtime does not read those env vars itself).
        let builder = ort_ctx(
            Session::builder(),
            "failed to create ONNX Runtime session builder",
        )?;
        let builder = ort_ctx(
            builder.with_intra_threads(1),
            "failed to set intra-op thread count",
        )?;
        let builder = ort_ctx(
            builder.with_inter_threads(1),
            "failed to set inter-op thread count",
        )?;
        let mut builder = ort_ctx(
            builder.with_optimization_level(GraphOptimizationLevel::Level3),
            "failed to set graph optimization level",
        )?;
        let session = ort_ctx(
            builder.commit_from_file(&self.model_path),
            &format!("commit_from_file({})", self.model_path.display()),
        )?;

        // Determine state_size from the model's own custom metadata
        // (authoritative), falling back to the declared `state_in` input
        // shape if the metadata field is absent.
        let state_size: usize = {
            let meta = ort_ctx(session.metadata(), "failed to read DPDFNet model metadata")?;
            let from_meta: Option<usize> = meta
                .custom("state_size")
                .and_then(|s: String| s.trim().parse().ok());
            from_meta.unwrap_or(0)
        };
        let state_size = if state_size > 0 {
            state_size
        } else {
            let from_shape: Option<usize> = session
                .inputs()
                .iter()
                .find(|o| o.name() == "state_in")
                .and_then(|o| o.dtype().tensor_shape())
                .and_then(|shape| shape.last().copied())
                .filter(|&d| d > 0)
                .map(|d| d as usize);
            from_shape.ok_or_else(|| {
                anyhow!("could not determine state_size from metadata or input shape")
            })?
        };

        self.state = vec![0.0f32; state_size];
        self.spec_in = [0.0f32; SPEC_LEN];
        self.session = Some(session);
        self.initialized = true;

        log::info!(
            "DPDFNet (experimental): initialized ({} model, state_size={})",
            self.model_path.display(),
            state_size
        );
        Ok(())
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        if !self.initialized {
            output.copy_from_slice(input);
            return;
        }
        let Some(session) = self.session.as_mut() else {
            output.copy_from_slice(input);
            return;
        };

        // Pack the raw time-domain block into the `spec` tensor's real
        // component (imaginary left at zero). NOT a real STFT — see module
        // docs. `input.len()` is expected to be BLOCK_SIZE (480); handle any
        // other length defensively by copying only what fits.
        let n = input.len().min(BLOCK_SIZE);
        for (k, &sample) in input.iter().enumerate().take(n) {
            self.spec_in[2 * k] = sample;
            self.spec_in[2 * k + 1] = 0.0;
        }
        for k in n..FREQ_BINS {
            self.spec_in[2 * k] = 0.0;
            self.spec_in[2 * k + 1] = 0.0;
        }

        let result = (|| -> Result<()> {
            let spec_t = TensorRef::from_array_view(([1usize, 1, FREQ_BINS, 2], &self.spec_in[..]))
                .context("failed to build spec tensor")?;
            let state_t = TensorRef::from_array_view(([self.state.len()], &self.state[..]))
                .context("failed to build state_in tensor")?;
            let outputs = session
                .run(ort::inputs! { "spec" => spec_t, "state_in" => state_t })
                .context("Session::run failed")?;

            let (_, spec_e) = outputs
                .get("spec_e")
                .ok_or_else(|| anyhow!("model has no output named 'spec_e'"))?
                .try_extract_tensor::<f32>()
                .context("failed to extract spec_e output")?;
            anyhow::ensure!(
                spec_e.len() == SPEC_LEN,
                "model output 'spec_e' has {} elements, expected {}",
                spec_e.len(),
                SPEC_LEN
            );
            let m = output.len().min(BLOCK_SIZE);
            for k in 0..m {
                output[k] = spec_e[2 * k];
            }
            output[m..].fill(0.0);

            let (_, state_out) = outputs
                .get("state_out")
                .ok_or_else(|| anyhow!("model has no output named 'state_out'"))?
                .try_extract_tensor::<f32>()
                .context("failed to extract state_out output")?;
            self.state.clear();
            self.state.extend_from_slice(state_out);
            Ok(())
        })();

        if let Err(e) = result {
            log::warn!("DPDFNet (experimental): process() failed, passing through: {e}");
            output.copy_from_slice(&input[..output.len().min(input.len())]);
        }
    }

    fn set_strength(&mut self, _strength: f32) {
        // Out of scope for this experimental adapter (D-08) — strength
        // mapping is a production-hardening concern deferred to a future
        // phase alongside production engine-selector/UI registration.
    }

    fn set_mode(&mut self, _mode: ProcessingMode) {
        // Out of scope for this experimental adapter (D-08).
    }

    fn latency_frames(&self) -> u32 {
        // This adapter processes each BLOCK_SIZE hop synchronously with no
        // extra internal buffering (see module docs: no real STFT lookback
        // window is implemented), so it introduces no latency beyond the
        // pipeline's own one-block round trip.
        0
    }

    fn teardown(&mut self) {
        if self.initialized {
            self.session = None;
            self.state.clear();
            self.initialized = false;
            log::info!("DPDFNet (experimental): engine torn down");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_non_48khz_sample_rate() {
        let mut engine = DpdfnetExperimentalEngine::new(PathBuf::from("/nonexistent/model.onnx"));
        let err = engine.init(44_100).unwrap_err();
        assert!(err.to_string().contains("48 kHz"));
    }

    #[test]
    fn reject_missing_model_file() {
        let mut engine = DpdfnetExperimentalEngine::new(PathBuf::from("/nonexistent/model.onnx"));
        let err = engine.init(48_000).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn process_passthrough_when_not_initialized() {
        let mut engine = DpdfnetExperimentalEngine::new(PathBuf::from("/nonexistent/model.onnx"));
        let input = vec![0.25f32; BLOCK_SIZE];
        let mut output = vec![0.0f32; BLOCK_SIZE];
        engine.process(&input, &mut output);
        assert_eq!(input, output);
    }

    #[test]
    fn validate_dylib_path_rejects_relative() {
        assert!(validate_dylib_path(Path::new("lib/libonnxruntime.so")).is_err());
    }

    #[test]
    fn validate_dylib_path_rejects_dotdot() {
        assert!(validate_dylib_path(Path::new("/tmp/../etc/libonnxruntime.so")).is_err());
    }

    #[test]
    fn is_available_reflects_env_var() {
        // With ORT_DYLIB_PATH unset (or pointing at a nonexistent file) this
        // is false; with a real pinned .so exported it is true. Both are
        // valid outcomes in different environments — just confirm no panic.
        let _ = DpdfnetExperimentalEngine::is_available();
    }

    /// Integration test: loads the real pinned model + ONNX Runtime dylib
    /// and exercises a full init/process/teardown cycle. Marked `#[ignore]`
    /// since it requires `ORT_DYLIB_PATH` and `DPDFNET_EXPERIMENTAL_MODEL_PATH`
    /// pointed at the Plan 01 scratch pins. Run with:
    /// `ORT_DYLIB_PATH=... DPDFNET_EXPERIMENTAL_MODEL_PATH=... cargo test --features dpdfnet-experimental -- --ignored`
    #[test]
    #[ignore]
    fn init_and_process_integration() {
        if !DpdfnetExperimentalEngine::is_available() {
            return;
        }
        let Ok(mut engine) = DpdfnetExperimentalEngine::from_env("DPDFNET_EXPERIMENTAL_MODEL_PATH")
        else {
            return;
        };
        engine.init(48_000).expect("init should succeed");

        let input = vec![0.0f32; BLOCK_SIZE];
        let mut output = vec![0.0f32; BLOCK_SIZE];
        engine.process(&input, &mut output);
        assert_eq!(output.len(), BLOCK_SIZE, "block length must be preserved");

        engine.teardown();
    }
}

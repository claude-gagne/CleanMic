//! Production DPDFNet-2 / DPDFNet-8 noise suppression engine (D-01/D-02).
//!
//! Unlike [`super::dpdfnet_experimental`] (a throwaway, never-shipping cost
//! probe that packs raw time-domain samples straight into the model's
//! frequency-domain tensor), this module performs the model's actual
//! streaming digital-signal-processing contract: a causal 960-sample
//! Vorbis-windowed analysis STFT over a 480-sample hop, one recurrent ONNX
//! inference per hop, an attenuation-limited blend against a delayed noisy
//! reference spectrum, and a matching inverse STFT with overlap-add
//! synthesis. This is the sequence the pinned HushMic `dpdfnet-ladspa`
//! reference renderer implements (`stft.rs`/`model.rs`/`attn.rs`/`engine.rs`
//! at the commit pinned by Plan 01's vendor acquisition); the golden vectors
//! in `tests/fixtures/dpdfnet/golden-{dpdfnet2,dpdfnet8}.json` were captured
//! directly from that renderer, and `tests/dpdfnet_production.rs` checks this
//! adapter's output against them within an FFT-implementation epsilon (this
//! adapter uses the approved `realfft` crate; the reference probe uses
//! `rustfft` directly — both are correct, but not guaranteed bit-identical).
//!
//! ## Pipeline (one 480-sample hop)
//!
//! 1. **Analysis** — shift a 960-sample ring by one hop, append the new hop,
//!    multiply by the exact Vorbis (COLA, 50% overlap) window, real-FFT to
//!    481 complex bins, interleave into a `[1,1,481,2]`-shaped `spec` tensor.
//! 2. **Inference** — `Session::run` with `spec` + `state_in`, single
//!    intra/inter-op thread. Output `spec_e`/`state_out` are validated
//!    (correct length, all-finite) before being committed; on any failure
//!    the prior recurrent state is kept untouched and a zero spectrum is
//!    substituted (never corrupt, never propagate non-finite values).
//! 3. **Attenuation limit** — blend the enhanced spectrum with a
//!    `NOISY_FRAME_OFFSET`-hop-delayed copy of the noisy input spectrum,
//!    weighted by a per-variant, strength-derived dB cap (never a raw
//!    dry/wet bypass — see [`DpdfnetEngine::strength_to_attn_db`], D-14/15/16).
//! 4. **Synthesis** — inverse real-FFT the blended spectrum, window, and
//!    overlap-add into the 960-sample synthesis ring; emit the first 480
//!    samples.
//!
//! ## Runtime asset resolution (T-15.1-03)
//!
//! Both the ONNX Runtime dynamic library (`ORT_DYLIB_PATH`) and the model
//! file passed to [`DpdfnetEngine::new`] must resolve to an absolute,
//! non-traversal, existing regular file ([`validate_asset_path`]). The
//! production factory ([`super::create_engine`]) additionally restricts its
//! own default resolution to `$APPDIR/usr/lib` and
//! `$APPDIR/usr/share/cleanmic/models` — this module's constructor itself
//! stays flexible (any validated absolute path) so tests can point it at the
//! Plan 01 vendor-staged reference assets directly.
//!
//! ## Real-time safety
//!
//! All transform plans, tensors, recurrent state, and overlap buffers are
//! allocated in [`NoiseEngine::init`]. [`NoiseEngine::process`] performs no
//! allocation, no locking, and no file I/O — mirroring
//! `src/engine/deepfilter.rs`'s init/hot-path discipline.

use super::{NoiseEngine, ProcessingMode};
use anyhow::{Context, Result, anyhow, bail};
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::TensorRef;
use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

/// STFT analysis window length (samples @ 48 kHz).
const N_FFT: usize = 960;
/// Streaming hop size (samples @ 48 kHz) — also CleanMic's own pipeline block size.
const HOP: usize = 480;
/// Frequency bins the model's `spec`/`spec_e` tensors carry (`N_FFT/2 + 1`).
const FREQ_BINS: usize = 481;
/// Interleaved real/imaginary length of the `spec`/`spec_e` tensors (`FREQ_BINS * 2`).
const SPEC_LEN: usize = FREQ_BINS * 2;
/// Number of hops the attenuation limiter delays its retained noisy-floor
/// reference by, aligning it with the model's own group delay (matches the
/// pinned HushMic reference's `attn.rs::NOISY_FRAME_OFFSET`).
const NOISY_FRAME_OFFSET: usize = 4;

/// Environment variable pointing at the exact `libonnxruntime.so` to dlopen.
/// Shared name/contract with [`super::dpdfnet_experimental`]'s identically
/// named constant (both mirror the pinned HushMic reference's own env-driven
/// override), but this module keeps its own private statics below since the
/// two adapters are independently feature-gated and never assumed to run
/// in the same process.
const ORT_DYLIB_PATH_VAR: &str = "ORT_DYLIB_PATH";

static RUNTIME_INIT: Once = Once::new();
static RUNTIME_OK: AtomicBool = AtomicBool::new(false);

/// Which of the two independently gated (D-02), independently registered
/// (D-01) DPDFNet models this engine instance wraps. Never share a session,
/// state, or gate outcome between variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DpdfnetVariant {
    /// Lighter/faster of the two bundled DPDFNet models.
    Dpdfnet2,
    /// Larger/higher-capacity of the two bundled DPDFNet models.
    Dpdfnet8,
}

impl DpdfnetVariant {
    /// The bundled model filename for this variant (matches
    /// `scripts/generate-dpdfnet-goldens.py`'s `VARIANTS` mapping).
    pub fn model_filename(self) -> &'static str {
        match self {
            DpdfnetVariant::Dpdfnet2 => "dpdfnet2_48khz_hr.onnx",
            DpdfnetVariant::Dpdfnet8 => "dpdfnet8_48khz_hr.onnx",
        }
    }
}

/// Vorbis (sin-of-sin^2) window, COLA at 50% overlap — bit-for-bit the same
/// formula as the pinned reference's `stft.rs::vorbis_window`.
fn vorbis_window() -> [f32; N_FFT] {
    let h = (N_FFT as f32) / 2.0;
    let mut w = [0f32; N_FFT];
    for (n, wn) in w.iter_mut().enumerate() {
        let s = (0.5 * std::f32::consts::PI * (n as f32 + 0.5) / h).sin();
        *wn = (0.5 * std::f32::consts::PI * s * s).sin();
    }
    w
}

/// Causal analysis STFT: a 960-sample ring shifted by one 480-sample hop per
/// call, windowed, and real-FFT'd to 481 interleaved re/im bins. All scratch
/// is allocated once in [`Analysis::new`]; [`Analysis::push_hop`] allocates
/// nothing.
struct Analysis {
    window: [f32; N_FFT],
    ring: [f32; N_FFT],
    r2c: Arc<dyn RealToComplex<f32>>,
    fft_in: Vec<f32>,
    fft_out: Vec<Complex32>,
    scratch: Vec<Complex32>,
}

impl Analysis {
    fn new(planner: &mut RealFftPlanner<f32>) -> Self {
        let r2c = planner.plan_fft_forward(N_FFT);
        let fft_in = r2c.make_input_vec();
        let fft_out = r2c.make_output_vec();
        let scratch = r2c.make_scratch_vec();
        Self {
            window: vorbis_window(),
            ring: [0f32; N_FFT],
            r2c,
            fft_in,
            fft_out,
            scratch,
        }
    }

    fn push_hop(&mut self, in_hop: &[f32; HOP], out_spec: &mut [f32; SPEC_LEN]) {
        self.ring.copy_within(HOP.., 0);
        self.ring[N_FFT - HOP..].copy_from_slice(in_hop);
        for i in 0..N_FFT {
            self.fft_in[i] = self.ring[i] * self.window[i];
        }
        self.r2c
            .process_with_scratch(&mut self.fft_in, &mut self.fft_out, &mut self.scratch)
            .expect("realfft forward transform: fixed-size buffer invariant violated");
        for k in 0..FREQ_BINS {
            out_spec[2 * k] = self.fft_out[k].re;
            out_spec[2 * k + 1] = self.fft_out[k].im;
        }
    }
}

/// Inverse STFT + overlap-add synthesis. Mirrors [`Analysis`]'s window;
/// emits one 480-sample hop per call. All scratch is allocated once in
/// [`Synthesis::new`].
struct Synthesis {
    window: [f32; N_FFT],
    ola: [f32; N_FFT],
    c2r: Arc<dyn ComplexToReal<f32>>,
    ifft_in: Vec<Complex32>,
    ifft_out: Vec<f32>,
    scratch: Vec<Complex32>,
}

impl Synthesis {
    fn new(planner: &mut RealFftPlanner<f32>) -> Self {
        let c2r = planner.plan_fft_inverse(N_FFT);
        let ifft_in = c2r.make_input_vec();
        let ifft_out = c2r.make_output_vec();
        let scratch = c2r.make_scratch_vec();
        Self {
            window: vorbis_window(),
            ola: [0f32; N_FFT],
            c2r,
            ifft_in,
            ifft_out,
            scratch,
        }
    }

    fn add_frame(&mut self, spec: &[f32; SPEC_LEN], out_hop: &mut [f32; HOP]) {
        for k in 0..FREQ_BINS {
            self.ifft_in[k] = Complex32::new(spec[2 * k], spec[2 * k + 1]);
        }
        // realfft reconstructs the Hermitian-symmetric full spectrum
        // internally from these FREQ_BINS bins. An `InputValues` error here
        // only means bin 0 or the Nyquist bin carried a stray (should-be-zero)
        // imaginary component -- per realfft's own documented contract the
        // transform is still performed correctly, so this is logged, not
        // treated as a processing failure.
        if let Err(e) =
            self.c2r
                .process_with_scratch(&mut self.ifft_in, &mut self.ifft_out, &mut self.scratch)
        {
            log::debug!("DPDFNet inverse transform note: {e}");
        }
        // realfft's inverse (like rustfft's) is unnormalized: divide by N_FFT.
        const NORM: f32 = 1.0 / (N_FFT as f32);
        self.ola.copy_within(HOP.., 0);
        for s in &mut self.ola[N_FFT - HOP..] {
            *s = 0.0;
        }
        for i in 0..N_FFT {
            self.ola[i] += self.ifft_out[i] * NORM * self.window[i];
        }
        out_hop.copy_from_slice(&self.ola[..HOP]);
    }
}

/// Blends the enhanced spectrum with a delayed noisy-floor reference,
/// weighted by a dB attenuation-limit cap. Never a raw dry/wet bypass at any
/// setting reachable via [`DpdfnetEngine::strength_to_attn_db`] (D-15).
/// Mirrors the pinned reference's `attn.rs::AttnLimiter` exactly.
struct AttnLimiter {
    /// Residual noisy fraction; 0 = fully enhanced, 1 = fully noisy.
    alpha: f32,
    enabled: bool,
    ring: VecDeque<[f32; SPEC_LEN]>,
}

impl AttnLimiter {
    fn new() -> Self {
        Self {
            alpha: 0.0,
            enabled: false,
            ring: VecDeque::with_capacity(NOISY_FRAME_OFFSET + 1),
        }
    }

    fn reset(&mut self) {
        self.ring.clear();
    }

    /// dB cap -> alpha = 10^(-dB/20). Values >= 200 dB disable blending
    /// entirely (pure enhanced output) purely as a numerical-underflow guard;
    /// [`DpdfnetEngine::strength_to_attn_db`] never produces a value anywhere
    /// near that range, so this path is not reachable from normalized
    /// strength (D-15's "never raw bypass" applies to the low end, not this
    /// guard, which only ever makes suppression MORE aggressive).
    fn set_db(&mut self, db: f32) {
        if !db.is_finite() || db >= 200.0 {
            self.enabled = false;
            self.alpha = 0.0;
            return;
        }
        self.alpha = 10f32.powf(-db / 20.0).min(1.0);
        self.enabled = self.alpha > 1e-6;
    }

    fn apply(&mut self, noisy: &[f32; SPEC_LEN], enhanced: &mut [f32; SPEC_LEN]) {
        self.ring.push_back(*noisy);
        let delayed = if self.ring.len() > NOISY_FRAME_OFFSET {
            self.ring.pop_front()
        } else {
            None
        };
        if !self.enabled {
            return;
        }
        if let Some(d) = delayed {
            let a = self.alpha;
            for i in 0..SPEC_LEN {
                enhanced[i] = a * d[i] + (1.0 - a) * enhanced[i];
            }
        }
    }
}

/// Validate a bundled runtime/model asset path: absolute, no `..` traversal,
/// existing regular file (T-15.1-03). Mirrors
/// `dpdfnet_experimental::validate_dylib_path`'s contract, applied here to
/// both the ONNX Runtime dylib and the model file.
fn validate_asset_path(path: &Path, what: &str) -> Result<()> {
    anyhow::ensure!(
        path.is_absolute(),
        "{what} must be an absolute path, got: {}",
        path.display()
    );
    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            bail!("{what} contains '..' traversal: {}", path.display());
        }
    }
    anyhow::ensure!(
        path.is_file(),
        "{what} does not point to a regular file: {}",
        path.display()
    );
    Ok(())
}

/// Resolve the `libonnxruntime.so` path to dlopen, via `ORT_DYLIB_PATH`.
pub fn resolve_dylib_path() -> Result<PathBuf> {
    let raw = std::env::var_os(ORT_DYLIB_PATH_VAR)
        .ok_or_else(|| anyhow!("{ORT_DYLIB_PATH_VAR} is not set"))?;
    let path = PathBuf::from(raw);
    validate_asset_path(&path, ORT_DYLIB_PATH_VAR)?;
    Ok(path)
}

/// Return true if a `libonnxruntime.so` is resolvable via `ORT_DYLIB_PATH`.
/// Does not check for a model file — callers combine this with their own
/// per-variant model path check (see `super::dpdfnet_model_path`).
pub fn is_dylib_available() -> bool {
    resolve_dylib_path().is_ok()
}

/// Map an `ort::Error` (which does not implement `std::error::Error`) to an
/// `anyhow::Error`. Mirrors `dpdfnet_experimental::ort_ctx`.
fn ort_ctx<T, E: std::fmt::Display>(result: std::result::Result<T, E>, msg: &str) -> Result<T> {
    result.map_err(|e| anyhow!("{msg}: {e}"))
}

/// Install the ONNX Runtime environment from the resolved dylib path.
/// Idempotent; safe to call from every `init()`.
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

/// Parse a comma-separated list of floats (matches the pinned reference's
/// `model.rs::parse_csv_f32`, used to decode `erb_norm_init`/`spec_norm_init`
/// custom ONNX metadata).
fn parse_csv_f32(s: &str) -> Vec<f32> {
    s.split(',')
        .filter_map(|t| t.trim().parse::<f32>().ok())
        .collect()
}

/// Production DPDFNet-2 / DPDFNet-8 noise suppression engine.
pub struct DpdfnetEngine {
    variant: DpdfnetVariant,
    model_path: PathBuf,
    session: Option<Session>,
    initialized: bool,
    analysis: Option<Analysis>,
    synthesis: Option<Synthesis>,
    attn: AttnLimiter,
    /// Recurrent state threaded across calls (`state_out` -> next `state_in`).
    state: Vec<f32>,
    /// Scratch destination for the model's `state_out` output, swapped with
    /// `state` on a successful run (no per-call allocation).
    state_out: Vec<f32>,
    spec: [f32; SPEC_LEN],
    spec_e: [f32; SPEC_LEN],
    out_hop: [f32; HOP],
    /// Current normalized strength (0.0..=1.0), remembered so `init()` can
    /// re-apply it after (re)initialization.
    strength: f32,
    /// Current processing mode (CPU/quality trade-off, D-03). Controls how
    /// often [`Self::process`] runs the (expensive) ONNX inference step.
    mode: ProcessingMode,
    /// Hops processed since the mode was last set (or since construction).
    /// Wraps naturally via `%` in [`Self::decimation_ratio`]'s modulo check;
    /// overflow is harmless (only the remainder matters).
    hop_counter: u32,
}

impl DpdfnetEngine {
    /// Create a new engine targeting the given variant and `.onnx` model
    /// path. The path is validated (absolute, no traversal, regular file)
    /// inside [`NoiseEngine::init`], not here — mirrors
    /// `DpdfnetExperimentalEngine::new`'s lazy-validation contract.
    pub fn new(variant: DpdfnetVariant, model_path: PathBuf) -> Self {
        Self {
            variant,
            model_path,
            session: None,
            initialized: false,
            analysis: None,
            synthesis: None,
            attn: AttnLimiter::new(),
            state: Vec::new(),
            state_out: Vec::new(),
            spec: [0f32; SPEC_LEN],
            spec_e: [0f32; SPEC_LEN],
            out_hop: [0f32; HOP],
            strength: 0.5,
            mode: ProcessingMode::Balanced,
            hop_counter: 0,
        }
    }

    /// Which variant this engine instance wraps.
    pub fn variant(&self) -> DpdfnetVariant {
        self.variant
    }

    /// Map normalized strength (0.0..=1.0) to this variant's attenuation-limit
    /// cap (dB), via piecewise-linear interpolation between three anchors
    /// (Light @ 0.0, Balanced @ 0.5, Strong @ 1.0). Independent per variant
    /// (D-14) and smooth/monotonic across the whole range (D-16): higher
    /// strength always means a higher dB cap, which always means a smaller
    /// (never negative) residual noisy-floor fraction, so suppression can
    /// never get WEAKER as strength increases.
    ///
    /// **Provisional.** These anchors are placeholders pending Plan 08's
    /// per-variant blind-listening validation and Plan 09's frozen
    /// production constants (RESEARCH.md Open Question 1). Strength 0.0
    /// deliberately still selects a real, non-trivial suppression anchor —
    /// never the disabled/unlimited-noisy-floor extreme — so it is never a
    /// raw dry/wet bypass (D-15).
    fn strength_to_attn_db(variant: DpdfnetVariant, strength: f32) -> f32 {
        let s = strength.clamp(0.0, 1.0);
        let (light, balanced, strong) = match variant {
            DpdfnetVariant::Dpdfnet2 => (20.0, 50.0, 100.0),
            DpdfnetVariant::Dpdfnet8 => (15.0, 45.0, 90.0),
        };
        if s <= 0.5 {
            light + (balanced - light) * (s / 0.5)
        } else {
            balanced + (strong - balanced) * ((s - 0.5) / 0.5)
        }
    }

    /// Map a [`ProcessingMode`] to the inference decimation ratio: ONNX
    /// inference runs once every `decimation_ratio(mode)` hops, holding the
    /// prior `spec_e` on the hops in between (D-03). `MaxQuality` == 1 means
    /// every hop (today's unconditional behavior); `Balanced` == 2;
    /// `LowCpu` == 4. **Provisional** starting values — confirmed/adjusted by
    /// owner ear + the CPU-time sweep harness in 15.2-04, same discretionary
    /// status as `strength_to_attn_db`'s anchors.
    fn decimation_ratio(mode: ProcessingMode) -> u32 {
        match mode {
            ProcessingMode::MaxQuality => 1,
            ProcessingMode::Balanced => 2,
            ProcessingMode::LowCpu => 4,
        }
    }
}

impl NoiseEngine for DpdfnetEngine {
    fn init(&mut self, sample_rate: u32) -> Result<()> {
        anyhow::ensure!(
            sample_rate == 48_000,
            "DPDFNet ({:?}) requires 48 kHz, got {sample_rate}",
            self.variant
        );

        validate_asset_path(&self.model_path, "DPDFNet model path")?;
        let dylib_path = resolve_dylib_path()?;
        log::info!(
            "DPDFNet ({:?}): loading ONNX Runtime from {}",
            self.variant,
            dylib_path.display()
        );
        ensure_runtime(&dylib_path)?;

        let builder = ort_ctx(
            Session::builder(),
            "failed to create ONNX Runtime session builder",
        )?;
        let builder = ort_ctx(
            builder.with_execution_providers([ort::ep::CPU::default().build()]),
            "failed to register CPU execution provider",
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

        // Determine state_size + metadata-derived initial recurrent state,
        // mirroring the pinned reference's `Model::load` exactly.
        let (state_size, init_state) = {
            let meta = ort_ctx(session.metadata(), "failed to read DPDFNet model metadata")?;
            let state_size: usize = meta
                .custom("state_size")
                .and_then(|s| s.trim().parse().ok())
                .or_else(|| {
                    session
                        .inputs()
                        .get(1)
                        .and_then(|outlet| outlet.dtype().tensor_shape())
                        .and_then(|shape| shape.last().copied())
                        .filter(|&d| d > 0)
                        .map(|d| d as usize)
                })
                .ok_or_else(|| {
                    anyhow!("could not determine state_size from metadata or input shape")
                })?;
            let erb_sz: usize = meta
                .custom("erb_norm_state_size")
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(FREQ_BINS);
            let spec_sz: usize = meta
                .custom("spec_norm_state_size")
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(96);
            let erb_init = meta
                .custom("erb_norm_init")
                .map(|s| parse_csv_f32(&s))
                .unwrap_or_default();
            let spec_init = meta
                .custom("spec_norm_init")
                .map(|s| parse_csv_f32(&s))
                .unwrap_or_default();
            drop(meta);

            let mut init_state = vec![0f32; state_size];
            if erb_init.len() == erb_sz && erb_sz <= state_size {
                init_state[0..erb_sz].copy_from_slice(&erb_init);
            }
            if spec_init.len() == spec_sz && erb_sz + spec_sz <= state_size {
                init_state[erb_sz..erb_sz + spec_sz].copy_from_slice(&spec_init);
            }
            anyhow::ensure!(
                init_state.iter().all(|v| v.is_finite()),
                "model metadata-derived initial state contains non-finite values"
            );
            (state_size, init_state)
        };

        let mut planner = RealFftPlanner::<f32>::new();
        self.analysis = Some(Analysis::new(&mut planner));
        self.synthesis = Some(Synthesis::new(&mut planner));
        self.attn.reset();
        self.attn
            .set_db(Self::strength_to_attn_db(self.variant, self.strength));

        self.state = init_state;
        self.state_out = vec![0f32; state_size];
        self.spec = [0f32; SPEC_LEN];
        self.spec_e = [0f32; SPEC_LEN];
        self.out_hop = [0f32; HOP];
        self.session = Some(session);
        self.initialized = true;

        log::info!(
            "DPDFNet ({:?}): initialized ({} model, state_size={state_size})",
            self.variant,
            self.model_path.display()
        );
        Ok(())
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        if !self.initialized {
            let n = output.len().min(input.len());
            output[..n].copy_from_slice(&input[..n]);
            if output.len() > n {
                output[n..].fill(0.0);
            }
            return;
        }
        let (Some(analysis), Some(synthesis)) = (self.analysis.as_mut(), self.synthesis.as_mut())
        else {
            let n = output.len().min(input.len());
            output[..n].copy_from_slice(&input[..n]);
            return;
        };

        // Sanitize non-finite input defensively before it ever reaches the
        // analysis ring/model — never let NaN/Inf enter the transform or
        // recurrent state (T-15.1-04).
        let mut in_hop = [0f32; HOP];
        let n = input.len().min(HOP);
        for i in 0..n {
            in_hop[i] = if input[i].is_finite() { input[i] } else { 0.0 };
        }

        analysis.push_hop(&in_hop, &mut self.spec);
        let noisy_spec = self.spec;

        // D-03 decimation gate: on a held hop, skip inference AND the
        // recurrent state swap entirely — self.spec_e already holds the
        // prior run's value, and self.state/self.state_out must stay
        // byte-identical so the ONNX recurrent state never desyncs
        // (RESEARCH.md Pitfall 1). This `if` wraps the WHOLE atomic
        // inference+swap unit; never gate only `Session::run`.
        if self.hop_counter % Self::decimation_ratio(self.mode) == 0 {
            let run_result: Result<()> = (|| {
                let session = self
                    .session
                    .as_mut()
                    .ok_or_else(|| anyhow!("DPDFNet session not initialized"))?;
                let spec_t =
                    TensorRef::from_array_view(([1usize, 1, FREQ_BINS, 2], &self.spec[..]))
                        .context("failed to build spec tensor")?;
                let state_t = TensorRef::from_array_view(([self.state.len()], &self.state[..]))
                    .context("failed to build state_in tensor")?;
                let outputs = session
                    .run(ort::inputs! { "spec" => spec_t, "state_in" => state_t })
                    .context("Session::run failed")?;

                let (_, spec_e_out) = outputs
                    .get("spec_e")
                    .ok_or_else(|| anyhow!("model has no output named 'spec_e'"))?
                    .try_extract_tensor::<f32>()
                    .context("failed to extract spec_e output")?;
                anyhow::ensure!(
                    spec_e_out.len() == SPEC_LEN,
                    "model output 'spec_e' has {} elements, expected {}",
                    spec_e_out.len(),
                    SPEC_LEN
                );
                anyhow::ensure!(
                    spec_e_out.iter().all(|v| v.is_finite()),
                    "model output 'spec_e' contains non-finite values"
                );

                let (_, state_out_slice) = outputs
                    .get("state_out")
                    .ok_or_else(|| anyhow!("model has no output named 'state_out'"))?
                    .try_extract_tensor::<f32>()
                    .context("failed to extract state_out output")?;
                anyhow::ensure!(
                    state_out_slice.len() == self.state.len(),
                    "model output 'state_out' has {} elements, expected {}",
                    state_out_slice.len(),
                    self.state.len()
                );
                anyhow::ensure!(
                    state_out_slice.iter().all(|v| v.is_finite()),
                    "model output 'state_out' contains non-finite values"
                );

                self.spec_e.copy_from_slice(spec_e_out);
                self.state_out.clear();
                self.state_out.extend_from_slice(state_out_slice);
                Ok(())
            })();

            match run_result {
                Ok(()) => {
                    std::mem::swap(&mut self.state, &mut self.state_out);
                }
                Err(e) => {
                    log::warn!(
                        "DPDFNet ({:?}): inference failed, degrading to a muted frame (prior state kept): {e}",
                        self.variant
                    );
                    self.spec_e = [0f32; SPEC_LEN];
                }
            }
        }
        self.hop_counter = self.hop_counter.wrapping_add(1);

        self.attn.apply(&noisy_spec, &mut self.spec_e);
        synthesis.add_frame(&self.spec_e, &mut self.out_hop);

        // Final safety net: never propagate a non-finite sample.
        for v in self.out_hop.iter_mut() {
            if !v.is_finite() {
                *v = 0.0;
            }
        }
        let m = output.len().min(HOP);
        output[..m].copy_from_slice(&self.out_hop[..m]);
        if output.len() > m {
            output[m..].fill(0.0);
        }
    }

    fn set_strength(&mut self, strength: f32) {
        let clamped = strength.clamp(0.0, 1.0);
        self.strength = clamped;
        let db = Self::strength_to_attn_db(self.variant, clamped);
        self.attn.set_db(db);
        log::debug!(
            "DPDFNet ({:?}): strength {:.2} -> attenuation limit {:.1} dB",
            self.variant,
            clamped,
            db
        );
    }

    fn set_mode(&mut self, mode: ProcessingMode) {
        self.mode = mode;
        self.hop_counter = 0;
        log::debug!(
            "DPDFNet ({:?}): mode set to {:?} -> inference decimation ratio {}",
            self.variant,
            mode,
            Self::decimation_ratio(mode)
        );
    }

    fn latency_frames(&self) -> u32 {
        // The pinned golden vectors (tests/fixtures/dpdfnet/golden-*.json)
        // empirically show exactly two hops (960 samples = 20 ms) of
        // near-zero output before the model's recurrent/normalization state
        // has converged on real signal -- this is the true, measured
        // algorithmic warm-up, not the experimental adapter's zero-latency
        // placeholder.
        N_FFT as u32
    }

    fn teardown(&mut self) {
        if self.initialized {
            self.session = None;
            self.analysis = None;
            self.synthesis = None;
            self.attn.reset();
            self.state.clear();
            self.state_out.clear();
            self.initialized = false;
            log::info!("DPDFNet ({:?}): engine torn down", self.variant);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_non_48khz_sample_rate() {
        let mut engine = DpdfnetEngine::new(
            DpdfnetVariant::Dpdfnet2,
            PathBuf::from("/nonexistent/model.onnx"),
        );
        let err = engine.init(44_100).unwrap_err();
        assert!(err.to_string().contains("48 kHz"));
    }

    #[test]
    fn reject_missing_model_file() {
        let mut engine = DpdfnetEngine::new(
            DpdfnetVariant::Dpdfnet2,
            PathBuf::from("/nonexistent/model.onnx"),
        );
        let err = engine.init(48_000).unwrap_err();
        assert!(err.to_string().contains("regular file"));
    }

    #[test]
    fn reject_relative_model_path() {
        let mut engine = DpdfnetEngine::new(
            DpdfnetVariant::Dpdfnet2,
            PathBuf::from("relative/model.onnx"),
        );
        let err = engine.init(48_000).unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn process_passthrough_when_not_initialized() {
        let mut engine = DpdfnetEngine::new(
            DpdfnetVariant::Dpdfnet8,
            PathBuf::from("/nonexistent/model.onnx"),
        );
        let input = vec![0.25f32; HOP];
        let mut output = vec![0.0f32; HOP];
        engine.process(&input, &mut output);
        assert_eq!(input, output);
    }

    #[test]
    fn variant_accessor_reports_constructed_variant() {
        let engine = DpdfnetEngine::new(
            DpdfnetVariant::Dpdfnet8,
            PathBuf::from("/nonexistent/model.onnx"),
        );
        assert_eq!(engine.variant(), DpdfnetVariant::Dpdfnet8);
    }

    #[test]
    fn strength_curves_are_independent_per_variant() {
        // Same normalized strength must map to different anchors for the two
        // variants (D-14): the curves are structurally independent, not a
        // shared formula that happens to be parameterized identically.
        let d2 = DpdfnetEngine::strength_to_attn_db(DpdfnetVariant::Dpdfnet2, 1.0);
        let d8 = DpdfnetEngine::strength_to_attn_db(DpdfnetVariant::Dpdfnet8, 1.0);
        assert_ne!(d2, d8);
    }

    #[test]
    fn strength_zero_never_disables_the_limiter() {
        // D-15: strength 0.0 must remain a real, active suppression anchor,
        // never the "disabled/unlimited noisy floor" extreme (>= 200 dB).
        for variant in [DpdfnetVariant::Dpdfnet2, DpdfnetVariant::Dpdfnet8] {
            let db = DpdfnetEngine::strength_to_attn_db(variant, 0.0);
            assert!(db > 0.0 && db < 200.0, "variant {variant:?} db={db}");
        }
    }

    #[test]
    fn strength_mapping_is_monotonic_across_the_full_range() {
        for variant in [DpdfnetVariant::Dpdfnet2, DpdfnetVariant::Dpdfnet8] {
            let mut prior = f32::MIN;
            for step in 0..=20 {
                let s = step as f32 / 20.0;
                let db = DpdfnetEngine::strength_to_attn_db(variant, s);
                assert!(
                    db >= prior,
                    "variant {variant:?} strength mapping not monotonic at step {step}: {db} < {prior}"
                );
                prior = db;
            }
        }
    }

    #[test]
    fn strength_is_clamped_outside_0_to_1() {
        for variant in [DpdfnetVariant::Dpdfnet2, DpdfnetVariant::Dpdfnet8] {
            assert_eq!(
                DpdfnetEngine::strength_to_attn_db(variant, -1.0),
                DpdfnetEngine::strength_to_attn_db(variant, 0.0)
            );
            assert_eq!(
                DpdfnetEngine::strength_to_attn_db(variant, 2.0),
                DpdfnetEngine::strength_to_attn_db(variant, 1.0)
            );
        }
    }

    #[test]
    fn vorbis_window_is_cola_symmetric() {
        // The Vorbis window used for 50% overlap-add must be symmetric
        // (w[n] == w[N-1-n]) for correct constant-overlap-add reconstruction.
        let w = vorbis_window();
        for n in 0..N_FFT {
            assert!(
                (w[n] - w[N_FFT - 1 - n]).abs() < 1e-6,
                "window not symmetric at {n}"
            );
        }
    }

    #[test]
    fn attn_limiter_zero_db_passes_delayed_noisy_through() {
        let mut a = AttnLimiter::new();
        a.set_db(0.0); // alpha = 1.0 -> output equals the delayed noisy reference
        let mut noisy = [0f32; SPEC_LEN];
        for (i, v) in noisy.iter_mut().enumerate() {
            *v = i as f32;
        }
        for _ in 0..NOISY_FRAME_OFFSET {
            let mut enh = [0f32; SPEC_LEN];
            a.apply(&noisy, &mut enh);
        }
        let mut enh = [7f32; SPEC_LEN];
        a.apply(&noisy, &mut enh);
        assert!((enh[10] - noisy[10]).abs() < 1e-4);
    }

    #[test]
    fn attn_limiter_high_db_keeps_enhanced() {
        let mut a = AttnLimiter::new();
        a.set_db(100.0); // alpha = 1e-5 -> tiny noisy floor
        let noisy = [1000f32; SPEC_LEN];
        for _ in 0..NOISY_FRAME_OFFSET {
            let mut scratch = [0f32; SPEC_LEN];
            a.apply(&noisy, &mut scratch);
        }
        let mut enh = [3f32; SPEC_LEN];
        a.apply(&noisy, &mut enh);
        assert!((enh[10] - 3.01).abs() < 0.05);
    }

    #[test]
    fn is_dylib_available_reflects_env_var() {
        // With ORT_DYLIB_PATH unset (or pointing at a nonexistent file) this
        // is false; with a real pinned .so exported it is true. Both are
        // valid outcomes in different environments -- just confirm no panic.
        let _ = is_dylib_available();
    }

    // ── Decimation gate (D-03) ────────────────────────────────────────────

    #[test]
    fn decimation_ratio_values_match_d03_starting_points() {
        assert_eq!(
            DpdfnetEngine::decimation_ratio(ProcessingMode::MaxQuality),
            1
        );
        assert_eq!(DpdfnetEngine::decimation_ratio(ProcessingMode::Balanced), 2);
        assert_eq!(DpdfnetEngine::decimation_ratio(ProcessingMode::LowCpu), 4);
    }

    #[test]
    fn decimation_gate_runs_exactly_ceil_n_over_ratio_times() {
        // Mirrors the exact gating condition used in `process()`
        // (`hop_counter % decimation_ratio(mode) == 0`) without needing a
        // live model -- proves the counting math itself over N hops.
        const N: u32 = 17;
        for mode in [
            ProcessingMode::MaxQuality,
            ProcessingMode::Balanced,
            ProcessingMode::LowCpu,
        ] {
            let ratio = DpdfnetEngine::decimation_ratio(mode);
            let mut hop_counter: u32 = 0;
            let mut inference_runs: u32 = 0;
            for _ in 0..N {
                if hop_counter % ratio == 0 {
                    inference_runs += 1;
                }
                hop_counter = hop_counter.wrapping_add(1);
            }
            let expected = N.div_ceil(ratio);
            assert_eq!(
                inference_runs, expected,
                "mode {mode:?}: expected {expected} inference runs over {N} hops, got {inference_runs}"
            );
        }
    }

    /// Resolve the vendor-staged reference model/runtime paths this repo
    /// checks in for local dev/CI (mirrors `tests/dpdfnet_production.rs`'s
    /// `build_engine` convention: honest skip if unavailable, never fabricate
    /// a result).
    fn build_test_engine(variant: DpdfnetVariant) -> Option<DpdfnetEngine> {
        let name = match variant {
            DpdfnetVariant::Dpdfnet2 => "dpdfnet2",
            DpdfnetVariant::Dpdfnet8 => "dpdfnet8",
        };
        let model = std::env::var(format!("{}_MODEL_PATH", name.to_uppercase()))
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(format!("vendor/dpdfnet-reference/models/{name}_48khz_hr.onnx"))
            });
        let dylib = std::env::var("ORT_DYLIB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("vendor/dpdfnet-reference/lib/libonnxruntime.so"));
        if !model.is_file() || !dylib.is_file() {
            return None;
        }
        if std::env::var_os("ORT_DYLIB_PATH").is_none() {
            // SAFETY: test-only; every caller in this module sets the
            // identical value, and this crate's dpdfnet tests always run
            // with `--test-threads=1`.
            unsafe {
                std::env::set_var("ORT_DYLIB_PATH", &dylib);
            }
        }
        let mut engine = DpdfnetEngine::new(variant, model);
        engine.init(48_000).ok()?;
        Some(engine)
    }

    #[test]
    fn decimation_holds_recurrent_state_on_skipped_hops() {
        let Some(mut engine) = build_test_engine(DpdfnetVariant::Dpdfnet2) else {
            eprintln!(
                "[dpdfnet decimation] SKIP: pinned model/runtime unavailable in this environment"
            );
            return;
        };

        // LowCpu -> decimation_ratio == 4; set_mode resets hop_counter to 0.
        engine.set_mode(ProcessingMode::LowCpu);

        let input = [0.02f32; HOP];
        let mut output = [0f32; HOP];

        // Hop 0: hop_counter == 0 -> 0 % 4 == 0 -> runs inference.
        engine.process(&input, &mut output);
        let state_after_inference = engine.state.clone();

        // Hops 1..=3: hop_counter in {1,2,3} -> all held (not divisible by
        // 4). self.state must stay BYTE-IDENTICAL across every held hop --
        // this is the single highest-risk correctness bug this phase guards
        // against (RESEARCH.md Pitfall 1): gating only Session::run while
        // leaving the state/state_out swap unconditional would silently
        // desync the recurrent state even though this assertion would still
        // (wrongly) look fine if the swap were skipped along with the run.
        for hop in 1..=3u32 {
            engine.process(&input, &mut output);
            assert_eq!(
                engine.state, state_after_inference,
                "self.state must be byte-identical across held hop {hop} (D-03, RESEARCH Pitfall 1)"
            );
        }

        engine.teardown();
    }
}

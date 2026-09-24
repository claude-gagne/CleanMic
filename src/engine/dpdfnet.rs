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
/// pinned HushMic reference's `attn.rs::NOISY_FRAME_OFFSET`). At decimation
/// ratio `r` (D-03), the model-aligned offset is `NOISY_FRAME_OFFSET * r`
/// hops, not `NOISY_FRAME_OFFSET` alone -- the model's 4-frame delay counts
/// MODEL STEPS, and a model step only equals one hop at `r == 1`
/// (quick-260923-v4q E1).
const NOISY_FRAME_OFFSET: usize = 4;

/// Largest value [`DpdfnetEngine::decimation_ratio`] can return, across every
/// [`ProcessingMode`]. Bounds [`NOISY_HISTORY_LEN`] so a future mode with a
/// larger ratio would fail `noisy_history_covers_every_mode_alignment`
/// rather than silently indexing past the ring's actual coverage
/// (T-v4q-04).
const MAX_DECIMATION_RATIO: u32 = 4;

/// Capacity of [`NoisyHistory`]'s ring. The oldest alignment any decimated
/// mode ever needs is `NOISY_FRAME_OFFSET * MAX_DECIMATION_RATIO` hops old
/// (an "age" of that many hops); `+ 1` gives that age a valid slot (age 0 is
/// the first slot, the current hop).
const NOISY_HISTORY_LEN: usize = NOISY_FRAME_OFFSET * MAX_DECIMATION_RATIO as usize + 1;

/// Floor for [`derive_gain`]'s division denominator, in the spectrum units of
/// the unnormalized 960-point FFT of windowed `f32` PCM -- far below the
/// 16-bit quantization floor of about `7e-4`, so it only guards
/// near-digital-zero bins against a divide-by-zero/near-zero blowup, never
/// the ordinary signal range.
const GAIN_EPS: f32 = 1e-6;

/// Unity clamp for [`derive_gain`]'s output: a held gain mask must never
/// amplify (D-16 monotonicity — see [`derive_gain`]'s docs).
const GAIN_MAX: f32 = 1.0;

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

/// Fixed-capacity ring of noisy analysis-spectrum frames, one entry per hop,
/// used to recover the noisy STFT frame the model's enhanced spectrum
/// (`spec_e`) was actually computed against (D-03 gain-mask hold,
/// quick-260923-v4q E1): at an inference hop in a mode with decimation ratio
/// `r`, `spec_e` describes the analysis frame fed `NOISY_FRAME_OFFSET * r`
/// hops earlier, not `NOISY_FRAME_OFFSET` hops earlier — that simpler offset
/// only holds at `r == 1` (the model's 4-frame delay counts MODEL STEPS, and
/// a model step equals one hop only in `MaxQuality`). Pushed every hop in
/// every mode (never reset by [`DpdfnetEngine::set_mode`]) so a mode switch
/// always finds it primed. All storage is a fixed-size array — `push`
/// overwrites the oldest slot in place, allocating nothing.
struct NoisyHistory {
    ring: [[f32; SPEC_LEN]; NOISY_HISTORY_LEN],
    head: usize,
    filled: usize,
}

impl NoisyHistory {
    fn new() -> Self {
        Self {
            ring: [[0f32; SPEC_LEN]; NOISY_HISTORY_LEN],
            head: 0,
            filled: 0,
        }
    }

    /// Clears the fill count (not the buffer contents — stale samples behind
    /// `filled` are simply never read). Called only from
    /// [`NoiseEngine::init`]/[`NoiseEngine::teardown`], never from
    /// [`DpdfnetEngine::set_mode`] — a mode switch must never lose history.
    fn reset(&mut self) {
        self.head = 0;
        self.filled = 0;
    }

    /// Records the current hop's noisy analysis spectrum, overwriting the
    /// oldest retained frame once the ring is full.
    fn push(&mut self, frame: &[f32; SPEC_LEN]) {
        self.ring[self.head] = *frame;
        self.head = (self.head + 1) % NOISY_HISTORY_LEN;
        if self.filled < NOISY_HISTORY_LEN {
            self.filled += 1;
        }
    }

    /// Returns the frame pushed `age` hops ago (`age == 0` is the current
    /// hop, i.e. the frame from the most recent [`Self::push`]), or `None`
    /// while fewer than `age + 1` frames have ever been pushed (index safety
    /// per T-v4q-04 — never panics, never wraps into unrelated data).
    fn get(&self, age: usize) -> Option<&[f32; SPEC_LEN]> {
        if age >= self.filled || age >= NOISY_HISTORY_LEN {
            return None;
        }
        let idx = (self.head + NOISY_HISTORY_LEN - 1 - age) % NOISY_HISTORY_LEN;
        Some(&self.ring[idx])
    }
}

/// Derives a per-bin real gain mask from the model's most recent enhanced
/// spectrum and the noisy analysis frame it was actually computed against
/// (the `NOISY_FRAME_OFFSET * ratio`-hop-aligned frame from
/// [`NoisyHistory::get`]). Real, not complex: a complex ratio is unbounded
/// wherever the noisy magnitude is small, and its phase rotation is specific
/// to the exact inference frame — not something a later, held hop can
/// legitimately reuse (quick-260923-v4q E2). The [`GAIN_MAX`] clamp means a
/// held mask can never amplify, which keeps suppression monotonic in
/// strength on every decimated path (D-16): the magnitude [`apply_gain`]
/// synthesizes is non-increasing as the attenuation-limit blend's `alpha`
/// rises, only because `g <= 1` here.
fn derive_gain(
    enhanced: &[f32; SPEC_LEN],
    aligned_noisy: &[f32; SPEC_LEN],
    gain: &mut [f32; FREQ_BINS],
) {
    for k in 0..FREQ_BINS {
        let e = (enhanced[2 * k] * enhanced[2 * k] + enhanced[2 * k + 1] * enhanced[2 * k + 1])
            .sqrt();
        let x = (aligned_noisy[2 * k] * aligned_noisy[2 * k]
            + aligned_noisy[2 * k + 1] * aligned_noisy[2 * k + 1])
            .sqrt();
        let g = e / x.max(GAIN_EPS);
        gain[k] = if g.is_finite() { g.clamp(0.0, GAIN_MAX) } else { 0.0 };
    }
}

/// Synthesizes a held-hop spectrum by scaling the CURRENT, correctly aligned
/// noisy frame's real and imaginary parts by the same per-bin real gain —
/// preserving the true, continuous noisy phase rather than replaying the
/// model's old (stale) enhanced-spectrum phase every held hop. This is the
/// fix for the robotic buzz: consecutive held-hop outputs are no longer
/// near-exact repeats, because the underlying noisy phase keeps moving even
/// when the gain mask does not.
fn apply_gain(gain: &[f32; FREQ_BINS], aligned_noisy: &[f32; SPEC_LEN], out: &mut [f32; SPEC_LEN]) {
    for k in 0..FREQ_BINS {
        out[2 * k] = gain[k] * aligned_noisy[2 * k];
        out[2 * k + 1] = gain[k] * aligned_noisy[2 * k + 1];
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

    /// The ring push/pop half of [`Self::apply`], with no blend performed —
    /// used on decimated (`ratio > 1`) hops so the `MaxQuality`-alignment
    /// ring keeps advancing every hop, in lockstep with what [`Self::apply`]
    /// would have done, even though decimated hops synthesize through the
    /// gain-mask path instead. Keeps a later switch back to `MaxQuality`
    /// from seeing a stale (under-filled or drifted) ring.
    fn advance(&mut self, noisy: &[f32; SPEC_LEN]) {
        self.ring.push_back(*noisy);
        if self.ring.len() > NOISY_FRAME_OFFSET {
            self.ring.pop_front();
        }
    }

    /// The blend half of [`Self::apply`] (identical `a * d + (1 - a) * e`
    /// arithmetic), applied in place to `out` against an already-aligned
    /// `reference` frame instead of the ring — used on decimated hops, where
    /// the correctly aligned noisy frame comes from [`NoisyHistory::get`],
    /// not from this limiter's own (`NOISY_FRAME_OFFSET`-only) ring. A no-op
    /// when the limiter is disabled, matching [`Self::apply`]'s contract.
    fn blend(&self, reference: &[f32; SPEC_LEN], out: &mut [f32; SPEC_LEN]) {
        if !self.enabled {
            return;
        }
        let a = self.alpha;
        for i in 0..SPEC_LEN {
            out[i] = a * reference[i] + (1.0 - a) * out[i];
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
    /// Fixed-capacity history of noisy analysis spectra, used to recover the
    /// correctly `NOISY_FRAME_OFFSET * ratio`-hop-aligned noisy frame on
    /// decimated modes (D-03 gain-mask hold). Pushed every hop in every mode.
    history: NoisyHistory,
    /// Per-bin real gain mask, updated only on inferred hops of a decimated
    /// mode (`ratio > 1`); held byte-identical across held hops.
    gain: [f32; FREQ_BINS],
    /// Per-hop working copy that is actually synthesized: a plain copy of
    /// `spec_e` blended once (MaxQuality), or a freshly gain-masked-and-blended
    /// aligned noisy frame (decimated modes) — never `spec_e` itself, so the
    /// attenuation-limit blend can never compound across held hops (R2).
    spec_out: [f32; SPEC_LEN],
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
            history: NoisyHistory::new(),
            gain: [0f32; FREQ_BINS],
            spec_out: [0f32; SPEC_LEN],
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
    /// **Owner-approved (provisional).** These per-variant anchors were
    /// reviewed by the owner during the 15.2-03 checkpoint (D-05) and
    /// approved unchanged as "for now" — a provisional sign-off, not a
    /// permanent freeze; see `15.2-STRENGTH-ANCHORS.json`'s
    /// `owner_decision.status: APPROVED_PROVISIONAL` for the full rationale
    /// and caveats. Strength 0.0 deliberately still selects a real,
    /// non-trivial suppression anchor — never the disabled/unlimited-noisy-
    /// floor extreme — so it is never a raw dry/wet bypass (D-15).
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
        self.history.reset();
        self.gain = [0f32; FREQ_BINS];
        self.spec_out = [0f32; SPEC_LEN];
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
        // Pushed every hop in every mode (D-03 gain-mask hold), so a mode
        // switch always finds it primed.
        self.history.push(&noisy_spec);

        let ratio = Self::decimation_ratio(self.mode);
        let inferred = self.hop_counter.is_multiple_of(ratio);

        // D-03 decimation gate: on a held hop, skip inference AND the
        // recurrent state swap entirely — self.spec_e already holds the
        // prior run's value, and self.state/self.state_out must stay
        // byte-identical so the ONNX recurrent state never desyncs
        // (RESEARCH.md Pitfall 1). This `if` wraps the WHOLE atomic
        // inference+swap unit; never gate only `Session::run`.
        if inferred {
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

        if ratio == 1 {
            // MaxQuality: a per-hop working copy of spec_e, blended exactly
            // once via the unchanged AttnLimiter::apply — identical
            // arithmetic to the pre-fix engine, so MaxQuality output stays
            // bit-identical (R4). No gain-mask work at all on this path.
            self.spec_out = self.spec_e;
            self.attn.apply(&noisy_spec, &mut self.spec_out);
        } else {
            // Keep the MaxQuality-alignment ring advancing every hop, in
            // lockstep with what AttnLimiter::apply would have done, so a
            // later switch back to MaxQuality never sees a stale ring.
            self.attn.advance(&noisy_spec);

            // The model's group delay counts MODEL STEPS, not hops: at
            // decimation ratio `ratio`, spec_e describes the noisy analysis
            // frame from NOISY_FRAME_OFFSET * ratio hops ago, not
            // NOISY_FRAME_OFFSET hops ago (quick-260923-v4q E1).
            let aligned_age = NOISY_FRAME_OFFSET * ratio as usize;
            let aligned_frame: Option<[f32; SPEC_LEN]> = self.history.get(aligned_age).copied();

            if inferred {
                match aligned_frame {
                    Some(aligned) => derive_gain(&self.spec_e, &aligned, &mut self.gain),
                    // No aligned frame yet (warm-up only, at most
                    // aligned_age hops after init) -- and a failed
                    // inference already zeroes spec_e above, so mirror that
                    // fail-closed behavior here: stay muted until the next
                    // successful, alignable inference.
                    None => self.gain = [0f32; FREQ_BINS],
                }
            }
            // self.gain is untouched here on held hops (R5): the stored
            // mask is only ever written on inferred hops, above.

            match aligned_frame {
                Some(aligned) => {
                    apply_gain(&self.gain, &aligned, &mut self.spec_out);
                    self.attn.blend(&aligned, &mut self.spec_out);
                }
                None => self.spec_out = [0f32; SPEC_LEN],
            }
        }

        synthesis.add_frame(&self.spec_out, &mut self.out_hop);

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
            self.history.reset();
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
                if hop_counter.is_multiple_of(ratio) {
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
        // Absolute default paths (quick 260923-v4q, E3): a relative default
        // silently returned None from every model-backed in-module test even
        // though the pinned vendor assets exist in this dev tree, because
        // `DpdfnetEngine::init` -> `validate_asset_path` rejects relative
        // paths outright. `CARGO_MANIFEST_DIR` is always absolute, so this
        // still resolves correctly under any cwd `cargo test` is invoked
        // from, and still honors the env var overrides first.
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let model = std::env::var(format!("{}_MODEL_PATH", name.to_uppercase()))
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                manifest_dir.join(format!(
                    "vendor/dpdfnet-reference/models/{name}_48khz_hr.onnx"
                ))
            });
        let dylib = std::env::var("ORT_DYLIB_PATH").map(PathBuf::from).unwrap_or_else(|_| {
            manifest_dir.join("vendor/dpdfnet-reference/lib/libonnxruntime.so")
        });
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

    // ── D-03 gain-mask hold: alignment, no-compounding, RED-then-GREEN ───
    // (quick-260923-v4q). The two model-backed tests below were proven
    // FAILING (RED) against the pre-fix engine (which held the model's
    // enhanced spectrum verbatim and blended it in place on every held hop)
    // before this fix landed -- see the plan's SUMMARY for the captured
    // panic output.

    /// LCG xorshift step, mapped to roughly [-1.0, 1.0].
    fn lcg_next(seed: &mut u32) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        (*seed as f32 / u32::MAX as f32 - 0.5) * 2.0
    }

    /// Deterministic non-stationary test input: seeded LCG white noise
    /// (amplitude ~0.05) plus a 150 Hz harmonic complex whose on/off state
    /// changes every 7 hops.
    fn non_stationary_hop(hop_index: usize, seed: &mut u32) -> [f32; HOP] {
        let harmonic_on = (hop_index / 7) % 2 == 0;
        let mut out = [0f32; HOP];
        for (i, s) in out.iter_mut().enumerate() {
            let n = (hop_index * HOP + i) as f32;
            let t = n / 48_000.0;
            let noise = lcg_next(seed) * 0.05;
            let harmonic = if harmonic_on {
                0.1 * (2.0 * std::f32::consts::PI * 150.0 * t).sin()
                    + 0.05 * (2.0 * std::f32::consts::PI * 300.0 * t).sin()
            } else {
                0.0
            };
            *s = noise + harmonic;
        }
        out
    }

    #[test]
    fn held_hop_output_is_not_a_replay_of_the_previous_hop() {
        let Some(mut engine) = build_test_engine(DpdfnetVariant::Dpdfnet2) else {
            eprintln!(
                "[dpdfnet decimation] SKIP: pinned model/runtime unavailable in this environment"
            );
            return;
        };
        engine.set_strength(1.0);
        engine.set_mode(ProcessingMode::LowCpu);

        let mut seed = 0x1357_9BDFu32;
        let mut prev_output: Option<[f32; HOP]> = None;
        let mut evaluated = 0usize;
        for h in 0..80usize {
            let input = non_stationary_hop(h, &mut seed);
            let mut output = [0f32; HOP];
            engine.process(&input, &mut output);

            if h >= 24 && (h % 4 == 2 || h % 4 == 3) {
                let out_rms = (output.iter().map(|v| v * v).sum::<f32>() / HOP as f32).sqrt();
                if out_rms > 1e-7 {
                    if let Some(prev) = prev_output {
                        let diff_l2 = output
                            .iter()
                            .zip(prev.iter())
                            .map(|(a, b)| (a - b) * (a - b))
                            .sum::<f32>()
                            .sqrt();
                        let out_l2 = output.iter().map(|v| v * v).sum::<f32>().sqrt();
                        let rel = diff_l2 / out_l2;
                        evaluated += 1;
                        assert!(
                            rel > 1e-2,
                            "hop {h}: held-hop output looks like a replay of the previous hop (rel={rel})"
                        );
                    }
                }
            }
            prev_output = Some(output);
        }
        assert!(
            evaluated >= 10,
            "expected at least 10 evaluated held hops, got {evaluated}"
        );
        engine.teardown();
    }

    #[test]
    fn held_hops_do_not_mutate_the_raw_model_spectrum() {
        let Some(mut engine) = build_test_engine(DpdfnetVariant::Dpdfnet2) else {
            eprintln!(
                "[dpdfnet decimation] SKIP: pinned model/runtime unavailable in this environment"
            );
            return;
        };
        engine.set_strength(0.0);
        engine.set_mode(ProcessingMode::LowCpu);

        let mut seed = 0x1357_9BDFu32;
        let mut recorded: Option<[f32; SPEC_LEN]> = None;
        for h in 0..40usize {
            let input = non_stationary_hop(h, &mut seed);
            let mut output = [0f32; HOP];
            engine.process(&input, &mut output);

            if h % 4 == 0 {
                recorded = Some(engine.spec_e);
            } else if h >= 8 {
                assert_eq!(
                    engine.spec_e,
                    recorded.expect("an inferred hop must have run before hop 8"),
                    "held hop {h}: raw model spectrum spec_e must stay byte-identical across held hops"
                );
            }
        }
        engine.teardown();
    }

    #[test]
    fn gain_round_trip_recovers_real_mask() {
        // Arbitrary nonzero complex noisy bins.
        let mut noisy = [0f32; SPEC_LEN];
        for k in 0..FREQ_BINS {
            let n = k as f32;
            noisy[2 * k] = 0.3 + 0.01 * n;
            noisy[2 * k + 1] = -0.2 + 0.007 * n;
        }

        for g_true in [0.0f32, 0.25, 0.5, 1.0] {
            let mut enhanced = [0f32; SPEC_LEN];
            for i in 0..SPEC_LEN {
                enhanced[i] = g_true * noisy[i];
            }

            let mut gain = [0f32; FREQ_BINS];
            derive_gain(&enhanced, &noisy, &mut gain);
            for (k, &g) in gain.iter().enumerate() {
                assert!(
                    (g - g_true).abs() < 1e-6,
                    "bin {k}: derive_gain recovered {g}, expected {g_true}"
                );
            }

            let mut out = [0f32; SPEC_LEN];
            apply_gain(&gain, &noisy, &mut out);
            for i in 0..SPEC_LEN {
                let expected = g_true * noisy[i];
                let mag = expected.abs().max(1.0);
                assert!(
                    (out[i] - expected).abs() < 1e-6 * mag,
                    "index {i}: apply_gain produced {}, expected {expected}",
                    out[i]
                );
            }
        }
    }
}

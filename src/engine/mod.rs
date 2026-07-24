//! Noise suppression engine trait and implementations.
//!
//! Each engine wraps a specific noise suppression library behind the common
//! [`NoiseEngine`] trait. Only one engine is active at a time. The user-facing
//! "Strength" slider (0.0..=1.0) is mapped per-engine to internal DSP parameters.

pub mod deepfilter;
#[cfg(feature = "dpdfnet")]
pub mod dpdfnet;
#[cfg(feature = "dpdfnet-experimental")]
pub mod dpdfnet_experimental;
pub mod dpdfnet_policy;
pub mod khip;
pub mod rnnoise;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The type of noise suppression engine.
///
/// Variant order mirrors the intended selector/tray row order (D-07):
/// RNNoise, DeepFilterNet, DPDFNet-2, DPDFNet-8, Khip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EngineType {
    /// Lightweight baseline — links upstream librnnoise via FFI.
    RNNoise,
    /// High-quality default — wraps DeepFilterNet via libdf.
    DeepFilterNet,
    /// Production DPDFNet, lighter/faster variant (D-01). Never default in
    /// this phase (D-09 gates any future default change).
    #[serde(rename = "Dpdfnet2")]
    Dpdfnet2,
    /// Production DPDFNet, larger/higher-capacity variant (D-01). Never
    /// default-eligible (D-09) — user-selectable only.
    #[serde(rename = "Dpdfnet8")]
    Dpdfnet8,
    /// Advanced/experimental — dynamically loads user-supplied Khip library.
    Khip,
}

impl EngineType {
    /// Every supported engine, in the stable, total order that mirrors the
    /// intended selector/tray row order (D-07). Used by config serialization
    /// (per-engine strength seeding), migration, and tests so no engine —
    /// including Khip — is a special case requiring separate enumeration.
    pub const ALL: [EngineType; 5] = [
        EngineType::RNNoise,
        EngineType::DeepFilterNet,
        EngineType::Dpdfnet2,
        EngineType::Dpdfnet8,
        EngineType::Khip,
    ];

    /// Iterate every supported engine type in the stable order above.
    pub fn all() -> impl Iterator<Item = EngineType> {
        Self::ALL.into_iter()
    }

    /// Short, untranslated brand-name label (RNNoise/DeepFilterNet/DPDFNet-2/
    /// DPDFNet-8/Khip are proper nouns, matching `window::engine_label`'s
    /// established untranslated convention). Kept here — rather than only in
    /// the `gui`-gated `src/ui/window.rs` — so non-GUI code (e.g. a fallback
    /// notice built for `UiState`, added by Task 2) can name an engine
    /// without depending on the `gui` feature.
    pub fn short_name(self) -> &'static str {
        match self {
            EngineType::RNNoise => "RNNoise",
            EngineType::DeepFilterNet => "DeepFilterNet",
            EngineType::Dpdfnet2 => "DPDFNet-2",
            EngineType::Dpdfnet8 => "DPDFNet-8",
            EngineType::Khip => "Khip",
        }
    }
}

/// Processing mode controlling the quality/CPU trade-off.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ProcessingMode {
    /// Default balance between quality and CPU usage.
    Balanced,
    /// Reduced quality for lower CPU consumption.
    LowCpu,
    /// Best possible quality regardless of CPU cost.
    MaxQuality,
}

/// Common interface for all noise suppression engines.
///
/// Implementations must be `Send` so they can be owned by the audio thread.
/// All methods receive `&mut self` because engines carry internal state
/// (model weights, ring buffers, etc.).
pub trait NoiseEngine: Send {
    /// Initialize the engine for the given sample rate.
    fn init(&mut self, sample_rate: u32) -> Result<()>;

    /// Process one buffer of audio.
    ///
    /// `input` and `output` have the same length. The engine reads from
    /// `input` and writes the cleaned signal to `output`. This runs on the
    /// audio thread and must be lock-free.
    fn process(&mut self, input: &[f32], output: &mut [f32]);

    /// Set the normalized suppression strength (0.0..=1.0).
    fn set_strength(&mut self, strength: f32);

    /// Set the processing mode (quality vs. CPU trade-off).
    fn set_mode(&mut self, mode: ProcessingMode);

    /// Report the engine's processing latency in frames at the current
    /// sample rate.
    fn latency_frames(&self) -> u32;

    /// Release resources held by the engine.
    fn teardown(&mut self);
}

/// Check whether a given engine type is available on this system.
///
/// - RNNoise is available when the `rnnoise` feature is enabled.
/// - DeepFilterNet is available when the `deepfilter` feature is enabled.
/// - Khip is only available if the user has installed the library.
///
/// Without their respective features, RNNoise and DeepFilterNet still exist
/// as types but `init()` will return an error at runtime.
pub fn is_engine_available(engine: EngineType) -> bool {
    match engine {
        EngineType::RNNoise => cfg!(feature = "rnnoise"),
        EngineType::DeepFilterNet => cfg!(feature = "deepfilter"),
        EngineType::Dpdfnet2 => dpdfnet_is_available(EngineType::Dpdfnet2),
        EngineType::Dpdfnet8 => dpdfnet_is_available(EngineType::Dpdfnet8),
        EngineType::Khip => khip::KhipEngine::is_available(),
    }
}

/// Point `ORT_DYLIB_PATH` at the bundled `$APPDIR/usr/lib/libonnxruntime.so`
/// if it isn't already set (T-15.1-03: resolve only the AppImage-owned
/// runtime, never a system fallback). Never clobbers an existing value — a
/// developer/test override always wins. Without this, [`dpdfnet::DpdfnetEngine`]
/// has no way to discover the bundled runtime at all in the real shipped
/// AppImage, since it only reads the env var (mirrors
/// `dpdfnet_experimental`'s established contract) and nothing in
/// `src/app.rs`/AppRun sets it from `$APPDIR` today.
#[cfg(feature = "dpdfnet")]
fn dpdfnet_ensure_ort_dylib_env(appdir: &std::path::Path) {
    if std::env::var_os("ORT_DYLIB_PATH").is_some() {
        return;
    }
    let candidate = appdir.join("usr/lib/libonnxruntime.so");
    if candidate.is_file() {
        // SAFETY: called only from the single-threaded engine
        // construction/availability-check path, before any DPDFNet session
        // exists — mirrors `khip`'s established `env::set_var` usage in
        // `KhipEngine::init` (src/engine/khip/mod.rs).
        unsafe {
            std::env::set_var("ORT_DYLIB_PATH", &candidate);
        }
    }
}

/// Resolve a bundled DPDFNet variant's model path, restricted to
/// `$APPDIR/usr/share/cleanmic/models` (T-15.1-03) — the stricter allowlist
/// the shipping factory applies on top of [`dpdfnet::DpdfnetEngine`]'s own
/// general absolute/no-traversal/regular-file validation. Returns an error
/// (never panics) when `APPDIR` is unset or the file is missing, so a
/// dev/test environment without an AppImage degrades to "unavailable"
/// rather than a hard failure.
#[cfg(feature = "dpdfnet")]
fn dpdfnet_model_path(variant: dpdfnet::DpdfnetVariant) -> Result<std::path::PathBuf> {
    let appdir = std::env::var_os("APPDIR").ok_or_else(|| {
        anyhow::anyhow!("APPDIR is not set; DPDFNet requires the AppImage runtime")
    })?;
    let appdir = std::path::PathBuf::from(appdir);
    dpdfnet_ensure_ort_dylib_env(&appdir);
    let path = appdir
        .join("usr/share/cleanmic/models")
        .join(variant.model_filename());
    anyhow::ensure!(
        path.is_file(),
        "DPDFNet model not found at {}",
        path.display()
    );
    Ok(path)
}

#[cfg(feature = "dpdfnet")]
fn dpdfnet_is_available(engine: EngineType) -> bool {
    let variant = match engine {
        EngineType::Dpdfnet2 => dpdfnet::DpdfnetVariant::Dpdfnet2,
        EngineType::Dpdfnet8 => dpdfnet::DpdfnetVariant::Dpdfnet8,
        _ => return false,
    };
    dpdfnet_model_path(variant).is_ok() && dpdfnet::is_dylib_available()
}

#[cfg(not(feature = "dpdfnet"))]
fn dpdfnet_is_available(_engine: EngineType) -> bool {
    false
}

/// Construct the requested DPDFNet variant's production engine (feature
/// `dpdfnet`). A failure here (missing model/runtime, bad session) never
/// touches the other variant's state (D-02) — this function only ever
/// resolves and constructs the ONE requested variant.
#[cfg(feature = "dpdfnet")]
fn create_dpdfnet_engine(engine_type: EngineType) -> Result<Box<dyn NoiseEngine>> {
    let variant = match engine_type {
        EngineType::Dpdfnet2 => dpdfnet::DpdfnetVariant::Dpdfnet2,
        EngineType::Dpdfnet8 => dpdfnet::DpdfnetVariant::Dpdfnet8,
        _ => unreachable!("create_dpdfnet_engine called with a non-DPDFNet engine type"),
    };
    let model_path = dpdfnet_model_path(variant)?;
    Ok(Box::new(dpdfnet::DpdfnetEngine::new(variant, model_path)))
}

#[cfg(not(feature = "dpdfnet"))]
fn create_dpdfnet_engine(_engine_type: EngineType) -> Result<Box<dyn NoiseEngine>> {
    anyhow::bail!("DPDFNet support is not compiled in (missing `dpdfnet` feature)")
}

/// Create and initialize a noise engine of the given type.
///
/// Returns a boxed trait object ready to process audio at 48 kHz.
/// For Khip, this will fail if the library is not installed.
pub fn create_engine(engine_type: EngineType) -> Result<Box<dyn NoiseEngine>> {
    let mut engine: Box<dyn NoiseEngine> = match engine_type {
        EngineType::RNNoise => Box::new(rnnoise::RNNoiseEngine::new()),
        EngineType::DeepFilterNet => Box::new(deepfilter::DeepFilterEngine::new()),
        EngineType::Dpdfnet2 => create_dpdfnet_engine(EngineType::Dpdfnet2)?,
        EngineType::Dpdfnet8 => create_dpdfnet_engine(EngineType::Dpdfnet8)?,
        EngineType::Khip => Box::new(khip::KhipEngine::new()),
    };
    engine.init(48_000)?;
    Ok(engine)
}

/// No-op engine that copies input to output unchanged.
///
/// Used as the ultimate fallback when all real engines fail to initialize (D-02).
pub struct PassthroughEngine;

impl NoiseEngine for PassthroughEngine {
    fn init(&mut self, _sample_rate: u32) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let len = input.len().min(output.len());
        output[..len].copy_from_slice(&input[..len]);
    }

    fn set_strength(&mut self, _strength: f32) {}

    fn set_mode(&mut self, _mode: ProcessingMode) {}

    fn latency_frames(&self) -> u32 {
        0
    }

    fn teardown(&mut self) {}
}

/// Create an engine with fallback chain per D-02:
/// Khip -> DeepFilter -> RNNoise -> passthrough.
///
/// Returns the created engine and the actual engine type used (which may differ
/// from `preferred` if fallback occurred). When all real engines fail, returns
/// a [`PassthroughEngine`] that copies audio unchanged.
pub fn create_engine_with_fallback(preferred: EngineType) -> (Box<dyn NoiseEngine>, EngineType) {
    let chain: &[EngineType] = match preferred {
        EngineType::Khip => &[
            EngineType::Khip,
            EngineType::DeepFilterNet,
            EngineType::RNNoise,
        ],
        EngineType::DeepFilterNet => &[EngineType::DeepFilterNet, EngineType::RNNoise],
        // D-02: a DPDFNet variant's failure must not fall back to the OTHER
        // DPDFNet variant (they are independently gated, not interchangeable
        // quality tiers) — fall back to DeepFilterNet/RNNoise instead, same
        // as DeepFilterNet's own chain.
        EngineType::Dpdfnet2 => &[
            EngineType::Dpdfnet2,
            EngineType::DeepFilterNet,
            EngineType::RNNoise,
        ],
        EngineType::Dpdfnet8 => &[
            EngineType::Dpdfnet8,
            EngineType::DeepFilterNet,
            EngineType::RNNoise,
        ],
        EngineType::RNNoise => &[EngineType::RNNoise],
    };
    for &engine_type in chain {
        match create_engine(engine_type) {
            Ok(engine) => return (engine, engine_type),
            Err(e) => log::warn!("failed to create {:?} engine: {}", engine_type, e),
        }
    }
    log::error!("all engines failed — falling back to passthrough (no noise suppression)");
    // Return preferred type so config retains the user's selection.
    (Box::new(PassthroughEngine), preferred)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial engine that copies input to output unchanged.
    /// Used to verify the trait compiles and works.
    struct PassthroughEngine {
        initialized: bool,
    }

    impl PassthroughEngine {
        fn new() -> Self {
            Self { initialized: false }
        }
    }

    impl NoiseEngine for PassthroughEngine {
        fn init(&mut self, _sample_rate: u32) -> Result<()> {
            self.initialized = true;
            Ok(())
        }

        fn process(&mut self, input: &[f32], output: &mut [f32]) {
            output.copy_from_slice(input);
        }

        fn set_strength(&mut self, _strength: f32) {}

        fn set_mode(&mut self, _mode: ProcessingMode) {}

        fn latency_frames(&self) -> u32 {
            0
        }

        fn teardown(&mut self) {
            self.initialized = false;
        }
    }

    #[test]
    fn passthrough_engine_copies_input() {
        let mut engine = PassthroughEngine::new();
        engine.init(48000).unwrap();

        let input = [0.1_f32, 0.2, 0.3, 0.4];
        let mut output = [0.0_f32; 4];
        engine.process(&input, &mut output);

        assert_eq!(input, output);
        engine.teardown();
        assert!(!engine.initialized);
    }

    #[test]
    fn engine_type_serde_roundtrip() {
        for engine_type in [
            EngineType::RNNoise,
            EngineType::DeepFilterNet,
            EngineType::Dpdfnet2,
            EngineType::Dpdfnet8,
            EngineType::Khip,
        ] {
            #[derive(Serialize, Deserialize, PartialEq, Debug)]
            struct Wrapper {
                engine: EngineType,
            }
            let original = Wrapper {
                engine: engine_type,
            };
            let serialized = toml::to_string(&original).unwrap();
            let deserialized: Wrapper = toml::from_str(&serialized).unwrap();
            assert_eq!(original, deserialized);
        }
    }

    #[test]
    fn processing_mode_serde_roundtrip() {
        for mode in [
            ProcessingMode::Balanced,
            ProcessingMode::LowCpu,
            ProcessingMode::MaxQuality,
        ] {
            #[derive(Serialize, Deserialize, PartialEq, Debug)]
            struct Wrapper {
                mode: ProcessingMode,
            }
            let original = Wrapper { mode };
            let serialized = toml::to_string(&original).unwrap();
            let deserialized: Wrapper = toml::from_str(&serialized).unwrap();
            assert_eq!(original, deserialized);
        }
    }

    #[test]
    fn create_engine_rnnoise_succeeds() {
        let engine = create_engine(EngineType::RNNoise);
        assert!(engine.is_ok());
    }

    /// Requires libdeep_filter_ladspa.so. Marked #[ignore] — parallel LADSPA
    /// init is not thread-safe. Run with: cargo test -- --ignored
    #[cfg(feature = "deepfilter")]
    #[test]
    #[ignore]
    fn create_engine_deepfilter_succeeds() {
        if !deepfilter::is_available() {
            return; // Library not installed; skip.
        }
        let engine = create_engine(EngineType::DeepFilterNet);
        assert!(engine.is_ok(), "DeepFilterNet init failed");
    }

    #[cfg(feature = "deepfilter")]
    #[test]
    fn create_engine_deepfilter_fails_when_unavailable() {
        if deepfilter::is_available() {
            return; // Library is installed; skip the "unavailable" path.
        }
        let engine = create_engine(EngineType::DeepFilterNet);
        assert!(engine.is_err());
    }

    #[cfg(not(feature = "deepfilter"))]
    #[test]
    fn create_engine_deepfilter_fails_without_feature() {
        let engine = create_engine(EngineType::DeepFilterNet);
        assert!(engine.is_err());
    }

    #[test]
    fn create_engine_khip_fails_when_unavailable() {
        if khip::KhipEngine::is_available() {
            return; // Library is installed; skip the "unavailable" path.
        }
        let engine = create_engine(EngineType::Khip);
        assert!(engine.is_err());
    }

    /// Without the `dpdfnet` feature, both DPDFNet variants must fail to
    /// construct rather than silently degrade — mirrors
    /// `create_engine_deepfilter_fails_without_feature`.
    #[cfg(not(feature = "dpdfnet"))]
    #[test]
    fn create_engine_dpdfnet_fails_without_feature() {
        assert!(create_engine(EngineType::Dpdfnet2).is_err());
        assert!(create_engine(EngineType::Dpdfnet8).is_err());
    }

    /// D-02: DPDFNet-2 and DPDFNet-8 must be independently gated — one
    /// variant being unavailable (no bundled model/`APPDIR`) must not affect
    /// `is_engine_available`'s report for the other.
    #[cfg(feature = "dpdfnet")]
    #[test]
    fn dpdfnet_variants_are_independently_reported_unavailable_without_appdir() {
        // SAFETY: test-only; no other test in this binary reads/writes APPDIR
        // concurrently with this assertion.
        unsafe {
            std::env::remove_var("APPDIR");
        }
        assert!(!is_engine_available(EngineType::Dpdfnet2));
        assert!(!is_engine_available(EngineType::Dpdfnet8));
        assert!(create_engine(EngineType::Dpdfnet2).is_err());
        assert!(create_engine(EngineType::Dpdfnet8).is_err());
    }

    // ── EngineType::all() / total ordering (Task 1) ─────────────────────────

    #[test]
    fn engine_type_all_lists_every_variant_in_selector_order() {
        let all: Vec<EngineType> = EngineType::all().collect();
        assert_eq!(
            all,
            vec![
                EngineType::RNNoise,
                EngineType::DeepFilterNet,
                EngineType::Dpdfnet2,
                EngineType::Dpdfnet8,
                EngineType::Khip,
            ]
        );
    }

    #[test]
    fn engine_type_has_a_total_order() {
        // Ord must agree with the declared ALL/all() order — used as
        // BTreeMap keys by Config::strengths (and, once Task 2 lands,
        // UiState::availability).
        let all: Vec<EngineType> = EngineType::all().collect();
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(all, sorted, "EngineType::all() must already be sorted");
    }

    #[test]
    fn engine_type_short_names_are_distinct_proper_nouns() {
        let names: Vec<&str> = EngineType::all().map(EngineType::short_name).collect();
        assert_eq!(
            names,
            vec!["RNNoise", "DeepFilterNet", "DPDFNet-2", "DPDFNet-8", "Khip"]
        );
    }
}

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

use crate::tr;

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
    /// notice built for `UiState`) can name an engine without depending on
    /// the `gui` feature.
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

/// Why an engine is or is not available, in machine-checkable form
/// (T-15.1-07/T-15.1-08). Supersedes ad hoc booleans so no engine —
/// including Khip, which previously had the only dedicated availability
/// field (`UiState::khip_available`, `TrayState::khip_available`) — is a
/// special case in the shared runtime-policy model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvailabilityReason {
    /// The engine is compiled in and its runtime dependency (library/model)
    /// was found — ready to construct.
    Available,
    /// The crate was built without this engine's Cargo feature; `init()`
    /// would fail even though the `EngineType` variant itself always exists.
    FeatureDisabled,
    /// Compiled in, but the runtime library/model/dylib was not found on
    /// this system (e.g. Khip not installed, DPDFNet model/runtime missing).
    RuntimeMissing,
}

/// Truthful per-engine availability: a boolean plus the reason behind it.
/// One variant's unavailability never implies anything about another's
/// (D-02) — every [`engine_availability`] call resolves exactly one engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineAvailability {
    pub available: bool,
    pub reason: AvailabilityReason,
}

/// Resolve one engine's truthful availability and reason.
///
/// - RNNoise/DeepFilterNet: available iff their Cargo feature is compiled
///   in (`init()` would otherwise fail at runtime even though the type
///   exists).
/// - DPDFNet-2/DPDFNet-8: available iff the `dpdfnet` feature is compiled in
///   AND the bundled model/runtime resolve under `$APPDIR` — independently
///   per variant (D-02); one variant's missing asset never affects the
///   other's report.
/// - Khip: available iff the user-supplied library was detected at runtime.
pub fn engine_availability(engine: EngineType) -> EngineAvailability {
    use AvailabilityReason::{Available, FeatureDisabled, RuntimeMissing};
    match engine {
        EngineType::RNNoise => {
            let available = cfg!(feature = "rnnoise");
            EngineAvailability {
                available,
                reason: if available {
                    Available
                } else {
                    FeatureDisabled
                },
            }
        }
        EngineType::DeepFilterNet => {
            let available = cfg!(feature = "deepfilter");
            EngineAvailability {
                available,
                reason: if available {
                    Available
                } else {
                    FeatureDisabled
                },
            }
        }
        EngineType::Dpdfnet2 | EngineType::Dpdfnet8 => {
            if !cfg!(feature = "dpdfnet") {
                return EngineAvailability {
                    available: false,
                    reason: FeatureDisabled,
                };
            }
            let available = dpdfnet_is_available(engine);
            EngineAvailability {
                available,
                reason: if available { Available } else { RuntimeMissing },
            }
        }
        EngineType::Khip => {
            let available = khip::KhipEngine::is_available();
            EngineAvailability {
                available,
                reason: if available { Available } else { RuntimeMissing },
            }
        }
    }
}

/// Independent five-engine availability map (D-02/D-08). Building this from
/// [`engine_availability`] per [`EngineType::all`] guarantees one variant's
/// failure can never leak into another's entry.
pub fn all_engine_availability() -> std::collections::BTreeMap<EngineType, EngineAvailability> {
    EngineType::all()
        .map(|engine| (engine, engine_availability(engine)))
        .collect()
}

/// Check whether a given engine type is available on this system.
///
/// Thin boolean wrapper over [`engine_availability`], kept for existing
/// call sites (`src/ui/window.rs`, `src/tray.rs`, and this module's own
/// factory) that only need the yes/no answer.
pub fn is_engine_available(engine: EngineType) -> bool {
    engine_availability(engine).available
}

/// Build a translated, human-readable fallback notice when `active` differs
/// from `requested` (T-15.1-07, D-11). Returns `None` when no fallback
/// occurred. The caller must never claim `requested` is active while this is
/// `Some` — e.g. a migrated DPDFNet-2 selection that failed to initialize
/// and fell back to DeepFilterNet.
pub fn fallback_notice(requested: EngineType, active: EngineType) -> Option<String> {
    if requested == active {
        return None;
    }
    Some(format!(
        "{} {}",
        tr!("Unable to start the selected engine — using instead:"),
        active.short_name()
    ))
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
        // BTreeMap keys by Config::strengths and UiState::availability.
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

    // ── EngineAvailability / all_engine_availability (Task 2) ───────────────

    #[test]
    fn all_engine_availability_covers_every_engine_exactly_once() {
        let map = all_engine_availability();
        assert_eq!(map.len(), 5);
        for engine in EngineType::all() {
            assert!(map.contains_key(&engine), "{engine:?} missing from map");
        }
    }

    #[test]
    fn is_engine_available_matches_engine_availability_bool() {
        for engine in EngineType::all() {
            assert_eq!(
                is_engine_available(engine),
                engine_availability(engine).available
            );
        }
    }

    /// D-02: each DPDFNet variant gets its own independently-computed
    /// `EngineAvailability` entry in the shared map — never derived from,
    /// aliased to, or defaulted from the other variant's result.
    #[test]
    fn dpdfnet_variants_have_independent_availability_entries() {
        let map = all_engine_availability();
        let dpdfnet2 = map.get(&EngineType::Dpdfnet2).expect("Dpdfnet2 entry");
        let dpdfnet8 = map.get(&EngineType::Dpdfnet8).expect("Dpdfnet8 entry");
        // Both independently equal what a direct, single-engine call
        // computes — i.e. the map is not a shared/aliased fallback value.
        assert_eq!(*dpdfnet2, engine_availability(EngineType::Dpdfnet2));
        assert_eq!(*dpdfnet8, engine_availability(EngineType::Dpdfnet8));
    }

    // ── fallback_notice (D-11) ───────────────────────────────────────────────

    #[test]
    fn fallback_notice_is_none_when_requested_equals_active() {
        assert_eq!(
            fallback_notice(EngineType::DeepFilterNet, EngineType::DeepFilterNet),
            None
        );
    }

    #[test]
    fn fallback_notice_is_some_and_names_active_engine_when_they_differ() {
        let notice =
            fallback_notice(EngineType::Dpdfnet2, EngineType::DeepFilterNet).expect("some notice");
        assert!(
            notice.contains("DeepFilterNet"),
            "notice must truthfully name the ACTIVE engine, got: {notice}"
        );
        assert!(
            !notice.contains("DPDFNet-2"),
            "notice must never claim the requested (failed) engine, got: {notice}"
        );
    }
}

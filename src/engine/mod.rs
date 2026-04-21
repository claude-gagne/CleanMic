//! Noise suppression engine trait and implementations.
//!
//! Each engine wraps a specific noise suppression library behind the common
//! [`NoiseEngine`] trait. Only one engine is active at a time. The user-facing
//! "Strength" slider (0.0..=1.0) is mapped per-engine to internal DSP parameters.

pub mod deepfilter;
pub mod khip;
pub mod rnnoise;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The type of noise suppression engine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum EngineType {
    /// Lightweight baseline — links upstream librnnoise via FFI.
    RNNoise,
    /// High-quality default — wraps DeepFilterNet via libdf.
    DeepFilterNet,
    /// Advanced/experimental — dynamically loads user-supplied Khip library.
    Khip,
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
        EngineType::Khip => khip::KhipEngine::is_available(),
    }
}

/// Create and initialize a noise engine of the given type.
///
/// Returns a boxed trait object ready to process audio at 48 kHz.
/// For Khip, this will fail if the library is not installed.
pub fn create_engine(engine_type: EngineType) -> Result<Box<dyn NoiseEngine>> {
    let mut engine: Box<dyn NoiseEngine> = match engine_type {
        EngineType::RNNoise => Box::new(rnnoise::RNNoiseEngine::new()),
        EngineType::DeepFilterNet => Box::new(deepfilter::DeepFilterEngine::new()),
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
        EngineType::Khip => &[EngineType::Khip, EngineType::DeepFilterNet, EngineType::RNNoise],
        EngineType::DeepFilterNet => &[EngineType::DeepFilterNet, EngineType::RNNoise],
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
}

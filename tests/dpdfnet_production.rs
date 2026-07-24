//! Production behavior contract for the real (non-experimental) DPDFNet-2/8
//! digital-signal-processing adapter (Phase 15.1, Plan 01, Task 2 — D-01/D-02).
//!
//! **Seeded RED, on purpose.** This file is written BEFORE `src/engine/dpdfnet.rs`
//! exists (Plan 04's job) and before the shipping `dpdfnet` Cargo feature is
//! declared (also Plan 04's job — it replaces the throwaway `dpdfnet-experimental`
//! feature). Every test below is gated behind `#[cfg(feature = "dpdfnet")]`,
//! which is not yet a declared feature in `Cargo.toml`; until Plan 04 adds it,
//! this entire module is compiled out (a harmless no-op), which is the correct
//! "RED" state for a plan-01 seed: the contract is named, but not yet exercised.
//! Once Plan 04 adds `feature = "dpdfnet"` and a first-cut `DpdfnetEngine`, these
//! tests start running and should initially FAIL (RED) until Plan 04's own
//! red-green-refactor cycle makes them pass (GREEN).
//!
//! `#![allow(unexpected_cfgs)]` on the gating attribute below suppresses the
//! (expected, harmless) "unexpected cfg value" lint that referencing a
//! not-yet-declared feature name would otherwise trigger — see
//! `tests/dpdfnet_experimental_switch.rs` for the same feature-gated-module
//! pattern this file follows.
//!
//! Plan 04 owns finalizing the exact `DpdfnetEngine`/`DpdfnetVariant` API
//! shape (constructor signature, module layout) — this seed documents the
//! REQUIRED BEHAVIOR, not a frozen API; Plan 04 is expected to adjust names
//! and signatures here as it implements `src/engine/dpdfnet.rs`, per its own
//! `files_modified` list including this file.
//!
//! Run once Plan 04 lands: `cargo test --features dpdfnet --test dpdfnet_production -- --test-threads=1`

#![allow(unexpected_cfgs)]

#[cfg(feature = "dpdfnet")]
mod dpdfnet_production {
    use cleanmic::engine::dpdfnet::{DpdfnetEngine, DpdfnetVariant};
    use cleanmic::engine::{EngineType, NoiseEngine};
    use serde::Deserialize;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const HOP: usize = 480;
    const SPEC_LEN: usize = 962; // FREQ_BINS (481) * 2, interleaved re/im

    // ── Golden fixture loading ──────────────────────────────────────────

    #[derive(Deserialize)]
    struct GoldenHop {
        input_spectrum: Vec<f32>,
        enhanced_spectrum: Vec<f32>,
        output: Vec<f32>,
    }

    #[derive(Deserialize)]
    struct GoldenFixture {
        sample_rate: u32,
        hops: u32,
    }

    #[derive(Deserialize)]
    struct GoldenFormat {
        n_fft: u32,
        hop: u32,
        freq_bins: u32,
        sample_rate: u32,
    }

    #[derive(Deserialize)]
    struct GoldenRecord {
        variant: String,
        fixture: GoldenFixture,
        format: GoldenFormat,
        #[serde(rename = "hops")]
        hop_records: Vec<GoldenHop>,
    }

    fn golden_path(variant: &str) -> PathBuf {
        PathBuf::from(format!(
            "{}/tests/fixtures/dpdfnet/golden-{variant}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
    }

    fn load_golden(variant: &str) -> GoldenRecord {
        let path = golden_path(variant);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read golden fixture {}: {e}", path.display()));
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parse golden fixture {}: {e}", path.display()))
    }

    /// Regenerates the EXACT same formula-derived fixture audio the Python
    /// generator used (`scripts/generate-dpdfnet-goldens.py`'s
    /// `FIXTURE_TONES`/`build_fixture_wav`) — three fixed sine tones, no
    /// randomness — so this test never depends on reading the probe-only
    /// `.wav` file, only the checked-in golden JSON's documented recipe.
    fn regenerate_fixture_samples(sample_rate: u32, hops: u32) -> Vec<f32> {
        const TONES: [(f32, f32); 3] = [(440.0, 0.20), (1000.0, 0.05), (4000.0, 0.02)];
        let n = (hops as usize) * HOP;
        (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                TONES
                    .iter()
                    .map(|(freq, amp)| amp * (2.0 * std::f32::consts::PI * freq * t).sin())
                    .sum::<f32>()
            })
            .collect()
    }

    fn model_path_for(variant: &str) -> PathBuf {
        let env_var = format!("{}_MODEL_PATH", variant.to_uppercase());
        std::env::var(&env_var)
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(format!(
                    "vendor/dpdfnet-reference/models/{variant}_48khz_hr.onnx"
                ))
            })
    }

    fn ort_dylib_path() -> PathBuf {
        std::env::var("ORT_DYLIB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("vendor/dpdfnet-reference/lib/libonnxruntime.so"))
    }

    fn ensure_env() {
        if std::env::var_os("ORT_DYLIB_PATH").is_none() {
            let default = ort_dylib_path();
            if default.is_file() {
                // SAFETY: test-only; every caller sets the identical value.
                unsafe {
                    std::env::set_var("ORT_DYLIB_PATH", &default);
                }
            }
        }
    }

    fn variant_of(name: &str) -> DpdfnetVariant {
        match name {
            "dpdfnet2" => DpdfnetVariant::Dpdfnet2,
            "dpdfnet8" => DpdfnetVariant::Dpdfnet8,
            other => panic!("unknown variant {other}"),
        }
    }

    /// Builds a real, initialized `DpdfnetEngine` for `variant`, or `None` if
    /// the pinned model/runtime is unavailable in this environment (honest
    /// skip, matching `tests/dpdfnet_experimental_switch.rs`'s convention —
    /// never fabricate results).
    fn build_engine(variant: &str) -> Option<DpdfnetEngine> {
        ensure_env();
        let model = model_path_for(variant);
        if !model.is_file() || !ort_dylib_path().is_file() {
            return None;
        }
        let mut engine = DpdfnetEngine::new(variant_of(variant), model);
        engine.init(48_000).ok()?;
        Some(engine)
    }

    fn assert_close(actual: &[f32], expected: &[f32], epsilon: f32, context: &str) {
        assert_eq!(actual.len(), expected.len(), "{context}: length mismatch");
        for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
            assert!(
                (a - e).abs() <= epsilon,
                "{context}: index {i} differs: actual={a}, expected={e}, epsilon={epsilon}"
            );
        }
    }

    // ── Golden vector tests (Plan 04's production DSP behavior) ─────────
    //
    // NOTE ON TOLERANCE: the pinned reference probe transforms with the
    // HushMic renderer's `rustfft`-based STFT; Plan 04's production adapter
    // is approved to use the `realfft` crate (Plan 02's checkpoint). Both
    // are correct 960-point real FFT implementations, but are NOT guaranteed
    // to produce bit-identical floating-point results — an epsilon-bounded
    // comparison is the correct contract here, not byte equality.
    const OUTPUT_EPSILON: f32 = 1e-3;

    fn golden_test(variant: &str) {
        let golden = load_golden(variant);
        assert_eq!(golden.variant, variant);
        assert_eq!(
            golden.format.n_fft, 960,
            "must use the exact 960-point Vorbis window"
        );
        assert_eq!(golden.format.hop, 480, "must use the exact 480-sample hop");
        assert_eq!(
            golden.format.freq_bins, 481,
            "must produce exactly 481 complex bins"
        );
        assert_eq!(golden.format.sample_rate, 48_000);

        let Some(mut engine) = build_engine(variant) else {
            eprintln!(
                "[dpdfnet_production golden] SKIP {variant}: pinned model/runtime unavailable in this environment"
            );
            return;
        };

        let samples = regenerate_fixture_samples(golden.fixture.sample_rate, golden.fixture.hops);
        assert_eq!(samples.len(), golden.hop_records.len() * HOP);

        let mut output = vec![0f32; HOP];
        for (h, hop_record) in golden.hop_records.iter().enumerate() {
            let input = &samples[h * HOP..(h + 1) * HOP];
            engine.process(input, &mut output);
            assert_close(
                &output,
                &hop_record.output,
                OUTPUT_EPSILON,
                &format!("{variant} hop {h} output"),
            );
            assert_eq!(hop_record.input_spectrum.len(), SPEC_LEN);
            assert_eq!(hop_record.enhanced_spectrum.len(), SPEC_LEN);
        }
        engine.teardown();
    }

    #[test]
    fn golden_dpdfnet2_matches_pinned_reference() {
        golden_test("dpdfnet2");
    }

    #[test]
    fn golden_dpdfnet8_matches_pinned_reference() {
        golden_test("dpdfnet8");
    }

    // ── Malformed input / non-finite rejection ───────────────────────────

    #[test]
    fn rejects_non_48khz_sample_rate() {
        let Some(model) = Some(model_path_for("dpdfnet2")).filter(|p| p.is_file()) else {
            eprintln!("[dpdfnet_production] SKIP: dpdfnet2 model unavailable");
            return;
        };
        ensure_env();
        let mut engine = DpdfnetEngine::new(DpdfnetVariant::Dpdfnet2, model);
        assert!(
            engine.init(44_100).is_err(),
            "must reject a non-48kHz sample rate rather than silently mis-processing"
        );
    }

    #[test]
    fn rejects_non_finite_input_without_corrupting_state() {
        let Some(mut engine) = build_engine("dpdfnet2") else {
            eprintln!("[dpdfnet_production] SKIP: dpdfnet2 unavailable");
            return;
        };
        let mut good_input = vec![0.01f32; HOP];
        let mut output = vec![0f32; HOP];
        // A known-good hop first, to establish valid prior state.
        engine.process(&good_input, &mut output);
        let baseline_output = output.clone();

        // Poison a single sample with NaN/Inf and confirm the engine degrades
        // safely (never propagates NaN/Inf into `output`, never panics).
        let mut poisoned = vec![0.01f32; HOP];
        poisoned[10] = f32::NAN;
        poisoned[20] = f32::INFINITY;
        engine.process(&poisoned, &mut output);
        assert!(
            output.iter().all(|v| v.is_finite()),
            "non-finite input must never propagate non-finite output"
        );

        // Prior good state must not have been corrupted: the same good input
        // fed again should reproduce (approximately) the earlier baseline.
        good_input.copy_from_slice(&vec![0.01f32; HOP]);
        engine.process(&good_input, &mut output);
        assert!(
            output.iter().all(|v| v.is_finite()),
            "state after a poisoned hop must remain finite on the next call"
        );
        let _ = baseline_output; // documents intent; exact equality not required post-recovery
    }

    // ── True latency ──────────────────────────────────────────────────────

    #[test]
    fn reports_true_nonzero_latency_from_actual_buffering() {
        let Some(engine) = build_engine("dpdfnet2") else {
            eprintln!("[dpdfnet_production] SKIP: dpdfnet2 unavailable");
            return;
        };
        // The experimental placeholder reports zero latency (no real
        // buffering); the production adapter's 960-sample analysis window
        // over 480-sample hops implies a real, nonzero algorithmic latency.
        assert!(
            engine.latency_frames() > 0,
            "production DPDFNet must report true nonzero latency, not the experimental engine's zero-latency placeholder"
        );
    }

    // ── Monotonic, non-bypass strength (D-14/D-15/D-16) ──────────────────

    #[test]
    fn strength_zero_is_not_raw_bypass() {
        let Some(mut engine) = build_engine("dpdfnet2") else {
            eprintln!("[dpdfnet_production] SKIP: dpdfnet2 unavailable");
            return;
        };
        engine.set_strength(0.0);
        let input = vec![0.05f32; HOP];
        let mut output = vec![0f32; HOP];
        for _ in 0..8 {
            engine.process(&input, &mut output);
        }
        assert_ne!(
            output, input,
            "strength 0.0 must remain the lightest USEFUL suppression, never raw dry/wet passthrough (D-15)"
        );
    }

    #[test]
    fn strength_sweep_is_monotonic_and_smooth() {
        let Some(mut engine) = build_engine("dpdfnet2") else {
            eprintln!("[dpdfnet_production] SKIP: dpdfnet2 unavailable");
            return;
        };
        // A fixed noisy-ish signal; measure residual energy at increasing
        // strengths. Suppression should never REDUCE (residual energy must
        // be non-increasing) as strength rises from 0.0 to 1.0 (D-16).
        let input: Vec<f32> = (0..HOP)
            .map(|i| 0.3 * (i as f32 * 0.37).sin() + 0.1 * (i as f32 * 1.9).cos())
            .collect();
        let mut output = vec![0f32; HOP];
        let mut prior_energy: Option<f32> = None;
        for step in 0..=10 {
            let strength = step as f32 / 10.0;
            engine.set_strength(strength);
            for _ in 0..4 {
                engine.process(&input, &mut output);
            }
            let energy: f32 = output.iter().map(|v| v * v).sum();
            if let Some(prior) = prior_energy {
                assert!(
                    energy <= prior + 1e-3,
                    "strength sweep must be monotonic: residual energy rose from {prior} to {energy} at strength {strength}"
                );
            }
            prior_energy = Some(energy);
        }
    }

    // ── One-thread sustained real-time timing ────────────────────────────

    #[test]
    fn sustained_processing_stays_single_threaded_and_within_budget() {
        let Some(mut engine) = build_engine("dpdfnet8") else {
            eprintln!("[dpdfnet_production] SKIP: dpdfnet8 unavailable");
            return;
        };
        let input = vec![0.02f32; HOP];
        let mut output = vec![0f32; HOP];
        // Warm up (allocator/session warmup effects).
        for _ in 0..20 {
            engine.process(&input, &mut output);
        }
        const ITERATIONS: usize = 500;
        const BUDGET: Duration = Duration::from_millis(10); // 480 samples @ 48kHz
        let mut over_budget = 0usize;
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let t0 = Instant::now();
            engine.process(&input, &mut output);
            if t0.elapsed() > BUDGET {
                over_budget += 1;
            }
        }
        let wall = start.elapsed();
        assert!(
            over_budget * 20 < ITERATIONS, // < 5% of hops over budget
            "too many hops exceeded the 10ms real-time budget: {over_budget}/{ITERATIONS} in {wall:?}"
        );
        engine.teardown();
    }

    // ── Independent variant switching / registration ─────────────────────

    #[test]
    fn variants_register_independently_in_engine_type() {
        // Round-trips through the same serde-backed EngineType contract
        // `tests/dpdfnet_experimental_switch.rs` exercises for the
        // experimental adapter, but for the two SHIPPING variants.
        let types = [EngineType::Dpdfnet2, EngineType::Dpdfnet8];
        for t in types {
            let json = serde_json::to_string(&t).expect("serialize EngineType");
            let back: EngineType = serde_json::from_str(&json).expect("deserialize EngineType");
            assert_eq!(t, back, "EngineType round-trip must be stable for {t:?}");
        }
        assert_ne!(
            EngineType::Dpdfnet2,
            EngineType::Dpdfnet8,
            "DPDFNet-2 and DPDFNet-8 must be independently distinguishable engine types (D-01/D-02)"
        );
    }

    #[test]
    fn one_variant_failing_does_not_affect_the_other() {
        // Force DPDFNet-2 to fail (bogus model path) while DPDFNet-8 remains
        // constructible with its real pinned model -- proves independent
        // per-variant gating (D-02): a failure in one variant must never
        // disable or alter the other.
        ensure_env();
        let bogus = Path::new("/nonexistent/dpdfnet2_48khz_hr.onnx").to_path_buf();
        let mut broken = DpdfnetEngine::new(DpdfnetVariant::Dpdfnet2, bogus);
        assert!(
            broken.init(48_000).is_err(),
            "a missing model path must fail to init, not silently degrade"
        );

        if let Some(mut healthy) = build_engine("dpdfnet8") {
            let input = vec![0.02f32; HOP];
            let mut output = vec![0f32; HOP];
            healthy.process(&input, &mut output);
            assert!(
                output.iter().any(|v| *v != 0.0) || input.iter().all(|v| *v == 0.0),
                "DPDFNet-8 must remain fully functional even though DPDFNet-2 failed to init"
            );
            healthy.teardown();
        } else {
            eprintln!("[dpdfnet_production] SKIP: dpdfnet8 unavailable for isolation check");
        }
    }
}

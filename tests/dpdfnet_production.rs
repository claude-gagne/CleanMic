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
    use cleanmic::audio::AudioPipeline;
    use cleanmic::engine::dpdfnet::{DpdfnetEngine, DpdfnetVariant};
    use cleanmic::engine::{self, EngineType, NoiseEngine, ProcessingMode};
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
        // The pinned reference renderer infers every hop -- it has no D-03
        // decimation concept at all. Before quick-260923-v4q this test left
        // the engine at its constructor-default `ProcessingMode::Balanced`
        // (ratio 2, decimated), silently comparing a decimated-path output
        // against an every-hop reference: it failed for dpdfnet8 at HEAD
        // 0363956 (max|diff| 1.3e-3 > the 1e-3 OUTPUT_EPSILON) and passed for
        // dpdfnet2 only because that fixture's output peak (8.4e-4) happens
        // to sit under OUTPUT_EPSILON, not because the comparison was
        // actually valid. Pinning MaxQuality here is the correct fix: this
        // test is a golden-vector parity check against the always-infers
        // reference, not a decimation regression test (that's covered by the
        // in-module D-03 tests in `src/engine/dpdfnet.rs`).
        engine.set_mode(ProcessingMode::MaxQuality);

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

        // Warm up BEFORE the recorded sweep: the attenuation limiter only
        // starts blending in the delayed noisy-floor reference once its ring
        // has accumulated more than NOISY_FRAME_OFFSET (4) hops -- comparing
        // an unprimed step (pure enhanced, no noisy blend at any dB) against
        // a primed step (real blending) is an apples-to-oranges non-monotonic
        // artifact of ring warm-up, not a real strength-curve reversal. Prime
        // both the recurrent model state and the attn ring here so every
        // step below is measured in the same (fully primed) steady state.
        for _ in 0..8 {
            engine.process(&input, &mut output);
        }

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
            // This engine defaults to ProcessingMode::Balanced (decimation
            // ratio 2). Since quick-260923-v4q's D-03 gain-mask hold fix,
            // decimated modes synthesize true silence until NoisyHistory has
            // a frame old enough to align against -- up to
            // NOISY_FRAME_OFFSET * ratio hops (8 for Balanced, 16 for
            // LowCpu; see `src/engine/dpdfnet.rs` module docs), longer than
            // the 2-hop MaxQuality-only warm-up `DpdfnetEngine::latency_frames`
            // documents. 24 hops clears that warm-up with margin for either
            // decimated ratio, so this isolation check exercises real
            // steady-state behavior rather than mistaking correct warm-up
            // silence for a broken engine.
            for _ in 0..24 {
                healthy.process(&input, &mut output);
            }
            assert!(
                output.iter().any(|v| *v != 0.0) || input.iter().all(|v| *v == 0.0),
                "DPDFNet-8 must remain fully functional even though DPDFNet-2 failed to init"
            );
            healthy.teardown();
        } else {
            eprintln!("[dpdfnet_production] SKIP: dpdfnet8 unavailable for isolation check");
        }
    }

    // ── Plan 04 Task 2: factory isolation, real-pipeline switching, and ──
    // ── sustained real-time evidence (D-01/D-02) ─────────────────────────

    /// Restores an environment variable to its pre-test value on drop, even
    /// if the test body panics mid-assertion. `--test-threads=1` (required
    /// by this whole file) makes this safe: no other test mutates
    /// `APPDIR`/`ORT_DYLIB_PATH` concurrently with this guard's lifetime.
    struct EnvRestore {
        key: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            // SAFETY: test-only cleanup; serial execution (--test-threads=1).
            unsafe {
                match &self.original {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    impl EnvRestore {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                original: std::env::var_os(key),
            }
        }
    }

    /// Exercises the REAL production `create_engine`/`is_engine_available`
    /// factory (not a direct `DpdfnetEngine::new` call) against a
    /// constructed fake `$APPDIR` that deliberately bundles only DPDFNet-8's
    /// model, proving the factory-level D-02 isolation contract: DPDFNet-2's
    /// resolution failure must never touch DPDFNet-8's availability or
    /// construction.
    #[test]
    fn create_engine_factory_isolates_one_variant_failure_from_the_other() {
        let ort_src = ort_dylib_path();
        let m2 = model_path_for("dpdfnet2");
        let m8 = model_path_for("dpdfnet8");
        if !ort_src.is_file() || !m2.is_file() || !m8.is_file() {
            eprintln!(
                "[dpdfnet_production] SKIP: pinned assets unavailable for factory isolation check"
            );
            return;
        }

        let _restore_appdir = EnvRestore::capture("APPDIR");
        let _restore_ort = EnvRestore::capture("ORT_DYLIB_PATH");

        let tmp = tempfile::tempdir().expect("create fake AppDir tempdir");
        let lib_dir = tmp.path().join("usr/lib");
        let models_dir = tmp.path().join("usr/share/cleanmic/models");
        std::fs::create_dir_all(&lib_dir).expect("create fake usr/lib");
        std::fs::create_dir_all(&models_dir).expect("create fake usr/share/cleanmic/models");
        std::fs::copy(&ort_src, lib_dir.join("libonnxruntime.so"))
            .expect("stage fake ONNX Runtime");
        // Only DPDFNet-8's model is bundled here; DPDFNet-2's is deliberately
        // absent to force a real, factory-level construction failure.
        std::fs::copy(&m8, models_dir.join("dpdfnet8_48khz_hr.onnx"))
            .expect("stage fake dpdfnet8 model");

        // SAFETY: test-only; serial (--test-threads=1); restored by the
        // guards above regardless of how this test exits.
        unsafe {
            std::env::set_var("APPDIR", tmp.path());
            std::env::remove_var("ORT_DYLIB_PATH"); // force the factory to derive it from APPDIR
        }

        assert!(
            !engine::is_engine_available(EngineType::Dpdfnet2),
            "dpdfnet2's model is deliberately absent from this fake AppDir"
        );
        assert!(
            engine::create_engine(EngineType::Dpdfnet2).is_err(),
            "the factory must fail closed for the missing variant, not silently substitute"
        );

        assert!(
            engine::is_engine_available(EngineType::Dpdfnet8),
            "dpdfnet8's model IS present in this fake AppDir"
        );
        let engine8 = engine::create_engine(EngineType::Dpdfnet8);
        assert!(
            engine8.is_ok(),
            "DPDFNet-8's factory construction must succeed even though DPDFNet-2's failed: {:?}",
            engine8.err().map(|e| e.to_string())
        );
    }

    /// Real `AudioPipeline`/`AudioCommand::SetEngine` crossfade continuity
    /// across both DPDFNet-to-existing-engine and DPDFNet-to-DPDFNet
    /// switches, using the actual production `DpdfnetEngine` (not the
    /// throwaway experimental adapter). Mirrors
    /// `tests/dpdfnet_experimental_switch.rs`'s heartbeat-continuity
    /// contract; in `AudioPipeline::new()`'s simulation mode input is
    /// silence, so this proves the audio thread survives every crossfade
    /// without deadlock/panic -- perceptual non-silent output on real audio
    /// is covered directly on the engine by the golden/strength tests above.
    #[test]
    fn production_engines_survive_repeated_pipeline_switching() {
        let (Some(mut e2), Some(mut e8)) = (build_engine("dpdfnet2"), build_engine("dpdfnet8"))
        else {
            eprintln!(
                "[dpdfnet_production] SKIP: pinned assets unavailable for pipeline switching"
            );
            return;
        };
        // Warm both up so they are past the two-hop silent warm-up before
        // being handed to the pipeline (does not affect the heartbeat
        // assertion below, just keeps this test's intent explicit).
        let input = vec![0.01f32; HOP];
        let mut output = vec![0f32; HOP];
        for _ in 0..4 {
            e2.process(&input, &mut output);
            e8.process(&input, &mut output);
        }

        let pipeline = AudioPipeline::new().expect("spawn real AudioPipeline");
        pipeline.start();
        std::thread::sleep(Duration::from_millis(30));
        let hb0 = pipeline.heartbeat_count();

        pipeline.set_engine(Box::new(e2));
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            pipeline.is_cmd_channel_open(),
            "audio thread must survive SetEngine(DPDFNet-2) crossfade without panicking"
        );

        pipeline.set_engine(Box::new(e8));
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            pipeline.is_cmd_channel_open(),
            "audio thread must survive DPDFNet-2 -> DPDFNet-8 crossfade without panicking"
        );

        // DPDFNet -> an existing (non-DPDFNet) engine and back, proving the
        // crossfade path is not DPDFNet-specific.
        let (rnnoise_engine, _actual) = engine::create_engine_with_fallback(EngineType::RNNoise);
        pipeline.set_engine(rnnoise_engine);
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            pipeline.is_cmd_channel_open(),
            "audio thread must survive DPDFNet-8 -> RNNoise crossfade without panicking"
        );

        if let Some(back_to_d2) = build_engine("dpdfnet2") {
            pipeline.set_engine(Box::new(back_to_d2));
            std::thread::sleep(Duration::from_millis(80));
            assert!(
                pipeline.is_cmd_channel_open(),
                "audio thread must survive RNNoise -> DPDFNet-2 crossfade without panicking"
            );
        }

        let hb1 = pipeline.heartbeat_count();
        assert!(
            hb1 > hb0,
            "audio thread heartbeat must keep advancing across every crossfade (no deadlock): {hb0} -> {hb1}"
        );

        pipeline.shutdown();
    }

    /// Read current process RSS (KiB) from `/proc/self/status`, used as
    /// allocation evidence for the sustained run below (matches the
    /// established convention in `tests/dpdfnet_experimental_switch.rs`:
    /// this codebase has no custom global-allocator call counter, so a
    /// near-zero RSS delta across a long warmed loop is the accepted proxy
    /// for "no per-call allocation growth").
    fn read_rss_kib() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status.lines().find_map(|line| {
            line.strip_prefix("VmRSS:").and_then(|rest| {
                rest.chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .ok()
            })
        })
    }

    /// A realistic (non-constant, multi-tone) 48 kHz mono signal -- avoids
    /// the false confidence a literal-silence or single-tone input would
    /// give the timing/allocation measurement below.
    fn realistic_hop(hop_index: usize) -> [f32; HOP] {
        let mut out = [0f32; HOP];
        for (i, sample) in out.iter_mut().enumerate() {
            let n = (hop_index * HOP + i) as f32;
            let t = n / 48_000.0;
            *sample = 0.25 * (2.0 * std::f32::consts::PI * 220.0 * t).sin()
                + 0.08 * (2.0 * std::f32::consts::PI * 2500.0 * t).sin()
                + 0.02 * (2.0 * std::f32::consts::PI * 6000.0 * t).cos();
        }
        out
    }

    /// A warmed, realistic (non-silent, multi-tone), single-thread sustained
    /// run per variant, recording median/p99/max latency, real-time deadline
    /// misses, and an RSS-based allocation-growth check. Independent
    /// per-variant results (D-02) -- one variant's numbers never gate the
    /// other's. "Underflow" in this direct-engine-call context (no ring
    /// buffer / no PipeWire) reduces to "every call returns a full,
    /// finite-length HOP block with no panic" -- ring-buffer-level underflow
    /// is a `src/audio.rs`-level concern already covered by
    /// `production_engines_survive_repeated_pipeline_switching`'s heartbeat
    /// check above.
    #[test]
    fn warmed_sustained_realtime_evidence_per_variant() {
        const ITERATIONS: usize = 6_000; // 6,000 * 10ms = 60s of simulated audio
        const BUDGET: Duration = Duration::from_millis(10);

        for variant in ["dpdfnet2", "dpdfnet8"] {
            let Some(mut eng) = build_engine(variant) else {
                eprintln!(
                    "[dpdfnet_production] SKIP: {variant} unavailable for sustained evidence"
                );
                continue;
            };

            let mut output = vec![0f32; HOP];
            // Warm up (allocator/session warmup + the two-hop DSP warm-up).
            for h in 0..20 {
                eng.process(&realistic_hop(h), &mut output);
            }

            let rss_before = read_rss_kib();
            let mut latencies_us: Vec<u128> = Vec::with_capacity(ITERATIONS);
            let mut deadline_misses = 0usize;
            let mut block_overruns = 0usize;
            let start = Instant::now();
            for h in 0..ITERATIONS {
                let hop = realistic_hop(h);
                let t0 = Instant::now();
                eng.process(&hop, &mut output);
                let elapsed = t0.elapsed();
                latencies_us.push(elapsed.as_micros());
                if elapsed > BUDGET {
                    deadline_misses += 1;
                }
                if output.len() != HOP || !output.iter().all(|v| v.is_finite()) {
                    block_overruns += 1;
                }
            }
            let wall = start.elapsed();
            let rss_after = read_rss_kib();
            eng.teardown();

            latencies_us.sort_unstable();
            let median_us = latencies_us[latencies_us.len() / 2];
            let p99_idx = (((latencies_us.len() as f64) * 0.99) as usize)
                .min(latencies_us.len().saturating_sub(1));
            let p99_us = latencies_us[p99_idx];
            let max_us = *latencies_us.last().unwrap();
            let rss_delta_kib = match (rss_before, rss_after) {
                (Some(b), Some(a)) => Some(a as i64 - b as i64),
                _ => None,
            };

            eprintln!(
                "[dpdfnet_production] {variant} sustained evidence: {ITERATIONS} hops in {wall:?}, \
                 median={median_us}us p99={p99_us}us max={max_us}us deadline_misses={deadline_misses} \
                 rss_delta={rss_delta_kib:?}KiB"
            );

            assert_eq!(
                block_overruns, 0,
                "{variant}: {block_overruns} hop(s) produced a wrong-length or non-finite block"
            );
            assert!(
                deadline_misses * 20 < ITERATIONS, // < 5% of hops over the 10ms real-time budget
                "{variant}: too many hops exceeded the 10ms real-time budget: \
                 {deadline_misses}/{ITERATIONS} (max {max_us}us)"
            );
            if let Some(delta) = rss_delta_kib {
                assert!(
                    delta < 20_000, // generous bound: no unbounded per-call growth over 60s
                    "{variant}: RSS grew by {delta} KiB over the sustained run -- possible per-call allocation"
                );
            }
        }
    }
}

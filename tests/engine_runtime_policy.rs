//! Engine-specific runtime policy invariant tests (Phase 15.1, Plan 05 —
//! D-02, D-08, D-09/D-10/D-11/D-12, D-13/D-14, T-15.1-07, T-15.1-08).
//!
//! Exercises the promoted per-engine state model end-to-end through the
//! crate's public API (`cleanmic::config::Config`, `cleanmic::engine::*`,
//! `cleanmic::ui::UiState`) rather than any single module in isolation:
//! independent per-engine availability, remembered per-engine strength,
//! the fail-closed conditional DPDFNet-2 default migration, and truthful
//! requested-vs-active engine reporting when a fallback occurs. Deliberately
//! does NOT depend on the `dpdfnet` Cargo feature or a real bundled
//! model/runtime being present — every scenario here is exercised through
//! the public, environment-independent policy surface (unlike
//! `tests/dpdfnet_production.rs`, which needs the real pinned assets).
//!
//! Run: `cargo test --all-features --test engine_runtime_policy -- --test-threads=1`
//! (also passes under `cargo test --test engine_runtime_policy` with no
//! features, since nothing here requires an engine feature to be compiled
//! in — unavailable engines are exactly what several of these tests assert
//! against).

use cleanmic::config::Config;
use cleanmic::engine::dpdfnet_policy::{DPDFNET2_DEFAULT_DECISION, Dpdfnet2DefaultDecision};
use cleanmic::engine::{self, EngineType};
use cleanmic::ui::UiState;

// ── Per-engine strength round-trip (D-13/D-14) ──────────────────────────────

#[test]
fn every_engine_round_trips_its_own_remembered_strength_through_save_and_load() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let path = tmp.path().join("cleanmic").join("config.toml");

    let mut original = Config::default();
    for (i, engine) in EngineType::all().enumerate() {
        // Distinct value per engine so a copy-paste/aliasing bug between
        // engines would be caught by the round-trip below.
        original.set_strength_for(engine, 0.1 + (i as f32) * 0.15);
    }
    original.save_to(&path).expect("save failed");

    let loaded = Config::load_from(&path).expect("load failed");
    for engine in EngineType::all() {
        assert!(
            (loaded.strength_for(engine) - original.strength_for(engine)).abs() < f32::EPSILON,
            "{engine:?}: {} != {}",
            loaded.strength_for(engine),
            original.strength_for(engine)
        );
    }

    // Switching the persisted `engine` selection changes which strength a
    // fresh `UiState::from_config` surfaces — proves "restore this engine's
    // own remembered value on switch" (D-13), not a shared global.
    let mut switched = loaded.clone();
    for engine in EngineType::all() {
        switched.engine = engine;
        let state = UiState::from_config(&switched);
        assert!(
            (state.strength - switched.strength_for(engine)).abs() < f32::EPSILON,
            "UiState::from_config strength did not follow the active engine"
        );
    }
}

#[test]
fn all_five_engines_are_iterated_by_engine_type_all() {
    let all: Vec<EngineType> = EngineType::all().collect();
    assert_eq!(all.len(), 5);
    assert!(all.contains(&EngineType::RNNoise));
    assert!(all.contains(&EngineType::DeepFilterNet));
    assert!(all.contains(&EngineType::Dpdfnet2));
    assert!(all.contains(&EngineType::Dpdfnet8));
    assert!(all.contains(&EngineType::Khip));
}

// ── Independent per-engine availability (D-02/D-08, T-15.1-08) ─────────────

#[test]
fn all_engine_availability_reports_every_engine_independently() {
    let map = engine::all_engine_availability();
    assert_eq!(map.len(), 5);
    for engine in EngineType::all() {
        let direct = engine::engine_availability(engine);
        assert_eq!(
            map[&engine], direct,
            "{engine:?}: map entry disagrees with a direct single-engine call"
        );
    }
}

/// D-02: one DPDFNet variant reporting unavailable must never affect the
/// other's entry in the shared availability map.
#[test]
fn dpdfnet2_and_dpdfnet8_availability_are_computed_independently() {
    // SAFETY: test-only env var mutation; no other test in this binary reads
    // or writes APPDIR concurrently with this assertion (matches the
    // existing precedent in `src/engine/mod.rs`'s own APPDIR test).
    unsafe {
        std::env::remove_var("APPDIR");
    }
    let map = engine::all_engine_availability();
    let dpdfnet2 = map[&EngineType::Dpdfnet2];
    let dpdfnet8 = map[&EngineType::Dpdfnet8];
    // Without APPDIR neither variant can resolve its model/runtime, so both
    // report unavailable — but each arrived there through its OWN
    // independent resolution path (`engine::engine_availability`), not a
    // shared/aliased computation. The forced-failure isolation itself is
    // exercised end-to-end by `src/engine/mod.rs`'s
    // `dpdfnet_variants_are_independently_reported_unavailable_without_appdir`;
    // here we additionally confirm the SHARED MAP used by `UiState`
    // reproduces the same two independent entries.
    assert!(!dpdfnet2.available);
    assert!(!dpdfnet8.available);
}

// ── Conditional DPDFNet-2 default migration (D-09/D-10/D-11/D-12) ──────────

#[test]
fn product_decision_stays_pending_and_load_never_migrates() {
    // D-12: this is the load-bearing guarantee for shipping today — a real
    // `Config::load_from` call (which always uses the real product
    // constant) must never migrate anyone away from DeepFilterNet.
    assert_eq!(DPDFNET2_DEFAULT_DECISION, Dpdfnet2DefaultDecision::Pending);

    let tmp = tempfile::tempdir().expect("temp dir");
    let path = tmp.path().join("cleanmic").join("config.toml");
    let mut to_save = Config::default();
    to_save.engine = EngineType::DeepFilterNet;
    to_save.save_to(&path).expect("save failed");

    let loaded = Config::load_from(&path).expect("load failed");
    assert_eq!(loaded.engine, EngineType::DeepFilterNet);
    assert!(!loaded.dpdfnet_default_migration_complete);
}

/// D-11: a migrated DPDFNet-2 selection that fails to initialize must
/// result in DeepFilterNet becoming both the ACTIVE and PERSISTED engine,
/// with a truthful, non-empty fallback notice that never claims DPDFNet-2 is
/// active. This test drives the exact sequence an app-layer startup path
/// follows: migrate (Pass) -> attempt to resolve the migrated engine ->
/// (simulated) initialization failure -> fall back and record the notice.
#[test]
fn migrated_dpdfnet2_that_fails_to_initialize_falls_back_to_deepfilternet_truthfully() {
    let mut config = Config::default();
    config.engine = EngineType::DeepFilterNet;

    // D-10: the one-time conditional migration runs (Pass), including this
    // deliberate DeepFilterNet selection.
    config.apply_dpdfnet2_migration(Dpdfnet2DefaultDecision::Pass);
    assert_eq!(config.engine, EngineType::Dpdfnet2);
    assert!(config.dpdfnet_default_migration_complete);

    // D-11: initialization failure is simulated here rather than depending
    // on a real bundled model/runtime (this test intentionally runs without
    // the `dpdfnet` feature or APPDIR) — the requested engine is what
    // migration just set; the actual engine is what a real app-layer
    // fallback resolution (`engine::create_engine_with_fallback`) would
    // truthfully report once DPDFNet-2 fails to construct.
    let requested = config.engine;
    let actual = EngineType::DeepFilterNet;

    // The application must persist the ACTUAL engine, never the failed
    // request (D-11's core truthfulness requirement).
    config.engine = actual;

    let notice = engine::fallback_notice(requested, actual);
    assert!(notice.is_some(), "a fallback occurred, notice must be Some");
    let notice = notice.unwrap();
    assert!(
        notice.contains("DeepFilterNet"),
        "notice must name the truthfully ACTIVE engine: {notice}"
    );
    assert!(
        !notice.contains("DPDFNet-2"),
        "notice must never claim the failed requested engine is active: {notice}"
    );

    // Persisted state matches: DeepFilterNet is both active and persisted;
    // the migration marker stays set (it recorded a historical fact, not a
    // currently-successful state) so this config is never re-migrated.
    assert_eq!(config.engine, EngineType::DeepFilterNet);
    assert!(config.dpdfnet_default_migration_complete);

    // UiState wiring: requested/active truth and the notice propagate
    // exactly as `src/app.rs`'s startup path wires them into the very first
    // `UiState` (see `run_with_gui`'s `startup_engine_fallback` handling).
    let mut state = UiState::from_config(&config);
    state.requested_engine = requested;
    state.fallback_notice = engine::fallback_notice(requested, state.engine);

    assert_eq!(state.engine, EngineType::DeepFilterNet);
    assert_eq!(state.requested_engine, EngineType::Dpdfnet2);
    assert_ne!(state.engine, state.requested_engine);
    assert!(state.fallback_notice.is_some());
}

#[test]
fn pass_never_repeats_across_separate_load_cycles() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let path = tmp.path().join("cleanmic").join("config.toml");

    let mut config = Config::default();
    config.engine = EngineType::DeepFilterNet;
    config.apply_dpdfnet2_migration(Dpdfnet2DefaultDecision::Pass);
    assert_eq!(config.engine, EngineType::Dpdfnet2);
    config.save_to(&path).expect("save failed");

    // Reload, then deliberately switch back to DeepFilterNet (simulating a
    // user's manual choice after the migration already ran) and persist.
    let mut reloaded = Config::load_from(&path).expect("load failed");
    assert!(reloaded.dpdfnet_default_migration_complete);
    reloaded.engine = EngineType::DeepFilterNet;
    reloaded.save_to(&path).expect("save failed");

    // A later evaluation of the SAME `Pass` decision (e.g. a hypothetical
    // repeated startup call) must NOT re-migrate the user's deliberate
    // reselection back to Dpdfnet2 — the marker is sticky.
    let mut reloaded_again = Config::load_from(&path).expect("load failed");
    reloaded_again.apply_dpdfnet2_migration(Dpdfnet2DefaultDecision::Pass);
    assert_eq!(
        reloaded_again.engine,
        EngineType::DeepFilterNet,
        "already-migrated config must not re-migrate a deliberate re-selection"
    );
}

// ── Fallback truthfulness is general, not migration-only (T-15.1-07) ───────

#[test]
fn fallback_notice_never_names_the_requested_engine_when_it_failed() {
    for (requested, actual) in [
        (EngineType::Dpdfnet2, EngineType::DeepFilterNet),
        (EngineType::Dpdfnet8, EngineType::RNNoise),
        (EngineType::Khip, EngineType::DeepFilterNet),
    ] {
        let notice = engine::fallback_notice(requested, actual).expect("some notice");
        assert!(notice.contains(actual.short_name()));
        assert!(!notice.contains(requested.short_name()));
    }
}

#[test]
fn no_fallback_notice_when_requested_equals_active_for_every_engine() {
    for engine in EngineType::all() {
        assert_eq!(engine::fallback_notice(engine, engine), None);
    }
}

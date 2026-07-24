//! Engine-specific runtime policy invariant tests (Phase 15.1, Plan 05 —
//! D-09/D-10/D-12, D-13/D-14).
//!
//! Exercises the promoted per-engine configuration state model end-to-end
//! through the crate's public API (`cleanmic::config::Config`,
//! `cleanmic::engine::*`) rather than any single module in isolation:
//! independent per-engine remembered strength and the fail-closed
//! conditional DPDFNet-2 default migration. Deliberately does NOT depend on
//! the `dpdfnet` Cargo feature or a real bundled model/runtime being
//! present.
//!
//! Task 2 (application/UI wiring — availability map, requested-vs-active
//! truth, fallback notice) extends this same file with additional tests.
//!
//! Run: `cargo test --all-features --test engine_runtime_policy -- --test-threads=1`
//! (also passes under `cargo test --test engine_runtime_policy` with no
//! features).

use cleanmic::config::Config;
use cleanmic::engine::EngineType;
use cleanmic::engine::dpdfnet_policy::{DPDFNET2_DEFAULT_DECISION, Dpdfnet2DefaultDecision};

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

// ── Conditional DPDFNet-2 default migration (D-09/D-10/D-12) ───────────────

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

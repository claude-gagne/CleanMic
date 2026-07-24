//! Fail-closed default-eligibility policy for DPDFNet-2 (D-09/D-12).
//!
//! Whether DPDFNet-2 may replace DeepFilterNet as the default engine is an
//! explicit, evidence-gated product decision — never inferred from ship-gate
//! results, runtime availability, or any other proxy. Until a future plan
//! records approved low-end-processor and lower-quality-microphone default
//! evidence (see `15.1-EVIDENCE.schema.json`'s `default_evidence` record
//! kind) and flips [`DPDFNET2_DEFAULT_DECISION`] to [`Dpdfnet2DefaultDecision::Pass`],
//! DeepFilterNet remains the default and [`Config`](crate::config::Config)
//! performs zero migration (D-12). This module owns that single decision
//! point so the fail-closed default cannot be bypassed from any call site.

use crate::engine::EngineType;

/// The three possible states of the DPDFNet-2 default-eligibility decision
/// (D-09).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dpdfnet2DefaultDecision {
    /// Default evidence has not yet been recorded/approved. Fail-closed —
    /// migration never runs in this state (D-12).
    Pending,
    /// Default evidence was recorded and approved by the owner (D-09). The
    /// one-time conditional migration (D-10) is permitted to run.
    Pass,
    /// Default evidence was recorded and rejected. Migration never runs;
    /// DeepFilterNet remains the default, same as `Pending`.
    Fail,
}

impl Dpdfnet2DefaultDecision {
    /// `true` only for `Pass` — the sole state in which the one-time
    /// conditional migration (D-10) may run. Both `Pending` and `Fail` are
    /// treated identically (no migration) so a bug can never accidentally
    /// treat "not yet decided" as "decided no".
    pub fn allows_migration(self) -> bool {
        matches!(self, Self::Pass)
    }
}

/// Fail-closed, compile-time-fixed default-eligibility decision for
/// DPDFNet-2. Deliberately `Pending` as landed by this plan: a later plan is
/// the only place this constant may change, and only after the
/// default-evidence manifest records an approved `Pass`. D-12 requires
/// DeepFilterNet stay the default and zero migration occur until then.
pub const DPDFNET2_DEFAULT_DECISION: Dpdfnet2DefaultDecision = Dpdfnet2DefaultDecision::Pending;

/// Pure, decision-parameterized migration step (D-10/D-12).
///
/// Exposed as a free function — rather than hardcoding
/// [`DPDFNET2_DEFAULT_DECISION`] inside [`Config`](crate::config::Config) —
/// so tests can exercise the `Pass`/`Fail`/`Pending` state machine without
/// needing to flip the real product constant (which stays `Pending` until a
/// future plan approves default evidence).
///
/// Returns `(new_engine, migration_now_complete)`. A no-op (`engine`
/// unchanged, `migration_now_complete` unchanged) unless
/// `decision.allows_migration()` and `migration_already_complete` is
/// `false`. When migration runs it always marks complete — even if `engine`
/// was not `DeepFilterNet` at the time — because D-10's migration is a
/// single historical "did this config get its one chance to migrate" event,
/// not a continual enforcement that could re-trigger if the user later
/// deliberately switches back to DeepFilterNet.
pub fn migrate_engine_for_decision(
    engine: EngineType,
    migration_already_complete: bool,
    decision: Dpdfnet2DefaultDecision,
) -> (EngineType, bool) {
    if migration_already_complete || !decision.allows_migration() {
        return (engine, migration_already_complete);
    }
    let migrated = if engine == EngineType::DeepFilterNet {
        EngineType::Dpdfnet2
    } else {
        engine
    };
    (migrated, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_never_allows_migration() {
        assert!(!Dpdfnet2DefaultDecision::Pending.allows_migration());
    }

    #[test]
    fn fail_never_allows_migration() {
        assert!(!Dpdfnet2DefaultDecision::Fail.allows_migration());
    }

    #[test]
    fn pass_allows_migration() {
        assert!(Dpdfnet2DefaultDecision::Pass.allows_migration());
    }

    #[test]
    fn product_constant_is_pending() {
        // D-12: this must stay `Pending` until a future plan approves
        // default evidence. This test is a tripwire — if it ever fails,
        // someone flipped the constant without going through that process.
        assert_eq!(DPDFNET2_DEFAULT_DECISION, Dpdfnet2DefaultDecision::Pending);
    }

    #[test]
    fn pending_never_migrates_deepfilternet_selection() {
        let (engine, complete) = migrate_engine_for_decision(
            EngineType::DeepFilterNet,
            false,
            Dpdfnet2DefaultDecision::Pending,
        );
        assert_eq!(engine, EngineType::DeepFilterNet);
        assert!(!complete);
    }

    #[test]
    fn fail_never_migrates_deepfilternet_selection() {
        let (engine, complete) = migrate_engine_for_decision(
            EngineType::DeepFilterNet,
            false,
            Dpdfnet2DefaultDecision::Fail,
        );
        assert_eq!(engine, EngineType::DeepFilterNet);
        assert!(!complete);
    }

    #[test]
    fn pass_migrates_every_deepfilternet_selection_including_deliberate() {
        // D-10 explicitly overrides the usual preserve-existing-preference
        // policy for this one-time transition, including selections a user
        // deliberately made.
        let (engine, complete) = migrate_engine_for_decision(
            EngineType::DeepFilterNet,
            false,
            Dpdfnet2DefaultDecision::Pass,
        );
        assert_eq!(engine, EngineType::Dpdfnet2);
        assert!(complete);
    }

    #[test]
    fn pass_marks_complete_even_when_engine_was_not_deepfilternet() {
        // The marker is "this config had its one chance to migrate, ever" —
        // not "this config currently needs migrating".
        let (engine, complete) =
            migrate_engine_for_decision(EngineType::RNNoise, false, Dpdfnet2DefaultDecision::Pass);
        assert_eq!(engine, EngineType::RNNoise);
        assert!(complete);
    }

    #[test]
    fn pass_never_repeats_once_complete() {
        // Simulates a user who deliberately switches back to DeepFilterNet
        // AFTER a completed migration — must NOT be re-migrated.
        let (engine, complete) = migrate_engine_for_decision(
            EngineType::DeepFilterNet,
            true,
            Dpdfnet2DefaultDecision::Pass,
        );
        assert_eq!(engine, EngineType::DeepFilterNet);
        assert!(complete, "already-complete marker must stay true");
    }

    #[test]
    fn non_deepfilternet_selection_is_left_untouched_on_pass() {
        for engine in [
            EngineType::RNNoise,
            EngineType::Dpdfnet2,
            EngineType::Dpdfnet8,
            EngineType::Khip,
        ] {
            let (result, complete) =
                migrate_engine_for_decision(engine, false, Dpdfnet2DefaultDecision::Pass);
            assert_eq!(result, engine, "{engine:?} must not be rewritten");
            assert!(complete);
        }
    }
}

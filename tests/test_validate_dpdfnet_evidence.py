"""Unit tests for scripts/validate-dpdfnet-evidence.py.

Run with: python3 -m unittest tests.test_validate_dpdfnet_evidence -v

Proves the Phase 15.1 evidence validator rejects malformed, stale, skipped,
non-finite, cross-variant, DPDFNet-8-default, and incomplete package
records, and accepts well-formed independent records (including the two
committed seed files, ``15.1-DPDFNET2-GATE.json`` and
``15.1-DPDFNET8-GATE.json``).
"""

from __future__ import annotations

import copy
import importlib.util
import json
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / "scripts" / "validate-dpdfnet-evidence.py"
PHASE_DIR = REPO_ROOT / ".planning" / "phases" / "15.1-dpdfnet-production-integration-and-engine-selection"


def _load_validator_module():
    spec = importlib.util.spec_from_file_location("validate_dpdfnet_evidence", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


validator = _load_validator_module()

NOW = datetime(2026, 7, 24, 16, 0, 0, tzinfo=timezone.utc)


def _write(directory: Path, name: str, payload) -> Path:
    path = directory / name
    if isinstance(payload, str):
        path.write_text(payload, encoding="utf-8")
    else:
        path.write_text(json.dumps(payload), encoding="utf-8")
    return path


def _valid_gate_entry(owner: str = "Plan 04 (15.1-04)", status: str = "PENDING") -> dict:
    return {
        "status": status,
        "command": "cargo test --features dpdfnet dpdfnet -- --test-threads=1",
        "rationale": "Not yet run under this contract.",
        "owner": owner,
        "sources": [],
    }


def _valid_variant_gate(variant: str = "dpdfnet2") -> dict:
    return {
        "schema_version": 1,
        "record_kind": "variant_gate",
        "variant": variant,
        "source_commit": "a" * 40,
        "evaluated_at": "2026-07-24T15:32:50Z",
        "freshness_window_days": 120,
        "gates": {
            "production_correctness": _valid_gate_entry(),
            "sustained_realtime": _valid_gate_entry(),
            "strength_behavior": _valid_gate_entry("Plan 05 (15.1-05)"),
            "licensing": _valid_gate_entry("Phase 15 evidence, independently re-affirmed"),
            "supported_distribution": {
                "status": "NOT_ESTABLISHED",
                "command": "grep -q 'Phase 17' .planning/ROADMAP.md",
                "rationale": "Permanently owned by Phase 17.",
                "owner": "Phase 17",
                "sources": [],
            },
        },
        "ship_eligible": False,
        "default_eligible": False,
    }


def _valid_default_evidence(variant: str = "dpdfnet2") -> dict:
    mic_path = {
        "device_id": "usb-webcam-0",
        "sample_rate_hz": 48000,
        "median_ms": 3.2,
        "p99_ms": 6.1,
        "max_ms": 8.4,
        "deadline_misses": 0,
        "clip_before_path": "tests/fixtures/dpdfnet/before.wav",
        "clip_after_path": "tests/fixtures/dpdfnet/after.wav",
    }
    return {
        "schema_version": 1,
        "record_kind": "default_evidence",
        "variant": variant,
        "source_commit": "b" * 40,
        "evaluated_at": "2026-07-24T15:32:50Z",
        "freshness_window_days": 120,
        "owner": "project owner",
        "low_end_processor": {
            "host": "low-end-test-host",
            "governor": "powersave",
            "median_ms": 4.1,
            "p99_ms": 7.8,
            "max_ms": 9.9,
            "deadline_misses": 0,
        },
        "microphone_paths": {
            "webcam": mic_path,
            "laptop": dict(mic_path, device_id="laptop-internal-0"),
            "headset": dict(mic_path, device_id="headset-usb-0"),
        },
        "default_eligible": False,
    }


def _valid_build_artifact(label: str, commit: str, settings: str) -> dict:
    return {
        "label": label,
        "commit": commit,
        "settings": settings,
        "compressed_bytes": 12_345_678,
        "sha256": "c" * 64,
    }


def _valid_package_delta() -> dict:
    commit = "d" * 40
    settings = "release --all-features"
    return {
        "schema_version": 1,
        "record_kind": "package_delta",
        "source_commit": commit,
        "evaluated_at": "2026-07-24T15:32:50Z",
        "freshness_window_days": 120,
        "builds": {
            "baseline": _valid_build_artifact("baseline", commit, settings),
            "dpdfnet2_only": _valid_build_artifact("dpdfnet2_only", commit, settings),
            "dpdfnet8_only": _valid_build_artifact("dpdfnet8_only", commit, settings),
            "both_variants": _valid_build_artifact("both_variants", commit, settings),
        },
        "material_increase_checkpoint": {
            "status": "PENDING",
            "owner": "project owner",
            "rationale": "Not yet measured.",
        },
    }


class SeedFilesTest(unittest.TestCase):
    """The two committed pending records must validate independently."""

    def test_dpdfnet2_seed_file_is_valid(self) -> None:
        validator.validate_record(
            PHASE_DIR / "15.1-DPDFNET2-GATE.json",
            "variant_gate",
            expect_variant="dpdfnet2",
            check_hashes=False,
            now=NOW,
        )

    def test_dpdfnet8_seed_file_is_valid(self) -> None:
        validator.validate_record(
            PHASE_DIR / "15.1-DPDFNET8-GATE.json",
            "variant_gate",
            expect_variant="dpdfnet8",
            check_hashes=False,
            now=NOW,
        )

    def test_dpdfnet8_seed_file_has_no_default_eligible_state(self) -> None:
        instance = json.loads((PHASE_DIR / "15.1-DPDFNET8-GATE.json").read_text(encoding="utf-8"))
        self.assertFalse(instance["default_eligible"])
        self.assertFalse(instance["ship_eligible"])

    def test_schema_file_declares_all_three_record_types(self) -> None:
        schema = validator.load_schema()
        self.assertEqual(
            set(schema["record_types"]),
            {"variant_gate", "default_evidence", "package_delta"},
        )


class ValidRecordsAcceptedTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_valid_variant_gate_accepted(self) -> None:
        path = _write(self.directory, "gate.json", _valid_variant_gate())
        validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)

    def test_valid_default_evidence_accepted(self) -> None:
        path = _write(self.directory, "default.json", _valid_default_evidence())
        validator.validate_record(path, "default_evidence", expect_variant="dpdfnet2", check_hashes=False, now=NOW)

    def test_valid_package_delta_accepted(self) -> None:
        path = _write(self.directory, "package.json", _valid_package_delta())
        validator.validate_record(path, "package_delta", expect_variant=None, check_hashes=False, now=NOW)


class MalformedTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_duplicate_key_rejected(self) -> None:
        raw = (
            '{"schema_version": 1, "record_kind": "variant_gate", "variant": "dpdfnet2", '
            '"variant": "dpdfnet8"}'
        )
        path = _write(self.directory, "dup.json", raw)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "duplicate key"):
            validator.load_json_strict(path)

    def test_unknown_key_rejected(self) -> None:
        record = _valid_variant_gate()
        record["unexpected_extra_field"] = "should not be here"
        path = _write(self.directory, "unknown.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "unknown key"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)

    def test_missing_required_key_rejected(self) -> None:
        record = _valid_variant_gate()
        del record["ship_eligible"]
        path = _write(self.directory, "missing.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "missing required key"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)

    def test_invalid_json_rejected(self) -> None:
        path = _write(self.directory, "broken.json", "{not json at all")
        with self.assertRaisesRegex(validator.EvidenceValidationError, "malformed JSON"):
            validator.load_json_strict(path)


class StaleEvidenceTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_stale_record_rejected(self) -> None:
        record = _valid_variant_gate()
        record["freshness_window_days"] = 30
        record["evaluated_at"] = (NOW - timedelta(days=400)).strftime("%Y-%m-%dT%H:%M:%SZ")
        path = _write(self.directory, "stale.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "stale"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)

    def test_future_dated_record_rejected(self) -> None:
        record = _valid_variant_gate()
        record["evaluated_at"] = (NOW + timedelta(days=10)).strftime("%Y-%m-%dT%H:%M:%SZ")
        path = _write(self.directory, "future.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "future"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)


class SkippedEvidenceTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_skipped_command_rejected(self) -> None:
        record = _valid_variant_gate()
        record["gates"]["production_correctness"]["command"] = "SKIPPED - owner did not run this yet"
        path = _write(self.directory, "skipped.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "placeholder"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)

    def test_todo_owner_rejected(self) -> None:
        record = _valid_variant_gate()
        record["gates"]["production_correctness"]["owner"] = "TODO: assign an owner"
        path = _write(self.directory, "todo.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "placeholder"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)

    def test_unassigned_owner_rejected(self) -> None:
        record = _valid_variant_gate()
        record["gates"]["licensing"]["owner"] = "unassigned"
        path = _write(self.directory, "unassigned.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "placeholder"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)

    def test_real_prose_owner_is_not_falsely_flagged(self) -> None:
        # Regression guard: prose containing substrings like "na" (as in
        # "informational") must not be treated as a placeholder.
        record = _valid_variant_gate()
        record["gates"]["licensing"]["owner"] = "Phase 15 evidence, informational and independently re-affirmed"
        path = _write(self.directory, "prose.json", record)
        validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)


class NonFiniteTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_nan_rejected(self) -> None:
        record = _valid_default_evidence()
        raw = json.dumps(record).replace('"median_ms": 4.1', '"median_ms": NaN')
        path = _write(self.directory, "nan.json", raw)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "non-finite"):
            validator.validate_record(path, "default_evidence", expect_variant="dpdfnet2", check_hashes=False, now=NOW)

    def test_infinity_rejected(self) -> None:
        record = _valid_package_delta()
        raw = json.dumps(record).replace('"compressed_bytes": 12345678', '"compressed_bytes": Infinity')
        path = _write(self.directory, "inf.json", raw)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "non-finite"):
            validator.validate_record(path, "package_delta", expect_variant=None, check_hashes=False, now=NOW)


class CrossVariantTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_variant_field_mismatch_rejected(self) -> None:
        record = _valid_variant_gate(variant="dpdfnet2")
        path = _write(self.directory, "mismatch.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "variant mismatch"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet8", check_hashes=False, now=NOW)

    def test_sibling_source_reference_rejected(self) -> None:
        record = _valid_variant_gate(variant="dpdfnet2")
        record["gates"]["licensing"]["sources"] = [
            {
                "path": str(PHASE_DIR.relative_to(REPO_ROOT) / "15.1-DPDFNET8-GATE.json"),
                "sha256": "e" * 64,
            }
        ]
        path = _write(self.directory, "cross.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "sibling variant"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)

    def test_status_cannot_be_derived_from_sibling_default_file(self) -> None:
        record = _valid_variant_gate(variant="dpdfnet8")
        record["gates"]["strength_behavior"]["sources"] = [
            {"path": "15.1-DPDFNET2-DEFAULT.json", "sha256": "f" * 64}
        ]
        path = _write(self.directory, "cross8.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "sibling variant"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet8", check_hashes=False, now=NOW)


class Dpdfnet8DefaultEligibilityTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_dpdfnet8_variant_gate_cannot_claim_default_eligible(self) -> None:
        record = _valid_variant_gate(variant="dpdfnet8")
        for gate in record["gates"].values():
            gate["status"] = "PASS"
        record["ship_eligible"] = True
        record["default_eligible"] = True  # forbidden regardless of ship_eligible
        path = _write(self.directory, "dpdfnet8-default.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "never be default_eligible"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet8", check_hashes=False, now=NOW)

    def test_dpdfnet8_default_evidence_record_cannot_claim_default_eligible(self) -> None:
        record = _valid_default_evidence(variant="dpdfnet8")
        record["default_eligible"] = True
        path = _write(self.directory, "dpdfnet8-default-evidence.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "never be default_eligible"):
            validator.validate_record(path, "default_evidence", expect_variant="dpdfnet8", check_hashes=False, now=NOW)

    def test_dpdfnet2_may_claim_default_eligible_only_when_ship_eligible(self) -> None:
        record = _valid_variant_gate(variant="dpdfnet2")
        record["default_eligible"] = True
        record["ship_eligible"] = False  # gates are still PENDING -> contradiction
        path = _write(self.directory, "dpdfnet2-bad-default.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "cannot be true while ship_eligible"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)


class ShipEligibleConsistencyTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_ship_eligible_true_with_pending_gates_rejected(self) -> None:
        record = _valid_variant_gate()
        record["ship_eligible"] = True  # gates are still PENDING
        path = _write(self.directory, "bad-ship.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "contradicts recomputed gate statuses"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)


class IncompletePackageTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_missing_build_label_rejected(self) -> None:
        record = _valid_package_delta()
        del record["builds"]["both_variants"]
        path = _write(self.directory, "incomplete.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "missing required key"):
            validator.validate_record(path, "package_delta", expect_variant=None, check_hashes=False, now=NOW)

    def test_mismatched_commit_across_builds_rejected(self) -> None:
        record = _valid_package_delta()
        record["builds"]["both_variants"]["commit"] = "9" * 40
        path = _write(self.directory, "diff-commit.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "share one commit"):
            validator.validate_record(path, "package_delta", expect_variant=None, check_hashes=False, now=NOW)

    def test_mismatched_settings_across_builds_rejected(self) -> None:
        record = _valid_package_delta()
        record["builds"]["dpdfnet8_only"]["settings"] = "debug build"
        path = _write(self.directory, "diff-settings.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "share one settings profile"):
            validator.validate_record(path, "package_delta", expect_variant=None, check_hashes=False, now=NOW)

    def test_mislabeled_build_rejected(self) -> None:
        record = _valid_package_delta()
        record["builds"]["baseline"]["label"] = "both_variants"
        path = _write(self.directory, "mislabel.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "does not match its own key"):
            validator.validate_record(path, "package_delta", expect_variant=None, check_hashes=False, now=NOW)


class HashVerificationTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)
        self.original_repo_root = validator.REPO_ROOT
        validator.REPO_ROOT = self.directory

    def tearDown(self) -> None:
        validator.REPO_ROOT = self.original_repo_root
        self.tempdir.cleanup()

    def test_correct_hash_accepted(self) -> None:
        evidence_file = self.directory / "source-evidence.txt"
        evidence_file.write_text("some evidence content", encoding="utf-8")
        actual_hash = validator._hash_file(evidence_file)
        record = _valid_variant_gate()
        record["gates"]["licensing"]["sources"] = [{"path": "source-evidence.txt", "sha256": actual_hash}]
        path = _write(self.directory, "gate.json", record)
        validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=True, now=NOW)

    def test_wrong_hash_rejected(self) -> None:
        evidence_file = self.directory / "source-evidence.txt"
        evidence_file.write_text("some evidence content", encoding="utf-8")
        record = _valid_variant_gate()
        record["gates"]["licensing"]["sources"] = [{"path": "source-evidence.txt", "sha256": "0" * 64}]
        path = _write(self.directory, "gate.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "sha256 mismatch"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=True, now=NOW)

    def test_missing_source_file_rejected(self) -> None:
        record = _valid_variant_gate()
        record["gates"]["licensing"]["sources"] = [{"path": "does-not-exist.txt", "sha256": "1" * 64}]
        path = _write(self.directory, "gate.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "source evidence file missing"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=True, now=NOW)


class SourceCommitBindingTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_all_zero_source_commit_rejected(self) -> None:
        record = _valid_variant_gate()
        record["source_commit"] = "0" * 40
        path = _write(self.directory, "zero-commit.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "all-zero sentinel"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)

    def test_short_source_commit_rejected_by_schema(self) -> None:
        record = _valid_variant_gate()
        record["source_commit"] = "abc123"
        path = _write(self.directory, "short-commit.json", record)
        with self.assertRaisesRegex(validator.EvidenceValidationError, "does not match pattern"):
            validator.validate_record(path, "variant_gate", expect_variant="dpdfnet2", check_hashes=False, now=NOW)


class CliEntryPointTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.directory = Path(self.tempdir.name)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_main_returns_zero_for_valid_seed_file(self) -> None:
        exit_code = validator.main(
            [
                "--record-type", "variant_gate",
                "--file", str(PHASE_DIR / "15.1-DPDFNET2-GATE.json"),
                "--expect-variant", "dpdfnet2",
                "--now", "2026-07-24T16:00:00Z",
            ]
        )
        self.assertEqual(exit_code, 0)

    def test_main_returns_one_for_malformed_file(self) -> None:
        path = _write(self.directory, "bad.json", "{not json")
        exit_code = validator.main(
            ["--record-type", "variant_gate", "--file", str(path), "--now", "2026-07-24T16:00:00Z"]
        )
        self.assertEqual(exit_code, 1)

    def test_package_alias_flag(self) -> None:
        path = _write(self.directory, "package.json", _valid_package_delta())
        exit_code = validator.main(["--package", str(path), "--now", "2026-07-24T16:00:00Z"])
        self.assertEqual(exit_code, 0)


if __name__ == "__main__":
    unittest.main()

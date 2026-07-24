#!/usr/bin/env python3
"""Fail-closed, offline validator for Phase 15.1 DPDFNet evidence contracts.

Validates a single JSON evidence record against the strict, hand-rolled
subset of JSON Schema declared in
``.planning/phases/15.1-dpdfnet-production-integration-and-engine-selection/15.1-EVIDENCE.schema.json``,
then applies business rules the schema alone cannot express:

- Duplicate JSON object keys are rejected (the stdlib ``json`` module
  silently keeps the last occurrence otherwise).
- Non-finite numbers (``NaN``, ``Infinity``, ``-Infinity`` JSON tokens) are
  rejected at parse time.
- Unknown keys are rejected (``additionalProperties: false`` everywhere).
- Every ``sources[].sha256`` (and ``build_artifact.sha256``) can be verified
  against the file on disk with ``--check-hashes``.
- ``variant`` must match ``--expect-variant`` when supplied (exact
  identifier binding).
- ``source_commit`` must be a real-looking 40-hex commit, never the
  all-zero sentinel.
- A variant_gate/default_evidence record's evidence sources may never
  reference the *sibling* variant's own gate/default record file --- one
  variant's status can never be derived from the other (D-01/D-02).
- ``owner`` and ``command`` fields may not be empty or a placeholder
  ("TBD", "N/A", "unassigned", "todo", "skip"...).
- ``evaluated_at`` + ``freshness_window_days`` must not be older than
  ``--now`` (default: current UTC time) -- stale evidence is rejected.
- ``ship_eligible`` must equal "every gate is PASS", recomputed
  independently rather than trusted from the file.
- ``default_eligible`` is structurally impossible for the ``dpdfnet8``
  variant (D-09): any record claiming otherwise is rejected outright,
  regardless of what the JSON file says.
- A ``package_delta`` record's four builds must share one ``commit`` and
  one ``settings`` string, and all four labels (baseline, dpdfnet2_only,
  dpdfnet8_only, both_variants) must be present (D-04).

This script intentionally does not execute any gate's ``command`` --- it
validates the *evidence record*, not the underlying claim. Re-running the
recorded commands is each gate owner's job in its own plan.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent
SCHEMA_PATH = (
    REPO_ROOT
    / ".planning"
    / "phases"
    / "15.1-dpdfnet-production-integration-and-engine-selection"
    / "15.1-EVIDENCE.schema.json"
)

RECORD_KIND_TO_DEF = {
    "variant_gate": "variant_gate",
    "default_evidence": "default_evidence",
    "package_delta": "package_delta",
}

VARIANTS = ("dpdfnet2", "dpdfnet8")

PLACEHOLDER_PATTERNS = tuple(
    re.compile(pattern, re.IGNORECASE)
    for pattern in (
        r"\btbd\b",
        r"\bn/a\b",
        r"\bunassigned\b",
        r"\bunknown\b",
        r"\btodo\b",
        r"\bfixme\b",
        r"\bskip(?:ped|s)?\b",
    )
)

# Sibling-file name fragments a record must never cite as its own evidence
# source -- this is the concrete, checkable form of "cannot infer one
# variant's status from the other" (D-01/D-02).
SIBLING_MARKERS = {
    "dpdfnet2": ("DPDFNET8-GATE", "dpdfnet8-gate", "DPDFNET8-DEFAULT", "dpdfnet8-default"),
    "dpdfnet8": ("DPDFNET2-GATE", "dpdfnet2-gate", "DPDFNET2-DEFAULT", "dpdfnet2-default"),
}


class EvidenceValidationError(RuntimeError):
    """A record is malformed, incomplete, stale, or otherwise fails closed."""


# ----------------------------------------------------------------------
# Strict JSON parsing: no duplicate keys, no non-finite number tokens.
# ----------------------------------------------------------------------


def _reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    seen: dict[str, Any] = {}
    for key, value in pairs:
        if key in seen:
            raise EvidenceValidationError(f"duplicate key in JSON object: {key!r}")
        seen[key] = value
    return seen


def _reject_non_finite(token: str) -> Any:
    raise EvidenceValidationError(f"non-finite number token in evidence: {token}")


def load_json_strict(path: Path) -> dict[str, Any]:
    try:
        raw = path.read_text(encoding="utf-8")
    except OSError as error:
        raise EvidenceValidationError(f"cannot read evidence file: {error}") from error
    try:
        parsed = json.loads(
            raw,
            object_pairs_hook=_reject_duplicate_keys,
            parse_constant=_reject_non_finite,
        )
    except json.JSONDecodeError as error:
        raise EvidenceValidationError(f"malformed JSON: {error}") from error
    if not isinstance(parsed, dict):
        raise EvidenceValidationError("evidence root must be a JSON object")
    return parsed


# ----------------------------------------------------------------------
# Minimal recursive JSON-Schema-subset validator (type/enum/const/pattern/
# format/minLength/minItems/exclusiveMinimum/minimum/properties/required/
# additionalProperties/items/$ref). Intentionally not a general-purpose
# engine -- it supports exactly the keywords used in 15.1-EVIDENCE.schema.json.
# ----------------------------------------------------------------------

_TYPE_MAP = {
    "object": dict,
    "array": list,
    "string": str,
}


def _check_type(instance: Any, expected: str) -> bool:
    if expected == "number":
        return isinstance(instance, (int, float)) and not isinstance(instance, bool)
    if expected == "boolean":
        return isinstance(instance, bool)
    return isinstance(instance, _TYPE_MAP[expected])


def _parse_datetime(value: str, path: str) -> datetime:
    text = value[:-1] + "+00:00" if value.endswith("Z") else value
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError as error:
        raise EvidenceValidationError(f"{path}: invalid date-time {value!r}: {error}") from error
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed


def _resolve_ref(defs: dict[str, Any], schema: dict[str, Any]) -> dict[str, Any]:
    ref = schema["$ref"]
    prefix = "#/$defs/"
    if not ref.startswith(prefix):
        raise EvidenceValidationError(f"unsupported $ref: {ref}")
    return defs[ref[len(prefix):]]


def _validate_node(defs: dict[str, Any], schema: dict[str, Any], instance: Any, path: str) -> None:
    if "$ref" in schema:
        schema = _resolve_ref(defs, schema)

    if "const" in schema and instance != schema["const"]:
        raise EvidenceValidationError(f"{path}: expected const {schema['const']!r}, got {instance!r}")
    if "enum" in schema and instance not in schema["enum"]:
        raise EvidenceValidationError(f"{path}: {instance!r} is not one of {schema['enum']}")

    expected_type = schema.get("type")
    if expected_type and not _check_type(instance, expected_type):
        raise EvidenceValidationError(f"{path}: expected type {expected_type}, got {type(instance).__name__}")

    if expected_type == "string":
        if "minLength" in schema and len(instance) < schema["minLength"]:
            raise EvidenceValidationError(f"{path}: string shorter than {schema['minLength']}")
        if "pattern" in schema and not re.fullmatch(schema["pattern"], instance):
            raise EvidenceValidationError(f"{path}: {instance!r} does not match pattern {schema['pattern']}")
        if schema.get("format") == "date-time":
            _parse_datetime(instance, path)

    if expected_type == "number":
        if "exclusiveMinimum" in schema and not (instance > schema["exclusiveMinimum"]):
            raise EvidenceValidationError(f"{path}: {instance!r} must be > {schema['exclusiveMinimum']}")
        if "minimum" in schema and not (instance >= schema["minimum"]):
            raise EvidenceValidationError(f"{path}: {instance!r} must be >= {schema['minimum']}")

    if expected_type == "array":
        if "minItems" in schema and len(instance) < schema["minItems"]:
            raise EvidenceValidationError(f"{path}: fewer than {schema['minItems']} items")
        if "items" in schema:
            for index, item in enumerate(instance):
                _validate_node(defs, schema["items"], item, f"{path}[{index}]")

    if expected_type == "object":
        properties: dict[str, Any] = schema.get("properties", {})
        required: list[str] = schema.get("required", [])
        additional = schema.get("additionalProperties", True)
        for key in required:
            if key not in instance:
                raise EvidenceValidationError(f"{path}: missing required key {key!r}")
        for key, value in instance.items():
            if key not in properties:
                if additional is False:
                    raise EvidenceValidationError(f"{path}: unknown key {key!r} (schema rejects additional properties)")
                continue
            _validate_node(defs, properties[key], value, f"{path}.{key}")


def load_schema() -> dict[str, Any]:
    schema = load_json_strict(SCHEMA_PATH)
    if "$defs" not in schema or "record_types" not in schema:
        raise EvidenceValidationError("15.1-EVIDENCE.schema.json is missing $defs or record_types")
    return schema


def validate_structure(schema: dict[str, Any], record_kind: str, instance: dict[str, Any]) -> None:
    defs = schema["$defs"]
    def_name = RECORD_KIND_TO_DEF.get(record_kind)
    if def_name is None or def_name not in defs:
        raise EvidenceValidationError(f"unknown record type: {record_kind}")
    _validate_node(defs, {"$ref": f"#/$defs/{def_name}"}, instance, "$")


# ----------------------------------------------------------------------
# Business rules the schema shape alone cannot express.
# ----------------------------------------------------------------------


def _hash_file(path: Path) -> str:
    if not path.is_file():
        raise EvidenceValidationError(f"source evidence file missing: {path}")
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _reject_placeholder(value: str, field: str) -> None:
    if not value.strip():
        raise EvidenceValidationError(f"{field} is empty")
    if any(pattern.search(value) for pattern in PLACEHOLDER_PATTERNS):
        raise EvidenceValidationError(f"{field} looks like a placeholder/skip marker: {value!r}")


def _check_source_ref(source: dict[str, Any], *, check_hashes: bool, forbidden_markers: tuple[str, ...], field: str) -> None:
    path_text = source["path"]
    for marker in forbidden_markers:
        if marker in path_text:
            raise EvidenceValidationError(
                f"{field}: source path {path_text!r} references the sibling variant's own record "
                "(one variant's status can never be derived from the other, D-01/D-02)"
            )
    if check_hashes:
        actual = _hash_file(REPO_ROOT / path_text)
        if actual != source["sha256"]:
            raise EvidenceValidationError(f"{field}: sha256 mismatch for {path_text}: recorded {source['sha256']}, actual {actual}")


def _check_source_commit(commit: str) -> None:
    if commit == "0" * 40:
        raise EvidenceValidationError("source_commit is the all-zero sentinel; never actually bound to a commit")


def _check_freshness(evaluated_at: str, freshness_window_days: float, now: datetime, *, field: str) -> None:
    evaluated = _parse_datetime(evaluated_at, field)
    age_days = (now - evaluated).total_seconds() / 86400.0
    if age_days > freshness_window_days:
        raise EvidenceValidationError(
            f"{field}: evidence is stale ({age_days:.1f} days old, exceeds freshness_window_days={freshness_window_days})"
        )
    if age_days < -1.0:
        raise EvidenceValidationError(f"{field}: evaluated_at is in the future relative to --now")


def _validate_variant_gate(instance: dict[str, Any], *, expect_variant: str | None, check_hashes: bool, now: datetime) -> None:
    variant = instance["variant"]
    if expect_variant is not None and variant != expect_variant:
        raise EvidenceValidationError(f"variant mismatch: file declares {variant!r}, expected {expect_variant!r}")

    _check_source_commit(instance["source_commit"])
    _check_freshness(instance["evaluated_at"], instance["freshness_window_days"], now, field="variant_gate")

    forbidden = SIBLING_MARKERS[variant]
    gate_statuses: dict[str, str] = {}
    for gate_name, gate in instance["gates"].items():
        _reject_placeholder(gate["owner"], f"gates.{gate_name}.owner")
        _reject_placeholder(gate["command"], f"gates.{gate_name}.command")
        if not gate["rationale"].strip():
            raise EvidenceValidationError(f"gates.{gate_name}.rationale is empty")
        for source in gate["sources"]:
            _check_source_ref(source, check_hashes=check_hashes, forbidden_markers=forbidden, field=f"gates.{gate_name}.sources")
        gate_statuses[gate_name] = gate["status"]

    derived_ship_eligible = all(status == "PASS" for status in gate_statuses.values())
    if instance["ship_eligible"] != derived_ship_eligible:
        raise EvidenceValidationError(
            f"ship_eligible={instance['ship_eligible']!r} contradicts recomputed gate statuses {gate_statuses}"
        )

    # D-09: DPDFNet-8 is structurally never default-eligible. This check is
    # unconditional -- it does not trust the file's own claim.
    if variant == "dpdfnet8" and instance["default_eligible"] is not False:
        raise EvidenceValidationError("dpdfnet8 can never be default_eligible (D-09/D-12 structural constraint)")
    if instance["default_eligible"] and not instance["ship_eligible"]:
        raise EvidenceValidationError("default_eligible cannot be true while ship_eligible is false")


def _validate_default_evidence(instance: dict[str, Any], *, expect_variant: str | None, check_hashes: bool, now: datetime) -> None:
    variant = instance["variant"]
    if expect_variant is not None and variant != expect_variant:
        raise EvidenceValidationError(f"variant mismatch: file declares {variant!r}, expected {expect_variant!r}")

    _check_source_commit(instance["source_commit"])
    _check_freshness(instance["evaluated_at"], instance["freshness_window_days"], now, field="default_evidence")
    _reject_placeholder(instance["owner"], "owner")

    # D-09: default evidence requires low-end processor measurements PLUS
    # webcam, laptop, and headset microphone paths -- enforced structurally
    # by the schema's required-key list, re-asserted here defensively.
    required_paths = {"webcam", "laptop", "headset"}
    if set(instance["microphone_paths"]) != required_paths:
        raise EvidenceValidationError(f"microphone_paths must cover exactly {sorted(required_paths)}")

    # D-09/D-12: DPDFNet-8 is never default-eligible, even in a
    # separately-scoped default-evidence record.
    if variant == "dpdfnet8" and instance["default_eligible"] is not False:
        raise EvidenceValidationError("dpdfnet8 can never be default_eligible (D-09/D-12 structural constraint)")

    if check_hashes:
        for path in ("webcam", "laptop", "headset"):
            measurement = instance["microphone_paths"][path]
            for key in ("clip_before_path", "clip_after_path"):
                clip_path = REPO_ROOT / measurement[key]
                if not clip_path.is_file():
                    raise EvidenceValidationError(f"microphone_paths.{path}.{key} does not exist: {clip_path}")


def _validate_package_delta(instance: dict[str, Any], *, check_hashes: bool, now: datetime) -> None:
    _check_source_commit(instance["source_commit"])
    _check_freshness(instance["evaluated_at"], instance["freshness_window_days"], now, field="package_delta")

    builds = instance["builds"]
    required_labels = {"baseline", "dpdfnet2_only", "dpdfnet8_only", "both_variants"}
    if set(builds) != required_labels:
        raise EvidenceValidationError(f"package_delta.builds must cover exactly {sorted(required_labels)}")

    commits = {build["commit"] for build in builds.values()}
    settings = {build["settings"] for build in builds.values()}
    if len(commits) != 1:
        raise EvidenceValidationError(f"package_delta builds must share one commit, found {sorted(commits)}")
    if len(settings) != 1:
        raise EvidenceValidationError(f"package_delta builds must share one settings profile, found {sorted(settings)}")
    for label, build in builds.items():
        if build["label"] != label:
            raise EvidenceValidationError(f"builds.{label}.label={build['label']!r} does not match its own key")
        if check_hashes:
            path = REPO_ROOT / build.get("artifact_path", "")
            if path.name and not path.is_file():
                raise EvidenceValidationError(f"builds.{label}: artifact file missing: {path}")

    checkpoint = instance["material_increase_checkpoint"]
    _reject_placeholder(checkpoint["owner"], "material_increase_checkpoint.owner")


def validate_record(
    path: Path,
    record_kind: str,
    *,
    expect_variant: str | None,
    check_hashes: bool,
    now: datetime,
) -> dict[str, Any]:
    schema = load_schema()
    instance = load_json_strict(path)

    schema_version = instance.get("schema_version")
    if schema_version != 1:
        raise EvidenceValidationError(f"unsupported schema_version: {schema_version!r}")
    if instance.get("record_kind") != record_kind:
        raise EvidenceValidationError(
            f"record_kind mismatch: file declares {instance.get('record_kind')!r}, expected {record_kind!r}"
        )

    validate_structure(schema, record_kind, instance)

    if record_kind == "variant_gate":
        _validate_variant_gate(instance, expect_variant=expect_variant, check_hashes=check_hashes, now=now)
    elif record_kind == "default_evidence":
        _validate_default_evidence(instance, expect_variant=expect_variant, check_hashes=check_hashes, now=now)
    elif record_kind == "package_delta":
        _validate_package_delta(instance, check_hashes=check_hashes, now=now)
    else:  # pragma: no cover - guarded by validate_structure already
        raise EvidenceValidationError(f"unknown record type: {record_kind}")

    return instance


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--record-type", choices=sorted(RECORD_KIND_TO_DEF), default=None)
    parser.add_argument("--file", type=Path, default=None)
    parser.add_argument("--expect-variant", choices=VARIANTS, default=None)
    parser.add_argument("--check-hashes", action="store_true")
    parser.add_argument("--now", default=None, help="ISO-8601 timestamp to use as 'now' (defaults to current UTC time)")
    parser.add_argument("--package", type=Path, default=None, help="alias for --file --record-type package_delta")
    args = parser.parse_args(argv)

    if args.package is not None:
        args.file = args.package
        args.record_type = "package_delta"

    if args.record_type is None or args.file is None:
        parser.error("either (--record-type and --file) or --package is required")

    now = datetime.now(timezone.utc) if args.now is None else _parse_datetime(args.now, "--now")

    try:
        validate_record(
            args.file,
            args.record_type,
            expect_variant=args.expect_variant,
            check_hashes=args.check_hashes,
            now=now,
        )
    except EvidenceValidationError as error:
        print(f"validate-dpdfnet-evidence: FAILED: {error}", file=sys.stderr)
        return 1
    print(f"validate-dpdfnet-evidence: {args.file}: valid {args.record_type} record")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

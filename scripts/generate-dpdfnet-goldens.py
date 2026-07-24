#!/usr/bin/env python3
"""Deterministic, offline-only golden-vector generator for DPDFNet-2/8.

Phase 15.1, Plan 01, Task 2 (D-01/D-02): consumes ONLY the atomically staged,
hash-verified `vendor/dpdfnet-reference/` tree produced by
`scripts/fetch-vendors.sh --dpdfnet-reference` and the additive
`dpdfnet-golden-probe` binary it contains. Never touches the network, never
imports or runs `DpdfnetExperimentalEngine`, and is safe to re-run any number
of times: two consecutive network-denied generations are byte-identical
(verified by --check-reproducible), and a tampered/incomplete staged input
fails BEFORE any golden file is replaced.

Usage:
    python3 scripts/generate-dpdfnet-goldens.py \\
        --reference-root vendor/dpdfnet-reference \\
        --output-dir tests/fixtures/dpdfnet \\
        [--check-reproducible]

Exit codes: 0 on success; 1 on any preflight, generation, or reproducibility
failure (never leaves a partially-written golden file behind).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import struct
import subprocess
import sys
import wave
from pathlib import Path
from typing import Any

# ─────────────────────────────────────────────────────────────────────────
# Pinned identifiers (mirror scripts/fetch-vendors.sh's DPDFNet reference
# constants exactly; see that script's own comments for provenance). Kept as
# independent constants here (not imported from bash) so this generator can
# never silently drift onto whatever a compromised MANIFEST.json claims --
# every value below is cross-checked against the manifest, never trusted
# from it alone.
# ─────────────────────────────────────────────────────────────────────────
HUSHMIC_COMMIT = "5f7d3180d07e5636d694ca6d4e2a1d1dfe3d42b7"
DPDFNET_COMMIT = "fadf269abc8207743cc7ff05e8cace6154008c04"
HUSHMIC_SOURCE_COMMIT_FILE_SHA256 = (
    "ab22118c97d680cb62ec24dfd3ccfeeec30a5d4dfba6878f9ee71ad2a10eded1"
)
RENDERER_SHA256 = "744459f2e227dfb9ff2fa0906fb56368feeeb6e9503c328052426ff9e587ce91"
ORT_RUNTIME_SHA256 = "4061866361d9a8d2872f5f419c5515ce35a830a0c5c77ce1723320ac0dbabfc7"

VARIANTS: dict[str, dict[str, str]] = {
    "dpdfnet2": {
        "model_path": "models/dpdfnet2_48khz_hr.onnx",
        "model_sha256": "7f0575a5cec0ba4ffd8f8bd657e06d007e4ccdd955d76faab922b9d3291dc14b",
        "version": "DPDFNet-2 v0.5.1",
    },
    "dpdfnet8": {
        "model_path": "models/dpdfnet8_48khz_hr.onnx",
        "model_sha256": "7b3afbb260a08fe9af3d16e3bda992971be1e7e951d1dee7c2d235f5c43f5631",
        "version": "DPDFNet-8 v0.5.1",
    },
}

# The fixed, documented 50 dB attenuation-limit convention already established
# by Spike 001 / the dpdfnet8+dpdfnet2 adapter descriptors ("CleanMic Balanced").
ATTN_DB = 50.0

# Fixture: a short, entirely formula-derived (non-random) 48 kHz mono signal.
# Three fixed sine tones summed at fixed amplitudes -- always produces the
# exact same samples on every machine/run.
FIXTURE_SAMPLE_RATE = 48_000
FIXTURE_HOP = 480
FIXTURE_HOPS = 8
FIXTURE_TONES = ((440.0, 0.20), (1000.0, 0.05), (4000.0, 0.02))
SINGLE_SPEAKER_CLAIM_LIMITATION = (
    "Golden vectors describe pinned reference RENDERER/MODEL behavior on a "
    "synthetic fixture, not a perceptual quality claim. Phase 15's evaluation "
    "used a single speaker (Simon Villeneuve); no broader-population quality "
    "claim is made or implied by this file."
)

EXPECTED_MANIFEST_SOURCE_KEYS = {"hushmic", "dpdfnet"}
EXPECTED_MANIFEST_ARTIFACT_KEYS = {
    "renderer",
    "golden_probe",
    "runtime",
    "dpdfnet2_model",
    "dpdfnet8_model",
}


class GoldenGenerationError(RuntimeError):
    """Raised on any preflight, generation, or validation failure."""


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def require_pinned_file(root: Path, rel_path: str, expected_sha256: str, label: str) -> Path:
    """Resolve one regular, non-symlink file strictly beneath `root` and
    verify its exact SHA-256. Rejects symlinks and any path that escapes
    `root` (traversal), matching the same fail-closed contract already used
    by `scripts/fetch-vendors.sh` and the model-eval adapters."""
    root = root.resolve()
    candidate = (root / rel_path).resolve()
    try:
        candidate.relative_to(root)
    except ValueError as error:
        raise GoldenGenerationError(f"{label}: escapes reference root ({candidate})") from error
    if (root / rel_path).is_symlink():
        raise GoldenGenerationError(f"{label}: refusing a symlink at {rel_path}")
    if not candidate.is_file():
        raise GoldenGenerationError(f"{label}: missing regular file at {rel_path}")
    actual = sha256_file(candidate)
    if actual != expected_sha256:
        raise GoldenGenerationError(
            f"{label}: SHA256 mismatch for {rel_path}\n"
            f"    expected: {expected_sha256}\n"
            f"    actual:   {actual}"
        )
    return candidate


def strict_load_manifest(reference_root: Path) -> dict[str, Any]:
    """Load and strictly validate `MANIFEST.json`: reject unknown top-level
    fields, unknown source/artifact identifiers, and any commit/hash value
    that does not match this script's own pinned constants (never trust the
    manifest as the sole source of truth for what a "pin" is)."""
    manifest_path = reference_root / "MANIFEST.json"
    if manifest_path.is_symlink():
        raise GoldenGenerationError("MANIFEST.json: refusing a symlink")
    if not manifest_path.is_file():
        raise GoldenGenerationError(f"MANIFEST.json missing at {manifest_path}")
    try:
        manifest = json.loads(manifest_path.read_text())
    except json.JSONDecodeError as error:
        raise GoldenGenerationError(f"MANIFEST.json is not valid JSON: {error}") from error

    known_top_level = {"schema_version", "generated_at", "sources", "artifacts"}
    unknown_top = set(manifest.keys()) - known_top_level
    if unknown_top:
        raise GoldenGenerationError(f"MANIFEST.json has unknown top-level field(s): {sorted(unknown_top)}")
    if manifest.get("schema_version") != 1:
        raise GoldenGenerationError(f"MANIFEST.json unexpected schema_version: {manifest.get('schema_version')!r}")

    sources = manifest.get("sources", {})
    unknown_sources = set(sources.keys()) - EXPECTED_MANIFEST_SOURCE_KEYS
    if unknown_sources or set(sources.keys()) != EXPECTED_MANIFEST_SOURCE_KEYS:
        raise GoldenGenerationError(f"MANIFEST.json sources must be exactly {sorted(EXPECTED_MANIFEST_SOURCE_KEYS)}, got {sorted(sources.keys())}")
    if sources["hushmic"].get("commit") != HUSHMIC_COMMIT:
        raise GoldenGenerationError("MANIFEST.json hushmic commit does not match the pinned commit")
    if sources["dpdfnet"].get("commit") != DPDFNET_COMMIT:
        raise GoldenGenerationError("MANIFEST.json dpdfnet commit does not match the pinned commit")

    artifacts = manifest.get("artifacts", {})
    if set(artifacts.keys()) != EXPECTED_MANIFEST_ARTIFACT_KEYS:
        raise GoldenGenerationError(
            f"MANIFEST.json artifacts must be exactly {sorted(EXPECTED_MANIFEST_ARTIFACT_KEYS)}, got {sorted(artifacts.keys())}"
        )
    if artifacts["renderer"].get("sha256") != RENDERER_SHA256:
        raise GoldenGenerationError("MANIFEST.json renderer sha256 does not match the pinned hash")
    if artifacts["runtime"].get("sha256") != ORT_RUNTIME_SHA256:
        raise GoldenGenerationError("MANIFEST.json runtime sha256 does not match the pinned hash")
    for variant, info in VARIANTS.items():
        key = f"{variant}_model"
        if artifacts[key].get("sha256") != info["model_sha256"]:
            raise GoldenGenerationError(f"MANIFEST.json {key} sha256 does not match the pinned hash")

    return manifest


def build_fixture_wav(path: Path) -> bytes:
    """Write a deterministic, formula-derived 48 kHz mono 16-bit PCM WAV and
    return its raw bytes. Never random; always the exact same samples."""
    n = FIXTURE_HOP * FIXTURE_HOPS
    frames = bytearray()
    for i in range(n):
        t = i / FIXTURE_SAMPLE_RATE
        value = sum(amp * math.sin(2.0 * math.pi * freq * t) for freq, amp in FIXTURE_TONES)
        quantized = max(-32768, min(32767, int(round(value * 32767.0))))
        frames += struct.pack("<h", quantized)
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(FIXTURE_SAMPLE_RATE)
        w.writeframes(bytes(frames))
    return path.read_bytes()


def network_denying_env(extra_bin_dir: Path, runtime_path: Path) -> dict[str, str]:
    """A restricted environment for invoking the probe: every proxy variable
    is cleared, and `git`/`curl`/`wget` are shimmed in front of PATH to exit
    non-zero if ever invoked. The probe itself makes no network calls in its
    own code (pure local file I/O + CPU-only ONNX Runtime inference); this is
    a defense-in-depth guard against an unexpected dependency regression, not
    a claim that this sandbox can enforce a kernel-level network deny (this
    environment's unprivileged `unshare --net` is unavailable)."""
    env = dict(os.environ)
    for var in (
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ):
        env.pop(var, None)
    extra_bin_dir.mkdir(parents=True, exist_ok=True)
    for tool in ("git", "curl", "wget"):
        shim = extra_bin_dir / tool
        shim.write_text('#!/usr/bin/env bash\necho "FORBIDDEN: network tool invoked during offline golden generation" >&2\nexit 99\n')
        shim.chmod(0o755)
    env["PATH"] = f"{extra_bin_dir}{os.pathsep}{env.get('PATH', '')}"
    # The probe binary's build-time default ONNX Runtime path is baked in as
    # an absolute path inside the (now-deleted) acquisition build directory;
    # once copied into the staged `vendor/dpdfnet-reference/bin/`, it must be
    # pointed explicitly at the staged runtime, or `ort::init_from` degrades
    # into an indefinite hang inside this sandbox rather than a fast error.
    env["ORT_DYLIB_PATH"] = str(runtime_path)
    return env


def run_probe(probe_path: Path, model_path: Path, fixture_wav: Path, out_json: Path, workdir: Path, runtime_path: Path) -> dict[str, Any]:
    env = network_denying_env(workdir / "deny-bin", runtime_path)
    result = subprocess.run(
        [str(probe_path), str(model_path), str(fixture_wav), str(out_json), str(ATTN_DB)],
        cwd=str(workdir),
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=300,
        check=False,
    )
    if result.returncode != 0:
        raise GoldenGenerationError(
            f"golden probe exited {result.returncode}\nstdout: {result.stdout.decode('utf-8', 'replace')}\nstderr: {result.stderr.decode('utf-8', 'replace')}"
        )
    if not out_json.is_file():
        raise GoldenGenerationError("golden probe reported success but produced no output JSON")
    return json.loads(out_json.read_text())


def validate_probe_output(raw: dict[str, Any], variant: str) -> None:
    if raw.get("sample_rate") != FIXTURE_SAMPLE_RATE:
        raise GoldenGenerationError(f"{variant}: probe sample_rate {raw.get('sample_rate')} != {FIXTURE_SAMPLE_RATE}")
    if raw.get("n_fft") != 960:
        raise GoldenGenerationError(f"{variant}: probe n_fft {raw.get('n_fft')} != 960")
    if raw.get("hop") != 480:
        raise GoldenGenerationError(f"{variant}: probe hop {raw.get('hop')} != 480")
    if raw.get("freq_bins") != 481:
        raise GoldenGenerationError(f"{variant}: probe freq_bins {raw.get('freq_bins')} != 481")
    hops = raw.get("hops", [])
    if len(hops) != FIXTURE_HOPS:
        raise GoldenGenerationError(f"{variant}: expected {FIXTURE_HOPS} hops, got {len(hops)}")
    for hop in hops:
        spec = hop["input_spectrum"]
        spec_e = hop["enhanced_spectrum"]
        out = hop["output"]
        if len(spec) != 962 or len(spec_e) != 962:
            raise GoldenGenerationError(f"{variant}: hop {hop['index']} spectrum length != 962 (FREQ_BINS*2)")
        if len(out) != 480:
            raise GoldenGenerationError(f"{variant}: hop {hop['index']} output length != 480")
        for value in (*spec, *spec_e, *out):
            if not math.isfinite(value):
                raise GoldenGenerationError(f"{variant}: hop {hop['index']} contains a non-finite value")
    for value in (*raw.get("initial_state", []), *raw.get("final_state", [])):
        if not math.isfinite(value):
            raise GoldenGenerationError(f"{variant}: recurrent state contains a non-finite value")


def floats_to_le_bytes(values: list[float]) -> bytes:
    return b"".join(struct.pack("<f", v) for v in values)


def build_golden_record(
    variant: str,
    manifest: dict[str, Any],
    manifest_sha256: str,
    probe_sha256: str,
    model_sha256: str,
    fixture_bytes: bytes,
    raw: dict[str, Any],
) -> dict[str, Any]:
    initial_state = raw["initial_state"]
    final_state = raw["final_state"]
    hops = [
        {
            "index": hop["index"],
            "input_spectrum": hop["input_spectrum"],
            "enhanced_spectrum": hop["enhanced_spectrum"],
            "output": hop["output"],
        }
        for hop in raw["hops"]
    ]
    return {
        "schema_version": 1,
        "variant": variant,
        "manifest_sha256": manifest_sha256,
        "probe": {"path": "bin/dpdfnet-golden-probe", "sha256": probe_sha256},
        "model": {"path": VARIANTS[variant]["model_path"], "sha256": model_sha256, "version": VARIANTS[variant]["version"]},
        "runtime": {"path": "lib/libonnxruntime.so", "sha256": ORT_RUNTIME_SHA256},
        "source_commit": {
            "hushmic": HUSHMIC_COMMIT,
            "dpdfnet": DPDFNET_COMMIT,
            "commit_file_sha256": HUSHMIC_SOURCE_COMMIT_FILE_SHA256,
        },
        "fixture": {
            "recipe": (
                "sum of three fixed sine tones (440 Hz @ 0.20, 1000 Hz @ 0.05, "
                "4000 Hz @ 0.02 amplitude) sampled at 48000 Hz for 8 hops "
                "(3840 samples), quantized to signed 16-bit PCM -- no randomness"
            ),
            "sample_rate": FIXTURE_SAMPLE_RATE,
            "hops": FIXTURE_HOPS,
            "sha256": sha256_bytes(fixture_bytes),
        },
        "format": {
            "n_fft": raw["n_fft"],
            "hop": raw["hop"],
            "freq_bins": raw["freq_bins"],
            "sample_rate": raw["sample_rate"],
            "attn_db": raw["attn_db"],
        },
        "metadata_derived_initial_state": {
            "state_size": raw["state_size"],
            "nonzero_count": sum(1 for v in initial_state if v != 0.0),
            "sha256": sha256_bytes(floats_to_le_bytes(initial_state)),
        },
        "final_state_sha256": sha256_bytes(floats_to_le_bytes(final_state)),
        "hops": hops,
        "single_speaker_claim_limitation": SINGLE_SPEAKER_CLAIM_LIMITATION,
    }


def generate_variant(variant: str, reference_root: Path, manifest: dict[str, Any], manifest_sha256: str, workdir: Path) -> dict[str, Any]:
    info = VARIANTS[variant]
    probe_path = require_pinned_file(reference_root, "bin/dpdfnet-golden-probe", manifest["artifacts"]["golden_probe"]["sha256"], f"{variant} golden probe")
    runtime_path = require_pinned_file(reference_root, "lib/libonnxruntime.so", ORT_RUNTIME_SHA256, f"{variant} ONNX Runtime")
    model_path = require_pinned_file(reference_root, info["model_path"], info["model_sha256"], f"{variant} model")

    workdir.mkdir(parents=True, exist_ok=True)
    fixture_wav = workdir / "fixture.wav"
    fixture_bytes_wav = build_fixture_wav(fixture_wav)
    out_json = workdir / f"{variant}-probe-raw.json"

    raw = run_probe(probe_path, model_path, fixture_wav, out_json, workdir, runtime_path)
    validate_probe_output(raw, variant)

    probe_sha256 = sha256_file(probe_path)
    return build_golden_record(variant, manifest, manifest_sha256, probe_sha256, info["model_sha256"], fixture_bytes_wav, raw)


def atomic_write_json(path: Path, data: dict[str, Any]) -> None:
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(data, indent=2) + "\n")
    tmp.rename(path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference-root", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--check-reproducible", action="store_true")
    args = parser.parse_args()

    reference_root: Path = args.reference_root.resolve()
    output_dir: Path = args.output_dir.resolve()

    try:
        manifest = strict_load_manifest(reference_root)
        manifest_sha256 = sha256_file(reference_root / "MANIFEST.json")

        import tempfile

        with tempfile.TemporaryDirectory(prefix="dpdfnet-golden-gen-") as td1:
            first_pass = {
                variant: generate_variant(variant, reference_root, manifest, manifest_sha256, Path(td1) / variant)
                for variant in VARIANTS
            }

            if args.check_reproducible:
                with tempfile.TemporaryDirectory(prefix="dpdfnet-golden-gen-repro-") as td2:
                    second_pass = {
                        variant: generate_variant(variant, reference_root, manifest, manifest_sha256, Path(td2) / variant)
                        for variant in VARIANTS
                    }
                for variant in VARIANTS:
                    if json.dumps(first_pass[variant], sort_keys=True) != json.dumps(second_pass[variant], sort_keys=True):
                        raise GoldenGenerationError(
                            f"{variant}: two consecutive network-denied generations were NOT byte-identical "
                            "-- refusing to write any golden file"
                        )
                print("[generate-dpdfnet-goldens] reproducibility check passed: both variants byte-identical across two runs", file=sys.stderr)

            output_dir.mkdir(parents=True, exist_ok=True)
            for variant, record in first_pass.items():
                atomic_write_json(output_dir / f"golden-{variant}.json", record)
                print(f"[generate-dpdfnet-goldens] wrote {output_dir / f'golden-{variant}.json'}", file=sys.stderr)

    except GoldenGenerationError as error:
        print(f"[generate-dpdfnet-goldens] ERROR: {error}", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())

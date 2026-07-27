#!/usr/bin/env bash
#
# run-dpdfnet-default-evidence.sh -- Precommitted, fail-closed, non-networking
# evidence runner for the Phase 15.2 DPDFNet-2 default-eligibility decision
# (D-01/D-06) and the accompanying webcam/laptop/headset microphone survey.
#
# This script is the exact procedure 15.1-08 (source-15.1-08.reference.md)
# precommitted before any measurement was taken, adapted to the evidence
# validator surface that actually exists today (--record-type default_evidence
# per scripts/validate-dpdfnet-evidence.py). It is meant to be run BY THE
# OWNER during the 15.2-03 checkpoint, on real webcam/built-in-laptop/headset
# hardware attached to this machine. It does not invent a default decision --
# it only captures measurements into .planning/phases/15.2-dpdfnet-evaluation-
# and-decision/15.2-DEFAULT-EVIDENCE.json's `low_end_processor` and
# `microphone_paths` fields, leaving `owner` and `default_eligible` untouched
# for the owner to fill in by hand once they have reviewed the numbers.
#
# ---------------------------------------------------------------------------
# REMINDER (D-06): every CPU figure this script produces is SIMULATED /
# DEV-BOX-DERIVED, using systemd-run's CPUQuota to throttle THIS machine's
# CPU to a fraction of one core (the exact method validated in
# .planning/spikes/005-lowend-cpu-headroom/README.md). It is NOT a
# measurement on genuine low-end hardware. A real weak-CPU run is an
# explicit DEFERRED FOLLOW-UP (see 15.2-CONTEXT.md "Deferred Ideas"),
# never a substitute for the owner's sign-off in the 15.2-03 checkpoint.
# ---------------------------------------------------------------------------
#
# No network access is performed anywhere in this script. The AppImage under
# test must already be built locally (scripts/build-appimage.sh); this
# runner only reads it from disk and verifies its hash.
#
# Usage:
#   scripts/run-dpdfnet-default-evidence.sh [options]
#
# Options:
#   --appimage <path>       Path to the built DPDFNet-2-only AppImage.
#                           Default: build/CleanMic-x86_64-dpdfnet2.AppImage
#   --cpu-quota <percent>   systemd-run CPUQuota percentage simulating the
#                           "low-end processor" figure persisted into
#                           low_end_processor (per Spike 005's table).
#                           Default: 25 (~4x slower than this dev box --
#                           the most aggressive level Spike 005 measured).
#   --clip-seconds <n>      Duration, in seconds, of each before/after
#                           microphone clip. Default: 15.
#   --with-khip-comparison  Also bench Khip (informational only -- Khip has
#                           no slot in the default_evidence schema) and log
#                           which libkhip.so (if any) is active (RESEARCH
#                           Pitfall 4).
#   --out <path>            Override the default_evidence JSON file to
#                           update in place.
#   -h, --help              Show this help and exit.
#
# Exit codes: 0 on a fully completed, recorded run. Non-zero (with a clear
# message on stderr) on ANY of: wrong AppImage hash, a missing/blank
# microphone-path field, a failed/empty clip recording, or a missing
# required command. This script never silently substitutes or skips a
# required measurement.

set -euo pipefail

# ── Paths ────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PHASE_DIR="$PROJECT_ROOT/.planning/phases/15.2-dpdfnet-evaluation-and-decision"
PACKAGE_DELTA_JSON="$PROJECT_ROOT/.planning/phases/15.1-dpdfnet-production-integration-and-engine-selection/15.1-PACKAGE-DELTA.json"

# ── Defaults (overridable via flags) ────────────────────────────────────
APPIMAGE_PATH="$PROJECT_ROOT/build/CleanMic-x86_64-dpdfnet2.AppImage"
DEFAULT_EVIDENCE_JSON="$PHASE_DIR/15.2-DEFAULT-EVIDENCE.json"
CPU_QUOTA_PERCENT=25
CLIP_SECONDS=15
WITH_KHIP_COMPARISON=0

# ── Fixed protocol constants (D-06 / 15.1-EVIDENCE.schema.json) ─────────
readonly BLOCK_SIZE_SAMPLES=480      # 10 ms hop @ 48 kHz -- CleanMic's fixed pipeline block size
readonly HOP_BUDGET_MS="10.0"        # real-time budget per hop; exceeding this on a hop is a deadline miss
readonly WARMUP_HOPS=500             # ~5 s warm-up before measurement starts
readonly MEASURE_HOPS=12000          # 120 s of audio at 10 ms/hop, per must_haves truth
readonly PINNED_THREADS=1            # one thread, per D-06 / schema's single-thread evidence requirement
readonly MIC_CLASSES=(webcam laptop headset)   # fixed, ordered; never substituted for one another

# ── Small helpers ────────────────────────────────────────────────────────
info()  { printf '\033[1;34m==> %s\033[0m\n' "$*"; }
warn()  { printf '\033[1;33m==> %s\033[0m\n' "$*"; }
error() { printf '\033[1;31m==> %s\033[0m\n' "$*" >&2; }
abort() { error "$*"; exit 1; }

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || abort "Required command not found on PATH: $1"
}

usage() {
  sed -n '2,45p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

# ── Cleanup (temp extraction dir + temp bench example) ──────────────────
EXTRACT_DIR=""
BENCH_EXAMPLE_PATH=""
cleanup() {
  if [ -n "$EXTRACT_DIR" ] && [ -d "$EXTRACT_DIR" ]; then
    rm -rf "$EXTRACT_DIR"
  fi
  if [ -n "$BENCH_EXAMPLE_PATH" ] && [ -f "$BENCH_EXAMPLE_PATH" ]; then
    rm -f "$BENCH_EXAMPLE_PATH"
  fi
}
trap cleanup EXIT

# ── Argument parsing ─────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
  case "$1" in
    --appimage) APPIMAGE_PATH="$2"; shift 2 ;;
    --cpu-quota) CPU_QUOTA_PERCENT="$2"; shift 2 ;;
    --clip-seconds) CLIP_SECONDS="$2"; shift 2 ;;
    --with-khip-comparison) WITH_KHIP_COMPARISON=1; shift ;;
    --out) DEFAULT_EVIDENCE_JSON="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) abort "Unknown argument: $1 (see --help)" ;;
  esac
done

# ── Step 0: the D-06 simulated/dev-box-derived banner ────────────────────
print_simulated_banner() {
  cat <<'BANNER'
================================================================================
 REMINDER (D-06): every CPU figure this runner produces is SIMULATED /
 DEV-BOX-DERIVED via systemd-run CPUQuota throttling on THIS machine's CPU.
 It is NOT a measurement on genuine low-end hardware. A real weak-CPU run
 is an explicit deferred follow-up (15.2-CONTEXT.md "Deferred Ideas"), never
 a substitute for the owner's sign-off in the 15.2-03 checkpoint.
================================================================================
BANNER
}

# ── Step 1: hash-verify the pinned DPDFNet-2-only AppImage (T-15.2-02) ──
# Aborts before ANY measurement on a missing file or a hash mismatch. This
# is one of the two required guard branches (package spoofing guard).
verify_appimage_hash() {
  [ -f "$APPIMAGE_PATH" ] || abort \
    "DPDFNet-2 AppImage not found at: $APPIMAGE_PATH
  Build it first, e.g.: DPDFNET_VARIANTS=dpdfnet2 scripts/build-appimage.sh"

  require_cmd python3
  require_cmd sha256sum

  local expected
  expected="$(python3 - "$PACKAGE_DELTA_JSON" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as fh:
    data = json.load(fh)
print(data["builds"]["dpdfnet2_only"]["sha256"])
PY
  )"
  [ -n "$expected" ] || abort "Could not read builds.dpdfnet2_only.sha256 from $PACKAGE_DELTA_JSON"

  local actual
  actual="$(sha256sum "$APPIMAGE_PATH" | awk '{print $1}')"

  if [ "$actual" != "$expected" ]; then
    abort "AppImage hash mismatch -- refusing to measure an unverified package.
  Expected (15.1-PACKAGE-DELTA.json builds.dpdfnet2_only.sha256): $expected
  Actual   ($APPIMAGE_PATH):                                      $actual
  This is exactly the tampering/spoofing case T-15.2-02 mitigates -- rebuild
  or re-point --appimage at the genuine dpdfnet2_only artifact before retrying."
  fi
  info "AppImage hash verified: $actual"
}

# ── Step 2: extract the verified AppImage locally (no network) ──────────
# Ensures the assets actually benched are the exact hash-verified package,
# not whatever happens to be in build/AppDir on this machine.
extract_appimage_assets() {
  require_cmd sha256sum
  [ -x "$APPIMAGE_PATH" ] || chmod +x "$APPIMAGE_PATH" 2>/dev/null || true

  EXTRACT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dpdfnet-default-evidence.XXXXXX")"
  info "Extracting the verified AppImage locally (no network; local squashfs extraction only) into $EXTRACT_DIR"
  ( cd "$EXTRACT_DIR" && "$APPIMAGE_PATH" --appimage-extract >/dev/null )
  [ -d "$EXTRACT_DIR/squashfs-root" ] || abort "AppImage extraction failed: no squashfs-root produced under $EXTRACT_DIR"
  printf '%s' "$EXTRACT_DIR/squashfs-root"
}

# ── Step 3: capture host/runtime/environment facts (informational) ──────
capture_environment() {
  local cpu_model os_pretty kernel pw_version app_version governor
  cpu_model="$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//')"
  [ -n "$cpu_model" ] || cpu_model="unknown (grep 'model name' /proc/cpuinfo returned nothing on this host)"

  if [ -f /etc/os-release ]; then
    os_pretty="$(sed -n 's/^PRETTY_NAME="\(.*\)"$/\1/p' /etc/os-release)"
  fi
  [ -n "${os_pretty:-}" ] || os_pretty="unknown"

  kernel="$(uname -srmo 2>/dev/null || uname -a)"

  if command -v pw-cli >/dev/null 2>&1; then
    pw_version="$(pw-cli --version 2>/dev/null | head -1)"
  else
    pw_version="pw-cli not found on PATH"
  fi
  [ -n "$pw_version" ] || pw_version="pw-cli --version produced no output"

  if [ -x "$APPIMAGE_PATH" ] || [ -f "$APPIMAGE_PATH" ]; then
    app_version="$("$APPIMAGE_PATH" --version 2>/dev/null || true)"
  fi
  [ -n "${app_version:-}" ] || app_version="AppImage --version unavailable"

  if [ -r /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor ]; then
    governor="$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"
  else
    governor="unavailable (no cpufreq scaling_governor sysfs node on this host)"
  fi

  cat <<EOF
  host_processor        : $cpu_model
  os                    : $os_pretty
  kernel                : $kernel
  pipewire              : $pw_version
  cleanmic_runtime      : $app_version
  cpu_governor/power    : $governor
  block_size_samples    : $BLOCK_SIZE_SAMPLES (10 ms hop @ 48 kHz, fixed)
  simulated_cpu_quota    : ${CPU_QUOTA_PERCENT}% of one core (systemd-run CPUQuota, Spike 005 method)
  measurement_hops       : $MEASURE_HOPS (120 s @ 10 ms/hop)
  warmup_hops            : $WARMUP_HOPS
  pinned_threads         : $PINNED_THREADS
EOF

  # Export for the JSON writer.
  ENV_HOST="$cpu_model ($os_pretty; $kernel; simulated CPUQuota=${CPU_QUOTA_PERCENT}%)"
  ENV_GOVERNOR="$governor"
}

# ── Step 4: log the active Khip library (RESEARCH Pitfall 4) ────────────
log_khip_library_status() {
  local candidates=(
    "${HOME:-}/.local/lib/libkhip.so"
    "/usr/local/lib/libkhip.so"
    "/usr/local/lib64/libkhip.so"
    "/usr/lib/libkhip.so"
    "/usr/lib/x86_64-linux-gnu/libkhip.so"
    "/usr/lib64/libkhip.so"
  )
  local found=""
  local candidate
  for candidate in "${candidates[@]}"; do
    if [ -f "$candidate" ]; then
      found="$candidate"
      break
    fi
  done
  if [ -n "$found" ]; then
    info "Khip library ACTIVE for this comparison run: $found"
  else
    warn "Khip library NOT found (checked: ${candidates[*]}). Khip is never bundled" \
         "(user-supplied only) -- any Khip figures from this session are not meaningful" \
         "without a real libkhip.so installed and must not be silently reused across machines."
  fi
}

# ── Step 5: write the throwaway CPU-time hop-timing bench source ────────
# Mirrors .planning/spikes/005-lowend-cpu-headroom/bench-cputime.rs's
# CLOCK_PROCESS_CPUTIME_ID methodology (per-process CPU time, sums all
# threads), extended to report per-hop median/p99/max/deadline-miss
# statistics rather than a single aggregate percentage -- what the
# low_end_processor_measurement / microphone_path_measurement schema defs
# (15.1-EVIDENCE.schema.json) actually require.
write_bench_source() {
  local out_path="$1"
  cat > "$out_path" <<'RUST'
// Auto-generated throwaway bench for scripts/run-dpdfnet-default-evidence.sh.
// Canonical CPU-time methodology copied from
// .planning/spikes/005-lowend-cpu-headroom/bench-cputime.rs (CLOCK_PROCESS_CPUTIME_ID,
// not wall-clock -- see that spike's DeepFilterNet correction for why).
use cleanmic::engine::{create_engine, EngineType, ProcessingMode};

const FRAME: usize = 480;
const HOP_BUDGET_MS: f64 = 10.0; // 480 samples @ 48 kHz real-time budget
const WARMUP_HOPS: usize = 500; // ~5 s
const MEASURE_HOPS: usize = 12000; // 120 s

fn cpu_ms() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    (ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9) * 1000.0
}

fn fill(buf: &mut [f32], phase: &mut f32, seed: &mut u32) {
    for s in buf.iter_mut() {
        *phase += 2.0 * std::f32::consts::PI * 200.0 / 48_000.0;
        let tone = 0.2 * phase.sin();
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        *s = tone + ((*seed as f32) / (u32::MAX as f32) - 0.5) * 0.3;
    }
}

fn main() {
    let variant = std::env::args().nth(1).unwrap_or_else(|| "dpdfnet2".to_string());
    let et = match variant.as_str() {
        "dpdfnet2" => EngineType::Dpdfnet2,
        "dpdfnet8" => EngineType::Dpdfnet8,
        "khip" => EngineType::Khip,
        other => {
            eprintln!("unknown variant: {other}");
            std::process::exit(2);
        }
    };

    let mut engine = match create_engine(et) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("engine unavailable: {err:#}");
            std::process::exit(3);
        }
    };
    engine.set_strength(0.5);
    engine.set_mode(ProcessingMode::Balanced);

    let mut input = vec![0f32; FRAME];
    let mut output = vec![0f32; FRAME];
    let mut phase = 0f32;
    let mut seed = 0x1234_5678u32;

    for _ in 0..WARMUP_HOPS {
        fill(&mut input, &mut phase, &mut seed);
        engine.process(&input, &mut output);
    }

    let mut hop_ms = Vec::with_capacity(MEASURE_HOPS);
    for _ in 0..MEASURE_HOPS {
        fill(&mut input, &mut phase, &mut seed);
        let c0 = cpu_ms();
        engine.process(&input, &mut output);
        hop_ms.push(cpu_ms() - c0);
    }

    hop_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = hop_ms.len();
    let median = hop_ms[n / 2];
    let p99_idx = ((n * 99) / 100).min(n - 1);
    let p99 = hop_ms[p99_idx];
    let max = hop_ms[n - 1];
    let deadline_misses = hop_ms.iter().filter(|&&v| v > HOP_BUDGET_MS).count();

    println!("median_ms={median:.4}");
    println!("p99_ms={p99:.4}");
    println!("max_ms={max:.4}");
    println!("deadline_misses={deadline_misses}");
    println!("hops={n}");
}
RUST
}

# ── Step 6: build + run the bench at the chosen CPUQuota (Spike 005) ────
# Reuses the Spike 005 systemd-run --user --scope -p CPUQuota=N% method
# verbatim, and pins single-threaded execution exactly as that spike does.
run_bench_at_quota() {
  local variant="$1"
  local quota_percent="$2"

  require_cmd cargo
  require_cmd systemd-run

  BENCH_EXAMPLE_PATH="$PROJECT_ROOT/examples/dpdfnet_default_evidence_bench.rs"
  mkdir -p "$PROJECT_ROOT/examples"
  write_bench_source "$BENCH_EXAMPLE_PATH"

  (
    cd "$PROJECT_ROOT"
    cargo build --release --example dpdfnet_default_evidence_bench --features "dpdfnet gui pipewire tray rnnoise deepfilter" >/dev/null
  )

  local bin="$PROJECT_ROOT/target/release/examples/dpdfnet_default_evidence_bench"
  [ -x "$bin" ] || abort "Bench binary did not build: $bin"

  local env_prefix=(env "APPDIR=$APPDIR" OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 FFTW_NUM_THREADS=1)
  local output
  if [ "$quota_percent" -ge 100 ]; then
    output="$("${env_prefix[@]}" "$bin" "$variant")"
  else
    output="$(systemd-run --user --scope -q -p "CPUQuota=${quota_percent}%" -- "${env_prefix[@]}" "$bin" "$variant")"
  fi
  printf '%s\n' "$output"
}

parse_bench_field() {
  local output="$1"
  local field="$2"
  printf '%s\n' "$output" | sed -n "s/^${field}=//p"
}

# ── Step 7: three required, non-substitutable microphone paths ──────────
# Aborts on any missing device_id/sample_rate/clip -- the second required
# guard branch (a missing microphone class can never silently pass).
declare -A MIC_DEVICE_ID
declare -A MIC_SAMPLE_RATE
declare -A MIC_BEFORE_CLIP
declare -A MIC_AFTER_CLIP

require_mic_path() {
  local class="$1"
  local out_dir="$2"
  local device_id sample_rate before_clip after_clip

  read -r -p "[$class] PipeWire device_id (Node name/serial) for the $class microphone: " device_id
  [ -n "$device_id" ] || abort \
    "Microphone path '$class' has no device_id -- every path (${MIC_CLASSES[*]}) is" \
    "required; none may be skipped or substituted for another (D-06)."

  read -r -p "[$class] Sample rate in Hz (e.g. 48000): " sample_rate
  case "$sample_rate" in
    ''|*[!0-9]*) abort "Microphone path '$class' has an invalid/blank sample_rate_hz: '$sample_rate'" ;;
  esac

  require_cmd pw-record
  mkdir -p "$out_dir"
  before_clip="$out_dir/${class}-before.wav"
  after_clip="$out_dir/${class}-after.wav"

  info "[$class] Recording ${CLIP_SECONDS}s BEFORE clip (raw mic, direct capture) from '$device_id' -> $before_clip"
  pw-record --target="$device_id" --rate="$sample_rate" --channels=1 "$before_clip" &
  local rec_pid=$!
  sleep "$CLIP_SECONDS"
  kill -INT "$rec_pid" 2>/dev/null || true
  wait "$rec_pid" 2>/dev/null || true
  [ -s "$before_clip" ] || abort "Microphone path '$class': before-clip was not recorded (empty/missing $before_clip)"

  info "[$class] Recording ${CLIP_SECONDS}s AFTER clip -- speak into the $class mic while CleanMic" \
       "(DPDFNet-2) processes it live; capturing the CleanMic virtual source -> $after_clip"
  pw-record --target=CleanMic --rate="$sample_rate" --channels=1 "$after_clip" &
  rec_pid=$!
  sleep "$CLIP_SECONDS"
  kill -INT "$rec_pid" 2>/dev/null || true
  wait "$rec_pid" 2>/dev/null || true
  [ -s "$after_clip" ] || abort "Microphone path '$class': after-clip was not recorded (empty/missing $after_clip)"

  MIC_DEVICE_ID["$class"]="$device_id"
  MIC_SAMPLE_RATE["$class"]="$sample_rate"
  MIC_BEFORE_CLIP["$class"]="$before_clip"
  MIC_AFTER_CLIP["$class"]="$after_clip"
}

# ── Step 8: write measurements into 15.2-DEFAULT-EVIDENCE.json ──────────
# Updates ONLY low_end_processor and microphone_paths in place; owner and
# default_eligible are left exactly as seeded, for the owner to fill by
# hand in the 15.2-03 checkpoint -- this script never invents a decision.
write_default_evidence() {
  export DEE_FILE="$DEFAULT_EVIDENCE_JSON"
  export DEE_HOST="$ENV_HOST"
  export DEE_GOVERNOR="$ENV_GOVERNOR"
  export DEE_MEDIAN_MS="$LOW_END_MEDIAN_MS"
  export DEE_P99_MS="$LOW_END_P99_MS"
  export DEE_MAX_MS="$LOW_END_MAX_MS"
  export DEE_DEADLINE_MISSES="$LOW_END_DEADLINE_MISSES"

  local class
  for class in "${MIC_CLASSES[@]}"; do
    local upper
    upper="$(printf '%s' "$class" | tr '[:lower:]' '[:upper:]')"
    export "DEE_MIC_${upper}_DEVICE_ID=${MIC_DEVICE_ID[$class]}"
    export "DEE_MIC_${upper}_SAMPLE_RATE=${MIC_SAMPLE_RATE[$class]}"
    export "DEE_MIC_${upper}_BEFORE_CLIP=${MIC_BEFORE_CLIP[$class]}"
    export "DEE_MIC_${upper}_AFTER_CLIP=${MIC_AFTER_CLIP[$class]}"
    # Per-hop timing is the engine's own CPU-time cost under the fixed 10 ms
    # hop cadence -- it does not vary by which physical mic feeds the
    # pipeline, so the same low-end-simulation figures are recorded for
    # every path. Any genuine per-device jitter is out of scope for this
    # simulation and is part of the real-low-end-hardware deferred follow-up.
    export "DEE_MIC_${upper}_MEDIAN_MS=${LOW_END_MEDIAN_MS}"
    export "DEE_MIC_${upper}_P99_MS=${LOW_END_P99_MS}"
    export "DEE_MIC_${upper}_MAX_MS=${LOW_END_MAX_MS}"
    export "DEE_MIC_${upper}_DEADLINE_MISSES=${LOW_END_DEADLINE_MISSES}"
  done

  python3 <<'PY'
import json
import os

path = os.environ["DEE_FILE"]
with open(path, "r", encoding="utf-8") as fh:
    data = json.load(fh)

data["low_end_processor"] = {
    "host": os.environ["DEE_HOST"],
    "governor": os.environ["DEE_GOVERNOR"],
    "median_ms": float(os.environ["DEE_MEDIAN_MS"]),
    "p99_ms": float(os.environ["DEE_P99_MS"]),
    "max_ms": float(os.environ["DEE_MAX_MS"]),
    "deadline_misses": int(os.environ["DEE_DEADLINE_MISSES"]),
}

mic_paths = {}
for cls in ("webcam", "laptop", "headset"):
    prefix = f"DEE_MIC_{cls.upper()}_"
    mic_paths[cls] = {
        "device_id": os.environ[prefix + "DEVICE_ID"],
        "sample_rate_hz": float(os.environ[prefix + "SAMPLE_RATE"]),
        "median_ms": float(os.environ[prefix + "MEDIAN_MS"]),
        "p99_ms": float(os.environ[prefix + "P99_MS"]),
        "max_ms": float(os.environ[prefix + "MAX_MS"]),
        "deadline_misses": int(os.environ[prefix + "DEADLINE_MISSES"]),
        "clip_before_path": os.environ[prefix + "BEFORE_CLIP"],
        "clip_after_path": os.environ[prefix + "AFTER_CLIP"],
    }
data["microphone_paths"] = mic_paths

with open(path, "w", encoding="utf-8") as fh:
    json.dump(data, fh, indent=2)
    fh.write("\n")
PY

  info "Wrote low_end_processor + microphone_paths into $DEFAULT_EVIDENCE_JSON"
  warn "owner and default_eligible were left untouched -- fill those by hand" \
       "(with a real owner name and a considered true/false) in the 15.2-03" \
       "checkpoint. This script never sets default_eligible."
}

# ── Main ──────────────────────────────────────────────────────────────────
main() {
  print_simulated_banner
  verify_appimage_hash

  local squashfs_root
  squashfs_root="$(extract_appimage_assets)"
  export APPDIR="$squashfs_root"

  capture_environment

  if [ "$WITH_KHIP_COMPARISON" -eq 1 ]; then
    log_khip_library_status
  fi

  info "Running the ${MEASURE_HOPS}-hop (120 s) single-thread CPU-time low-end" \
       "simulation for dpdfnet2 at CPUQuota=${CPU_QUOTA_PERCENT}% ..."
  local bench_output
  bench_output="$(run_bench_at_quota "dpdfnet2" "$CPU_QUOTA_PERCENT")"
  LOW_END_MEDIAN_MS="$(parse_bench_field "$bench_output" median_ms)"
  LOW_END_P99_MS="$(parse_bench_field "$bench_output" p99_ms)"
  LOW_END_MAX_MS="$(parse_bench_field "$bench_output" max_ms)"
  LOW_END_DEADLINE_MISSES="$(parse_bench_field "$bench_output" deadline_misses)"
  [ -n "$LOW_END_MEDIAN_MS" ] && [ -n "$LOW_END_P99_MS" ] && [ -n "$LOW_END_MAX_MS" ] \
    && [ -n "$LOW_END_DEADLINE_MISSES" ] \
    || abort "Bench produced incomplete timing output -- missing median/p99/max/deadline_misses. Never invent these numbers; re-run."

  info "  median_ms=$LOW_END_MEDIAN_MS  p99_ms=$LOW_END_P99_MS  max_ms=$LOW_END_MAX_MS  deadline_misses=$LOW_END_DEADLINE_MISSES"
  warn "Underflow counts are NOT part of the default_evidence schema" \
       "(15.1-EVIDENCE.schema.json has no such field -- see RESEARCH Pitfall 5)." \
       "If the live audio thread logs underflows during this run" \
       "(RUST_LOG=debug, 'ring buffer underflow'), record them alongside this" \
       "report by hand; do not invent a new schema field for them."

  info "Now capturing the three required microphone paths (${MIC_CLASSES[*]})."
  local out_dir="$PHASE_DIR/evidence-runs/$(date -u +%Y%m%dT%H%M%SZ)"
  local class
  for class in "${MIC_CLASSES[@]}"; do
    require_mic_path "$class" "$out_dir"
  done

  write_default_evidence
  print_simulated_banner
  info "Default-evidence run complete. Review $DEFAULT_EVIDENCE_JSON, then" \
       "have the owner fill 'owner' and 'default_eligible' by hand for the" \
       "15.2-03 checkpoint."
}

main "$@"

#!/usr/bin/env bash
# test-nested-run.sh -- sandboxed self-test of scripts/nested-run.sh.
#
# Self-contained: PATH-shims a fake `xrandr` for the screen/scale rule, and
# copies the real `sleep` binary to a fake `cleanmic` for the owner-process
# guard -- so the screen rule, the tiny-scale guard, the state-root guard,
# the bad-config guard, and the owner-process refusal/survival are ALL
# exercised WITHOUT a real X server, without real PipeWire, and without ever
# touching (or being confused for) a real running CleanMic.
#
# CLEANMIC_HARNESS_STATE=<sandbox>/state, display :99. Never starts a real
# Xephyr or plays audio (every case uses `--dry-run`, or fails validation
# before nested-run.sh would ever touch X or PipeWire).
#
# If a REAL cleanmic (not this test's own fake) happens to be running, the
# cases that need an otherwise-empty field (3, 4, 5) print SKIP with the
# reason and never signal that process.
#
# USAGE
#   bash scripts/test-nested-run.sh
#
# Exits 0 if every case PASSes (SKIPs are not failures), non-zero otherwise.

set -euo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")"
NESTED_RUN="$SCRIPT_DIR/nested-run.sh"

SANDBOX="$(mktemp -d)"
FAKE_CLEANMIC_PID=""

cleanup() {
  set +e
  if [ -n "$FAKE_CLEANMIC_PID" ] && [ "$(cat "/proc/$FAKE_CLEANMIC_PID/comm" 2>/dev/null || true)" = "cleanmic" ]; then
    kill "$FAKE_CLEANMIC_PID" 2>/dev/null
  fi
  rm -rf "$SANDBOX"
}
trap cleanup EXIT

FAIL_COUNT=0
pass() { printf 'PASS: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1"; FAIL_COUNT=$((FAIL_COUNT + 1)); }
skip() { printf 'SKIP: %s (%s)\n' "$1" "$2"; }

STATE="$SANDBOX/state"
DISP=":99"

run_nested_sandboxed() {
  CLEANMIC_HARNESS_STATE="$STATE" bash "$NESTED_RUN" "$@"
}

# A real, unmarked cleanmic (the owner's, or a leftover from another
# session) means cases 3-5 cannot safely run: signalling anything named
# cleanmic that ISN'T our own fake is exactly the bug this harness exists to
# prevent, so those cases SKIP rather than risk it.
REAL_CLEANMIC=""
for pid in $(pgrep -x cleanmic 2>/dev/null || true); do
  REAL_CLEANMIC="$pid"
  break
done

# ---------------------------------------------------------------------------
# Case 1: SCREEN RULE (R1), via a fake `xrandr` PATH shim + `xephyr --dry-run`
# ---------------------------------------------------------------------------

FAKE_BIN="$SANDBOX/bin"
mkdir -p "$FAKE_BIN"
cat >"$FAKE_BIN/xrandr" <<'XRANDR_SHIM'
#!/usr/bin/env bash
case "${FAKE_XRANDR_FIXTURE:-}" in
  laptop_only)
    cat <<'EOF'
Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 8192 x 8192
eDP-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 310mm x 170mm
   1920x1080     60.00*+
EOF
    ;;
  dual_dp2_bigger)
    cat <<'EOF'
Screen 0: minimum 320 x 200, current 9952 x 2880, maximum 16384 x 16384
eDP-1 connected 3072x1728+0+0 (normal left inverted right x axis y axis) 340mm x 190mm
   3072x1728     60.00*+
DP-2 connected 6880x2880+3072+0 (normal left inverted right x axis y axis) 700mm x 300mm
   6880x2880     60.00*+
EOF
    ;;
  external_smaller_than_default_logical)
    cat <<'EOF'
Screen 0: minimum 320 x 200, current 4480 x 1440, maximum 16384 x 16384
eDP-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 310mm x 170mm
   1920x1080     60.00*+
DP-1 connected 2560x1440+1920+0 (normal left inverted right x axis y axis) 550mm x 310mm
   2560x1440     60.00*+
EOF
    ;;
  external_no_geometry)
    cat <<'EOF'
Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 8192 x 8192
eDP-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 310mm x 170mm
   1920x1080     60.00*+
DP-2 connected (normal left inverted right x axis y axis)
EOF
    ;;
  none_connected)
    cat <<'EOF'
Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 8192 x 8192
eDP-1 disconnected (normal left inverted right x axis y axis)
DP-2 disconnected (normal left inverted right x axis y axis)
EOF
    ;;
  *)
    echo "test-nested-run: FAKE_XRANDR_FIXTURE not set" >&2
    exit 1
    ;;
esac
XRANDR_SHIM
chmod +x "$FAKE_BIN/xrandr"

screen_case() {
  local fixture="$1" expect_scale="$2" expect_geom="$3" label="$4" extra_env="${5:-}"
  local out rc
  out="$(env FAKE_XRANDR_FIXTURE="$fixture" PATH="$FAKE_BIN:$PATH" $extra_env \
    CLEANMIC_HARNESS_STATE="$SANDBOX/state-screen-$fixture" \
    bash "$NESTED_RUN" xephyr "$DISP" --dry-run 2>&1)" || rc=$?
  rc="${rc:-0}"
  if [ "$rc" != 0 ]; then
    fail "$label: expected exit 0, got $rc ($out)"
    return
  fi
  if echo "$out" | grep -q "scale ${expect_scale} | physical ${expect_geom}"; then
    pass "$label"
  else
    fail "$label: unexpected output: $out"
  fi
}

screen_case laptop_only 1 "520x960" "screen rule: laptop-only -> scale 1, 520x960"
screen_case dual_dp2_bigger 2 "1040x2600" "screen rule: external (DP-2) bigger -> scale 2 on DP-2, 1040x2600"
screen_case external_smaller_than_default_logical 2 "1040x1320" "screen rule: 2560x1440 external -> scale 2, clamped to 1040x1320"

out="$(env FAKE_XRANDR_FIXTURE=external_no_geometry PATH="$FAKE_BIN:$PATH" \
  CLEANMIC_HARNESS_STATE="$SANDBOX/state-screen-nogeom" \
  bash "$NESTED_RUN" xephyr "$DISP" --dry-run 2>&1)" || true
if echo "$out" | grep -q "(laptop)" && echo "$out" | grep -q "scale 1"; then
  pass "screen rule: external listed without geometry is ignored -> laptop rule"
else
  fail "screen rule: external-without-geometry case: unexpected output: $out"
fi

rc=0
out="$(env FAKE_XRANDR_FIXTURE=none_connected PATH="$FAKE_BIN:$PATH" \
  CLEANMIC_HARNESS_STATE="$SANDBOX/state-screen-none" \
  bash "$NESTED_RUN" xephyr "$DISP" --dry-run 2>&1)" || rc=$?
if [ "$rc" = 3 ]; then
  pass "screen rule: no connected output -> exit 3"
else
  fail "screen rule: no connected output: expected exit 3, got $rc ($out)"
fi

# ---------------------------------------------------------------------------
# Case 2: TINY-SCALE GUARD
# ---------------------------------------------------------------------------

out="$(env FAKE_XRANDR_FIXTURE=dual_dp2_bigger PATH="$FAKE_BIN:$PATH" CLEANMIC_GDK_SCALE=1 \
  CLEANMIC_HARNESS_STATE="$SANDBOX/state-tiny1" \
  bash "$NESTED_RUN" xephyr "$DISP" --dry-run 2>&1)" || true
if echo "$out" | grep -q "IGNORING CLEANMIC_GDK_SCALE=1" && echo "$out" | grep -q "scale 2"; then
  pass "tiny guard: CLEANMIC_GDK_SCALE=1 on an external screen is ignored -> scale 2"
else
  fail "tiny guard: override-ignored case: unexpected output: $out"
fi

out="$(env FAKE_XRANDR_FIXTURE=dual_dp2_bigger PATH="$FAKE_BIN:$PATH" CLEANMIC_GDK_SCALE=1 CLEANMIC_ALLOW_TINY=1 \
  CLEANMIC_HARNESS_STATE="$SANDBOX/state-tiny2" \
  bash "$NESTED_RUN" xephyr "$DISP" --dry-run 2>&1)" || true
if echo "$out" | grep -q "scale 1 | physical 520x1300"; then
  pass "tiny guard: CLEANMIC_ALLOW_TINY=1 honors the smaller override -> scale 1"
else
  fail "tiny guard: allow-tiny case: unexpected output: $out"
fi

out="$(env FAKE_XRANDR_FIXTURE=laptop_only PATH="$FAKE_BIN:$PATH" CLEANMIC_GDK_SCALE=2 \
  CLEANMIC_HARNESS_STATE="$SANDBOX/state-tiny3" \
  bash "$NESTED_RUN" xephyr "$DISP" --dry-run 2>&1)" || true
if echo "$out" | grep -q "scale 2" && ! echo "$out" | grep -q "IGNORING"; then
  pass "tiny guard: a LARGER override on the laptop screen is always allowed -> scale 2"
else
  fail "tiny guard: larger-override case: unexpected output: $out"
fi

# ---------------------------------------------------------------------------
# Case 3: OWNER PROTECTION (T-bjk-03) -- a fake owner cleanmic must survive
# launch-refusal, stop, and reap.
# ---------------------------------------------------------------------------

if [ -n "$REAL_CLEANMIC" ]; then
  skip "owner protection: launch refuses on a foreign cleanmic" "a real cleanmic (pid $REAL_CLEANMIC) is running"
  skip "owner protection: stop leaves the foreign cleanmic alive" "a real cleanmic (pid $REAL_CLEANMIC) is running"
  skip "owner protection: reap leaves the foreign cleanmic alive" "a real cleanmic (pid $REAL_CLEANMIC) is running"
else
  # A copied `sleep` binary won't do: on this system /usr/bin/sleep is a
  # symlink into a multi-call `coreutils` binary that dispatches on argv[0],
  # so a copy renamed "cleanmic" fails with "unknown program 'cleanmic'". A
  # trivial script named "cleanmic" gets /proc/<pid>/comm = "cleanmic" from
  # the kernel's own shebang handling instead (verified empirically).
  cat >"$SANDBOX/cleanmic" <<'FAKE_CLEANMIC'
#!/bin/bash
sleep "${1:-300}" &
wait $!
FAKE_CLEANMIC
  chmod +x "$SANDBOX/cleanmic"
  "$SANDBOX/cleanmic" 300 &
  FAKE_CLEANMIC_PID=$!
  disown 2>/dev/null || true
  sleep 0.2

  if [ "$(cat "/proc/$FAKE_CLEANMIC_PID/comm" 2>/dev/null || true)" != "cleanmic" ]; then
    fail "owner protection: setup -- fake cleanmic's comm is not 'cleanmic'"
  else
    rc=0
    out="$(PATH="$SANDBOX:$PATH" run_nested_sandboxed launch "$DISP" --lang en 2>&1)" || rc=$?
    if [ "$rc" = 11 ] && echo "$out" | grep -qi 'lock'; then
      pass "owner protection: launch refuses on a foreign cleanmic (exit 11, mentions the lock)"
    else
      fail "owner protection: launch refusal: expected exit 11 mentioning the lock, got $rc ($out)"
    fi

    run_nested_sandboxed stop "$DISP" >/dev/null 2>&1 || true
    if [ "$(cat "/proc/$FAKE_CLEANMIC_PID/comm" 2>/dev/null || true)" = "cleanmic" ]; then
      pass "owner protection: stop leaves the foreign cleanmic alive"
    else
      fail "owner protection: stop ended the foreign cleanmic!"
    fi

    run_nested_sandboxed reap >/dev/null 2>&1 || true
    if [ "$(cat "/proc/$FAKE_CLEANMIC_PID/comm" 2>/dev/null || true)" = "cleanmic" ]; then
      pass "owner protection: reap leaves the foreign cleanmic alive"
    else
      fail "owner protection: reap ended the foreign cleanmic!"
    fi
  fi

  kill "$FAKE_CLEANMIC_PID" 2>/dev/null || true
  wait "$FAKE_CLEANMIC_PID" 2>/dev/null || true
  FAKE_CLEANMIC_PID=""
fi

# ---------------------------------------------------------------------------
# Case 4: STATE-ROOT GUARD (T-bjk-05)
# ---------------------------------------------------------------------------

if [ -n "$REAL_CLEANMIC" ]; then
  skip "state-root guard: an owner XDG dir is refused" "a real cleanmic (pid $REAL_CLEANMIC) is running"
else
  rc=0
  out="$(CLEANMIC_HARNESS_STATE="$HOME/.config/cleanmic-x" bash "$NESTED_RUN" launch "$DISP" --lang en 2>&1)" || rc=$?
  if [ "$rc" = 11 ]; then
    pass "state-root guard: CLEANMIC_HARNESS_STATE under \$HOME/.config is refused (exit 11)"
  else
    fail "state-root guard: expected exit 11, got $rc ($out)"
  fi
fi

# ---------------------------------------------------------------------------
# Case 5: BAD CONFIG (validation runs BEFORE the Xephyr-alive check, so none
# of these need a real Xephyr; no fake cleanmic is running by this point).
# ---------------------------------------------------------------------------

bad_config_case() {
  local label="$1" expect_rc="$2"; shift 2
  # A real (unmarked) cleanmic running anywhere trips nested-run.sh's
  # FOREIGN-CLEANMIC GUARD (its Step 1, ahead of config validation), which
  # would mask every one of these cases behind its own exit 11 -- exactly
  # the "needs an empty field" situation the plan groups with cases 3/4.
  if [ -n "$REAL_CLEANMIC" ]; then
    skip "$label" "a real cleanmic (pid $REAL_CLEANMIC) is running"
    return
  fi
  local rc=0 out
  local case_state="$SANDBOX/state-badcfg-$RANDOM"
  out="$(CLEANMIC_HARNESS_STATE="$case_state" bash "$NESTED_RUN" launch "$DISP" --lang en "$@" 2>&1)" || rc=$?
  if [ "$rc" != "$expect_rc" ]; then
    fail "$label: expected exit $expect_rc, got $rc ($out)"
    return
  fi
  if [ -e "$case_state/display-99/home" ]; then
    fail "$label: exit $rc was right, but display-99/home was created anyway"
    return
  fi
  pass "$label"
}

bad_config_case "bad config: unknown --config key" 2 --config 'no_such_key = 1'
bad_config_case "bad config: value is not a TOML literal" 2 --config 'engine = RNNoise'
bad_config_case "bad config: monitor_enabled=true without --monitor-sink" 11 --config 'monitor_enabled = true'
bad_config_case "bad config: --monitor-sink not prefixed cmtest_" 11 --monitor-sink alsa_output.foo
bad_config_case "bad config: --no-pipewire refuses --monitor-sink" 2 --no-pipewire --monitor-sink cmtest_x
bad_config_case "bad config: --no-pipewire refuses monitor_enabled=true" 2 --no-pipewire --config 'monitor_enabled = true'

# ---------------------------------------------------------------------------
# summary
# ---------------------------------------------------------------------------

if [ "$FAIL_COUNT" -gt 0 ]; then
  echo ""
  echo "$FAIL_COUNT case(s) FAILED"
  exit 1
fi
echo ""
echo "all cases PASSED (or SKIPped for a safety reason)"
exit 0

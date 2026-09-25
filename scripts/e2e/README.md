# CleanMic silent E2E audio harness

## Purpose

`scripts/e2e-audio.sh` (driving `scripts/nested-run.sh`) is the maintained,
repo-owned replacement for the ad-hoc rig the 2026-09-24 base-latency debug
session built by hand to measure CleanMic's mic-to-virtual-source latency.
It runs the real AppImage against a real PipeWire graph, in an isolated
nested X server, and grades the result against one thresholds block — all
**without sound**, **without touching the owner's `~/.config/cleanmic`**,
and **without ever signalling a CleanMic that isn't its own**.

Every future latency, engine-swap, decimation, DC-offset, or auto-gain fix
should be checkable with one command: `make e2e-audio`.

## Prerequisites

- Packages: `xserver-xephyr`, `xdotool`, `x11-utils`, `x11-xserver-utils`,
  `imagemagick`, `pipewire-bin`, `dbus-bin`. `scripts/nested-run.sh`'s `need`
  helper checks these and prints the missing package names.
- `python3` with `numpy` (`sudo apt install python3-numpy`). `pytest` is
  optional — the unit tests also run as plain `python3 scripts/e2e/test_*.py`.
- `shellcheck` is optional; `make harness-test` falls back to `bash -n` alone
  when it's absent.
- A built AppImage under `build/CleanMic-*.AppImage` (or pass `--appimage` /
  `--binary`).

## Quick start

```bash
make harness-test          # offline: syntax, sandboxed self-test, pytest
make e2e-audio              # real, silent: the `baseline` scenario
SCENARIOS=all make e2e-audio            # every scenario (monitor needs the flag below)
E2E_ARGS=--monitor-null-sink SCENARIOS=all make e2e-audio
```

Or drive the scripts directly:

```bash
bash scripts/e2e-audio.sh baseline
bash scripts/e2e-audio.sh --out /tmp/my-run swaps toggle modes
bash scripts/e2e-audio.sh --monitor-null-sink monitor
```

## Safety model

- **No `Audio/Sink` node by default.** The 2026-09-24 debug session found
  that gnome-remote-desktop attaches a `GRD::RDP::AUDIO_PLAYBACK` capture
  stream to **every** `Audio/Sink` node the instant it appears — so the
  first version of this harness (an Audio/Sink test node) was silently
  mirroring test speech to a connected RDP client. This harness instead
  feeds the virtual mic through a `Stream/Input/Audio` capture side
  (`node.autoconnect=false`) into an `Audio/Source` (`cmtest_mic`); the
  player and recorder are unlinked streams wired only by explicit
  `pw-link`. See the Deviation note below.
- **The link audit runs before test audio ever flows, and after every
  playback link.** `scripts/e2e/pwgraph.py audit` reads live `pw-dump` JSON
  and flags: any `cmtest_*` node claiming `Audio/Sink` without
  `--allow-sink`; any link touching a harness node that isn't one of the
  handful of pairs the harness itself creates. A violation aborts (exit 4)
  **before** the player is linked, or immediately kills it if a violation
  appears mid-run.
- **Monitor only with `--monitor-null-sink`.** `nested-run.sh launch`
  refuses `monitor_enabled=true` without a harness sink (exit 11) — it would
  otherwise play into the owner's real default sink. The optional
  `cmtest_null` `Audio/Sink` is created only for the `monitor` scenario,
  audited before use, and torn down afterward.
- **Private XDG homes.** Every launch gets its own
  `XDG_CONFIG_HOME`/`XDG_DATA_HOME`/`XDG_CACHE_HOME`/`XDG_STATE_HOME` under
  the harness state root. `launch` refuses (exit 11) if any of those would
  resolve to an owner directory.
- **The owner-file stamp guard.** `~/.config/cleanmic/config.toml`, the
  autostart entry, the applications entry, and the icon are `stat`-stamped
  (never opened) at launch and compared at `stop`. A real change is an ALERT
  and exit 10; a foreign CleanMic being active downgrades it to a NOTE (the
  change probably isn't this harness's).
- **Process ownership by session + marker, never by name.** Every harness
  CleanMic process carries `CLEANMIC_HARNESS_LAUNCH=nested-run:<n>` and
  `CLEANMIC_HARNESS_STATE_ROOT=<root>` on its `dbus-run-session` environment
  (inherited by its bus and every service it activates), plus a recorded
  setsid session whose start time is re-verified before every signal. `stop`
  and `reap` never touch a `cleanmic` that lacks both. See P3 in the
  original planning notes for why the portal/bus stack needs the marker
  sweep too (orphaned `xdg-desktop-portal*`/`gvfsd`/`at-spi2-registryd`
  processes otherwise exhaust `fs.inotify.max_user_instances`).
- **A refusal, never a kill, when any CleanMic is already running.** `launch`
  exits 11 on ANY `cleanmic` process (marked or not) that isn't this
  harness's own on this display; `e2e-audio.sh`'s preflight refuses (exit 4)
  if `pgrep -x cleanmic` finds anything at all, before any test audio plays.
  The runtime lock `$XDG_RUNTIME_DIR/cleanmic.lock` would block a second
  instance anyway; the harness deliberately never touches it.
- **Never run `make appimage` or `make kill` while a harness run is active**
  — both end `cleanmic` by process name.

## `nested-run.sh` subcommands

| Subcommand | Purpose |
| --- | --- |
| `xephyr [:N] [--dry-run]` | Detect the screen, start (or reuse) a correctly-scaled Xephyr |
| `launch [:N] [--lang fr\|en] [--config 'K = V']... [--appimage P \| --binary P] [--monitor-sink NAME]` | Start CleanMic in isolation |
| `app-pid [:N]` | Print the marked CleanMic's pid |
| `status [:N]` | Human-readable dump of recorded state |
| `shot [:N] FILE` | Screenshot the nested display |
| `click [:N] X Y [--physical]` | Click at a logical (or physical) coordinate |
| `key [:N] KEYSYM...` | Send key events |
| `scroll [:N] top\|bottom` | Scroll the preferences page |
| `targets [:N]` | List calibrated UI targets for the recorded language |
| `click-target [:N] NAME [--expect REGEX] [--timeout S]` | Click a named, calibrated target, confirmed by the app log |
| `check-layout [:N]` | Pixel-probe every stateful target against the live config |
| `stop [:N]` | Close this display's app and Xephyr; nothing else |
| `reap` | `stop` every recorded display |

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | ok |
| 2 | bad usage / bad `--config` |
| 3 | `xrandr` unreadable, or a tool/locale/binary is missing |
| 5 | Xephyr did not come up, or is not recorded for this display |
| 6 | the app never mapped its window |
| 10 | an owner file changed during the run |
| 11 | refused BEFORE anything started or was signalled |
| 12 | `stop`/`reap` left harness processes alive |
| 13 | layout drift (`check-layout`) |
| 14 | a UI action was not confirmed by the app log |
| 15 | a target is unreachable at this viewport, or has no calibration for this language |

## Screen and scale rule

`xephyr` re-reads `xrandr` on every start (never reuses a value from a prior
run — the owner docks and undocks). Any connected output whose name is not
`eDP*`/`LVDS*` means an external screen: scale 2, sized against the largest
such output. Otherwise the laptop panel is used at scale 1. The logical
window is 520×1300 (420 px wide + combo-popover headroom); the **size** is
clamped to the screen (`width-40`, `height-120`) — the **scale** never is.
`CLEANMIC_GDK_SCALE` can override the automatic scale, but a value SMALLER
than the rule is ignored (with a printed explanation) unless
`CLEANMIC_ALLOW_TINY=1` is also set — a leftover tiny override from another
run must never silently reappear. The chosen scale and geometry are recorded
per display; `launch`/`click`/`click-target`/`check-layout` read that record
instead of recomputing it.

## UI driving

**Approach:** calibrated logical targets (`scripts/nested-run-targets.tsv`)
with a dual anchor (top-of-page and/or bottom-of-page), keyboard navigation
for `AdwComboRow` items (click the row, `Home`, `Down` × index, `Return`),
app-log confirmation of every action (`--expect REGEX`, polled), and a
pixel-based `check-layout` that classifies ACCENT/NEUTRAL against the live
`config.toml`.

**Trade-off against the alternatives:**

- **AT-SPI** is layout-independent, but needs the accessibility bus on the
  (already orphan-prone, see the marker-sweep note above) private session,
  needs `pyatspi`, and GTK4-on-X11 AT-SPI inside Xephyr is fragile in
  practice.
- **Tab-order navigation** breaks silently the moment a row's sensitivity or
  position changes (e.g. Khip becoming available).
- **Calibrated coordinates** need recalibration after a layout change, but
  drift is detected LOUDLY (exit 13/14/15) instead of silently mis-clicking
  the wrong row.

### Recalibration procedure

After any layout change (new row, wrapped subtitle, reordered engines):

1. For each language: `nested-run.sh xephyr :47`, `launch :47 --lang LANG`,
   `scroll :47 top`, `shot :47 top.png`, `scroll :47 bottom`,
   `shot :47 bottom.png`.
2. Read the PNGs (the `Read` tool, or PIL) to find each row's vertical
   center: scan a fixed x for the accent/neutral color transition.
   - Switches: probe directly at the click point (dx=0, dy=0).
   - Radios: probe `dy=-6` from the click point — a SELECTED radio renders
     as a ring with a hollow WHITE center, so a dx=0,dy=0 probe on a
     selected item false-reads NEUTRAL.
3. `top_y` = the row's y when scrolled to top (or `-` if not visible there).
   `bottom_off` = `(window bottom edge y) - (row's y when scrolled to
   bottom)` (or `-` if not visible there). Note that rows ABOVE a
   variable-height row (e.g. DPDFNet-8's wrapped subtitle) keep a CONSTANT
   `top_y` across languages but a language-dependent `bottom_off`; rows
   BELOW it are the reverse.
4. Update `scripts/nested-run-targets.tsv`, then re-run `check-layout` in
   both languages — it must exit 0 (OK/UNREACHABLE only, never DRIFT).
5. Sanity-check scaling: `CLEANMIC_GDK_SCALE=2 nested-run.sh xephyr :47`
   then `check-layout` — still OK/UNREACHABLE only, never DRIFT, even though
   the smaller viewport makes more targets UNREACHABLE.

## `e2e-audio.sh`: scenarios, signals, metrics

**Signals** (`scripts/e2e/gen_signals.py`, from `assets/demo/*-before.wav`):
`speech.wav` (native level), `speech_m40.wav` (-40 dBFS, tiled), `speech_dc.wav`
(+0.1 DC), `silence_dc.wav` (10 s of 0.1), `pink_m45.wav` (-45 dBFS pink
noise), `speech_loop60.wav` (tiled ×3), `speech_loop150.wav` (tiled to
>= 150 s — quick 260924-n4s: covers the swaps scenario's full 15-swap +
3-mode sequence, which runs to ~127 s; `speech_loop60` used to cut it off
after only 8 of 15 swaps).

**Scenarios:**

| Scenario | What it does |
| --- | --- |
| `baseline` | An attempts loop (up to `$E2E_MAX_ATTEMPTS`) per `$BASELINE_ENGINES` entry (default: Dpdfnet2, Dpdfnet8, DeepFilterNet, RNNoise): fresh launch, an unrecorded `$BASELINE_PREROLL_S`-second pre-roll, then a recording. An engine this build doesn't have is SKIP, not FAIL. See "Load-aware verdict" below. |
| `swaps` | Pre/during(15 live engine swaps + 3 mode changes, driven concurrently with a `speech_loop150.wav` playback)/post recordings, the logged swap-sequence check, per-swap zero-run attribution (leading/swap/dfn_underrun/dfn_shed/unattributed), and the pre-vs-post latency drift check |
| `toggle` | Pre, Activer/Enable off (5.8 s) then on, `check-layout`, post; drift check; "≥2 Discarded lines after restart" check |
| `modes` | LowCpu, Balanced, LowCpu, Balanced; each mode's own latency allowance, plus a same-mode-repeat drift check |
| `dc` | Speech+DC and silence+DC; both must show the DC blocker removing the offset |
| `autogain` | ON boosts a quiet (-40 dBFS) mic; OFF is near-unity; ON vs OFF must not audibly pump steady pink noise |
| `stress` | Per `$STRESS_ENGINES` entry (default DeepFilterNet): fresh launch, settle, then **controlled synthetic CPU load** — every thread of the harness's own CleanMic pinned (`taskset -a`) to one CPU next to `$STRESS_SPINNERS` busy loops — while recording; unload; record again. Asserts the app survives, the vendored DeepFilterNet plugin never reaches its "Processing too slow!" abort, the mic never goes dead (`dead_run_ms`), a runtime fallback (if any) lands in `[$STRESS_RECOVERY_MIN_S, $STRESS_RECOVERY_MAX_S]` (D-01: too EARLY is now a FAIL too — the 5 s grace was not honoured), and the post-load recording passes the normal speech rules for whichever engine is then active. Spinners are killed and the app's affinity restored on every exit path. |
| `spike` | D-01's counterpart to `stress`: per `$SPIKE_ENGINES` entry (default DeepFilterNet, Dpdfnet8), a short `$SPIKE_BURST_S` (1.3 s) CPU burst — `$SPIKE_SPINNERS` (12) busy loops pinned with the app, each self-terminating via `timeout` even if the harness dies — must NEVER trigger a runtime fallback. Asserts zero `Engine fallback:` lines, the app survives, no plugin abort, the mic never goes dead, and the engine selector still shows the launched engine at the end. |
| `monitor` (needs `--monitor-null-sink`) | `CleanMic-monitor` routed ONLY to a second, audited, RDP-safe null-sink loopback |
| `all` | `baseline swaps toggle modes dc autogain stress spike`, plus `monitor` when `--monitor-null-sink` is given |

**Metrics** (`scripts/e2e/analyze.py`): `latency_ms`/`lag_corr` (band-passed
log-envelope cross-correlation, ±5 ms accuracy), `latency_spread_ms` (drift
across 5 s windows), `exact_repeat_frac` (the decimated-mode "held frame"
bug signature), `holes` (brief silent gaps inside loud audio), `zero_run_ms`,
`out_dc`/`in_dc`, `settled_gain_db` (auto-gain), `dead_run_ms` (longest
stretch of digital silence, < -100 dBFS out, while the mic carried speech,
> -35 dBFS in — speech pauses skipped; the pre-fix DeepFilterNet crash scored
13150 ms), and a log scan for fell-behind/error/panic/Discarded counts, the
ordered engine/mode-change sequence, runtime `Engine fallback:` lines,
DeepFilterNet guard restarts, and the vendored plugin's abort message.

**Load flag:** every recording samples `/proc/stat` and `/proc/loadavg`
before and after; its report gets an INFO `cpu_busy_pct` row (all-core busy
share, steal, 1-min load average), noted "ran under load" above
`LOAD_FLAG_BUSY_PCT`, and the Environment table gets `max_cpu_busy_pct` /
`ran_under_load`. A recording that ran next to someone else's build is
therefore visible as such instead of looking like a regression.

**App death:** `record_pair` re-checks the harness's own app pid after every
recording; a dead app adds an `app_alive = DIED` FAIL row and aborts with
exit 5 (it used to surface only as "latency unmeasurable").

**Thresholds:** one block at the top of `scripts/e2e-audio.sh`, every value
env-overridable, each with a one-line rationale citing the measurement it
comes from. `LATENCY_MAX_MS_*` per-engine ceilings and `MONITOR_LATENCY_MAX_MS`
were derived from real post-fix measurements
(`.planning/debug/resolved/base-latency-330ms.md`) plus margin — **never**
loosen a threshold just to make a run pass; a genuine regression should FAIL.

**Output:** `target/e2e-audio/<UTC timestamp>/` (or `--out DIR`), containing
`signals/`, `rec/*.wav` + `*.json`, `logs/*.log` + `*.logscan.json`,
`shots/*.png`, and `report.md`.

**Report format:** an Environment table (binary, sha256, git HEAD/dirty,
display, scale, lang, RDP-session-detected), a Thresholds table, one
`## Scenario: <name>` section per scenario with a
`| Recording | Metric | Value | Threshold | Result |` table
(PASS/FAIL/SKIP/INFO), an `## Aborted` section when the run didn't finish
normally, a `**Result: PASS**` / `**Result: FAIL (n failed)**` summary, a
Caveats section, and finally a `Cleanup: complete` or `Cleanup: INCOMPLETE`
line appended by the trap handler on every exit path.

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | every metric PASSed |
| 1 | at least one metric FAILed (the report was still written) |
| 2 | bad usage |
| 3 | missing prerequisite: a tool, numpy, a demo asset, or the binary |
| 4 | unsafe or busy environment — refused BEFORE any test audio played |
| 5 | harness/driver error: `nested-run.sh` failed, an action was unconfirmed, a recording came back empty, or the app died |
| 6 | cleanup was left incomplete — overrides every other code |
| 7 | INCONCLUSIVE — no FAIL, but the machine never got quiet (pre-flight), or a scheduling-sensitive metric only failed under contention/host-starvation, never on a quiet attempt. Precedence: 6 > abort codes (2-5, 130, 143) > 1 > 7 > 0 |

## Load-aware verdict (quick 260924-n4s, R5)

The harness runs next to the owner's real desktop, other agents' builds, and
background services (Syncthing, a browser) — a FAIL recorded while the HOST
was starved is not evidence of a CleanMic defect. `scripts/e2e/contention.py`
(stdlib-only /proc parsers, no new dependency) makes that visible instead of
silent:

- **Contention sampling.** A background sampler (`contention.py run`) runs
  alongside every recording: fast (0.1 s) per-thread `schedstat` samples of
  the harness's own app process, plus slower (1 s) whole-`/proc` scans
  classifying every pid as `harness` (descendant of the harness process, its
  recorded setsid session, the recorded Xephyr, or the exact
  `CLEANMIC_HARNESS_STATE_ROOT` marker), `audio_daemon` (pipewire /
  wireplumber / pipewire-pulse), or `other`. `contention.py summarize` turns
  the samples plus a `[REC_LINK_EPOCH, end]` window into `other_busy_pct`
  (mean/p95), `steal_pct`, `iowait_pct`, `loadavg_1m_max`, the
  `cleanmic-audio` thread's own CPU share and max scheduling wait, and a
  `contended` verdict with reasons (`other_busy_pct > 20`, `steal_pct > 2`,
  `iowait_pct > 10`, or `app_wait_ms_max >= 5`). Every recording's report
  section gets an INFO `contention` row.
- **Pre-flight quiet gate.** After the RDP-safe graph comes up (before any
  test audio plays), `contention.py wait-quiet` blocks until
  `QUIET_WINDOWS` (3) consecutive `QUIET_WINDOW_S` (2 s) windows sit at or
  below `QUIET_OTHER_BUSY_PCT` (15%), or `QUIET_WAIT_MAX_S` (180 s) elapses.
  A never-quiet machine ends the run as INCONCLUSIVE (exit 7) with a
  `preflight_quiet` check row — no scenario runs, nothing is signalled.
  Each scenario/attempt launch also does a shorter, best-effort
  `QUIET_WAIT_ATTEMPT_S` (60 s) wait that never aborts on timeout — it just
  means that attempt's own contention sampling may mark it `contended`.
- **Majority-of-attempts retries.** `holes`, `latency_spread_ms`,
  `fell_behind`, and `latency_ms` (DeepFilterNet only — its vendored plugin
  adds +10 ms per underrun, a scheduling effect) are scheduling-sensitive.
  The `baseline` scenario runs each engine as an attempts loop (up to
  `E2E_MAX_ATTEMPTS`, default 3, each a fresh launch with an unrecorded
  `BASELINE_PREROLL_S`-second pre-roll before the measured recording):
  deterministic metrics never retry (any FAIL anywhere is final); a
  scheduling-sensitive metric that never FAILs resolves PASS on attempt 1;
  once it FAILs, a FAIL taken while `contended` (or, for `holes`, a FAIL
  whose failing holes are ALL `host_starved`) is excluded from the "quiet"
  tally — evidence, not a vote. The metric resolves the moment either side
  of the quiet tally reaches a majority of `E2E_MAX_ATTEMPTS`, or, once
  attempts are exhausted, by comparing the quiet tally (a tie — including
  0-0, i.e. every failure was excluded — is INCONCLUSIVE, never FAIL). See
  `analyze.py`'s `decide_attempts()`.
- **Hole attribution.** Every `holes` frame is mapped back to a wall-clock
  epoch (`REC_LINK_EPOCH + source_onset_s + (t_wav - input_onset_wav)`) and
  classified `host_starved` (some app thread waited >= `AUDIO_WAIT_STARVED_MS`
  inside `[t-250ms, t+150ms]`), else `engine_slow` (the `cleanmic-audio`
  thread's own CPU share in that window was >= 90%), else `unexplained`. A
  recording's `hole_classes` gets its own INFO row, and an all-`host_starved`
  `holes` FAIL downgrades to INCONCLUSIVE.
- **Offline-tested.** Every rule above is a pure function
  (`contention.py`'s parsers/classification/`summarize`/`decide_quiet`,
  `analyze.py`'s `decide_attempts`/`attribute_holes`/`downgrade_row`) with
  synthetic-Instant-style fixtures in `test_analyze.py` — no root, no live
  PipeWire graph, no real waiting.

## Troubleshooting

- **Leftover `cmtest_*` nodes** after a crash: `pw-dump | python3
  scripts/e2e/pwgraph.py nodes --prefix cmtest_` to list them, then
  `nested-run.sh reap` and re-check.
- **A display already in use**: `nested-run.sh stop :47` (or `reap`) before
  starting a new run; `xephyr` refuses (exit 11) rather than reusing a
  server it didn't start.
- **`check-layout` reports DRIFT**: the layout changed; see Recalibration
  above.

## Limits

- No perceptual (subjective) quality judgement — a human listen is still the
  final word for anything the metrics can't see.
- No hardware clock drift (the synthetic mic and the recorder share one
  PipeWire graph clock).
- The updater still performs its normal GitHub check during a run (it does
  not touch the owner's config; harmless, just a real network call).

## Deviation from the original request

The original request named the topology "pw-loopback `cmtest_sink` ->
`cmtest_mic`". The 2026-09-24 base-latency debug session proved that
topology is **not** silent: gnome-remote-desktop attaches a
`GRD::RDP::AUDIO_PLAYBACK` capture stream to every `Audio/Sink` node, so test
speech would be mirrored to a connected RDP client. This harness uses the
RDP-safe topology that session validated instead: `cmtest_in` (a
`Stream/Input/Audio` with `node.autoconnect=false`) feeding `cmtest_mic`,
fed in turn by an unlinked `pw-play` through an explicit `pw-link`, plus the
`pwgraph.py` link audit before and after every playback link. The optional
monitor path needs a real `Audio/Sink` (`cmtest_null`); it only runs under
`--monitor-null-sink`, and the run refuses (exit 4) if a remote-desktop
capture (or anything else foreign) attaches to it.

//! DeepFilterNet overload regression (debug session dfn-panic-under-load).
//!
//! The vendored DeepFilterNet LADSPA plugin (v0.5.6) counts every `run()`
//! call that takes longer than one block as an "underrun", adds 10 ms of
//! latency and a 10 ms block of digital silence for each one, and on the
//! underrun that finds its delay counter at one second it calls `panic!()`
//! on the HOST's thread, inside its own Rust runtime — which aborts the
//! whole CleanMic process (reproduced: SIGABRT, `catch_unwind` never sees
//! it). The virtual mic then vanishes: "dead output" under CPU load.
//!
//! This test starves the plugin's worker thread for real (the child process
//! is pinned to one CPU next to busy spinner threads) and drives
//! [`DeepFilterEngine`] at real-time pace, in a CHILD process so an abort is
//! observable as an exit status instead of killing the test harness.
//!
//! Quick 260924-n4s (D-01): the guard no longer gives up outright at the
//! third fast trip — it bypasses/shadows the plugin and can self-heal. This
//! child now proves the grace window survives sustained starvation, not an
//! immediate give-up: it runs [`cleanmic::audio::ENGINE_FALLBACK_GRACE`] + 2 s
//! past the first Overloaded reading (capped at 25 s total), and the output
//! must equal the dry input on EVERY block processed while Overloaded (never
//! silence) and the engine must still be Overloaded when the child stops
//! (the starvation never let up, so recovery must never have been claimed).
//!
//! Needs the vendored plugin (`vendor/libdeep_filter_ladspa.so`, fetched by
//! `scripts/fetch-vendors.sh`); skipped when it is absent. `#[ignore]`d
//! because it deliberately saturates a CPU for up to ~25 s:
//!
//! `cargo test --release --features deepfilter --test deepfilter_overload -- --ignored --nocapture`

#![cfg(feature = "deepfilter")]

use cleanmic::engine::deepfilter::DeepFilterEngine;
use cleanmic::engine::{EngineHealth, NoiseEngine};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BLOCK: usize = 480;
const CHILD_ENV: &str = "CLEANMIC_DFN_OVERLOAD_CHILD";
const TEST_NAME: &str = "starved_deepfilternet_never_aborts_and_never_goes_silent";

/// Spinner threads sharing the child's single CPU. With 12, the unguarded
/// plugin reached its 100th underrun and aborted in ~4 s (probe run,
/// 2026-09-24); the DF worker gets at most ~1/13 of the CPU.
const SPINNERS: usize = 12;

fn pin_current_thread_to(cpu: usize) {
    // SAFETY: `set` is a zeroed, uniquely-owned cpu_set_t; pid 0 = caller.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        assert_eq!(
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set),
            0,
            "sched_setaffinity failed"
        );
    }
}

/// Child body: starve the plugin, feed a never-silent signal at real-time
/// pace in the 1024-quantum pattern the audio thread uses (2 blocks back to
/// back per ~21 ms), and print one machine-readable result line.
fn run_child() {
    let cpu = std::thread::available_parallelism().map_or(1, |n| n.get()) - 1;
    // Pin BEFORE any thread exists: the spinners and the plugin's worker
    // (spawned by instantiate) inherit this single-CPU mask.
    pin_current_thread_to(cpu);
    for _ in 0..SPINNERS {
        std::thread::spawn(|| {
            let mut x: u64 = 1;
            loop {
                x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
            }
        });
    }

    let mut engine = DeepFilterEngine::new();
    engine.set_strength(0.5);
    engine.init(48_000).expect("DeepFilterNet init");

    let mut lcg: u32 = 12_345;
    let mut n: u64 = 0;
    let mut next_sample = move || {
        lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (lcg >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
        let t = n as f32 / 48_000.0;
        n += 1;
        0.2 * (2.0 * std::f32::consts::PI * 220.0 * t).sin() + 0.01 * noise
    };

    let mut input = vec![0.0f32; BLOCK];
    let mut output = vec![0.0f32; BLOCK];
    let start = Instant::now();
    let period = Duration::from_millis(20);
    let mut deadline = start;
    let (mut blocks, mut zero_blocks, mut zero_run, mut longest_zero_run) =
        (0u64, 0u64, 0u64, 0u64);
    let mut overloaded_at: Option<Duration> = None;
    let mut mismatch_while_overloaded = 0u64;
    // D-01: keep the starvation going ENGINE_FALLBACK_GRACE + 2 s past the
    // first Overloaded reading (to prove the grace window's bypass/shadow
    // self-healing survives SUSTAINED starvation without ever reaching the
    // plugin's own abort), or 25 s if it never gets there.
    let overload_hold = cleanmic::audio::ENGINE_FALLBACK_GRACE + Duration::from_secs(2);
    while start.elapsed() < Duration::from_secs(25)
        && overloaded_at.is_none_or(|t| start.elapsed() < t + overload_hold)
    {
        for _ in 0..2 {
            for s in input.iter_mut() {
                *s = next_sample();
            }
            // Health BEFORE this block: the trip block itself (Normal ->
            // Bypassed) legitimately carries a real (non-dry) plugin result
            // -- it was still Healthy when its own processing began. Only a
            // block that started ALREADY Overloaded must come out dry.
            let health_before = engine.health();
            engine.process(&input, &mut output);
            blocks += 1;
            if output.iter().all(|&s| s == 0.0) {
                zero_blocks += 1;
                zero_run += 1;
                longest_zero_run = longest_zero_run.max(zero_run);
            } else {
                zero_run = 0;
            }
            if overloaded_at.is_none() && engine.health() == EngineHealth::Overloaded {
                overloaded_at = Some(start.elapsed());
            }
            if health_before == EngineHealth::Overloaded && output != input {
                mismatch_while_overloaded += 1;
            }
        }
        deadline += period;
        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }
    }
    let bypass_is_live = output.iter().zip(&input).all(|(o, i)| o == i);
    let overloaded_at_end = engine.health() == EngineHealth::Overloaded;
    println!(
        "CHILD_RESULT blocks={blocks} zero_blocks={zero_blocks} longest_zero_run={longest_zero_run} overloaded={} overloaded_after_ms={} bypass_is_live={bypass_is_live} mismatch_while_overloaded={mismatch_while_overloaded} overloaded_at_end={overloaded_at_end}",
        overloaded_at.is_some(),
        overloaded_at.map_or(-1, |t| t.as_millis() as i64),
    );
    // Deliberately no teardown: the child exits right away and a spinner
    // could otherwise delay it. Exit code 0 == "the process survived".
    std::process::exit(0);
}

fn field(line: &str, key: &str) -> String {
    line.split_whitespace()
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("missing {key} in {line:?}"))
        .to_string()
}

#[test]
#[ignore = "saturates one CPU for up to ~25 s; needs vendor/libdeep_filter_ladspa.so"]
fn starved_deepfilternet_never_aborts_and_never_goes_silent() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_child();
        return;
    }
    let plugin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/libdeep_filter_ladspa.so");
    if !plugin.is_file() {
        eprintln!(
            "SKIP: {} not found (run scripts/fetch-vendors.sh)",
            plugin.display()
        );
        return;
    }
    // DeepFilterEngine looks for $APPDIR/usr/lib/libdeep_filter_ladspa.so first.
    let appdir = tempfile::tempdir().expect("temp APPDIR");
    std::fs::create_dir_all(appdir.path().join("usr/lib")).unwrap();
    std::os::unix::fs::symlink(
        &plugin,
        appdir.path().join("usr/lib/libdeep_filter_ladspa.so"),
    )
    .unwrap();

    // Files, not pipes: the plugin logs two lines per underrun, and an unread
    // pipe would block the child once its buffer fills.
    let out_path = appdir.path().join("child.stdout");
    let err_path = appdir.path().join("child.stderr");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            TEST_NAME,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .env("APPDIR", appdir.path())
        .env("RUST_LOG", "warn")
        .stdout(Stdio::from(std::fs::File::create(&out_path).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()))
        .spawn()
        .expect("spawn child");
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("child did not finish within 90 s");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    let panics = stderr
        .matches("Processing too slow! Please upgrade your CPU")
        .count();
    // Each plugin instance logs under its own random id ("DF <id> | ...");
    // the guard must really have replaced the instance, not just reset its
    // own counters. Also tally underruns PER instance id: the shadow
    // ceiling (SHADOW_UNDERRUN_CEILING = 40) bounds each instance's own
    // count, not just the total across every instance this run churns
    // through.
    let mut per_instance: std::collections::BTreeMap<&str, usize> =
        std::collections::BTreeMap::new();
    for l in stderr.lines().filter(|l| l.contains("Underrun detected")) {
        if let Some(id) = l
            .split("DF ")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
        {
            *per_instance.entry(id).or_insert(0) += 1;
        }
    }
    let instances: std::collections::BTreeSet<&str> = per_instance.keys().copied().collect();
    let underruns = stderr.matches("Underrun detected").count();
    let max_underruns_per_instance = per_instance.values().copied().max().unwrap_or(0);
    eprintln!(
        "child status: {status:?}; plugin underrun lines: {underruns} (max {max_underruns_per_instance} on one instance); plugin panic lines: {panics}"
    );

    assert!(
        status.success(),
        "the starved DeepFilterNet child did not survive (status {status:?}) — the plugin's \
         'Processing too slow' abort path is reachable.\n--- child stderr tail ---\n{}",
        stderr
            .lines()
            .rev()
            .take(15)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(panics, 0, "the plugin reached its panic!() path");
    assert!(
        instances.len() >= 2,
        "sustained overload must restart the plugin before giving up (instances seen: {instances:?})"
    );
    assert!(
        max_underruns_per_instance <= 40,
        "one plugin instance logged {max_underruns_per_instance} underruns \
         (SHADOW_UNDERRUN_CEILING should cap any one instance at 40, per instance: {per_instance:?})"
    );

    // The libtest harness prints "test <name> ... " on the same line first.
    let line = stdout
        .lines()
        .find_map(|l| l.find("CHILD_RESULT").map(|at| &l[at..]))
        .unwrap_or_else(|| panic!("no CHILD_RESULT line; stdout:\n{stdout}"));
    eprintln!("{line}");
    assert_eq!(
        field(line, "overloaded"),
        "true",
        "a worker starved to ~1/13 of a CPU must be reported Overloaded"
    );
    assert_eq!(
        field(line, "bypass_is_live"),
        "true",
        "after giving up, the engine must pass the live input through (never silence)"
    );
    assert_eq!(
        field(line, "mismatch_while_overloaded"),
        "0",
        "output must equal the dry input on EVERY block processed while Overloaded"
    );
    assert_eq!(
        field(line, "overloaded_at_end"),
        "true",
        "the starvation never let up: still Overloaded ENGINE_FALLBACK_GRACE + 2s after it began"
    );
    let overloaded_after_ms: i64 = field(line, "overloaded_after_ms").parse().unwrap();
    assert!(
        overloaded_after_ms <= 15_000,
        "overload must be detected within 15 s, took {overloaded_after_ms} ms"
    );
    // Each plugin underrun inserts one 480-sample block of zeros, and every
    // fresh instance starts with one block of prefill zeros; the guard caps
    // both, so digital silence stays a handful of isolated 10 ms blocks.
    let longest_zero_run: u64 = field(line, "longest_zero_run").parse().unwrap();
    assert!(
        longest_zero_run <= 10,
        "output went silent for {longest_zero_run} consecutive 10 ms blocks"
    );
    let zero_blocks: u64 = field(line, "zero_blocks").parse().unwrap();
    assert!(
        zero_blocks <= 40,
        "{zero_blocks} silent 10 ms output blocks — the underrun budget is not bounding the plugin's holes"
    );
}

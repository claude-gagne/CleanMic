//! DeepFilterNet LADSPA overload probe (debug session dfn-panic-under-load).
//!
//! Drives the real vendored `libdeep_filter_ladspa.so` through
//! [`DeepFilterEngine`] at real-time pace, optionally starving the plugin's
//! worker thread by pinning the whole process to one CPU next to busy spinner
//! threads, and prints per-second health statistics: `run()` wall times, how
//! many calls took at least one block duration (the plugin's own "Underrun"
//! condition), and how many output blocks came back as exact digital silence.
//!
//! With the underrun guard in place the probe also shows the guard at work
//! (the adapter logs its restarts / give-up; `RUST_LOG=warn` shows them and
//! the plugin's own "Underrun detected" lines). The automated regression
//! for the same starvation is `tests/deepfilter_overload.rs`
//! (`make test-dfn-overload`); this probe is for tuning on other hardware.
//!
//! Flags: `--seconds N` (default 30), `--spinners K` busy threads (default
//! 0), `--pin CPU` pin the process (and so every later thread) to one CPU,
//! `--burst B` blocks back to back per period (default 2 = the 1024-sample
//! quantum pattern), `--strength S` (default 0.5), `--reinit N` time N extra
//! instantiations and print the RSS/thread count each leaves behind.
//!
//! Run (needs `$APPDIR/usr/lib/libdeep_filter_ladspa.so`):
//! `APPDIR=/path/to/appdir RUST_LOG=warn cargo run --release --example probe_dfn_overload --features deepfilter -- --seconds 30 --spinners 6 --pin 0`

use cleanmic::engine::NoiseEngine;
use cleanmic::engine::deepfilter::DeepFilterEngine;
use std::time::{Duration, Instant};

const BLOCK: usize = 480;
const BLOCK_DUR: Duration = Duration::from_millis(10);

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn pin_to_cpu(cpu: usize) {
    // SAFETY: `set` is a zeroed, uniquely-owned cpu_set_t; pid 0 = this thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            eprintln!("probe: sched_setaffinity failed");
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seconds: u64 = arg(&args, "--seconds", 30);
    let spinners: usize = arg(&args, "--spinners", 0);
    let pin: i64 = arg(&args, "--pin", -1);
    let burst: usize = arg(&args, "--burst", 2); // blocks per period (1024-quantum ~ 2)
    let strength: f32 = arg(&args, "--strength", 0.5);

    if pin >= 0 {
        pin_to_cpu(pin as usize);
    }
    for _ in 0..spinners {
        std::thread::spawn(|| {
            let mut x: u64 = 1;
            loop {
                x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
            }
        });
    }

    let mut engine = DeepFilterEngine::new();
    engine.set_strength(strength);
    let t_init = Instant::now();
    engine.init(48_000).expect("DeepFilterNet init");
    println!(
        "probe: first init took {:.1} ms",
        t_init.elapsed().as_secs_f64() * 1000.0
    );
    let reinit: usize = arg(&args, "--reinit", 0);
    for i in 0..reinit {
        let mut e2 = DeepFilterEngine::new();
        let t = Instant::now();
        e2.init(48_000).expect("DeepFilterNet re-init");
        let dt = t.elapsed().as_secs_f64() * 1000.0;
        let mut o = vec![0.0f32; BLOCK];
        let t = Instant::now();
        e2.process(&vec![0.1f32; BLOCK], &mut o);
        e2.process(&vec![0.1f32; BLOCK], &mut o);
        let dp = t.elapsed().as_secs_f64() * 1000.0;
        e2.teardown();
        std::thread::sleep(Duration::from_millis(50));
        let rss = std::fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .find(|l| l.starts_with("VmRSS"))
            .unwrap_or("")
            .to_string();
        let threads = std::fs::read_dir("/proc/self/task")
            .map(|d| d.count())
            .unwrap_or(0);
        println!(
            "probe: re-init #{i} took {dt:.1} ms, first 2 blocks {dp:.1} ms, {rss}, threads={threads}"
        );
    }

    // Speech-like test signal: a 220 Hz tone with a 3 Hz syllable envelope
    // plus low-level noise (never digital silence on the input side).
    let mut lcg: u32 = 12345;
    let mut n: u64 = 0;
    let mut next_sample = move || {
        lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (lcg >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
        let t = n as f32 / 48_000.0;
        n += 1;
        let env = 0.5 + 0.5 * (2.0 * std::f32::consts::PI * 3.0 * t).sin();
        0.2 * env * (2.0 * std::f32::consts::PI * 220.0 * t).sin() + 0.01 * noise
    };

    let mut input = vec![0.0f32; BLOCK];
    let mut output = vec![0.0f32; BLOCK];
    let start = Instant::now();
    let period = BLOCK_DUR * burst as u32;
    let mut deadline = start;
    let (mut calls, mut slow, mut zero_blocks) = (0u64, 0u64, 0u64);
    let mut max_wall = Duration::ZERO;
    let mut total_slow = 0u64;
    let mut sec = 0u64;
    println!("probe: seconds={seconds} spinners={spinners} pin={pin} burst={burst}");
    while start.elapsed() < Duration::from_secs(seconds) {
        for _ in 0..burst {
            for s in input.iter_mut() {
                *s = next_sample();
            }
            let t0 = Instant::now();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                engine.process(&input, &mut output);
            }));
            let wall = t0.elapsed();
            if r.is_err() {
                println!("probe: catch_unwind CAUGHT a panic from process()");
            }
            calls += 1;
            max_wall = max_wall.max(wall);
            if wall >= BLOCK_DUR {
                slow += 1;
                total_slow += 1;
            }
            if output.iter().all(|&s| s == 0.0) {
                zero_blocks += 1;
            }
        }
        deadline += period;
        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }
        let s = start.elapsed().as_secs();
        if s > sec {
            sec = s;
            println!(
                "probe: t={s:>3}s calls={calls:>4} slow(>=10ms)={slow:>3} total_slow={total_slow:>4} zero_blocks={zero_blocks:>3} max_wall={:.1}ms",
                max_wall.as_secs_f64() * 1000.0
            );
            calls = 0;
            slow = 0;
            zero_blocks = 0;
            max_wall = Duration::ZERO;
        }
    }
    engine.teardown();
    println!("probe: SURVIVED {seconds}s (total_slow={total_slow})");
}

//! CPU-time per-`ProcessingMode` sweep harness for the D-03 DPDFNet
//! inference-decimation gate.
//!
//! Adapted from `.planning/spikes/005-lowend-cpu-headroom/bench-cputime.rs`
//! (the canonical TRUE-CPU-time methodology this repo already validated),
//! extended to sweep `ProcessingMode` (`MaxQuality`/`Balanced`/`LowCpu`) per
//! DPDFNet variant (DPDFNet-2, DPDFNet-8) via the real production
//! `create_engine` factory -- the same factory the shipping app uses, not a
//! direct `DpdfnetEngine::new` call, so these numbers reflect the actual
//! `$APPDIR`-resolved bundled models/runtime.
//!
//! ## Methodology (RESEARCH.md Pitfall 2)
//!
//! Measures TRUE CPU-time via `CLOCK_PROCESS_CPUTIME_ID` (sums all threads),
//! never wall-clock alone -- Spike 005 proved wall-clock-only measurement
//! gives a *backwards* conclusion for DeepFilterNet (looked cheaper under
//! throttling because it was blocking/scheduling overhead, not compute).
//! Each cell: a warm-up pass, then a fixed measured window, with math-library
//! thread pools pinned to one thread throughout (mirrors `src/main.rs`'s own
//! `limit_thread_pools`; DPDFNet's own ONNX session is already pinned to a
//! single intra/inter-op thread internally, but this defends against any
//! other library the process links spawning extra threads).
//!
//! ## Labeling (D-06)
//!
//! Every number this harness prints is **simulated / dev-box-derived** --
//! this dev box's absolute percentages are not a hardware-verified claim,
//! only the ordering/ratios are the robust takeaway. This harness ONLY
//! measures; it records no evidence file (that is 15.2-04's job).
//!
//! ## Khip note (RESEARCH.md Pitfall 4)
//!
//! This harness does not sweep Khip (D-03/decimation is DPDFNet-only), but
//! logs which `libkhip.so` (if any) is active on this system for context --
//! Khip's cost is inherently environment-specific (depends on whichever
//! library the user installed) and must never be silently reused across
//! machines/sessions.
//!
//! Run: `cargo run --release --example bench_mode_cpu --features dpdfnet`
//! (requires `$APPDIR` pointing at a built AppDir with the bundled DPDFNet
//! models + ONNX Runtime dylib -- exactly what the shipping AppImage
//! provides. A variant whose assets are unavailable in this environment
//! prints `UNAVAILABLE` rather than fabricating a number, matching
//! `create_engine`'s own fail-closed contract.)

use cleanmic::engine::{EngineType, ProcessingMode, create_engine};
use std::time::Instant;

const FRAME: usize = 480;
const WARMUP: usize = 100;
const MEASURE: usize = 2000; // 20 s of simulated audio per cell

/// True CPU-time (sums across all threads of this process), distinct from
/// wall-clock -- see the module docs' RESEARCH.md Pitfall 2 note.
fn cpu_secs() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, uniquely-owned out-parameter for this call.
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// Deterministic tone + noise signal generator, identical recipe to
/// `bench-cputime.rs` so results stay comparable to that spike's baseline.
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

/// Constrain math-library thread pools before any engine is constructed --
/// mirrors `src/main.rs`'s own `limit_thread_pools`, so the measured window
/// stays pinned to one thread regardless of which native library the active
/// engine links.
fn limit_thread_pools() {
    // SAFETY: called at the very start of `main`, before any engine (and any
    // thread pool a linked native library might spawn) is constructed.
    unsafe {
        std::env::set_var("OPENBLAS_NUM_THREADS", "1");
        std::env::set_var("OMP_NUM_THREADS", "1");
        std::env::set_var("FFTW_NUM_THREADS", "1");
    }
}

fn bench(mode_label: &str, engine_type: EngineType, mode: ProcessingMode) {
    let mut engine = match create_engine(engine_type) {
        Ok(e) => e,
        Err(e) => {
            println!("    {mode_label:<20}: UNAVAILABLE ({e})");
            return;
        }
    };
    engine.set_strength(0.5);
    engine.set_mode(mode);

    let mut input = vec![0f32; FRAME];
    let mut output = vec![0f32; FRAME];
    let mut phase = 0f32;
    let mut seed = 0x1234_5678u32;

    for _ in 0..WARMUP {
        fill(&mut input, &mut phase, &mut seed);
        engine.process(&input, &mut output);
    }

    let audio_secs = (MEASURE * FRAME) as f64 / 48_000.0;
    let wall0 = Instant::now();
    let cpu0 = cpu_secs();
    for _ in 0..MEASURE {
        fill(&mut input, &mut phase, &mut seed);
        engine.process(&input, &mut output);
    }
    let wall = wall0.elapsed().as_secs_f64();
    let cpu = cpu_secs() - cpu0;
    let wall_pct = wall / audio_secs * 100.0;
    let cpu_pct = cpu / audio_secs * 100.0;

    println!("    {mode_label:<20}: wall={wall_pct:6.1}%  TRUE-cpu={cpu_pct:7.1}%");
}

fn main() {
    limit_thread_pools();

    println!(
        "D-03 DPDFNet Mode decimation CPU-time sweep -- SIMULATED / DEV-BOX-DERIVED numbers only."
    );
    println!(
        "TRUE CPU-time (CLOCK_PROCESS_CPUTIME_ID, all threads), {MEASURE} hops (~{:.0}s simulated audio) per cell, single-thread env pinning.\n",
        (MEASURE * FRAME) as f64 / 48_000.0
    );

    match cleanmic::engine::khip::find_library() {
        Some(path) => println!(
            "Khip library detected at: {} (not swept by this harness -- Khip's cost is environment-specific per RESEARCH.md Pitfall 4)\n",
            path.display()
        ),
        None => {
            println!("Khip library: none detected on this system (not swept by this harness)\n")
        }
    }

    for (variant_name, engine_type) in [
        ("DPDFNet-2", EngineType::Dpdfnet2),
        ("DPDFNet-8", EngineType::Dpdfnet8),
    ] {
        println!("{variant_name}:");
        for (mode_label, mode) in [
            ("MaxQuality (1x)", ProcessingMode::MaxQuality),
            ("Balanced (2x)", ProcessingMode::Balanced),
            ("LowCpu (4x)", ProcessingMode::LowCpu),
        ] {
            bench(mode_label, engine_type, mode);
        }
        println!();
    }

    println!(
        "All percentages above are SIMULATED / DEV-BOX-DERIVED (D-06) -- not a hardware-verified claim."
    );
    println!("This harness only measures; it records no evidence file (15.2-04's job).");
}

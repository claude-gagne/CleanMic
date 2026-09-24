//! D-03 Mode quality and bit-identity probe for quick 260923-v4q and 15.2-04.
//!
//! Every number this harness prints is **simulated / dev-box-derived** (D-06)
//! -- this dev box's numbers are not a hardware-verified claim, and none of
//! them are a perceptual verdict: they are objective proxies (bit-identity
//! hash, energy-envelope correlation/lag, an exact-repeat proxy for the
//! robotic buzz, and level) meant to corroborate or contradict what the
//! owner's ear decides in 15.2-04's re-listen pass, never to replace it.
//!
//! For each variant (DPDFNet-2, DPDFNet-8) x clip (the two tracked
//! `assets/demo/*-before.wav` speech/noise clips) x mode (`MaxQuality`,
//! `Balanced`, `LowCpu`), this builds a real, initialized production engine
//! via the same `create_engine` factory the shipping app uses (so it
//! compiles and runs unchanged in every feature configuration, exactly like
//! `bench_mode_cpu.rs`), processes the whole clip in real 480-sample hops,
//! and prints:
//!
//! - `HASH <variant> <clip> MaxQuality <hash>` -- a 16-hex FNV-1a-64 digest
//!   over every output sample's `f32::to_bits` little-endian bytes. Run
//!   before vs. after a change with `$APPDIR` pointing at the same staged
//!   assets: identical `HASH` lines are the bit-identity proof for
//!   `MaxQuality` (R4).
//! - `MODE <variant> <clip> <mode> ...` -- for every mode, the decimation
//!   ratio, output level (dBFS), level delta vs. this clip's `MaxQuality`
//!   output, the best-lag envelope correlation and lag (hops) vs.
//!   `MaxQuality`, the theoretically expected lag (`4 * (ratio - 1)`, E1),
//!   and `exact_repeat_frac` -- the fraction of hops whose output looks like
//!   a near-exact repeat of the previous hop, the objective proxy for the
//!   pre-fix robotic buzz.
//!
//! Every rendered output is also written as a 16-bit PCM mono 48 kHz WAV
//! under `$PROBE_OUT_DIR/$PROBE_LABEL/<variant>-<clip>-<mode>.wav`
//! (`PROBE_OUT_DIR` defaults to `<manifest>/build/mode-renders`,
//! `PROBE_LABEL` to `"current"`) for an optional owner A/B listen.
//!
//! Run: `cargo run --release --example probe_mode_quality --features dpdfnet`
//! (requires `$APPDIR` pointing at a built AppDir with the bundled DPDFNet
//! models + ONNX Runtime dylib, exactly like `bench_mode_cpu.rs`). A variant
//! that can't be built in this environment prints `UNAVAILABLE` for every
//! one of its clip/mode combinations rather than fabricating a number.

use cleanmic::config::Config;
use cleanmic::engine::{EngineType, NoiseEngine, ProcessingMode, create_engine};
use std::path::{Path, PathBuf};

const HOP: usize = 480;
const SAMPLE_RATE: u32 = 48_000;

/// FNV-1a-64: simple, dependency-free, and stable across runs/platforms --
/// unlike `std::collections::hash_map::DefaultHasher` (SipHash, randomly
/// seeded per-process), which would make the "identical hash pre vs post"
/// bit-identity check meaningless.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// A minimal RIFF/WAVE chunk walker: requires a `fmt ` chunk describing PCM,
/// 1 channel, 48000 Hz, 16-bit, and reads the `data` chunk -- tolerating (and
/// skipping) any other chunk (e.g. `LIST`/`INFO` metadata, present in this
/// repo's tracked demo clips).
fn read_wav_mono_48k_16bit_pcm(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        bytes.len() >= 12,
        "{}: file too short for RIFF header",
        path.display()
    );
    assert_eq!(
        &bytes[0..4],
        b"RIFF",
        "{}: missing RIFF magic",
        path.display()
    );
    assert_eq!(
        &bytes[8..12],
        b"WAVE",
        "{}: missing WAVE magic",
        path.display()
    );

    let mut pos = 12usize;
    let mut fmt_seen = false;
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let chunk_id = &bytes[pos..pos + 4];
        let chunk_len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + chunk_len).min(bytes.len());
        let body = &bytes[body_start..body_end];

        if chunk_id == b"fmt " {
            assert!(body.len() >= 16, "{}: fmt chunk too short", path.display());
            let format_tag = u16::from_le_bytes(body[0..2].try_into().unwrap());
            let channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
            let sample_rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            let bits_per_sample = u16::from_le_bytes(body[14..16].try_into().unwrap());
            assert_eq!(
                format_tag,
                1,
                "{}: must be PCM (format tag 1)",
                path.display()
            );
            assert_eq!(channels, 1, "{}: must be mono", path.display());
            assert_eq!(
                sample_rate,
                SAMPLE_RATE,
                "{}: must be 48 kHz",
                path.display()
            );
            assert_eq!(bits_per_sample, 16, "{}: must be 16-bit", path.display());
            fmt_seen = true;
        } else if chunk_id == b"data" {
            data = Some(body);
        }

        // RIFF chunks are word-aligned: a chunk with an odd byte length has
        // one padding byte after it that is not part of `chunk_len`.
        pos = body_end + (chunk_len % 2);
    }

    assert!(fmt_seen, "{}: no fmt chunk found", path.display());
    let data = data.unwrap_or_else(|| panic!("{}: no data chunk found", path.display()));

    data.chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}

/// Writes `samples` (already in -1.0..=1.0 range, clamped defensively here)
/// as a minimal 16-bit PCM mono 48 kHz WAV file.
fn write_wav_mono_48k_16bit_pcm(path: &Path, samples: &[f32]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("create dir {}: {e}", parent.display()));
    }
    let data_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    let byte_rate = SAMPLE_RATE * 2;
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let quantized = (clamped * 32767.0).round() as i16;
        out.extend_from_slice(&quantized.to_le_bytes());
    }
    std::fs::write(path, &out).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Processes `input` in whole 480-sample hops (a partial trailing hop, if
/// any, is dropped -- matching CleanMic's own fixed-block pipeline
/// contract), returning one output sample per processed input sample.
fn process_whole_hops(engine: &mut dyn NoiseEngine, input: &[f32]) -> Vec<f32> {
    let whole_hops = input.len() / HOP;
    let mut output = Vec::with_capacity(whole_hops * HOP);
    let mut out_hop = [0f32; HOP];
    for h in 0..whole_hops {
        let in_hop = &input[h * HOP..(h + 1) * HOP];
        engine.process(in_hop, &mut out_hop);
        output.extend_from_slice(&out_hop);
    }
    output
}

/// Per-hop dB energy envelope (20*log10(rms), floored) over a whole-hop
/// output buffer.
fn hop_envelope_db(output: &[f32]) -> Vec<f32> {
    output
        .chunks_exact(HOP)
        .map(|hop| {
            let rms = (hop.iter().map(|v| v * v).sum::<f32>() / HOP as f32).sqrt();
            20.0 * rms.max(1e-9).log10()
        })
        .collect()
}

fn rms_dbfs(output: &[f32]) -> f32 {
    let rms = (output.iter().map(|v| v * v).sum::<f32>() / output.len().max(1) as f32).sqrt();
    20.0 * rms.max(1e-9).log10()
}

/// Best-lag Pearson correlation between `candidate`'s and `reference`'s
/// per-hop energy envelopes, searching lags 0..=20 hops and skipping the
/// first 50 hops of both envelopes (onset/warm-up transients would
/// otherwise dominate a short envelope's correlation). Returns
/// `(best_lag, best_corr)`; `(0, f32::NAN)` if there is not enough overlap
/// to compute a correlation at any lag.
fn best_lag_correlation(candidate: &[f32], reference: &[f32]) -> (i32, f32) {
    const SKIP: usize = 50;
    const MAX_LAG: i32 = 20;

    let pearson = |a: &[f32], b: &[f32]| -> f32 {
        let n = a.len().min(b.len());
        if n < 2 {
            return f32::NAN;
        }
        let mean_a = a[..n].iter().sum::<f32>() / n as f32;
        let mean_b = b[..n].iter().sum::<f32>() / n as f32;
        let mut cov = 0f32;
        let mut var_a = 0f32;
        let mut var_b = 0f32;
        for i in 0..n {
            let da = a[i] - mean_a;
            let db = b[i] - mean_b;
            cov += da * db;
            var_a += da * da;
            var_b += db * db;
        }
        if var_a <= 1e-9 || var_b <= 1e-9 {
            return f32::NAN;
        }
        cov / (var_a.sqrt() * var_b.sqrt())
    };

    if candidate.len() <= SKIP || reference.len() <= SKIP {
        return (0, f32::NAN);
    }
    let cand_tail = &candidate[SKIP..];
    let ref_tail = &reference[SKIP..];

    let mut best_lag = 0i32;
    let mut best_corr = f32::NEG_INFINITY;
    let mut any = false;
    for lag in -MAX_LAG..=MAX_LAG {
        // candidate[t] vs reference[t - lag]: positive lag means candidate
        // lags reference (candidate's hop t matches reference's earlier hop).
        let (cand_slice, ref_slice) = if lag >= 0 {
            let l = lag as usize;
            if l >= cand_tail.len() {
                continue;
            }
            (
                &cand_tail[l..],
                &ref_tail[..ref_tail.len().saturating_sub(l)],
            )
        } else {
            let l = (-lag) as usize;
            if l >= ref_tail.len() {
                continue;
            }
            (
                &cand_tail[..cand_tail.len().saturating_sub(l)],
                &ref_tail[l..],
            )
        };
        let corr = pearson(cand_slice, ref_slice);
        if corr.is_finite() && corr > best_corr {
            best_corr = corr;
            best_lag = lag;
            any = true;
        }
    }
    if any {
        (best_lag, best_corr)
    } else {
        (0, f32::NAN)
    }
}

/// Fraction of hops `h >= 50` with output RMS > 1e-4 for which
/// `L2(out_h - out_{h-1}) / L2(out_h) < 1e-2` -- the objective proxy for a
/// held hop replaying the previous hop almost exactly (the pre-fix robotic
/// buzz).
fn exact_repeat_frac(output: &[f32]) -> f32 {
    let hops: Vec<&[f32]> = output.chunks_exact(HOP).collect();
    let mut evaluated = 0usize;
    let mut repeats = 0usize;
    for h in 50..hops.len() {
        let cur = hops[h];
        let prev = hops[h - 1];
        let out_rms = (cur.iter().map(|v| v * v).sum::<f32>() / HOP as f32).sqrt();
        if out_rms <= 1e-4 {
            continue;
        }
        let diff_l2 = cur
            .iter()
            .zip(prev.iter())
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f32>()
            .sqrt();
        let out_l2 = cur.iter().map(|v| v * v).sum::<f32>().sqrt();
        evaluated += 1;
        if out_l2 > 0.0 && diff_l2 / out_l2 < 1e-2 {
            repeats += 1;
        }
    }
    if evaluated == 0 {
        0.0
    } else {
        repeats as f32 / evaluated as f32
    }
}

fn decimation_ratio_for(mode: ProcessingMode) -> u32 {
    match mode {
        ProcessingMode::MaxQuality => 1,
        ProcessingMode::Balanced => 2,
        ProcessingMode::LowCpu => 4,
    }
}

fn out_dir() -> PathBuf {
    let base = std::env::var("PROBE_OUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("build/mode-renders"));
    let label = std::env::var("PROBE_LABEL").unwrap_or_else(|_| "current".to_string());
    base.join(label)
}

fn main() {
    println!(
        "D-03 DPDFNet Mode quality and bit-identity probe -- SIMULATED / DEV-BOX-DERIVED numbers only."
    );
    println!(
        "Objective proxies (bit-identity hash, envelope correlation/lag, exact-repeat fraction, level) -- not a perceptual verdict (the owner's ear decides).\n"
    );

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let clips: &[(&str, &str)] = &[
        ("keyboard", "assets/demo/deepfilter-keyboard-before.wav"),
        ("fan", "assets/demo/rnnoise-fan-before.wav"),
    ];
    let variants: &[(&str, EngineType)] = &[
        ("dpdfnet2", EngineType::Dpdfnet2),
        ("dpdfnet8", EngineType::Dpdfnet8),
    ];
    let modes: &[ProcessingMode] = &[
        ProcessingMode::MaxQuality,
        ProcessingMode::Balanced,
        ProcessingMode::LowCpu,
    ];

    let config = Config::default();
    let out_root = out_dir();

    for &(variant_name, engine_type) in variants {
        for &(clip_name, clip_rel) in clips {
            let clip_path = manifest_dir.join(clip_rel);
            let input = read_wav_mono_48k_16bit_pcm(&clip_path);

            let mut maxq_output: Option<Vec<f32>> = None;
            let mut maxq_dbfs: Option<f32> = None;

            for &mode in modes {
                let strength = config.strength_for(engine_type);
                let mut engine = match create_engine(engine_type) {
                    Ok(e) => e,
                    Err(e) => {
                        println!("UNAVAILABLE {variant_name} {clip_name} {mode:?}: {e}");
                        continue;
                    }
                };
                engine.set_strength(strength);
                engine.set_mode(mode);

                let output = process_whole_hops(engine.as_mut(), &input);
                engine.teardown();

                if mode == ProcessingMode::MaxQuality {
                    let mut bytes = Vec::with_capacity(output.len() * 4);
                    for &s in &output {
                        bytes.extend_from_slice(&s.to_bits().to_le_bytes());
                    }
                    let hash = fnv1a64(&bytes);
                    println!("HASH {variant_name} {clip_name} MaxQuality {hash:016x}");
                    maxq_dbfs = Some(rms_dbfs(&output));
                    maxq_output = Some(output.clone());
                }

                let ratio = decimation_ratio_for(mode);
                let level_db = rms_dbfs(&output);
                let delta_vs_maxq_db = match maxq_dbfs {
                    Some(m) => level_db - m,
                    None => f32::NAN,
                };
                let (lag, env_corr) = match &maxq_output {
                    Some(reference) => {
                        best_lag_correlation(&hop_envelope_db(&output), &hop_envelope_db(reference))
                    }
                    None => (0, f32::NAN),
                };
                let expected_lag = 4 * (ratio as i32 - 1);
                let repeat_frac = exact_repeat_frac(&output);

                println!(
                    "MODE {variant_name} {clip_name} {mode:?} ratio={ratio} level_db={level_db:.2} delta_vs_maxq_db={delta_vs_maxq_db:.2} lag_vs_maxq_hops={lag} expected_lag_hops={expected_lag} env_corr={env_corr:.4} exact_repeat_frac={repeat_frac:.4}"
                );

                let wav_path = out_root.join(format!("{variant_name}-{clip_name}-{mode:?}.wav"));
                write_wav_mono_48k_16bit_pcm(&wav_path, &output);
            }
        }
    }

    println!(
        "\nAll numbers above are SIMULATED / DEV-BOX-DERIVED (D-06) -- objective proxies, not a perceptual verdict."
    );
    println!(
        "WAV renders written under {} for an optional owner A/B listen.",
        out_root.display()
    );
}

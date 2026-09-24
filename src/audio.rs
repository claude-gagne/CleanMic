//! Audio processing loop.
//!
//! Pulls frames from the physical microphone via PipeWire, runs them through
//! the active [`NoiseEngine`](crate::engine::NoiseEngine), and pushes the
//! cleaned audio to the "CleanMic" virtual source. Optionally copies processed
//! frames to a monitor output so the user can hear the result.
//!
//! All processing is 48 kHz mono f32. The audio thread must remain lock-free:
//! no allocations, no mutexes, no I/O on the hot path.
//!
//! Captured input is DC-blocked (20 Hz one-pole, see [`DcBlocker`]) and then
//! run through a speech-gated, boost-only input auto-gain (see [`AutoGain`])
//! before it reaches the engine, so a microphone's constant DC offset never
//! gets amplified by a suppression engine or read as signal by the input
//! meter, and a too-quiet microphone's speech is brought up to a normal
//! conferencing level before either sees it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::dsp::{AutoGain, DcBlocker};
use crate::engine::{EngineHealth, NoiseEngine};
use crate::pipewire::monitor::MonitorOutput;
use crate::pipewire::ringbuf::{RingBufReader, RingBufWriter};

/// Default processing buffer size in samples (10 ms at 48 kHz).
const BUFFER_SIZE: usize = 480;

/// Sample rate used throughout the pipeline.
const SAMPLE_RATE: u32 = 48_000;

/// Cutoff frequency for the input DC blocker ([`DcBlocker`]) that runs on
/// every captured block before it reaches the engine and the input level
/// meter.
///
/// Fixes a laptop DMIC ("Ryzen HD Audio Controller Digital Microphone")
/// reported to carry ~0.109 FS of DC at typical software volume boost, which
/// filled the input meter (reading about half full in a silent room) and was
/// amplified into every suppression engine (DeepFilterNet's LADSPA plugin
/// logged "Possible clipping detected"). 20 Hz settles a DC step to within
/// 1e-3 of the step in ~55 ms (vs ~110 ms at 10 Hz), costs at most ~0.25 dB
/// at 80 Hz (well inside the 0.5 dB passband budget down to 100 Hz), and
/// removes sub-sonic rumble (fan/desk vibration) before the suppressors.
const INPUT_DC_BLOCK_CUTOFF_HZ: f32 = 20.0;

/// Duration of the crossfade window in samples (~10 ms at 48 kHz).
const CROSSFADE_SAMPLES: usize = 480;

/// Hard cap on queued capture audio (200 ms). Above this the audio thread is
/// running behind (engine slower than real time, long stall) and the oldest
/// audio is dropped rather than played out late. Must stay above the largest
/// single PipeWire capture delivery (default max quantum 8192) plus one block
/// so a normal large quantum is never trimmed.
const CAPTURE_MAX_BACKLOG: usize = 9_600;

/// Capture audio kept after trimming to [`CAPTURE_MAX_BACKLOG`] (20 ms).
const CAPTURE_KEEP_AFTER_TRIM: usize = 2 * BUFFER_SIZE;

/// Maximum blocks processed before the loop returns to service commands and
/// the heartbeat. Covers a full [`CAPTURE_MAX_BACKLOG`] in one pass, while
/// guaranteeing a slower-than-real-time engine cannot starve Stop/SetEngine.
const MAX_BLOCKS_PER_PASS: usize = CAPTURE_MAX_BACKLOG / BUFFER_SIZE + 1;

/// Capture-backlog trims (the audio thread fell more than
/// [`CAPTURE_MAX_BACKLOG`] behind) within [`FELL_BEHIND_FAULT_WINDOW`] that
/// mean the active engine is slower than real time rather than hit by a
/// one-off stall. An RTF 1.25 engine trims every ~0.7 s; DPDFNet-8 MaxQuality
/// under load trimmed 56 times in one 21 s recording (E2E 2026-09-24); quiet
/// E2E runs trim 0 times.
const FELL_BEHIND_FAULT_COUNT: usize = 3;

/// Window for [`FELL_BEHIND_FAULT_COUNT`].
const FELL_BEHIND_FAULT_WINDOW: Duration = Duration::from_secs(10);

/// Convert a sample count to milliseconds for logging.
fn samples_to_ms(samples: usize) -> f64 {
    samples as f64 * 1000.0 / f64::from(SAMPLE_RATE)
}

/// Commands sent from the control thread to the audio thread.
pub enum AudioCommand {
    /// Start processing audio.
    Start,
    /// Stop processing audio (pause).
    Stop,
    /// Swap the active noise suppression engine.
    SetEngine(Box<dyn NoiseEngine>),
    /// Change the input device by PipeWire node name.
    SetInputDevice(String),
    /// Set the normalized suppression strength (0.0..=1.0) on the active engine.
    SetStrength(f32),
    /// Set the processing mode on the active engine.
    SetMode(crate::engine::ProcessingMode),
    /// Enable or disable monitor output.
    SetMonitor(bool),
    /// Enable or disable the input auto-gain (speech-gated, boost-only
    /// leveler for too-quiet mics — see [`crate::dsp::AutoGain`]).
    SetAutoGain(bool),
    /// Attach (Some) or detach (None) the PipeWire ring-buffer writer for the
    /// monitor output. Must be sent *before* SetMonitor(true) so the first
    /// write has somewhere to go.
    SetMonitorWriter(Option<RingBufWriter>),
    /// Replace the PipeWire ring buffers after a reconnect.
    ///
    /// The audio thread swaps out its capture reader and output writer so
    /// audio flows through the newly created PipeWire streams. Pass `None`
    /// for either half to switch the thread to simulation mode for that buffer.
    SetRingBuffers {
        capture_reader: Option<RingBufReader>,
        output_writer: Option<RingBufWriter>,
    },
    /// Replace only the capture ring-buffer reader (e.g. after retargeting
    /// the capture stream to a different physical mic). Leaves the output
    /// writer untouched so the virtual source keeps feeding downstream apps.
    ReplaceCaptureReader(Option<RingBufReader>),
    /// Shut down the audio thread entirely.
    Shutdown,
}

/// Level information reported from the audio thread to the UI.
#[derive(Debug, Clone, Copy)]
pub struct LevelReport {
    /// RMS level of the DC-blocked, auto-gained capture signal (linear,
    /// 0.0..=1.0+) — exactly what the engine receives.
    ///
    /// A microphone's DC offset is inaudible (0 Hz) and is not sound, so
    /// including it in this level made a silent room read about half full on
    /// a laptop DMIC that carried ~0.109 FS of DC. With the offset removed,
    /// the input and output meters compare the same signal before and after
    /// suppression. Since quick task 260923-x24, a too-quiet mic's speech is
    /// also boosted toward a normal conferencing level (see
    /// [`crate::dsp::AutoGain`]) before this RMS is computed, so the meter
    /// shows what the engine actually receives, not the mic's raw level.
    pub input_rms: f32,
    /// RMS level of the output buffer (linear, 0.0..=1.0+).
    pub output_rms: f32,
}

/// Calculate RMS (root mean square) of a sample buffer.
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

/// Why the audio thread asks the app to replace the active engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineFault {
    /// The engine cannot keep up with real time: it said so itself
    /// ([`EngineHealth::Overloaded`], e.g. DeepFilterNet's underrun guard
    /// gave up and is passing audio through), or the audio thread fell more
    /// than [`CAPTURE_MAX_BACKLOG`] behind [`FELL_BEHIND_FAULT_COUNT`] times
    /// within [`FELL_BEHIND_FAULT_WINDOW`] while running it.
    Overloaded,
    /// The engine panicked; the audio thread already dropped it and passes
    /// audio through unprocessed.
    Panicked,
}

/// A fault of the engine installed by the `generation`-th
/// [`AudioPipeline::set_engine`] call (1-based), so the app can ignore a
/// report about an engine it has already replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineFaultReport {
    pub generation: u64,
    pub fault: EngineFault,
}

/// Watches the active engine for the failures that would otherwise leave the
/// virtual mic dead or unusable without anyone noticing, and reports each
/// engine at most once. Pure bookkeeping (no allocation, no I/O): safe on
/// the audio thread.
struct EngineWatchdog {
    /// Number of `SetEngine` commands processed; matches the app-side count
    /// of [`AudioPipeline::set_engine`] calls once the queue has drained.
    generation: u64,
    /// The current engine was already reported.
    reported: bool,
    /// Instants of the most recent capture-backlog trims (ring).
    trims: [Option<Instant>; FELL_BEHIND_FAULT_COUNT],
    next_trim: usize,
}

impl EngineWatchdog {
    fn new() -> Self {
        Self {
            generation: 0,
            reported: false,
            trims: [None; FELL_BEHIND_FAULT_COUNT],
            next_trim: 0,
        }
    }

    /// A new engine was installed: watch it from scratch.
    fn engine_replaced(&mut self) {
        self.generation += 1;
        self.reported = false;
        self.trims = [None; FELL_BEHIND_FAULT_COUNT];
    }

    fn report(&mut self, fault: EngineFault) -> Option<EngineFaultReport> {
        if self.reported {
            return None;
        }
        self.reported = true;
        Some(EngineFaultReport {
            generation: self.generation,
            fault,
        })
    }

    /// Poll the active engine's own health after a processed block.
    fn check_health(&mut self, health: EngineHealth) -> Option<EngineFaultReport> {
        match health {
            EngineHealth::Healthy => None,
            EngineHealth::Overloaded => self.report(EngineFault::Overloaded),
        }
    }

    /// The capture backlog was just trimmed at `now`.
    fn fell_behind(&mut self, now: Instant) -> Option<EngineFaultReport> {
        self.trims[self.next_trim] = Some(now);
        self.next_trim = (self.next_trim + 1) % FELL_BEHIND_FAULT_COUNT;
        // After the write, `next_trim` indexes the OLDEST of the last N.
        let oldest = self.trims[self.next_trim]?;
        if now.duration_since(oldest) <= FELL_BEHIND_FAULT_WINDOW {
            self.report(EngineFault::Overloaded)
        } else {
            None
        }
    }

    /// The active engine panicked and was dropped.
    fn panicked(&mut self) -> Option<EngineFaultReport> {
        self.report(EngineFault::Panicked)
    }
}

/// Send a fault report to the app (at most once per engine, see
/// [`EngineWatchdog`], so the channel send is rare).
fn send_fault(fault_tx: &mpsc::Sender<EngineFaultReport>, report: Option<EngineFaultReport>) {
    if let Some(report) = report {
        log::warn!(
            "Audio thread: active engine (#{}) reported {:?} — asking the app for a lighter engine",
            report.generation,
            report.fault
        );
        if fault_tx.send(report).is_err() {
            log::debug!("engine fault channel closed - app may have shut down");
        }
    }
}

/// The main audio pipeline that owns the processing thread.
///
/// Communication with the audio thread is entirely via channels — no shared
/// state or mutexes on the hot path.
pub struct AudioPipeline {
    cmd_tx: mpsc::Sender<AudioCommand>,
    level_rx: mpsc::Receiver<LevelReport>,
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Tracks whether the command channel is still open.
    /// Set to `false` whenever a send fails (audio thread is dead).
    channel_alive: Arc<AtomicBool>,
    /// Incremented by the audio thread on every main loop iteration.
    /// The health check compares successive values to detect a stuck/dead thread.
    heartbeat: Arc<AtomicU64>,
    /// Runtime engine faults reported by the audio thread.
    fault_rx: mpsc::Receiver<EngineFaultReport>,
    /// Number of engines handed to the audio thread via [`Self::set_engine`]
    /// (the generation of the most recently sent engine).
    engines_sent: AtomicU64,
}

impl Default for AudioPipeline {
    fn default() -> Self {
        // Only used in tests; unwrap is intentional per D-07.
        Self::new().unwrap()
    }
}

impl AudioPipeline {
    /// Create a new audio pipeline in **simulation mode** (no real audio I/O).
    ///
    /// Spawns the processing thread but does not start producing audio until
    /// [`AudioCommand::Start`] is sent. Input is silence; output is discarded.
    /// Useful for tests and when PipeWire is not available.
    pub fn new() -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCommand>();
        let (level_tx, level_rx) = mpsc::channel::<LevelReport>();
        let (fault_tx, fault_rx) = mpsc::channel::<EngineFaultReport>();
        let channel_alive = Arc::new(AtomicBool::new(true));
        let heartbeat = Arc::new(AtomicU64::new(0));
        let heartbeat_thread = heartbeat.clone();

        let thread_handle = spawn_audio_thread(
            cmd_rx,
            level_tx,
            fault_tx,
            None,
            None,
            heartbeat_thread,
            true,
        )?;

        Ok(Self {
            cmd_tx,
            level_rx,
            thread_handle: Some(thread_handle),
            channel_alive,
            heartbeat,
            fault_rx,
            engines_sent: AtomicU64::new(0),
        })
    }

    /// Create a new audio pipeline connected to PipeWire via ring buffers.
    ///
    /// - `capture_reader`: supplies raw mic audio from the PipeWire capture callback.
    /// - `output_writer`: receives processed audio, read by the PipeWire source callback.
    ///
    /// The audio thread reads from `capture_reader`, runs the noise engine, and
    /// writes the result to `output_writer`. If the capture ring buffer is empty
    /// the thread yields briefly instead of busy-spinning.
    pub fn with_ring_buffers(
        capture_reader: RingBufReader,
        output_writer: RingBufWriter,
    ) -> Result<Self> {
        Self::with_ring_buffers_impl(capture_reader, output_writer, true)
    }

    /// Test-only constructor that skips the whole input-conditioning chain
    /// (DC blocking and auto-gain) on the real-capture path.
    ///
    /// Production always conditions (see [`with_ring_buffers`](Self::with_ring_buffers)).
    /// This exists solely for the index-stamped latency/backlog harness
    /// (`FakePipeWire`), whose capture samples are a ramp (`value = sample
    /// index`): the DC blocker correctly turns a ramp into a near-constant,
    /// and the auto-gain would react to that ramp's ever-growing energy —
    /// either would defeat that harness's latency measurement. The latency
    /// measurement itself is independent of input conditioning, so bypassing
    /// the whole chain there does not weaken those regression tests.
    #[cfg(test)]
    pub(crate) fn with_ring_buffers_unfiltered(
        capture_reader: RingBufReader,
        output_writer: RingBufWriter,
    ) -> Result<Self> {
        Self::with_ring_buffers_impl(capture_reader, output_writer, false)
    }

    fn with_ring_buffers_impl(
        capture_reader: RingBufReader,
        output_writer: RingBufWriter,
        condition_input: bool,
    ) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCommand>();
        let (level_tx, level_rx) = mpsc::channel::<LevelReport>();
        let (fault_tx, fault_rx) = mpsc::channel::<EngineFaultReport>();
        let channel_alive = Arc::new(AtomicBool::new(true));
        let heartbeat = Arc::new(AtomicU64::new(0));
        let heartbeat_thread = heartbeat.clone();

        let thread_handle = spawn_audio_thread(
            cmd_rx,
            level_tx,
            fault_tx,
            Some(capture_reader),
            Some(output_writer),
            heartbeat_thread,
            condition_input,
        )?;

        Ok(Self {
            cmd_tx,
            level_rx,
            thread_handle: Some(thread_handle),
            channel_alive,
            heartbeat,
            fault_rx,
            engines_sent: AtomicU64::new(0),
        })
    }

    /// Returns the current heartbeat counter value.
    ///
    /// The health check compares successive values to detect a stuck/dead thread.
    pub fn heartbeat_count(&self) -> u64 {
        self.heartbeat.load(Ordering::Acquire)
    }

    /// Returns `true` if the audio thread command channel is still open.
    /// Used by the health check timer to detect a dead audio thread.
    pub fn is_cmd_channel_open(&self) -> bool {
        self.channel_alive.load(Ordering::Acquire)
    }

    /// Start the audio processing loop.
    pub fn start(&self) {
        if self.cmd_tx.send(AudioCommand::Start).is_err() {
            log::error!("audio thread channel closed - Start command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Stop the audio processing loop (keeps thread alive).
    pub fn stop(&self) {
        if self.cmd_tx.send(AudioCommand::Stop).is_err() {
            log::error!("audio thread channel closed - Stop command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Swap the active noise suppression engine.
    pub fn set_engine(&self, engine: Box<dyn NoiseEngine>) {
        self.engines_sent.fetch_add(1, Ordering::AcqRel);
        if self.cmd_tx.send(AudioCommand::SetEngine(engine)).is_err() {
            log::error!("audio thread channel closed - SetEngine command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Change the input device.
    pub fn set_input_device(&self, device_id: String) {
        if self
            .cmd_tx
            .send(AudioCommand::SetInputDevice(device_id))
            .is_err()
        {
            log::error!("audio thread channel closed - SetInputDevice command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Set the normalized suppression strength on the active engine.
    pub fn set_strength(&self, strength: f32) {
        if self
            .cmd_tx
            .send(AudioCommand::SetStrength(strength))
            .is_err()
        {
            log::error!("audio thread channel closed - SetStrength command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Set the processing mode on the active engine.
    pub fn set_mode(&self, mode: crate::engine::ProcessingMode) {
        if self.cmd_tx.send(AudioCommand::SetMode(mode)).is_err() {
            log::error!("audio thread channel closed - SetMode command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Enable or disable monitor output.
    pub fn set_monitor(&self, enabled: bool) {
        if self.cmd_tx.send(AudioCommand::SetMonitor(enabled)).is_err() {
            log::error!("audio thread channel closed - SetMonitor command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Enable or disable the input auto-gain (speech-gated, boost-only
    /// leveler for too-quiet mics). Mirrors [`set_monitor`](Self::set_monitor).
    pub fn set_auto_gain(&self, enabled: bool) {
        if self
            .cmd_tx
            .send(AudioCommand::SetAutoGain(enabled))
            .is_err()
        {
            log::error!("audio thread channel closed - SetAutoGain command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Attach or detach the PipeWire ring-buffer writer used for monitor output.
    ///
    /// Call with `Some(writer)` before `set_monitor(true)`, and with `None`
    /// after `set_monitor(false)` so the audio thread stops writing to a
    /// destroyed stream.
    pub fn set_monitor_writer(&self, writer: Option<RingBufWriter>) {
        if self
            .cmd_tx
            .send(AudioCommand::SetMonitorWriter(writer))
            .is_err()
        {
            log::error!("audio thread channel closed - SetMonitorWriter command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Hot-swap the PipeWire ring buffers after a daemon reconnect.
    ///
    /// The audio thread will start reading/writing the new buffers on its next
    /// processing cycle. Pass `None` for either half to fall back to simulation
    /// mode for that direction.
    pub fn set_ring_buffers(
        &self,
        capture_reader: Option<RingBufReader>,
        output_writer: Option<RingBufWriter>,
    ) {
        if self
            .cmd_tx
            .send(AudioCommand::SetRingBuffers {
                capture_reader,
                output_writer,
            })
            .is_err()
        {
            log::error!("audio thread channel closed - SetRingBuffers command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Replace the capture ring-buffer reader without touching the output writer.
    ///
    /// Used after the PipeWire capture stream has been re-created with a new
    /// `PW_KEY_TARGET_OBJECT` (i.e. the user picked a different mic in the GUI).
    /// Pass `None` to fall back to simulation mode for the input path while
    /// keeping the existing output writer intact.
    pub fn replace_capture_reader(&self, capture_reader: Option<RingBufReader>) {
        if self
            .cmd_tx
            .send(AudioCommand::ReplaceCaptureReader(capture_reader))
            .is_err()
        {
            log::error!("audio thread channel closed - ReplaceCaptureReader command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Drain any pending level reports. Returns the most recent one, if any.
    pub fn poll_levels(&self) -> Option<LevelReport> {
        let mut last = None;
        while let Ok(report) = self.level_rx.try_recv() {
            last = Some(report);
        }
        last
    }

    /// The next runtime engine fault reported by the audio thread about the
    /// engine that is still current, if any. Reports about an engine that
    /// has already been replaced by a later [`Self::set_engine`] call are
    /// dropped here (logged), so a caller never "falls back" away from an
    /// engine the user picked after the failing one.
    pub fn poll_engine_fault(&self) -> Option<EngineFaultReport> {
        while let Ok(report) = self.fault_rx.try_recv() {
            let current = self.engines_sent.load(Ordering::Acquire);
            if report.generation == current {
                return Some(report);
            }
            log::info!(
                "Ignoring {:?} for engine #{} (current engine is #{current})",
                report.fault,
                report.generation
            );
        }
        None
    }

    /// Shut down the audio thread and join it.
    pub fn shutdown(mut self) {
        if self.cmd_tx.send(AudioCommand::Shutdown).is_err() {
            log::error!("audio thread channel closed - Shutdown command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        // Send shutdown; if send fails the thread already exited.
        if self.cmd_tx.send(AudioCommand::Shutdown).is_err() {
            log::debug!("audio thread channel closed on drop - already exited");
        }
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn the audio processing thread. Shared by [`AudioPipeline::new`],
/// [`AudioPipeline::with_ring_buffers`] and the test-only
/// `with_ring_buffers_unfiltered` constructor so the spawn body is not
/// duplicated. `condition_input` is always `true` in production; it is only
/// `false` for the index-stamped latency/backlog test harness (see
/// `with_ring_buffers_unfiltered`'s doc comment).
fn spawn_audio_thread(
    cmd_rx: mpsc::Receiver<AudioCommand>,
    level_tx: mpsc::Sender<LevelReport>,
    fault_tx: mpsc::Sender<EngineFaultReport>,
    capture_reader: Option<RingBufReader>,
    output_writer: Option<RingBufWriter>,
    heartbeat: Arc<AtomicU64>,
    condition_input: bool,
) -> Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("cleanmic-audio".into())
        .spawn(move || {
            audio_thread_main(
                cmd_rx,
                level_tx,
                fault_tx,
                capture_reader,
                output_writer,
                heartbeat,
                condition_input,
            );
        })
        .context("failed to spawn audio thread")
}

/// Process a single buffer through the engine (or passthrough).
///
/// This is the hot-path callback: no allocations, no locks, no I/O.
fn process_buffer(engine: &mut Option<Box<dyn NoiseEngine>>, input: &[f32], output: &mut [f32]) {
    match engine {
        Some(eng) => eng.process(input, output),
        None => output.copy_from_slice(input),
    }
}

/// State for a crossfade transition between two engines.
///
/// During a crossfade, both the old and new engines process audio in parallel.
/// The output is a weighted mix: the old engine fades out while the new engine
/// fades in over `CROSSFADE_SAMPLES` total samples (spanning multiple buffers
/// if needed).
struct CrossfadeState {
    /// The outgoing engine (fading out).
    old_engine: Box<dyn NoiseEngine>,
    /// Number of crossfade samples already applied.
    samples_done: usize,
}

/// Apply crossfade mixing between old and new engine outputs.
///
/// Processes `input` through both the old engine (in `xfade`) and the current
/// engine, then blends the results into `output`. Returns `true` when the
/// crossfade is complete.
///
/// Pre-condition: `old_buf` must be at least `output.len()` long (pre-allocated).
fn process_with_crossfade(
    xfade: &mut CrossfadeState,
    new_engine: &mut Option<Box<dyn NoiseEngine>>,
    input: &[f32],
    output: &mut [f32],
    old_buf: &mut [f32],
) -> bool {
    let len = input.len();

    // Process through old engine (fading out).
    xfade.old_engine.process(input, &mut old_buf[..len]);

    // Process through new engine (fading in).
    process_buffer(new_engine, input, output);

    // Apply linear crossfade sample-by-sample.
    for i in 0..len {
        let pos = xfade.samples_done + i;
        if pos < CROSSFADE_SAMPLES {
            let t = pos as f32 / CROSSFADE_SAMPLES as f32;
            output[i] = old_buf[i] * (1.0 - t) + output[i] * t;
        }
        // else: new engine output is already in place (t >= 1.0)
    }

    xfade.samples_done += len;
    xfade.samples_done >= CROSSFADE_SAMPLES
}

/// Main function for the audio processing thread.
///
/// Without real PipeWire, simulates the loop by generating silent buffers at
/// the expected cadence. The command/level channel protocol is identical to
/// what the real PipeWire integration will use.
/// Handle a SetEngine command: initiate a crossfade from the old engine to the
/// new one. If there is no old engine (or not running), switch immediately.
fn handle_set_engine(
    engine: &mut Option<Box<dyn NoiseEngine>>,
    crossfade: &mut Option<CrossfadeState>,
    new_engine: Box<dyn NoiseEngine>,
    running: bool,
) {
    // If a crossfade is already in progress, finish it immediately: teardown
    // the old crossfading engine and discard the in-progress fade.
    if let Some(mut prev_xfade) = crossfade.take() {
        prev_xfade.old_engine.teardown();
    }

    if running {
        if let Some(old) = engine.take() {
            // Start crossfade: old engine fades out, new engine fades in.
            *crossfade = Some(CrossfadeState {
                old_engine: old,
                samples_done: 0,
            });
            *engine = Some(new_engine);
            log::info!("Engine swap started (crossfading)");
        } else {
            // No old engine — just set the new one directly.
            *engine = Some(new_engine);
            log::info!("Engine set (no crossfade needed)");
        }
    } else {
        // Not running — immediate swap, teardown old engine.
        if let Some(mut old) = engine.take() {
            old.teardown();
        }
        *engine = Some(new_engine);
        log::info!("Engine swapped (not running, no crossfade)");
    }
}

/// Process a command received on the audio thread. Returns `true` if the
/// thread should shut down.
#[allow(clippy::too_many_arguments)]
fn handle_command(
    cmd: AudioCommand,
    running: &mut bool,
    engine: &mut Option<Box<dyn NoiseEngine>>,
    crossfade: &mut Option<CrossfadeState>,
    monitor: &mut MonitorOutput,
    input_device: &mut String,
    auto_gain: &mut AutoGain,
    watchdog: &mut EngineWatchdog,
) -> bool {
    match cmd {
        AudioCommand::Start => {
            log::info!("Audio processing started");
            *running = true;
        }
        AudioCommand::Stop => {
            log::info!("Audio processing stopped");
            *running = false;
        }
        AudioCommand::SetEngine(new_engine) => {
            handle_set_engine(engine, crossfade, new_engine, *running);
            watchdog.engine_replaced();
        }
        AudioCommand::SetInputDevice(device) => {
            log::info!("Input device changed to: {}", device);
            *input_device = device;
        }
        AudioCommand::SetStrength(strength) => {
            if let Some(eng) = engine {
                eng.set_strength(strength);
                log::debug!("Engine strength set to {:.2}", strength);
            }
        }
        AudioCommand::SetMode(mode) => {
            if let Some(eng) = engine {
                eng.set_mode(mode);
                log::info!("Engine mode set to {:?}", mode);
            }
        }
        AudioCommand::SetMonitor(enabled) => {
            if enabled {
                if let Err(e) = monitor.enable() {
                    log::error!("Failed to enable monitor: {}", e);
                }
            } else if let Err(e) = monitor.disable() {
                log::error!("Failed to disable monitor: {}", e);
            }
        }
        AudioCommand::SetAutoGain(enabled) => {
            auto_gain.set_enabled(enabled);
            log::info!(
                "Input auto-gain {}",
                if enabled { "enabled" } else { "disabled" }
            );
        }
        AudioCommand::SetMonitorWriter(writer) => {
            if let Some(w) = writer {
                monitor.set_ring_writer(w);
            } else {
                monitor.clear_ring_writer();
            }
        }
        AudioCommand::SetRingBuffers { .. } => {
            // Handled in audio_thread_main before handle_command is called.
            // This arm is unreachable in practice.
            log::warn!("SetRingBuffers reached handle_command — this should not happen");
        }
        AudioCommand::ReplaceCaptureReader(_) => {
            // Handled in audio_thread_main before handle_command is called.
            log::warn!("ReplaceCaptureReader reached handle_command — this should not happen");
        }
        AudioCommand::Shutdown => {
            log::info!("Audio thread shutting down");
            if monitor.is_enabled() {
                let _ = monitor.disable();
            }
            if let Some(mut xfade) = crossfade.take() {
                xfade.old_engine.teardown();
            }
            if let Some(eng) = engine {
                eng.teardown();
            }
            return true;
        }
    }
    false
}

fn audio_thread_main(
    cmd_rx: mpsc::Receiver<AudioCommand>,
    level_tx: mpsc::Sender<LevelReport>,
    fault_tx: mpsc::Sender<EngineFaultReport>,
    capture_reader: Option<RingBufReader>,
    output_writer: Option<RingBufWriter>,
    heartbeat: Arc<AtomicU64>,
    condition_input: bool,
) {
    let mut running = false;
    let mut engine: Option<Box<dyn NoiseEngine>> = None;
    let mut crossfade: Option<CrossfadeState> = None;
    let mut monitor = MonitorOutput::new();
    let mut _input_device = String::new();
    let mut watchdog = EngineWatchdog::new();

    // Exponential moving average for level reporting. Smooths the per-batch
    // RMS so the UI meters don't dance on ambient noise fluctuations.
    // Coefficient of 0.08 at ~50 updates/sec gives ~250ms effective window.
    // Fast enough to track speech, slow enough to hide per-chunk variance.
    const LEVEL_SMOOTH: f32 = 0.08;
    let mut smooth_in_rms: f32 = 0.0;
    let mut smooth_out_rms: f32 = 0.0;

    // Pre-allocated buffers — no allocation on the hot path.
    let mut input_buf = vec![0.0f32; BUFFER_SIZE];
    let mut output_buf = vec![0.0f32; BUFFER_SIZE];
    let mut crossfade_old_buf = vec![0.0f32; BUFFER_SIZE];

    // DC-blocks the real-capture path before the engine and the input level
    // meter (see `INPUT_DC_BLOCK_CUTOFF_HZ`). `condition_input` is only
    // `false` for the index-stamped latency/backlog test harness.
    let mut input_dc_block = DcBlocker::new(INPUT_DC_BLOCK_CUTOFF_HZ, SAMPLE_RATE);

    // Speech-gated, boost-only input auto-gain, run immediately after the DC
    // blocker on the real-capture path (quick task 260923-x24). Starts
    // enabled to match `Config::default().auto_gain_enabled` (ON); the app
    // always sends the persisted value via `SetAutoGain` before `Start`.
    let mut input_auto_gain = AutoGain::new(SAMPLE_RATE);

    let tick_duration = std::time::Duration::from_secs_f64(BUFFER_SIZE as f64 / SAMPLE_RATE as f64);

    // Ring buffer halves. Made mutable so they can be hot-swapped after a
    // PipeWire reconnect via SetRingBuffers.
    let mut capture_reader = capture_reader;
    let mut output_writer = output_writer;

    // Whether we are connected to real PipeWire ring buffers or running in
    // simulation mode (silent input, discarded output). Recomputed on
    // SetRingBuffers.
    let mut has_ring_buffers = capture_reader.is_some() && output_writer.is_some();

    // PipeWire keeps filling the capture ring while we are not running (before
    // the first Start — engine init can take seconds — and after every Stop).
    // Set while idle, consumed by the first processing pass after Start, so
    // that stale audio is discarded instead of being replayed late and turned
    // into permanent output latency.
    let mut capture_is_stale = true;

    loop {
        // Increment heartbeat counter so the health check can detect liveness.
        heartbeat.fetch_add(1, Ordering::Release);

        // Drain all pending commands (non-blocking).
        loop {
            match cmd_rx.try_recv() {
                Ok(AudioCommand::SetRingBuffers {
                    capture_reader: new_cr,
                    output_writer: new_ow,
                }) => {
                    log::info!("Audio thread: ring buffers replaced (PipeWire reconnect)");
                    capture_reader = new_cr;
                    output_writer = new_ow;
                    has_ring_buffers = capture_reader.is_some() && output_writer.is_some();
                    // A reconnected ring can carry a different DC level than
                    // the one just torn down; priming on the next sample
                    // avoids injecting a DC step (a click) into the engine.
                    input_dc_block.reset();
                    // A reconnect does not guarantee the same physical
                    // device; carrying a learned gain across it risks
                    // blasting a hot mic with a quiet mic's boost.
                    input_auto_gain.reset();
                }
                Ok(AudioCommand::ReplaceCaptureReader(new_cr)) => {
                    log::info!("Audio thread: capture reader replaced (device retargeting)");
                    capture_reader = new_cr;
                    has_ring_buffers = capture_reader.is_some() && output_writer.is_some();
                    // A different physical mic can carry a different DC
                    // level; priming on the next sample avoids a DC-step
                    // click into the engine.
                    input_dc_block.reset();
                    // A different physical mic can have a very different
                    // sensitivity; never carry a learned gain onto it.
                    input_auto_gain.reset();
                }
                Ok(cmd) => {
                    if handle_command(
                        cmd,
                        &mut running,
                        &mut engine,
                        &mut crossfade,
                        &mut monitor,
                        &mut _input_device,
                        &mut input_auto_gain,
                        &mut watchdog,
                    ) {
                        return;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    log::info!("Command channel disconnected, shutting down audio thread");
                    let _ = monitor.disable();
                    if let Some(mut xfade) = crossfade.take() {
                        xfade.old_engine.teardown();
                    }
                    if let Some(ref mut eng) = engine {
                        eng.teardown();
                    }
                    return;
                }
            }
        }

        if running {
            if has_ring_buffers {
                // ----- Real PipeWire mode -----
                // Process all available data in a tight loop before yielding.
                // PipeWire delivers audio in quanta (typically 1024 samples) but
                // we process in BUFFER_SIZE (480) chunks. Processing all available
                // data at once prevents gaps in the output ring buffer that cause
                // pulsating audio when PipeWire reads between our iterations.
                // The pass is bounded (MAX_BLOCKS_PER_PASS) and the backlog is
                // capped (CAPTURE_MAX_BACKLOG) so a slow engine can neither grow
                // latency without limit nor starve the command loop.
                let reader = capture_reader.as_ref().unwrap();
                let mut processed_any = false;
                let mut in_sum_sq = 0.0f32;
                let mut out_sum_sq = 0.0f32;
                let mut total_samples = 0usize;

                if capture_is_stale {
                    capture_is_stale = false;
                    let dropped = reader.discard_all();
                    if dropped > 0 {
                        log::info!(
                            "Discarded {:.0} ms of capture audio queued while processing was stopped",
                            samples_to_ms(dropped)
                        );
                    }
                    // The stream just (re)started: a fresh mic connection or
                    // a resumed one after a stop can have a different DC
                    // level. Priming on the next sample avoids injecting a
                    // DC step (a click) into the engine.
                    input_dc_block.reset();
                    // Deliberately NOT resetting `input_auto_gain` here: a
                    // Stop/Start (Activer off/on) is the same mic in the
                    // same room, so the DC offset genuinely can jump but the
                    // learned speech level does not. Re-learning it here
                    // would make the user sound quiet again for several
                    // seconds every time they re-enable CleanMic.
                }

                let mut blocks = 0usize;
                while blocks < MAX_BLOCKS_PER_PASS && reader.available() >= BUFFER_SIZE {
                    blocks += 1;
                    let backlog = reader.available();
                    if backlog > CAPTURE_MAX_BACKLOG {
                        let dropped = reader.discard(backlog - CAPTURE_KEEP_AFTER_TRIM);
                        log::warn!(
                            "Audio thread fell {:.0} ms behind (engine slower than real time?) — dropped the oldest {:.0} ms to keep latency bounded",
                            samples_to_ms(backlog),
                            samples_to_ms(dropped)
                        );
                        send_fault(&fault_tx, watchdog.fell_behind(Instant::now()));
                    }
                    let read = reader.read(&mut input_buf);
                    for s in &mut input_buf[read..] {
                        *s = 0.0;
                    }

                    // DC-block, then apply the speech-gated input auto-gain,
                    // before the finalized block reaches the engine
                    // (passthrough, crossfade, or panic fallback all read
                    // `input_buf` below) and before the input-level
                    // accumulation, so every path and the meter share the
                    // same conditioned samples.
                    if condition_input {
                        input_dc_block.process_in_place(&mut input_buf);
                        input_auto_gain.process_in_place(&mut input_buf);
                    }

                    let process_result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            if let Some(ref mut xfade) = crossfade {
                                let done = process_with_crossfade(
                                    xfade,
                                    &mut engine,
                                    &input_buf,
                                    &mut output_buf,
                                    &mut crossfade_old_buf,
                                );
                                if done && let Some(mut finished) = crossfade.take() {
                                    finished.old_engine.teardown();
                                    log::info!("Engine crossfade complete, old engine torn down");
                                }
                            } else {
                                process_buffer(&mut engine, &input_buf, &mut output_buf);
                            }
                        }));

                    if process_result.is_err() {
                        log::error!(
                            "Audio engine panicked during processing — dropping engine and falling back to passthrough \
                             until the app installs a replacement."
                        );
                        engine = None;
                        crossfade = None;
                        output_buf.copy_from_slice(&input_buf);
                        send_fault(&fault_tx, watchdog.panicked());
                    } else if let Some(ref eng) = engine {
                        send_fault(&fault_tx, watchdog.check_health(eng.health()));
                    }

                    if let Some(ref writer) = output_writer {
                        writer.write(&output_buf);
                    }

                    // Accumulate squared samples for batch-wide RMS.
                    for &s in input_buf.iter() {
                        in_sum_sq += s * s;
                    }
                    for &s in output_buf.iter() {
                        out_sum_sq += s * s;
                    }
                    total_samples += BUFFER_SIZE;

                    monitor.write(&output_buf);
                    processed_any = true;
                }

                if processed_any {
                    // Compute batch RMS, then apply exponential smoothing.
                    let n = total_samples as f32;
                    let raw_in = (in_sum_sq / n).sqrt();
                    let raw_out = (out_sum_sq / n).sqrt();
                    smooth_in_rms += LEVEL_SMOOTH * (raw_in - smooth_in_rms);
                    smooth_out_rms += LEVEL_SMOOTH * (raw_out - smooth_out_rms);
                    let report = LevelReport {
                        input_rms: smooth_in_rms,
                        output_rms: smooth_out_rms,
                    };
                    if level_tx.send(report).is_err() {
                        log::debug!("level report channel closed - UI may have shut down");
                    }
                } else {
                    // No data available — yield briefly and retry.
                    std::thread::sleep(std::time::Duration::from_micros(500));
                    continue;
                }
            } else {
                // Simulation mode — process silent input_buf once.
                let process_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if let Some(ref mut xfade) = crossfade {
                        let done = process_with_crossfade(
                            xfade,
                            &mut engine,
                            &input_buf,
                            &mut output_buf,
                            &mut crossfade_old_buf,
                        );
                        if done && let Some(mut finished) = crossfade.take() {
                            finished.old_engine.teardown();
                            log::info!("Engine crossfade complete, old engine torn down");
                        }
                    } else {
                        process_buffer(&mut engine, &input_buf, &mut output_buf);
                    }
                }));

                if process_result.is_err() {
                    engine = None;
                    crossfade = None;
                    output_buf.copy_from_slice(&input_buf);
                    send_fault(&fault_tx, watchdog.panicked());
                } else if let Some(ref eng) = engine {
                    send_fault(&fault_tx, watchdog.check_health(eng.health()));
                }

                if let Some(ref writer) = output_writer {
                    writer.write(&output_buf);
                }

                let report = LevelReport {
                    input_rms: rms(&input_buf),
                    output_rms: rms(&output_buf),
                };
                if level_tx.send(report).is_err() {
                    log::debug!("level report channel closed - UI may have shut down");
                }

                monitor.write(&output_buf);

                // Simulation mode: sleep to approximate real-time cadence.
                std::thread::sleep(tick_duration);
            }
        } else {
            capture_is_stale = true;
            // When not running, block briefly to avoid busy-waiting.
            match cmd_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(AudioCommand::SetRingBuffers {
                    capture_reader: new_cr,
                    output_writer: new_ow,
                }) => {
                    log::info!("Audio thread: ring buffers replaced (PipeWire reconnect, idle)");
                    capture_reader = new_cr;
                    output_writer = new_ow;
                    has_ring_buffers = capture_reader.is_some() && output_writer.is_some();
                    // See the busy-loop arm above: a new ring can carry a
                    // different DC level, so re-arm priming.
                    input_dc_block.reset();
                    // See the busy-loop arm above: a reconnect does not
                    // guarantee the same device, so never carry a learned
                    // gain across it.
                    input_auto_gain.reset();
                }
                Ok(AudioCommand::ReplaceCaptureReader(new_cr)) => {
                    log::info!("Audio thread: capture reader replaced (device retargeting, idle)");
                    capture_reader = new_cr;
                    has_ring_buffers = capture_reader.is_some() && output_writer.is_some();
                    // See the busy-loop arm above: a different physical mic
                    // can carry a different DC level, so re-arm priming.
                    input_dc_block.reset();
                    // See the busy-loop arm above: a different physical mic
                    // can have a very different sensitivity.
                    input_auto_gain.reset();
                }
                Ok(cmd) => {
                    if handle_command(
                        cmd,
                        &mut running,
                        &mut engine,
                        &mut crossfade,
                        &mut monitor,
                        &mut _input_device,
                        &mut input_auto_gain,
                        &mut watchdog,
                    ) {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    log::info!("Command channel disconnected, shutting down audio thread");
                    let _ = monitor.disable();
                    if let Some(mut xfade) = crossfade.take() {
                        xfade.old_engine.teardown();
                    }
                    if let Some(ref mut eng) = engine {
                        eng.teardown();
                    }
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{NoiseEngine, ProcessingMode};

    /// A trivial passthrough engine for testing.
    struct PassthroughEngine {
        initialized: bool,
    }

    impl PassthroughEngine {
        fn new() -> Self {
            Self { initialized: false }
        }
    }

    impl NoiseEngine for PassthroughEngine {
        fn init(&mut self, _sample_rate: u32) -> anyhow::Result<()> {
            self.initialized = true;
            Ok(())
        }

        fn process(&mut self, input: &[f32], output: &mut [f32]) {
            output.copy_from_slice(input);
        }

        fn set_strength(&mut self, _strength: f32) {}
        fn set_mode(&mut self, _mode: ProcessingMode) {}

        fn latency_frames(&self) -> u32 {
            0
        }

        fn teardown(&mut self) {
            self.initialized = false;
        }
    }

    #[test]
    fn pipeline_create_and_drop_without_panic() {
        let pipeline = AudioPipeline::new().unwrap();
        drop(pipeline);
    }

    #[test]
    fn pipeline_start_and_stop_cycle() {
        let pipeline = AudioPipeline::new().unwrap();
        pipeline.start();
        std::thread::sleep(std::time::Duration::from_millis(30));
        pipeline.stop();
        std::thread::sleep(std::time::Duration::from_millis(30));
        // Second cycle.
        pipeline.start();
        std::thread::sleep(std::time::Duration::from_millis(30));
        pipeline.stop();
        drop(pipeline);
    }

    #[test]
    fn passthrough_copies_input_to_output() {
        let input = [0.5f32; BUFFER_SIZE];
        let mut output = [0.0f32; BUFFER_SIZE];
        let mut engine: Option<Box<dyn NoiseEngine>> = None;

        // Without engine: passthrough.
        process_buffer(&mut engine, &input, &mut output);
        assert_eq!(input.as_slice(), output.as_slice());
    }

    #[test]
    fn engine_passthrough_copies_input_to_output() {
        let input = [0.25f32; BUFFER_SIZE];
        let mut output = [0.0f32; BUFFER_SIZE];
        let mut eng = PassthroughEngine::new();
        eng.init(SAMPLE_RATE).unwrap();
        let mut engine: Option<Box<dyn NoiseEngine>> = Some(Box::new(eng));

        process_buffer(&mut engine, &input, &mut output);
        assert_eq!(input.as_slice(), output.as_slice());
    }

    #[test]
    fn rms_of_constant_signal_is_zero_dbfs() {
        // A constant signal of 1.0 has RMS = 1.0, which is 0 dBFS.
        let samples = [1.0f32; 480];
        let level = rms(&samples);
        assert!(
            (level - 1.0).abs() < 1e-6,
            "RMS of 1.0 constant should be 1.0, got {level}"
        );
    }

    #[test]
    fn rms_of_silence_is_zero() {
        let samples = [0.0f32; 480];
        let level = rms(&samples);
        assert!(level < 1e-10, "RMS of silence should be ~0, got {level}");
    }

    #[test]
    fn rms_of_half_amplitude() {
        let samples = [0.5f32; 480];
        let level = rms(&samples);
        assert!(
            (level - 0.5).abs() < 1e-6,
            "RMS of 0.5 constant should be 0.5, got {level}"
        );
    }

    #[test]
    fn commands_sent_via_channel_are_received() {
        let (tx, rx) = mpsc::channel::<AudioCommand>();

        tx.send(AudioCommand::Start).unwrap();
        tx.send(AudioCommand::Stop).unwrap();
        tx.send(AudioCommand::SetMonitor(true)).unwrap();
        tx.send(AudioCommand::SetInputDevice("test-mic".into()))
            .unwrap();
        tx.send(AudioCommand::Shutdown).unwrap();

        // Verify all commands arrive in order.
        assert!(matches!(rx.recv().unwrap(), AudioCommand::Start));
        assert!(matches!(rx.recv().unwrap(), AudioCommand::Stop));
        assert!(matches!(rx.recv().unwrap(), AudioCommand::SetMonitor(true)));
        assert!(matches!(
            rx.recv().unwrap(),
            AudioCommand::SetInputDevice(_)
        ));
        assert!(matches!(rx.recv().unwrap(), AudioCommand::Shutdown));
    }

    #[test]
    fn pipeline_reports_levels_when_running() {
        let pipeline = AudioPipeline::new().unwrap();
        pipeline.start();
        // Give the thread time to produce at least one level report.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let level = pipeline.poll_levels();
        assert!(
            level.is_some(),
            "Should have received at least one level report"
        );
        // Simulated input is silence, so RMS should be ~0.
        let report = level.unwrap();
        assert!(report.input_rms < 1e-6);
        assert!(report.output_rms < 1e-6);
        drop(pipeline);
    }

    #[test]
    fn rms_of_empty_buffer_is_zero() {
        let samples: [f32; 0] = [];
        let level = rms(&samples);
        assert!(level.abs() < 1e-10);
    }

    // -- Monitor output tests --

    /// An engine that halves the input signal, so we can distinguish processed
    /// audio from raw input in monitor tests.
    struct HalvingEngine;

    impl NoiseEngine for HalvingEngine {
        fn init(&mut self, _sample_rate: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn process(&mut self, input: &[f32], output: &mut [f32]) {
            for (o, &i) in output.iter_mut().zip(input.iter()) {
                *o = i * 0.5;
            }
        }
        fn set_strength(&mut self, _strength: f32) {}
        fn set_mode(&mut self, _mode: ProcessingMode) {}
        fn latency_frames(&self) -> u32 {
            0
        }
        fn teardown(&mut self) {}
    }

    #[test]
    fn monitor_enable_disable_toggles_state() {
        let mut monitor = MonitorOutput::new();
        assert!(!monitor.is_enabled());

        monitor.enable().unwrap();
        assert!(monitor.is_enabled());

        monitor.disable().unwrap();
        assert!(!monitor.is_enabled());
    }

    #[test]
    fn monitor_toggle_does_not_interrupt_main_output() {
        let pipeline = AudioPipeline::new().unwrap();
        pipeline.start();
        // Let a few buffers process.
        std::thread::sleep(std::time::Duration::from_millis(30));

        // Toggle monitor on.
        pipeline.set_monitor(true);
        std::thread::sleep(std::time::Duration::from_millis(30));

        // Main pipeline should still be producing level reports (not stalled).
        let level = pipeline.poll_levels();
        assert!(
            level.is_some(),
            "Pipeline must keep producing levels after monitor toggle"
        );

        // Toggle monitor off.
        pipeline.set_monitor(false);
        std::thread::sleep(std::time::Duration::from_millis(30));

        let level = pipeline.poll_levels();
        assert!(
            level.is_some(),
            "Pipeline must keep producing levels after monitor disable"
        );

        drop(pipeline);
    }

    #[test]
    fn monitor_state_survives_engine_hotswap() {
        // Use handle_command directly to verify monitor state is not reset
        // when the engine is swapped.
        let mut running = true;
        let mut engine: Option<Box<dyn NoiseEngine>> = Some(Box::new(PassthroughEngine::new()));
        let mut crossfade: Option<CrossfadeState> = None;
        let mut monitor = MonitorOutput::new();
        let mut input_device = String::new();
        let mut test_auto_gain = AutoGain::new(SAMPLE_RATE);
        let mut test_watchdog = EngineWatchdog::new();

        // Enable monitor.
        handle_command(
            AudioCommand::SetMonitor(true),
            &mut running,
            &mut engine,
            &mut crossfade,
            &mut monitor,
            &mut input_device,
            &mut test_auto_gain,
            &mut test_watchdog,
        );
        assert!(monitor.is_enabled());

        // Swap engine.
        handle_command(
            AudioCommand::SetEngine(Box::new(HalvingEngine)),
            &mut running,
            &mut engine,
            &mut crossfade,
            &mut monitor,
            &mut input_device,
            &mut test_auto_gain,
            &mut test_watchdog,
        );

        // Monitor must still be enabled after engine swap.
        assert!(
            monitor.is_enabled(),
            "Monitor state must survive engine hot-swap"
        );
    }

    #[test]
    fn monitor_graceful_when_no_playback_device() {
        // In stub mode, enable/disable always succeed (no real device needed).
        // This verifies the graceful path: no panic, no error.
        let mut monitor = MonitorOutput::new();
        assert!(monitor.enable().is_ok());
        assert!(monitor.is_enabled());
        assert!(monitor.disable().is_ok());
        assert!(!monitor.is_enabled());
    }

    #[test]
    fn monitor_output_contains_processed_audio_not_raw() {
        // Use the HalvingEngine so processed output differs from raw input.
        let mut engine: Option<Box<dyn NoiseEngine>> = Some(Box::new(HalvingEngine));
        let mut monitor = MonitorOutput::new();
        monitor.enable().unwrap();

        let input = [0.8f32; BUFFER_SIZE];
        let mut output = [0.0f32; BUFFER_SIZE];

        // Process through engine.
        process_buffer(&mut engine, &input, &mut output);

        // Write processed output to monitor (same as audio_thread_main does).
        monitor.write(&output);

        let monitor_buf = monitor.last_buffer();

        // Monitor should contain the processed (halved) signal, not the raw input.
        for (i, &sample) in monitor_buf.iter().enumerate() {
            assert!(
                (sample - 0.4).abs() < 1e-6,
                "Monitor sample[{i}] = {sample}, expected 0.4 (processed), not 0.8 (raw)"
            );
        }
    }

    // --- Engine hot-swap tests (Task 7) ---

    /// An engine that scales input by a constant factor (for verifying crossfade).
    struct ScalingEngine {
        factor: f32,
        teardown_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ScalingEngine {
        fn new(
            factor: f32,
            teardown_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            Self {
                factor,
                teardown_count,
            }
        }
    }

    impl NoiseEngine for ScalingEngine {
        fn init(&mut self, _sample_rate: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn process(&mut self, input: &[f32], output: &mut [f32]) {
            for (o, &i) in output.iter_mut().zip(input.iter()) {
                *o = i * self.factor;
            }
        }
        fn set_strength(&mut self, _strength: f32) {}
        fn set_mode(&mut self, _mode: ProcessingMode) {}
        fn latency_frames(&self) -> u32 {
            0
        }
        fn teardown(&mut self) {
            self.teardown_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[cfg(feature = "deepfilter")]
    #[test]
    #[ignore] // Parallel LADSPA init is not thread-safe; run with --ignored
    fn switch_rnnoise_to_deepfilter_no_panic() {
        use crate::engine::{
            deepfilter::{self, DeepFilterEngine},
            rnnoise::RNNoiseEngine,
        };
        if !deepfilter::is_available() {
            return;
        }

        let pipeline = AudioPipeline::new().unwrap();
        pipeline.start();

        let mut rnn = RNNoiseEngine::new();
        rnn.init(48000).unwrap();
        pipeline.set_engine(Box::new(rnn));
        std::thread::sleep(std::time::Duration::from_millis(30));

        let mut df = DeepFilterEngine::new();
        df.init(48000).unwrap();
        pipeline.set_engine(Box::new(df));
        std::thread::sleep(std::time::Duration::from_millis(30));

        pipeline.shutdown();
    }

    #[cfg(feature = "deepfilter")]
    #[test]
    #[ignore] // Parallel LADSPA init is not thread-safe; run with --ignored
    fn switch_deepfilter_to_rnnoise_no_panic() {
        use crate::engine::{
            deepfilter::{self, DeepFilterEngine},
            rnnoise::RNNoiseEngine,
        };
        if !deepfilter::is_available() {
            return;
        }

        let pipeline = AudioPipeline::new().unwrap();
        pipeline.start();

        let mut df = DeepFilterEngine::new();
        df.init(48000).unwrap();
        pipeline.set_engine(Box::new(df));
        std::thread::sleep(std::time::Duration::from_millis(30));

        let mut rnn = RNNoiseEngine::new();
        rnn.init(48000).unwrap();
        pipeline.set_engine(Box::new(rnn));
        std::thread::sleep(std::time::Duration::from_millis(30));

        pipeline.shutdown();
    }

    #[cfg(feature = "deepfilter")]
    #[test]
    #[ignore] // Parallel LADSPA init is not thread-safe; run with --ignored
    fn rapid_switches_no_crash_or_deadlock() {
        use crate::engine::{
            deepfilter::{self, DeepFilterEngine},
            rnnoise::RNNoiseEngine,
        };
        if !deepfilter::is_available() {
            return;
        }

        let pipeline = AudioPipeline::new().unwrap();
        pipeline.start();
        std::thread::sleep(std::time::Duration::from_millis(10));

        for i in 0..10 {
            if i % 2 == 0 {
                let mut rnn = RNNoiseEngine::new();
                rnn.init(48000).unwrap();
                pipeline.set_engine(Box::new(rnn));
            } else {
                let mut df = DeepFilterEngine::new();
                df.init(48000).unwrap();
                pipeline.set_engine(Box::new(df));
            }
        }

        // Allow time for the audio thread to process all switches.
        std::thread::sleep(std::time::Duration::from_millis(100));
        pipeline.shutdown();
    }

    #[test]
    fn audio_output_continuity_across_switch() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        // Verify that during a crossfade, the output buffer contains non-zero
        // values (no silence gap) when input is non-zero.
        let tc_old = Arc::new(AtomicUsize::new(0));
        let tc_new = Arc::new(AtomicUsize::new(0));

        let old_engine = ScalingEngine::new(1.0, tc_old);
        let new_engine = ScalingEngine::new(0.5, tc_new);

        let mut xfade = CrossfadeState {
            old_engine: Box::new(old_engine),
            samples_done: 0,
        };
        let mut engine: Option<Box<dyn NoiseEngine>> = Some(Box::new(new_engine));

        let input = [1.0f32; BUFFER_SIZE];
        let mut output = [0.0f32; BUFFER_SIZE];
        let mut old_buf = [0.0f32; BUFFER_SIZE];

        let done =
            process_with_crossfade(&mut xfade, &mut engine, &input, &mut output, &mut old_buf);
        assert!(done, "Crossfade should complete in exactly one buffer");

        // Every sample should be between 0.5 and 1.0 (blend from old=1.0 to new=0.5).
        for (i, &s) in output.iter().enumerate() {
            assert!(
                s >= 0.49 && s <= 1.01,
                "Sample {i} = {s}, expected between 0.5 and 1.0"
            );
        }

        // First sample ~1.0 (old engine), last ~0.5 (new engine).
        assert!(
            (output[0] - 1.0).abs() < 0.01,
            "First sample should be ~1.0, got {}",
            output[0]
        );
        assert!(
            (output[BUFFER_SIZE - 1] - 0.5).abs() < 0.02,
            "Last sample should be ~0.5, got {}",
            output[BUFFER_SIZE - 1]
        );
    }

    #[test]
    fn old_engine_teardown_called_after_crossfade() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tc = Arc::new(AtomicUsize::new(0));
        let old_engine = ScalingEngine::new(1.0, tc.clone());
        let new_engine = ScalingEngine::new(0.5, Arc::new(AtomicUsize::new(0)));

        let mut xfade = CrossfadeState {
            old_engine: Box::new(old_engine),
            samples_done: 0,
        };
        let mut engine: Option<Box<dyn NoiseEngine>> = Some(Box::new(new_engine));

        let input = [1.0f32; BUFFER_SIZE];
        let mut output = [0.0f32; BUFFER_SIZE];
        let mut old_buf = [0.0f32; BUFFER_SIZE];

        let done =
            process_with_crossfade(&mut xfade, &mut engine, &input, &mut output, &mut old_buf);
        assert!(done);

        assert_eq!(
            tc.load(Ordering::Relaxed),
            0,
            "Teardown should not be called yet"
        );
        xfade.old_engine.teardown();
        assert_eq!(
            tc.load(Ordering::Relaxed),
            1,
            "Teardown should be called once"
        );
    }

    #[test]
    fn switch_to_khip_when_unavailable_handled_gracefully() {
        use crate::engine::khip::KhipEngine;
        use crate::engine::{EngineType, create_engine};

        if KhipEngine::is_available() {
            return; // Library is installed; skip the "unavailable" path.
        }

        let result = create_engine(EngineType::Khip);
        assert!(
            result.is_err(),
            "Creating Khip engine should fail when library is not installed"
        );

        // Pipeline should continue working after a failed Khip creation.
        let pipeline = AudioPipeline::new().unwrap();
        pipeline.start();
        std::thread::sleep(std::time::Duration::from_millis(30));

        let rnn = create_engine(EngineType::RNNoise).unwrap();
        pipeline.set_engine(rnn);
        std::thread::sleep(std::time::Duration::from_millis(30));

        let level = pipeline.poll_levels();
        assert!(level.is_some(), "Pipeline should still report levels");

        pipeline.shutdown();
    }

    #[test]
    fn create_engine_factory_rnnoise() {
        use crate::engine::{EngineType, create_engine};
        let engine = create_engine(EngineType::RNNoise);
        assert!(engine.is_ok(), "Should create RNNoise engine successfully");
    }

    #[cfg(feature = "deepfilter")]
    #[test]
    #[ignore] // Parallel LADSPA init is not thread-safe; run with --ignored
    fn create_engine_factory_deepfilter() {
        use crate::engine::{EngineType, create_engine, deepfilter};
        if !deepfilter::is_available() {
            return;
        }
        let engine = create_engine(EngineType::DeepFilterNet);
        assert!(
            engine.is_ok(),
            "Should create DeepFilterNet engine successfully"
        );
    }

    #[cfg(not(feature = "deepfilter"))]
    #[test]
    fn create_engine_factory_deepfilter_fails_without_feature() {
        use crate::engine::{EngineType, create_engine};
        let engine = create_engine(EngineType::DeepFilterNet);
        assert!(
            engine.is_err(),
            "DeepFilterNet should fail without the deepfilter feature"
        );
    }

    #[test]
    fn crossfade_spans_multiple_buffers() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let old_engine = ScalingEngine::new(1.0, Arc::new(AtomicUsize::new(0)));
        let new_engine = ScalingEngine::new(0.0, Arc::new(AtomicUsize::new(0)));

        let mut xfade = CrossfadeState {
            old_engine: Box::new(old_engine),
            samples_done: 0,
        };
        let mut engine: Option<Box<dyn NoiseEngine>> = Some(Box::new(new_engine));

        // Use small buffers (48 samples = 1ms).
        let small_size = 48;
        let input = vec![1.0f32; small_size];
        let mut output = vec![0.0f32; small_size];
        let mut old_buf = vec![0.0f32; small_size];

        let mut iterations = 0;
        let mut done = false;
        while !done {
            done =
                process_with_crossfade(&mut xfade, &mut engine, &input, &mut output, &mut old_buf);
            iterations += 1;

            for &s in &output {
                assert!(
                    s >= -0.01 && s <= 1.01,
                    "Sample out of range during crossfade: {s}"
                );
            }
        }

        // 480 / 48 = 10 iterations to complete crossfade.
        assert_eq!(
            iterations, 10,
            "Crossfade should take 10 iterations of 48 samples"
        );
    }

    /// Verify that the audio thread survives a panicking engine and falls back
    /// to passthrough, continuing to produce level reports.
    #[test]
    fn audio_thread_recovers_after_engine_panic() {
        use crate::engine::NoiseEngine;
        use crate::engine::ProcessingMode;

        /// An engine that panics on the first process() call.
        struct PanickingEngine;
        impl NoiseEngine for PanickingEngine {
            fn init(&mut self, _: u32) -> anyhow::Result<()> {
                Ok(())
            }
            fn process(&mut self, _: &[f32], _: &mut [f32]) {
                panic!("intentional test panic");
            }
            fn set_strength(&mut self, _: f32) {}
            fn set_mode(&mut self, _: ProcessingMode) {}
            fn latency_frames(&self) -> u32 {
                0
            }
            fn teardown(&mut self) {}
        }

        let pipeline = AudioPipeline::new().unwrap();
        pipeline.start();

        // Give the thread a moment to start.
        std::thread::sleep(std::time::Duration::from_millis(20));

        // Install the panicking engine.
        let engine: Box<dyn NoiseEngine> = Box::new(PanickingEngine);
        pipeline.set_engine(engine);

        // Wait long enough for the engine to process at least one frame.
        std::thread::sleep(std::time::Duration::from_millis(60));

        // The pipeline should still be alive (not crashed).
        // poll_levels returns Some if the level channel is still open.
        // We check thread is still alive by sending a benign command.
        pipeline.stop();
        std::thread::sleep(std::time::Duration::from_millis(20));
        pipeline.start();
        std::thread::sleep(std::time::Duration::from_millis(40));

        let levels = pipeline.poll_levels();
        assert!(
            levels.is_some(),
            "Pipeline should still report levels after engine panic recovery"
        );

        pipeline.shutdown();
    }

    // --- Latency / backlog-bound harness ---------------------------------
    //
    // Regression coverage for "latency grows after engine swap" (debug
    // session latency-grows-after-engine-swap): a fake PipeWire thread plays
    // both RT callbacks on a real 1024-sample (21.3 ms) schedule around the
    // REAL audio thread. Every capture sample is stamped with its 1-based
    // index, so each output read reveals exactly how many samples old the
    // audio is. The output side goes through the same `BacklogLimiter` the
    // live output/monitor callbacks use.

    use crate::pipewire::ringbuf::{BacklogLimiter, ring_buffer};
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    const PW_QUANTUM: usize = 1024;

    struct FakePipeWire {
        stop: Arc<AtomicBool>,
        /// Latest measured capture->output latency, in samples.
        latency: Arc<AtomicUsize>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl FakePipeWire {
        fn start(capture_writer: RingBufWriter, output_reader: RingBufReader) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let latency = Arc::new(AtomicUsize::new(usize::MAX));
            let (stop_t, latency_t) = (stop.clone(), latency.clone());
            let handle = thread::spawn(move || {
                let period = Duration::from_secs_f64(PW_QUANTUM as f64 / f64::from(SAMPLE_RATE));
                let mut limiter = BacklogLimiter::new();
                let mut cap = vec![0.0f32; PW_QUANTUM];
                let mut out = vec![0.0f32; PW_QUANTUM];
                let mut produced = 0usize;
                let mut next = Instant::now();
                while !stop_t.load(Ordering::Acquire) {
                    // Capture callback: the mic delivers one quantum.
                    for (i, s) in cap.iter_mut().enumerate() {
                        *s = (produced + i + 1) as f32;
                    }
                    capture_writer.write(&cap);
                    produced += PW_QUANTUM;
                    // Output callback (same driver clock: no drift).
                    let n = limiter.read_padded(&output_reader, &mut out);
                    if n > 0 {
                        let newest = out[n - 1] as usize;
                        latency_t.store(produced - newest, Ordering::Release);
                    }
                    next += period;
                    if let Some(d) = next.checked_duration_since(Instant::now()) {
                        thread::sleep(d);
                    }
                }
            });
            Self {
                stop,
                latency,
                handle: Some(handle),
            }
        }

        /// Median of `n` latency readings taken 20 ms apart, in ms.
        fn latency_ms(&self, n: usize) -> f64 {
            let mut v: Vec<usize> = (0..n)
                .map(|_| {
                    thread::sleep(Duration::from_millis(20));
                    self.latency.load(Ordering::Acquire)
                })
                .collect();
            v.sort_unstable();
            v[n / 2] as f64 * 1000.0 / f64::from(SAMPLE_RATE)
        }
    }

    impl Drop for FakePipeWire {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// Passthrough engine that sleeps on its first `process()` call, like a
    /// freshly swapped-in model's slow first inference / old-engine teardown.
    struct StallOnceEngine {
        stall: Option<Duration>,
    }

    impl NoiseEngine for StallOnceEngine {
        fn init(&mut self, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn process(&mut self, input: &[f32], output: &mut [f32]) {
            if let Some(d) = self.stall.take() {
                thread::sleep(d);
            }
            output.copy_from_slice(input);
        }
        fn set_strength(&mut self, _: f32) {}
        fn set_mode(&mut self, _: ProcessingMode) {}
        fn latency_frames(&self) -> u32 {
            0
        }
        fn teardown(&mut self) {}
    }

    /// Passthrough engine that takes `per_block` per 10 ms block while
    /// `slow` is set (RTF > 1), like DeepFilterNet on a weak CPU.
    struct SlowEngine {
        per_block: Duration,
        slow: Arc<AtomicBool>,
    }

    impl NoiseEngine for SlowEngine {
        fn init(&mut self, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn process(&mut self, input: &[f32], output: &mut [f32]) {
            if self.slow.load(Ordering::Acquire) {
                thread::sleep(self.per_block);
            }
            output.copy_from_slice(input);
        }
        fn set_strength(&mut self, _: f32) {}
        fn set_mode(&mut self, _: ProcessingMode) {}
        fn latency_frames(&self) -> u32 {
            0
        }
        fn teardown(&mut self) {}
    }

    /// Startup: PipeWire streams capture before the first Start (engine init
    /// can take seconds; 180 ms on the owner's fresh launch, 1.8 s with
    /// DeepFilterNet). That queued audio must be discarded at Start. The gap
    /// is below CAPTURE_MAX_BACKLOG, so only the Start flush can catch it.
    #[test]
    fn first_start_discards_capture_queued_during_startup() {
        let (cw, cr) = ring_buffer(65_536);
        let (ow, or_) = ring_buffer(65_536);
        // Index stamps are a ramp, which the DC blocker correctly removes;
        // the latency measurement is independent of input conditioning.
        let pipeline = AudioPipeline::with_ring_buffers_unfiltered(cr, ow).unwrap();
        let pw = FakePipeWire::start(cw, or_);
        thread::sleep(Duration::from_millis(150)); // streaming, not started
        pipeline.start();
        thread::sleep(Duration::from_millis(150));
        let after = pw.latency_ms(5);
        drop(pw);
        pipeline.shutdown();
        assert!(
            after < 60.0,
            "latency {after:.1} ms after a 150 ms startup gap: capture queued before Start was replayed"
        );
    }

    /// Activer off briefly, then on: the audio queued while stopped must be
    /// discarded, not played out late. The gap is below CAPTURE_MAX_BACKLOG,
    /// so only the Start flush can catch it (measured before the output
    /// BacklogLimiter's 1 s window could shed it).
    #[test]
    fn restart_after_short_stop_does_not_replay_stale_capture() {
        let (cw, cr) = ring_buffer(65_536);
        let (ow, or_) = ring_buffer(65_536);
        // Index stamps are a ramp, which the DC blocker correctly removes;
        // the latency measurement is independent of input conditioning.
        let pipeline = AudioPipeline::with_ring_buffers_unfiltered(cr, ow).unwrap();
        let pw = FakePipeWire::start(cw, or_);
        pipeline.start();
        thread::sleep(Duration::from_millis(300));
        let baseline = pw.latency_ms(5);
        pipeline.stop();
        thread::sleep(Duration::from_millis(150));
        pipeline.start();
        thread::sleep(Duration::from_millis(150));
        let after = pw.latency_ms(5);
        drop(pw);
        pipeline.shutdown();
        assert!(
            after < baseline + 50.0,
            "latency after 150 ms Stop/Start = {after:.1} ms (baseline {baseline:.1} ms): \
             stale capture backlog was replayed into the output"
        );
    }

    /// Activer off for longer than the capture ring, then on (the owner's
    /// 2.85 s toggle against a 1.37 s ring): must not replay stale audio.
    #[test]
    fn restart_after_long_stop_does_not_replay_stale_capture() {
        // 16384-slot ring (341 ms) keeps the test short; production is 65536.
        let (cw, cr) = ring_buffer(16_384);
        let (ow, or_) = ring_buffer(16_384);
        // Index stamps are a ramp, which the DC blocker correctly removes;
        // the latency measurement is independent of input conditioning.
        let pipeline = AudioPipeline::with_ring_buffers_unfiltered(cr, ow).unwrap();
        let pw = FakePipeWire::start(cw, or_);
        pipeline.start();
        thread::sleep(Duration::from_millis(300));
        let baseline = pw.latency_ms(5);
        assert!(
            baseline < 60.0,
            "harness baseline too high: {baseline:.1} ms"
        );

        pipeline.stop();
        thread::sleep(Duration::from_millis(600)); // capture ring fills up
        pipeline.start();
        // Measure well inside the first second: this must be the Start
        // flush, not slow backlog shedding.
        thread::sleep(Duration::from_millis(150));
        let after = pw.latency_ms(5);
        drop(pw);
        pipeline.shutdown();
        assert!(
            after < baseline + 50.0,
            "latency after Stop/Start = {after:.1} ms (baseline {baseline:.1} ms): \
             stale capture backlog was replayed into the output"
        );
    }

    /// One long audio-thread stall (engine swap first-inference/teardown):
    /// the backlog it leaves must be shed, not kept as permanent latency.
    #[test]
    fn engine_stall_backlog_is_shed_not_kept_forever() {
        let (cw, cr) = ring_buffer(65_536);
        let (ow, or_) = ring_buffer(65_536);
        // Index stamps are a ramp, which the DC blocker correctly removes;
        // the latency measurement is independent of input conditioning.
        let pipeline = AudioPipeline::with_ring_buffers_unfiltered(cr, ow).unwrap();
        let pw = FakePipeWire::start(cw, or_);
        pipeline.start();
        thread::sleep(Duration::from_millis(300));
        let baseline = pw.latency_ms(5);

        // 150 ms stays under CAPTURE_MAX_BACKLOG, so the backlog really
        // reaches the output ring and only the consumer-side BacklogLimiter
        // can remove it (longer stalls are trimmed on the capture side).
        pipeline.set_engine(Box::new(StallOnceEngine {
            stall: Some(Duration::from_millis(150)),
        }));
        thread::sleep(Duration::from_millis(400));
        let right_after = pw.latency_ms(3);
        assert!(
            right_after > baseline + 100.0,
            "harness sanity: a 150 ms stall should first add backlog \
             (baseline {baseline:.1} ms, after stall {right_after:.1} ms)"
        );

        thread::sleep(Duration::from_millis(2_300));
        let settled = pw.latency_ms(5);
        drop(pw);
        pipeline.shutdown();
        assert!(
            settled < baseline + 50.0,
            "latency {settled:.1} ms (baseline {baseline:.1} ms) 2.7 s after a \
             150 ms stall: the stall backlog became permanent latency"
        );
    }

    /// An engine slower than real time must not grow latency without bound,
    /// and must not starve the command loop (heartbeat/Stop/SetEngine).
    #[test]
    fn slower_than_realtime_engine_keeps_latency_bounded_and_thread_responsive() {
        let (cw, cr) = ring_buffer(65_536);
        let (ow, or_) = ring_buffer(65_536);
        // Index stamps are a ramp, which the DC blocker correctly removes;
        // the latency measurement is independent of input conditioning.
        let pipeline = AudioPipeline::with_ring_buffers_unfiltered(cr, ow).unwrap();
        let pw = FakePipeWire::start(cw, or_);
        let slow = Arc::new(AtomicBool::new(true));
        pipeline.set_engine(Box::new(SlowEngine {
            per_block: Duration::from_micros(12_500), // RTF 1.25
            slow: slow.clone(),
        }));
        pipeline.start();

        thread::sleep(Duration::from_millis(1_000));
        let hb_1 = pipeline.heartbeat_count();
        thread::sleep(Duration::from_millis(1_000));
        let hb_2 = pipeline.heartbeat_count();
        let mut worst = 0.0f64;
        for _ in 0..5 {
            worst = worst.max(pw.latency_ms(3));
        }
        // Let the pre-fix code drain so shutdown cannot hang.
        slow.store(false, Ordering::Release);
        thread::sleep(Duration::from_millis(200));
        drop(pw);
        pipeline.shutdown();

        assert!(
            hb_2 > hb_1,
            "audio thread heartbeat froze ({hb_1} -> {hb_2}) while the engine \
             was behind: commands (Stop/SetEngine/Shutdown) would never run"
        );
        assert!(
            worst < 300.0,
            "latency reached {worst:.1} ms after ~2.3 s with an RTF 1.25 engine: \
             the capture backlog is unbounded"
        );
    }

    // --- Input DC blocker end-to-end harness -------------------------------
    //
    // Regression coverage for the laptop DMIC DC-offset fix (quick task
    // 260923-voj): a paced feeder plays a DC + 1 kHz tone into the capture
    // ring at real-time cadence, deliberately separate from `FakePipeWire`
    // (whose index-stamped ramp the DC blocker would flatten). The engine
    // sees the filtered input via a probe, and the output ring is drained
    // the same way the live output callback does (`BacklogLimiter::read_padded`).

    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    /// Feeds `dc + 0.01*sin(2*pi*1000*n/48000)` into a capture ring at
    /// real-time cadence (one `PW_QUANTUM` every ~21.3 ms), like the real
    /// PipeWire capture callback. `dc` is held in a shared atomic so tests can
    /// change it mid-run (device retarget / reconnect scenarios).
    struct PacedDcToneFeeder {
        stop: Arc<AtomicBool>,
        dc_bits: Arc<AtomicU32>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl PacedDcToneFeeder {
        fn start(capture_writer: RingBufWriter, initial_dc: f32) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let dc_bits = Arc::new(AtomicU32::new(initial_dc.to_bits()));
            let (stop_t, dc_t) = (stop.clone(), dc_bits.clone());
            let handle = thread::spawn(move || {
                let period = Duration::from_secs_f64(PW_QUANTUM as f64 / f64::from(SAMPLE_RATE));
                let mut n: u64 = 0;
                let mut buf = vec![0.0f32; PW_QUANTUM];
                let mut next = Instant::now();
                while !stop_t.load(Ordering::Acquire) {
                    let dc = f32::from_bits(dc_t.load(Ordering::Acquire));
                    for s in buf.iter_mut() {
                        let phase =
                            2.0 * std::f64::consts::PI * 1000.0 * n as f64 / f64::from(SAMPLE_RATE);
                        *s = dc + 0.01 * phase.sin() as f32;
                        n += 1;
                    }
                    capture_writer.write(&buf);
                    next += period;
                    if let Some(d) = next.checked_duration_since(Instant::now()) {
                        thread::sleep(d);
                    }
                }
            });
            Self {
                stop,
                dc_bits,
                handle: Some(handle),
            }
        }

        /// Change the DC level fed on the next quantum onward.
        fn set_dc(&self, dc: f32) {
            self.dc_bits.store(dc.to_bits(), Ordering::Release);
        }
    }

    impl Drop for PacedDcToneFeeder {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// Drains an output (or monitor) ring the same way the live PipeWire
    /// callback does — through `BacklogLimiter::read_padded` — and accumulates
    /// every sample into a shared `Vec` for later inspection.
    struct OutputDrain {
        stop: Arc<AtomicBool>,
        samples: Arc<Mutex<Vec<f32>>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl OutputDrain {
        fn start(output_reader: RingBufReader) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let samples = Arc::new(Mutex::new(Vec::new()));
            let (stop_t, samples_t) = (stop.clone(), samples.clone());
            let handle = thread::spawn(move || {
                let period = Duration::from_secs_f64(PW_QUANTUM as f64 / f64::from(SAMPLE_RATE));
                let mut limiter = BacklogLimiter::new();
                let mut buf = vec![0.0f32; PW_QUANTUM];
                let mut next = Instant::now();
                while !stop_t.load(Ordering::Acquire) {
                    limiter.read_padded(&output_reader, &mut buf);
                    samples_t.lock().unwrap().extend_from_slice(&buf);
                    next += period;
                    if let Some(d) = next.checked_duration_since(Instant::now()) {
                        thread::sleep(d);
                    }
                }
            });
            Self {
                stop,
                samples,
                handle: Some(handle),
            }
        }
    }

    impl Drop for OutputDrain {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// Records the running sum/count of the input it receives (skipping the
    /// first `skip_remaining` samples of settling time), then copies input to
    /// output unchanged. Used as the "engine-input probe" for the DC-blocker
    /// e2e tests.
    struct InputProbeEngine {
        stats: Arc<Mutex<(f64, usize)>>,
        skip_remaining: usize,
    }

    impl NoiseEngine for InputProbeEngine {
        fn init(&mut self, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn process(&mut self, input: &[f32], output: &mut [f32]) {
            let skip = self.skip_remaining.min(input.len());
            if skip < input.len() {
                let mut s = self.stats.lock().unwrap();
                for &x in &input[skip..] {
                    s.0 += f64::from(x);
                }
                s.1 += input.len() - skip;
            }
            self.skip_remaining -= skip;
            output.copy_from_slice(input);
        }
        fn set_strength(&mut self, _: f32) {}
        fn set_mode(&mut self, _: ProcessingMode) {}
        fn latency_frames(&self) -> u32 {
            0
        }
        fn teardown(&mut self) {}
    }

    /// 100 ms of settling time (matches the DC blocker's settling bound) to
    /// skip before accumulating engine-input statistics.
    const PROBE_SETTLE_SAMPLES: usize = SAMPLE_RATE as usize / 10;

    /// End-to-end: a laptop-DMIC-like 0.109 DC + 1 kHz 0.01-amplitude tone,
    /// fed through the production (filtered) constructor. The engine, the
    /// output ring, and `poll_levels().input_rms` must all show the DC-free
    /// signal. Pre-fix, `input_rms` climbs toward ~0.109.
    #[test]
    fn dc_offset_is_removed_before_engine_and_input_meter() {
        let (cw, cr) = ring_buffer(65_536);
        let (ow, or_) = ring_buffer(65_536);
        let pipeline = AudioPipeline::with_ring_buffers(cr, ow).unwrap();

        let stats: Arc<Mutex<(f64, usize)>> = Arc::new(Mutex::new((0.0, 0)));
        pipeline.set_engine(Box::new(InputProbeEngine {
            stats: stats.clone(),
            skip_remaining: PROBE_SETTLE_SAMPLES,
        }));

        // Start before feeding: the Start-edge fix discards capture queued
        // while stopped, so the very first fed samples are the ones measured.
        pipeline.start();
        let feeder = PacedDcToneFeeder::start(cw, 0.109);
        let drain = OutputDrain::start(or_);
        let drain_samples = drain.samples.clone();

        thread::sleep(Duration::from_millis(600));

        let level = pipeline.poll_levels();

        drop(feeder);
        drop(drain);
        pipeline.shutdown();

        // Engine-input probe.
        let (sum, count) = *stats.lock().unwrap();
        assert!(count > 0, "engine never received any (post-settling) input");
        let engine_mean = sum / count as f64;
        assert!(
            engine_mean.abs() < 1e-3,
            "engine-input mean = {engine_mean}, expected near 0 (DC removed before the engine)"
        );

        // Output ring, excluding the first 100 ms (4800 samples).
        let out = drain_samples.lock().unwrap();
        assert!(
            out.len() > PROBE_SETTLE_SAMPLES,
            "not enough output samples captured: {}",
            out.len()
        );
        let tail = &out[PROBE_SETTLE_SAMPLES..];
        let out_mean = tail.iter().map(|&s| f64::from(s)).sum::<f64>() / tail.len() as f64;
        assert!(
            out_mean.abs() < 1e-3,
            "output mean (after 100ms) = {out_mean}, expected near 0"
        );

        // Input meter.
        let level = level.expect("should have received at least one level report");
        assert!(
            level.input_rms < 0.03,
            "input_rms = {}, expected < 0.03 (pre-fix it climbs toward ~0.109)",
            level.input_rms
        );
    }

    /// An engine that panics on its first `process()` call. Reusable probe for
    /// exercising the audio thread's panic-recovery (passthrough fallback)
    /// path.
    struct PanicOnFirstProcessEngine;

    impl NoiseEngine for PanicOnFirstProcessEngine {
        fn init(&mut self, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn process(&mut self, _: &[f32], _: &mut [f32]) {
            panic!("intentional test panic (DC-blocker all-paths coverage)");
        }
        fn set_strength(&mut self, _: f32) {}
        fn set_mode(&mut self, _: ProcessingMode) {}
        fn latency_frames(&self) -> u32 {
            0
        }
        fn teardown(&mut self) {}
    }

    /// ReplaceCaptureReader must re-prime the DC blocker: a device retarget
    /// can carry a different DC level. Without the reset, switching from a
    /// steady 0.109 DC to -0.3 DC would show a step of about 0.409 (well
    /// above the 0.05 bound used below), so this bound is discriminating.
    #[test]
    fn dc_block_resets_on_capture_reader_replace() {
        let (cw1, cr1) = ring_buffer(65_536);
        let (ow, or_) = ring_buffer(65_536);
        let pipeline = AudioPipeline::with_ring_buffers(cr1, ow).unwrap();
        let drain = OutputDrain::start(or_);
        let drain_samples = drain.samples.clone();

        pipeline.start();
        let feeder1 = PacedDcToneFeeder::start(cw1, 0.109);
        thread::sleep(Duration::from_millis(300));

        // Mark roughly where the old feeder's output ends, then replace the
        // capture reader with a fresh ring fed a very different DC level.
        let switch_mark = drain_samples.lock().unwrap().len();
        let (cw2, cr2) = ring_buffer(65_536);
        pipeline.replace_capture_reader(Some(cr2));
        drop(feeder1);
        let feeder2 = PacedDcToneFeeder::start(cw2, -0.3);

        thread::sleep(Duration::from_millis(400));

        drop(feeder2);
        drop(drain);
        pipeline.shutdown();

        let out = drain_samples.lock().unwrap();
        // Skip 100 ms after the mark: pipeline latency plus the brief
        // zero-padded underrun while the new feeder catches up (zeros are
        // DC-free, so this skip cannot mask a real DC step).
        let settle = switch_mark + SAMPLE_RATE as usize / 10;
        assert!(
            out.len() > settle,
            "not enough post-replacement output captured: {}",
            out.len()
        );
        let tail = &out[settle..];
        let max_abs = tail.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(
            max_abs < 0.05,
            "max |output| after capture-reader replace = {max_abs}, expected < 0.05"
        );
    }

    /// The Start edge (running false -> true) must re-prime the DC blocker:
    /// a resumed stream can carry a different DC level than before the stop.
    #[test]
    fn dc_block_resets_on_start() {
        let (cw, cr) = ring_buffer(65_536);
        let (ow, or_) = ring_buffer(65_536);
        let pipeline = AudioPipeline::with_ring_buffers(cr, ow).unwrap();
        let drain = OutputDrain::start(or_);
        let drain_samples = drain.samples.clone();

        pipeline.start();
        let feeder = PacedDcToneFeeder::start(cw, 0.109);
        thread::sleep(Duration::from_millis(300));

        pipeline.stop();
        thread::sleep(Duration::from_millis(100));
        feeder.set_dc(-0.3); // DC changes while stopped (e.g. a new session)
        thread::sleep(Duration::from_millis(100));
        pipeline.start();
        thread::sleep(Duration::from_millis(300));

        drop(feeder);
        drop(drain);
        pipeline.shutdown();

        let out = drain_samples.lock().unwrap();
        let settle = SAMPLE_RATE as usize / 10; // 100 ms
        assert!(
            out.len() > settle,
            "not enough output captured: {}",
            out.len()
        );
        // The discard-stale-capture fix already drops everything queued
        // before Start, so the tail is exactly what the resumed stream
        // produced under the new DC level.
        let tail = &out[out.len() - settle..];
        let max_abs = tail.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(
            max_abs < 0.05,
            "max |output| after Start = {max_abs}, expected < 0.05"
        );
    }

    /// No reset on engine swap (DC is continuous across it), and every
    /// processing path — no-engine passthrough, a direct engine set (no old
    /// engine so no crossfade), a real two-engine crossfade, and the
    /// panic-recovery passthrough fallback — sees DC-free input.
    #[test]
    fn dc_free_input_on_every_processing_path() {
        let (cw, cr) = ring_buffer(65_536);
        let (ow, or_) = ring_buffer(65_536);
        let pipeline = AudioPipeline::with_ring_buffers(cr, ow).unwrap();
        let drain = OutputDrain::start(or_);
        let drain_samples = drain.samples.clone();

        pipeline.start();
        let feeder = PacedDcToneFeeder::start(cw, 0.109);

        // No engine: passthrough/bypass path.
        thread::sleep(Duration::from_millis(200));

        // Probe A: direct set, no crossfade (no old engine yet).
        let stats_a: Arc<Mutex<(f64, usize)>> = Arc::new(Mutex::new((0.0, 0)));
        pipeline.set_engine(Box::new(InputProbeEngine {
            stats: stats_a.clone(),
            skip_remaining: PROBE_SETTLE_SAMPLES,
        }));
        thread::sleep(Duration::from_millis(200));

        // Probe B: swapping from probe A (an active old engine) triggers a
        // real two-engine crossfade; both engines read the same filtered
        // block.
        let stats_b: Arc<Mutex<(f64, usize)>> = Arc::new(Mutex::new((0.0, 0)));
        pipeline.set_engine(Box::new(InputProbeEngine {
            stats: stats_b.clone(),
            skip_remaining: PROBE_SETTLE_SAMPLES,
        }));
        thread::sleep(Duration::from_millis(200));

        // An engine that panics on its first process(): triggers the
        // passthrough fallback.
        pipeline.set_engine(Box::new(PanicOnFirstProcessEngine));
        thread::sleep(Duration::from_millis(200));

        drop(feeder);
        drop(drain);
        pipeline.shutdown();

        let (sum_a, count_a) = *stats_a.lock().unwrap();
        assert!(count_a > 0, "probe A never received input");
        let mean_a = sum_a / count_a as f64;
        assert!(
            mean_a.abs() < 1e-3,
            "probe A input mean = {mean_a}, expected near 0 (DC removed before the engine)"
        );

        let (sum_b, count_b) = *stats_b.lock().unwrap();
        assert!(count_b > 0, "probe B never received input");
        let mean_b = sum_b / count_b as f64;
        assert!(
            mean_b.abs() < 1e-3,
            "probe B input mean = {mean_b}, expected near 0 (DC removed before the engine)"
        );

        let out = drain_samples.lock().unwrap();
        let skip = SAMPLE_RATE as usize / 10; // 100 ms
        assert!(
            out.len() > skip,
            "not enough output captured: {}",
            out.len()
        );
        let tail = &out[skip..];
        let max_abs = tail.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(
            max_abs < 0.05,
            "max |output| across all processing paths = {max_abs}, expected < 0.05"
        );
    }

    // --- Input auto-gain (quick task 260923-x24) --------------------------

    #[test]
    fn set_auto_gain_command_updates_state() {
        let mut running = true;
        let mut engine: Option<Box<dyn NoiseEngine>> = None;
        let mut crossfade: Option<CrossfadeState> = None;
        let mut monitor = MonitorOutput::new();
        let mut input_device = String::new();
        let mut test_auto_gain = AutoGain::new(SAMPLE_RATE);
        let mut test_watchdog = EngineWatchdog::new();
        assert!(test_auto_gain.is_enabled(), "AutoGain starts enabled");

        handle_command(
            AudioCommand::SetAutoGain(false),
            &mut running,
            &mut engine,
            &mut crossfade,
            &mut monitor,
            &mut input_device,
            &mut test_auto_gain,
            &mut test_watchdog,
        );
        assert!(!test_auto_gain.is_enabled());

        handle_command(
            AudioCommand::SetAutoGain(true),
            &mut running,
            &mut engine,
            &mut crossfade,
            &mut monitor,
            &mut input_device,
            &mut test_auto_gain,
            &mut test_watchdog,
        );
        assert!(test_auto_gain.is_enabled());
    }

    /// dBFS levels of the auto-gain e2e feeder's mixed signal — a quiet
    /// speech-like burst well above a steady noise floor, matching
    /// `dsp::AutoGain`'s own reference test signals but fed through the real
    /// ring-buffer/audio-thread pipeline instead of a bare function call.
    const AUTO_GAIN_E2E_BURST_DBFS: f32 = -34.0;
    const AUTO_GAIN_E2E_NOISE_DBFS: f32 = -75.0;

    /// Advance an LCG state and return a uniform sample in `[-1, 1)`. Matches
    /// `src/dsp.rs`'s test-only noise generator (kept as an independent copy
    /// per the plan — this module must stay free of a `dsp::tests` dependency).
    fn lcg_uniform(state: &mut u32) -> f64 {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(*state) / 2_147_483_648.0 - 1.0
    }

    /// One sample of the auto-gain e2e feeder's signal at absolute sample
    /// index `n`: a 1 kHz, 500 ms-period burst (300 ms on, 200 ms off) at
    /// [`AUTO_GAIN_E2E_BURST_DBFS`] plus LCG white noise at
    /// [`AUTO_GAIN_E2E_NOISE_DBFS`].
    fn auto_gain_e2e_sample(n: u64, lcg: &mut u32) -> f32 {
        let t = n as f64 / f64::from(SAMPLE_RATE);
        let phase = t.rem_euclid(0.5);
        let burst_amp =
            f64::from(10f32.powf(AUTO_GAIN_E2E_BURST_DBFS / 20.0)) * std::f64::consts::SQRT_2;
        let noise_amp = f64::from(10f32.powf(AUTO_GAIN_E2E_NOISE_DBFS / 20.0)) * 3f64.sqrt();
        let burst = if phase < 0.3 {
            burst_amp * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()
        } else {
            0.0
        };
        let noise = noise_amp * lcg_uniform(lcg);
        (burst + noise) as f32
    }

    /// Reference rms of `n` samples of the e2e feeder's signal, computed
    /// independently (no threads, no ring buffers) for the OFF-window sanity
    /// check in [`auto_gain_boosts_quiet_speech_before_engine_and_input_meter`].
    fn reference_auto_gain_feed_rms(n: u64) -> f32 {
        let mut lcg: u32 = 0xC0FF_EE42;
        let signal: Vec<f32> = (0..n).map(|i| auto_gain_e2e_sample(i, &mut lcg)).collect();
        rms(&signal)
    }

    /// Feeds the auto-gain e2e signal (see [`auto_gain_e2e_sample`]) into a
    /// capture ring at real-time cadence, reusing `PacedDcToneFeeder`'s
    /// pacing loop with a continuous sample index across the whole feed.
    struct PacedAutoGainFeeder {
        stop: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl PacedAutoGainFeeder {
        fn start(capture_writer: RingBufWriter) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_t = stop.clone();
            let handle = thread::spawn(move || {
                let period = Duration::from_secs_f64(PW_QUANTUM as f64 / f64::from(SAMPLE_RATE));
                let mut n: u64 = 0;
                let mut lcg: u32 = 0xC0FF_EE42;
                let mut buf = vec![0.0f32; PW_QUANTUM];
                let mut next = Instant::now();
                while !stop_t.load(Ordering::Acquire) {
                    for s in buf.iter_mut() {
                        *s = auto_gain_e2e_sample(n, &mut lcg);
                        n += 1;
                    }
                    capture_writer.write(&buf);
                    next += period;
                    if let Some(d) = next.checked_duration_since(Instant::now()) {
                        thread::sleep(d);
                    }
                }
            });
            Self {
                stop,
                handle: Some(handle),
            }
        }
    }

    impl Drop for PacedAutoGainFeeder {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// Records each processed block's mean-square (power) into a shared
    /// `Vec`, then copies input to output unchanged — an "energy probe"
    /// distinct from [`InputProbeEngine`] (which accumulates a running
    /// mean, not per-block values) so per-window rms can be recomputed after
    /// the fact.
    struct EnergyProbeEngine {
        blocks: Arc<Mutex<Vec<f32>>>,
    }

    impl NoiseEngine for EnergyProbeEngine {
        fn init(&mut self, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn process(&mut self, input: &[f32], output: &mut [f32]) {
            let mean_sq = input.iter().map(|&s| s * s).sum::<f32>() / input.len() as f32;
            self.blocks.lock().unwrap().push(mean_sq);
            output.copy_from_slice(input);
        }
        fn set_strength(&mut self, _: f32) {}
        fn set_mode(&mut self, _: ProcessingMode) {}
        fn latency_frames(&self) -> u32 {
            0
        }
        fn teardown(&mut self) {}
    }

    /// Rms of a window of per-block mean-squares (equal-size blocks, so the
    /// mean of means equals the overall mean-square).
    fn window_rms(mean_squares: &[f32]) -> f32 {
        let mean = mean_squares.iter().sum::<f32>() / mean_squares.len() as f32;
        mean.sqrt()
    }

    /// End-to-end (real audio thread, production constructor): the input
    /// auto-gain boosts a too-quiet mic's speech before both the engine and
    /// the input level meter. Real-time by design (~5.5s: 1.5s OFF baseline
    /// + 4s for the speech-gated adaptation to take hold).
    #[test]
    fn auto_gain_boosts_quiet_speech_before_engine_and_input_meter() {
        let (cw, cr) = ring_buffer(65_536);
        // The output side is never drained in this test — only the engine's
        // input (via `EnergyProbeEngine`) and `poll_levels()` are inspected —
        // so the reader half is intentionally unused (writes past capacity
        // simply drop the oldest samples; see `write_more_than_capacity_drops_excess`).
        let (ow, _or) = ring_buffer(65_536);
        let pipeline = AudioPipeline::with_ring_buffers(cr, ow).unwrap();

        let blocks: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        pipeline.set_engine(Box::new(EnergyProbeEngine {
            blocks: blocks.clone(),
        }));
        pipeline.set_auto_gain(false);
        pipeline.start();

        let feeder = PacedAutoGainFeeder::start(cw);

        thread::sleep(Duration::from_millis(1_500));
        let mark = blocks.lock().unwrap().len();
        let off_level = pipeline
            .poll_levels()
            .expect("should have received an OFF-phase level report");

        pipeline.set_auto_gain(true);
        thread::sleep(Duration::from_millis(4_000));
        let on_level = pipeline
            .poll_levels()
            .expect("should have received an ON-phase level report");

        drop(feeder);
        pipeline.shutdown();

        let all_blocks = blocks.lock().unwrap();
        assert!(mark >= 100, "not enough OFF-phase blocks: {mark}");
        assert!(
            all_blocks.len() >= mark + 100,
            "not enough ON-phase blocks: {}",
            all_blocks.len()
        );

        // The probe's rms just before the switch must be close to the raw
        // (unboosted) fed signal's own reference rms.
        let off_window = &all_blocks[mark - 100..mark];
        let off_probe_rms = window_rms(off_window);
        let reference_rms = reference_auto_gain_feed_rms(SAMPLE_RATE as u64);
        let ref_diff_db =
            20.0 * (f64::from(off_probe_rms) / f64::from(reference_rms).max(1e-12)).log10();
        assert!(
            ref_diff_db.abs() <= 1.0,
            "OFF-phase probe rms differs from the reference feed rms by {ref_diff_db}dB"
        );

        // The probe's rms over the last 100 blocks (well into the ON phase)
        // must exceed the OFF-window rms by the expected boost range.
        let on_window = &all_blocks[all_blocks.len() - 100..];
        let on_probe_rms = window_rms(on_window);
        let boost_db =
            20.0 * (f64::from(on_probe_rms) / f64::from(off_probe_rms).max(1e-12)).log10();
        assert!(
            (6.0..=14.5).contains(&boost_db),
            "ON-minus-OFF probe boost = {boost_db}dB, expected 6.0..=14.5"
        );

        // The meter (LevelReport::input_rms) must show at least a 2x boost.
        assert!(
            on_level.input_rms >= 2.0 * off_level.input_rms,
            "ON input_rms ({}) must be >= 2x OFF input_rms ({})",
            on_level.input_rms,
            off_level.input_rms
        );
    }

    // --- Engine fault watchdog (debug session dfn-panic-under-load) --------
    //
    // A failing-but-alive engine (DeepFilterNet's guard gave up, an engine
    // slower than real time, a caught panic) must be reported to the app
    // exactly once, tagged with the engine's generation so the app never
    // replaces an engine the user picked after the failing one.

    #[test]
    fn watchdog_reports_three_trims_within_the_window_once() {
        let mut w = EngineWatchdog::new();
        w.engine_replaced();
        let t0 = Instant::now();
        assert_eq!(w.fell_behind(t0), None);
        assert_eq!(
            w.fell_behind(t0 + Duration::from_secs(4)),
            None,
            "2 trims: a stall, not overload"
        );
        assert_eq!(
            w.fell_behind(t0 + FELL_BEHIND_FAULT_WINDOW),
            Some(EngineFaultReport {
                generation: 1,
                fault: EngineFault::Overloaded
            }),
            "3rd trim exactly at the window edge still counts"
        );
        assert_eq!(
            w.fell_behind(t0 + Duration::from_secs(11)),
            None,
            "reported once per engine"
        );
        assert_eq!(
            w.check_health(EngineHealth::Overloaded),
            None,
            "once, whatever the source"
        );
    }

    #[test]
    fn watchdog_ignores_trims_spread_wider_than_the_window() {
        let mut w = EngineWatchdog::new();
        w.engine_replaced();
        let t0 = Instant::now();
        let step = FELL_BEHIND_FAULT_WINDOW / 2 + Duration::from_millis(1);
        for k in 0..10 {
            assert_eq!(
                w.fell_behind(t0 + step * k),
                None,
                "trim #{k}: never 3 within the window"
            );
        }
    }

    #[test]
    fn watchdog_rearms_and_bumps_generation_on_engine_replaced() {
        let mut w = EngineWatchdog::new();
        w.engine_replaced();
        assert!(w.panicked().is_some());
        let t0 = Instant::now();
        w.fell_behind(t0);
        w.fell_behind(t0);
        w.engine_replaced();
        assert_eq!(w.fell_behind(t0), None, "old engine's trims must not count");
        assert_eq!(w.check_health(EngineHealth::Healthy), None);
        assert_eq!(
            w.check_health(EngineHealth::Overloaded),
            Some(EngineFaultReport {
                generation: 2,
                fault: EngineFault::Overloaded
            })
        );
    }

    /// Passthrough engine whose health turns `Overloaded` after `healthy_blocks`
    /// processed blocks (like DeepFilterNet's guard giving up).
    struct GivesUpEngine {
        healthy_blocks: usize,
        processed: usize,
    }

    impl NoiseEngine for GivesUpEngine {
        fn init(&mut self, _: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn process(&mut self, input: &[f32], output: &mut [f32]) {
            self.processed += 1;
            output.copy_from_slice(input);
        }
        fn set_strength(&mut self, _: f32) {}
        fn set_mode(&mut self, _: ProcessingMode) {}
        fn latency_frames(&self) -> u32 {
            0
        }
        fn teardown(&mut self) {}
        fn health(&self) -> EngineHealth {
            if self.processed > self.healthy_blocks {
                EngineHealth::Overloaded
            } else {
                EngineHealth::Healthy
            }
        }
    }

    fn wait_for_fault(pipeline: &AudioPipeline, within: Duration) -> Option<EngineFaultReport> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(r) = pipeline.poll_engine_fault() {
                return Some(r);
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    }

    #[test]
    fn engine_reporting_overloaded_reaches_the_app_once() {
        let pipeline = AudioPipeline::new().unwrap();
        pipeline.set_engine(Box::new(GivesUpEngine {
            healthy_blocks: 3,
            processed: 0,
        }));
        pipeline.start();
        assert_eq!(
            wait_for_fault(&pipeline, Duration::from_secs(2)),
            Some(EngineFaultReport {
                generation: 1,
                fault: EngineFault::Overloaded
            })
        );
        assert_eq!(
            wait_for_fault(&pipeline, Duration::from_millis(200)),
            None,
            "reported once"
        );
        pipeline.shutdown();
    }

    #[test]
    fn caught_engine_panic_reaches_the_app() {
        let pipeline = AudioPipeline::new().unwrap();
        pipeline.set_engine(Box::new(PanicOnFirstProcessEngine));
        pipeline.start();
        assert_eq!(
            wait_for_fault(&pipeline, Duration::from_secs(2)),
            Some(EngineFaultReport {
                generation: 1,
                fault: EngineFault::Panicked
            })
        );
        pipeline.shutdown();
    }

    #[test]
    fn fault_about_an_already_replaced_engine_is_dropped() {
        let pipeline = AudioPipeline::new().unwrap();
        pipeline.set_engine(Box::new(GivesUpEngine {
            healthy_blocks: 0,
            processed: 0,
        }));
        pipeline.start();
        // Let the audio thread report engine #1 ...
        thread::sleep(Duration::from_millis(150));
        // ... while the user has meanwhile picked engine #2.
        pipeline.set_engine(Box::new(PassthroughEngine::new()));
        assert_eq!(
            wait_for_fault(&pipeline, Duration::from_millis(300)),
            None,
            "a report about engine #1 must not trigger a fallback away from engine #2"
        );
        pipeline.shutdown();
    }

    /// Same two reports on the REAL capture path (ring buffers, what
    /// production runs), not only in simulation mode.
    #[test]
    fn faults_on_the_real_capture_path_reach_the_app() {
        for (engine, expected) in [
            (
                Box::new(GivesUpEngine {
                    healthy_blocks: 3,
                    processed: 0,
                }) as Box<dyn NoiseEngine>,
                EngineFault::Overloaded,
            ),
            (Box::new(PanicOnFirstProcessEngine), EngineFault::Panicked),
        ] {
            let (cw, cr) = ring_buffer(65_536);
            let (ow, or_) = ring_buffer(65_536);
            let pipeline = AudioPipeline::with_ring_buffers_unfiltered(cr, ow).unwrap();
            let pw = FakePipeWire::start(cw, or_);
            pipeline.set_engine(engine);
            pipeline.start();
            let fault = wait_for_fault(&pipeline, Duration::from_secs(2));
            drop(pw);
            pipeline.shutdown();
            assert_eq!(
                fault,
                Some(EngineFaultReport {
                    generation: 1,
                    fault: expected
                })
            );
        }
    }

    /// DPDFNet-8 under load: an engine slower than real time makes the audio
    /// thread trim its capture backlog again and again; that is reported as
    /// Overloaded (the engine itself never says so).
    #[test]
    fn slower_than_realtime_engine_is_reported_overloaded() {
        let (cw, cr) = ring_buffer(65_536);
        let (ow, or_) = ring_buffer(65_536);
        let pipeline = AudioPipeline::with_ring_buffers_unfiltered(cr, ow).unwrap();
        let pw = FakePipeWire::start(cw, or_);
        let slow = Arc::new(AtomicBool::new(true));
        pipeline.set_engine(Box::new(SlowEngine {
            per_block: Duration::from_micros(12_500), // RTF 1.25
            slow: slow.clone(),
        }));
        pipeline.start();
        let fault = wait_for_fault(&pipeline, Duration::from_secs(6));
        slow.store(false, Ordering::Release);
        thread::sleep(Duration::from_millis(100));
        drop(pw);
        pipeline.shutdown();
        assert_eq!(
            fault,
            Some(EngineFaultReport {
                generation: 1,
                fault: EngineFault::Overloaded
            })
        );
    }

    /// Held-out guard against false positives: a real-time engine on the
    /// same fake graph for 3 s is never reported.
    #[test]
    fn realtime_engine_is_never_reported() {
        let (cw, cr) = ring_buffer(65_536);
        let (ow, or_) = ring_buffer(65_536);
        let pipeline = AudioPipeline::with_ring_buffers_unfiltered(cr, ow).unwrap();
        let pw = FakePipeWire::start(cw, or_);
        pipeline.set_engine(Box::new(PassthroughEngine::new()));
        pipeline.start();
        let fault = wait_for_fault(&pipeline, Duration::from_secs(3));
        drop(pw);
        pipeline.shutdown();
        assert_eq!(fault, None);
    }
}

//! Audio processing loop.
//!
//! Pulls frames from the physical microphone via PipeWire, runs them through
//! the active [`NoiseEngine`](crate::engine::NoiseEngine), and pushes the
//! cleaned audio to the "CleanMic" virtual source. Optionally copies processed
//! frames to a monitor output so the user can hear the result.
//!
//! All processing is 48 kHz mono f32. The audio thread must remain lock-free:
//! no allocations, no mutexes, no I/O on the hot path.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

use anyhow::{Context, Result};

use crate::engine::NoiseEngine;
use crate::pipewire::monitor::MonitorOutput;
use crate::pipewire::ringbuf::{RingBufReader, RingBufWriter};

/// Default processing buffer size in samples (10 ms at 48 kHz).
const BUFFER_SIZE: usize = 480;

/// Sample rate used throughout the pipeline.
const SAMPLE_RATE: u32 = 48_000;

/// Duration of the crossfade window in samples (~10 ms at 48 kHz).
const CROSSFADE_SAMPLES: usize = 480;

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
    /// RMS level of the input buffer (linear, 0.0..=1.0+).
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
        let channel_alive = Arc::new(AtomicBool::new(true));
        let heartbeat = Arc::new(AtomicU64::new(0));
        let heartbeat_thread = heartbeat.clone();

        let thread_handle = thread::Builder::new()
            .name("cleanmic-audio".into())
            .spawn(move || {
                audio_thread_main(cmd_rx, level_tx, None, None, heartbeat_thread);
            })
            .context("failed to spawn audio thread")?;

        Ok(Self {
            cmd_tx,
            level_rx,
            thread_handle: Some(thread_handle),
            channel_alive,
            heartbeat,
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
    pub fn with_ring_buffers(capture_reader: RingBufReader, output_writer: RingBufWriter) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCommand>();
        let (level_tx, level_rx) = mpsc::channel::<LevelReport>();
        let channel_alive = Arc::new(AtomicBool::new(true));
        let heartbeat = Arc::new(AtomicU64::new(0));
        let heartbeat_thread = heartbeat.clone();

        let thread_handle = thread::Builder::new()
            .name("cleanmic-audio".into())
            .spawn(move || {
                audio_thread_main(cmd_rx, level_tx, Some(capture_reader), Some(output_writer), heartbeat_thread);
            })
            .context("failed to spawn audio thread")?;

        Ok(Self {
            cmd_tx,
            level_rx,
            thread_handle: Some(thread_handle),
            channel_alive,
            heartbeat,
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
        if self.cmd_tx.send(AudioCommand::SetEngine(engine)).is_err() {
            log::error!("audio thread channel closed - SetEngine command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Change the input device.
    pub fn set_input_device(&self, device_id: String) {
        if self.cmd_tx.send(AudioCommand::SetInputDevice(device_id)).is_err() {
            log::error!("audio thread channel closed - SetInputDevice command dropped");
            self.channel_alive.store(false, Ordering::Release);
        }
    }

    /// Set the normalized suppression strength on the active engine.
    pub fn set_strength(&self, strength: f32) {
        if self.cmd_tx.send(AudioCommand::SetStrength(strength)).is_err() {
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

    /// Attach or detach the PipeWire ring-buffer writer used for monitor output.
    ///
    /// Call with `Some(writer)` before `set_monitor(true)`, and with `None`
    /// after `set_monitor(false)` so the audio thread stops writing to a
    /// destroyed stream.
    pub fn set_monitor_writer(&self, writer: Option<RingBufWriter>) {
        if self.cmd_tx.send(AudioCommand::SetMonitorWriter(writer)).is_err() {
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
fn handle_command(
    cmd: AudioCommand,
    running: &mut bool,
    engine: &mut Option<Box<dyn NoiseEngine>>,
    crossfade: &mut Option<CrossfadeState>,
    monitor: &mut MonitorOutput,
    input_device: &mut String,
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
    capture_reader: Option<RingBufReader>,
    output_writer: Option<RingBufWriter>,
    heartbeat: Arc<AtomicU64>,
) {
    let mut running = false;
    let mut engine: Option<Box<dyn NoiseEngine>> = None;
    let mut crossfade: Option<CrossfadeState> = None;
    let mut monitor = MonitorOutput::new();
    let mut _input_device = String::new();

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

    let tick_duration = std::time::Duration::from_secs_f64(BUFFER_SIZE as f64 / SAMPLE_RATE as f64);

    // Ring buffer halves. Made mutable so they can be hot-swapped after a
    // PipeWire reconnect via SetRingBuffers.
    let mut capture_reader = capture_reader;
    let mut output_writer = output_writer;

    // Whether we are connected to real PipeWire ring buffers or running in
    // simulation mode (silent input, discarded output). Recomputed on
    // SetRingBuffers.
    let mut has_ring_buffers = capture_reader.is_some() && output_writer.is_some();

    loop {
        // Increment heartbeat counter so the health check can detect liveness.
        heartbeat.fetch_add(1, Ordering::Release);

        // Drain all pending commands (non-blocking).
        loop {
            match cmd_rx.try_recv() {
                Ok(AudioCommand::SetRingBuffers { capture_reader: new_cr, output_writer: new_ow }) => {
                    log::info!("Audio thread: ring buffers replaced (PipeWire reconnect)");
                    capture_reader = new_cr;
                    output_writer = new_ow;
                    has_ring_buffers = capture_reader.is_some() && output_writer.is_some();
                }
                Ok(AudioCommand::ReplaceCaptureReader(new_cr)) => {
                    log::info!("Audio thread: capture reader replaced (device retargeting)");
                    capture_reader = new_cr;
                    has_ring_buffers = capture_reader.is_some() && output_writer.is_some();
                }
                Ok(cmd) => {
                    if handle_command(
                        cmd,
                        &mut running,
                        &mut engine,
                        &mut crossfade,
                        &mut monitor,
                        &mut _input_device,
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
                // Process ALL available data in a tight loop before yielding.
                // PipeWire delivers audio in quanta (typically 1024 samples) but
                // we process in BUFFER_SIZE (480) chunks. Processing all available
                // data at once prevents gaps in the output ring buffer that cause
                // pulsating audio when PipeWire reads between our iterations.
                let reader = capture_reader.as_ref().unwrap();
                let mut processed_any = false;
                let mut in_sum_sq = 0.0f32;
                let mut out_sum_sq = 0.0f32;
                let mut total_samples = 0usize;

                while reader.available() >= BUFFER_SIZE {
                    let read = reader.read(&mut input_buf);
                    for s in &mut input_buf[read..] {
                        *s = 0.0;
                    }

                    let process_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if let Some(ref mut xfade) = crossfade {
                            let done = process_with_crossfade(
                                xfade,
                                &mut engine,
                                &input_buf,
                                &mut output_buf,
                                &mut crossfade_old_buf,
                            );
                            if done {
                                if let Some(mut finished) = crossfade.take() {
                                    finished.old_engine.teardown();
                                    log::info!("Engine crossfade complete, old engine torn down");
                                }
                            }
                        } else {
                            process_buffer(&mut engine, &input_buf, &mut output_buf);
                        }
                    }));

                    if process_result.is_err() {
                        log::error!(
                            "Audio engine panicked during processing — dropping engine and falling back to passthrough. \
                             Select a different engine in the UI to restore noise suppression."
                        );
                        engine = None;
                        crossfade = None;
                        output_buf.copy_from_slice(&input_buf);
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
                        if done {
                            if let Some(mut finished) = crossfade.take() {
                                finished.old_engine.teardown();
                                log::info!("Engine crossfade complete, old engine torn down");
                            }
                        }
                    } else {
                        process_buffer(&mut engine, &input_buf, &mut output_buf);
                    }
                }));

                if process_result.is_err() {
                    engine = None;
                    crossfade = None;
                    output_buf.copy_from_slice(&input_buf);
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
            // When not running, block briefly to avoid busy-waiting.
            match cmd_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(AudioCommand::SetRingBuffers { capture_reader: new_cr, output_writer: new_ow }) => {
                    log::info!("Audio thread: ring buffers replaced (PipeWire reconnect, idle)");
                    capture_reader = new_cr;
                    output_writer = new_ow;
                    has_ring_buffers = capture_reader.is_some() && output_writer.is_some();
                }
                Ok(AudioCommand::ReplaceCaptureReader(new_cr)) => {
                    log::info!("Audio thread: capture reader replaced (device retargeting, idle)");
                    capture_reader = new_cr;
                    has_ring_buffers = capture_reader.is_some() && output_writer.is_some();
                }
                Ok(cmd) => {
                    if handle_command(
                        cmd,
                        &mut running,
                        &mut engine,
                        &mut crossfade,
                        &mut monitor,
                        &mut _input_device,
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

        // Enable monitor.
        handle_command(
            AudioCommand::SetMonitor(true),
            &mut running,
            &mut engine,
            &mut crossfade,
            &mut monitor,
            &mut input_device,
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
        use crate::engine::{deepfilter::{self, DeepFilterEngine}, rnnoise::RNNoiseEngine};
        if !deepfilter::is_available() { return; }

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
        use crate::engine::{deepfilter::{self, DeepFilterEngine}, rnnoise::RNNoiseEngine};
        if !deepfilter::is_available() { return; }

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
        use crate::engine::{deepfilter::{self, DeepFilterEngine}, rnnoise::RNNoiseEngine};
        if !deepfilter::is_available() { return; }

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
        use crate::engine::{EngineType, create_engine};
        use crate::engine::khip::KhipEngine;

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
        if !deepfilter::is_available() { return; }
        let engine = create_engine(EngineType::DeepFilterNet);
        assert!(engine.is_ok(), "Should create DeepFilterNet engine successfully");
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
            fn init(&mut self, _: u32) -> anyhow::Result<()> { Ok(()) }
            fn process(&mut self, _: &[f32], _: &mut [f32]) { panic!("intentional test panic"); }
            fn set_strength(&mut self, _: f32) {}
            fn set_mode(&mut self, _: ProcessingMode) {}
            fn latency_frames(&self) -> u32 { 0 }
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
}

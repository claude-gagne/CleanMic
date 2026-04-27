//! Live PipeWire implementation (requires the `pipewire` feature).
//!
//! This module is only compiled when `feature = "pipewire"` is active.
//! It wraps `pipewire-rs` to create and manage real PipeWire nodes.
//!
//! The "CleanMic" virtual source is a null-audio-sink node created via
//! `core.create_object()`, which PipeWire and WirePlumber correctly register
//! in the graph with visible ports. A separate playback stream writes the
//! processed audio into the virtual source's input port.
//!
//! The PipeWire main loop runs on a dedicated thread. Commands from the main
//! application thread are sent via the loop's signal mechanism.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;

use pipewire as pw;
use pw::spa::param::ParamType;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::Pod;
use pw::stream::{Stream, StreamFlags};

use super::ringbuf::{RingBufReader, RingBufWriter};
use super::{NODE_CHANNELS, NODE_NAME, NODE_SAMPLE_RATE, PipeWireError};

/// Handle to the PipeWire main-loop thread and shared state needed to
/// create/destroy the virtual mic stream from any thread.
pub(super) struct LivePipeWireManager {
    /// Sender side of the main-loop. Lets us invoke closures on the PW thread.
    loop_sender: pw::channel::Sender<PipeWireCommand>,
    /// Join handle for the PipeWire thread.
    pw_thread: Option<thread::JoinHandle<()>>,
    /// Whether a stream is currently connected. Set by the PW thread, read by
    /// the main thread for status queries.
    #[allow(dead_code)]
    stream_active: Arc<AtomicBool>,
    /// Set to `true` by the PipeWire thread when the core connection is lost
    /// (daemon disconnect). Polled by the main thread via `check_disconnected()`.
    daemon_disconnected: Arc<AtomicBool>,
}

/// Commands sent from the main thread to the PipeWire thread.
enum PipeWireCommand {
    /// Create the virtual mic node plus capture and output streams.
    CreateStreams {
        capture_writer: RingBufWriter,
        output_reader: RingBufReader,
        /// PipeWire node name to pin the capture stream to (PW_KEY_TARGET_OBJECT).
        /// When `None`, the capture stream uses AUTOCONNECT and follows the
        /// system default source. Always pass `Some(name)` in production so
        /// WirePlumber's default-source policy cannot re-route the stream.
        capture_target: Option<String>,
    },
    DestroyStream,
    /// Replace the capture stream with a new one pinned to the given target.
    ///
    /// Used when the user picks a different physical mic in the GUI. The PW
    /// thread disconnects the existing capture stream and creates a new one
    /// with `PW_KEY_TARGET_OBJECT` set to `target_name`. The caller also
    /// swaps the audio thread's capture ring-buffer reader to match.
    SetCaptureTarget {
        target_name: Option<String>,
        capture_writer: RingBufWriter,
    },
    /// Create a playback (sink) stream for monitor output.
    CreateMonitorStream {
        monitor_reader: RingBufReader,
    },
    /// Destroy the monitor playback stream.
    DestroyMonitorStream,
    Quit,
}

impl LivePipeWireManager {
    /// Connect to the PipeWire daemon by initializing the library and spawning
    /// the main-loop thread.
    pub fn connect() -> Result<Self, PipeWireError> {
        pw::init();

        let stream_active = Arc::new(AtomicBool::new(false));
        let stream_active_clone = Arc::clone(&stream_active);

        let daemon_disconnected = Arc::new(AtomicBool::new(false));
        let daemon_disconnected_clone = Arc::clone(&daemon_disconnected);

        // Channel for sending commands to the PW thread.
        let (sender, receiver) = pw::channel::channel::<PipeWireCommand>();

        let pw_thread = thread::Builder::new()
            .name("cleanmic-pw".into())
            .spawn(move || {
                Self::pw_thread_main(receiver, stream_active_clone, daemon_disconnected_clone);
            })
            .map_err(|e| {
                PipeWireError::ConnectionFailed(format!("failed to spawn PW thread: {e}"))
            })?;

        log::info!("Connected to PipeWire daemon (main loop running on dedicated thread)");

        Ok(Self {
            loop_sender: sender,
            pw_thread: Some(pw_thread),
            stream_active,
            daemon_disconnected,
        })
    }

    /// Check if the PipeWire daemon connection has been lost.
    pub fn check_disconnected(&self) -> bool {
        self.daemon_disconnected.load(Ordering::Acquire)
    }

    /// Reset the disconnected flag after a successful reconnect.
    pub fn reset_disconnected(&self) {
        self.daemon_disconnected.store(false, Ordering::Release);
    }

    /// Body of the PipeWire thread: runs the main loop and handles commands.
    fn pw_thread_main(
        receiver: pw::channel::Receiver<PipeWireCommand>,
        stream_active: Arc<AtomicBool>,
        daemon_disconnected: Arc<AtomicBool>,
    ) {
        let mainloop =
            pw::main_loop::MainLoop::new(None).expect("failed to create PipeWire MainLoop");
        let context =
            pw::context::Context::new(&mainloop).expect("failed to create PipeWire Context");
        let core = context
            .connect(None)
            .expect("failed to connect to PipeWire Core");

        // Attach a core error listener to detect daemon disconnects.
        let daemon_disconnected_core = Arc::clone(&daemon_disconnected);
        let mainloop_weak_core = mainloop.downgrade();
        let _core_listener = core
            .add_listener_local()
            .error(move |id, _seq, res, msg| {
                if id == 0 {
                    log::warn!(
                        "PipeWire core error (id={}, res={}): {} — daemon may have disconnected",
                        id, res, msg
                    );
                    daemon_disconnected_core.store(true, Ordering::Release);
                    if let Some(ml) = mainloop_weak_core.upgrade() {
                        ml.quit();
                    }
                } else {
                    log::debug!("PipeWire core error (id={}, res={}): {}", id, res, msg);
                }
            })
            .register();

        // Hold the virtual mic node (created via create_object), streams, and
        // stream listeners. These live entirely on the PW thread so Rc<RefCell<_>>
        // is correct (no cross-thread sharing).
        let virtual_mic_holder: Rc<RefCell<Option<pw::node::Node>>> = Rc::new(RefCell::new(None));
        let output_stream_holder: Rc<RefCell<Option<Stream>>> = Rc::new(RefCell::new(None));
        let capture_holder: Rc<RefCell<Option<Stream>>> = Rc::new(RefCell::new(None));
        let monitor_holder: Rc<RefCell<Option<Stream>>> = Rc::new(RefCell::new(None));
        // Listeners must stay alive as long as the streams. Dropping a
        // StreamListener unregisters the callbacks from PipeWire.
        let output_listener_holder: Rc<RefCell<Option<Box<dyn std::any::Any>>>> =
            Rc::new(RefCell::new(None));
        let capture_listener_holder: Rc<RefCell<Option<Box<dyn std::any::Any>>>> =
            Rc::new(RefCell::new(None));
        let monitor_listener_holder: Rc<RefCell<Option<Box<dyn std::any::Any>>>> =
            Rc::new(RefCell::new(None));

        let virtual_mic_cmd = Rc::clone(&virtual_mic_holder);
        let output_stream_cmd = Rc::clone(&output_stream_holder);
        let capture_holder_cmd = Rc::clone(&capture_holder);
        let monitor_holder_cmd = Rc::clone(&monitor_holder);
        let output_listener_cmd = Rc::clone(&output_listener_holder);
        let capture_listener_cmd = Rc::clone(&capture_listener_holder);
        let monitor_listener_cmd = Rc::clone(&monitor_listener_holder);
        let stream_active_cmd = Arc::clone(&stream_active);
        let mainloop_weak = mainloop.downgrade();

        // Attach the command receiver to the main loop.
        let _receiver = receiver.attach(mainloop.loop_(), move |cmd| {
            match cmd {
                PipeWireCommand::CreateStreams {
                    capture_writer,
                    output_reader,
                    capture_target,
                } => {
                    let mut vm_holder = virtual_mic_cmd.borrow_mut();
                    let mut os_holder = output_stream_cmd.borrow_mut();
                    let mut c_holder = capture_holder_cmd.borrow_mut();
                    if vm_holder.is_some() {
                        log::debug!("PW thread: streams already exist, ignoring CreateStreams");
                        return;
                    }

                    // 1. Create the virtual mic node via server-side factory.
                    let vm_node_id;
                    match Self::create_virtual_mic_node(&core) {
                        Ok(node) => {
                            use pw::proxy::ProxyT;
                            vm_node_id = node.upcast_ref().id();
                            log::info!("PW thread: CleanMic virtual mic node created (id={})", vm_node_id);
                            *vm_holder = Some(node);
                        }
                        Err(e) => {
                            log::error!("PW thread: failed to create virtual mic node: {e}");
                            return;
                        }
                    }

                    // 2. Create the output (playback) stream that writes processed
                    //    audio to the virtual mic's input port.
                    match Self::create_output_stream(&core, output_reader, vm_node_id) {
                        Ok((stream, listener)) => {
                            log::info!("PW thread: CleanMic output stream created");
                            *os_holder = Some(stream);
                            *output_listener_cmd.borrow_mut() = Some(Box::new(listener));
                        }
                        Err(e) => {
                            log::error!("PW thread: failed to create output stream: {e}");
                            vm_holder.take();
                            return;
                        }
                    }

                    // 3. Create the capture stream (reads from physical mic).
                    match Self::create_capture_stream(
                        &core,
                        capture_writer,
                        capture_target.as_deref(),
                    ) {
                        Ok((stream, listener)) => {
                            log::info!(
                                "PW thread: CleanMic capture stream created (target={:?})",
                                capture_target,
                            );
                            *c_holder = Some(stream);
                            *capture_listener_cmd.borrow_mut() = Some(Box::new(listener));
                        }
                        Err(e) => {
                            log::error!("PW thread: failed to create capture stream: {e}");
                            if let Some(s) = os_holder.take() {
                                s.disconnect().ok();
                            }
                            output_listener_cmd.borrow_mut().take();
                            vm_holder.take();
                            return;
                        }
                    }

                    // Explicitly link the physical mic to CleanMic-capture.
                    // We disabled AUTOCONNECT on the capture stream so
                    // WirePlumber's default-source policy can't clobber this.
                    if let Some(t) = capture_target.clone() {
                        link_capture_to_target(t);
                    }

                    stream_active_cmd.store(true, Ordering::Release);

                    // Link the output stream to the virtual mic's input port.
                    // We use pw-link because the proxy ID from create_object
                    // is local, not the global node ID needed by stream.connect().
                    // pw-link resolves port names reliably after both nodes exist.
                    std::thread::spawn(|| {
                        // Brief delay for PipeWire to finish registering ports.
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        match std::process::Command::new("pw-link")
                            .arg("CleanMic-output:output_MONO")
                            .arg("CleanMic:input_MONO")
                            .output()
                        {
                            Ok(out) if out.status.success() => {
                                log::info!("Linked CleanMic-output -> CleanMic:input_MONO");
                            }
                            Ok(out) => {
                                let stderr = String::from_utf8_lossy(&out.stderr);
                                log::error!("pw-link failed: {}", stderr.trim());
                            }
                            Err(e) => {
                                log::error!("Failed to run pw-link: {e}");
                            }
                        }
                    });
                }
                PipeWireCommand::DestroyStream => {
                    let mut vm_holder = virtual_mic_cmd.borrow_mut();
                    let mut os_holder = output_stream_cmd.borrow_mut();
                    let mut c_holder = capture_holder_cmd.borrow_mut();

                    if let Some(stream) = c_holder.take() {
                        stream.disconnect().ok();
                        drop(stream);
                        capture_listener_cmd.borrow_mut().take();
                        log::info!("PW thread: capture stream destroyed");
                    }
                    if let Some(stream) = os_holder.take() {
                        stream.disconnect().ok();
                        drop(stream);
                        output_listener_cmd.borrow_mut().take();
                        log::info!("PW thread: output stream destroyed");
                    }
                    if let Some(_node) = vm_holder.take() {
                        log::info!("PW thread: virtual mic node destroyed");
                    }
                    stream_active_cmd.store(false, Ordering::Release);
                }
                PipeWireCommand::SetCaptureTarget {
                    target_name,
                    capture_writer,
                } => {
                    let mut c_holder = capture_holder_cmd.borrow_mut();

                    // Drop any stale links into the old capture stream before
                    // we tear it down. Also drops the WirePlumber-added
                    // self-loop if it ever sneaks back in.
                    unlink_all_into_cleanmic_capture();

                    // Tear down the existing capture stream (if any). Dropping
                    // it drops the captured RingBufWriter that was feeding the
                    // audio thread's old reader, so the caller MUST follow
                    // this command with a new reader handed to the audio thread.
                    if let Some(stream) = c_holder.take() {
                        stream.disconnect().ok();
                        drop(stream);
                        capture_listener_cmd.borrow_mut().take();
                        log::info!(
                            "PW thread: capture stream destroyed (retargeting to {:?})",
                            target_name,
                        );
                    }

                    match Self::create_capture_stream(
                        &core,
                        capture_writer,
                        target_name.as_deref(),
                    ) {
                        Ok((stream, listener)) => {
                            log::info!(
                                "PW thread: CleanMic capture stream re-created (target={:?})",
                                target_name,
                            );
                            *c_holder = Some(stream);
                            *capture_listener_cmd.borrow_mut() = Some(Box::new(listener));
                        }
                        Err(e) => {
                            log::error!(
                                "PW thread: failed to re-create capture stream for target {:?}: {e}",
                                target_name,
                            );
                        }
                    }

                    if let Some(t) = target_name {
                        link_capture_to_target(t);
                    }
                }
                PipeWireCommand::CreateMonitorStream { monitor_reader } => {
                    let mut m_holder = monitor_holder_cmd.borrow_mut();
                    if m_holder.is_some() {
                        log::debug!(
                            "PW thread: monitor stream already exists, ignoring CreateMonitorStream"
                        );
                        return;
                    }
                    match Self::create_monitor_stream(&core, monitor_reader) {
                        Ok((stream, listener)) => {
                            log::info!("PW thread: monitor playback stream created");
                            *m_holder = Some(stream);
                            *monitor_listener_cmd.borrow_mut() = Some(Box::new(listener));
                        }
                        Err(e) => {
                            log::error!("PW thread: failed to create monitor playback stream: {e}");
                        }
                    }
                }
                PipeWireCommand::DestroyMonitorStream => {
                    let mut m_holder = monitor_holder_cmd.borrow_mut();
                    if let Some(stream) = m_holder.take() {
                        stream.disconnect().ok();
                        drop(stream);
                        monitor_listener_cmd.borrow_mut().take();
                        log::info!("PW thread: monitor playback stream destroyed");
                    }
                }
                PipeWireCommand::Quit => {
                    let mut vm_holder = virtual_mic_cmd.borrow_mut();
                    let mut os_holder = output_stream_cmd.borrow_mut();
                    let mut c_holder = capture_holder_cmd.borrow_mut();
                    let mut m_holder = monitor_holder_cmd.borrow_mut();
                    if let Some(stream) = c_holder.take() {
                        stream.disconnect().ok();
                    }
                    capture_listener_cmd.borrow_mut().take();
                    if let Some(stream) = os_holder.take() {
                        stream.disconnect().ok();
                    }
                    output_listener_cmd.borrow_mut().take();
                    if let Some(stream) = m_holder.take() {
                        stream.disconnect().ok();
                    }
                    monitor_listener_cmd.borrow_mut().take();
                    vm_holder.take();
                    stream_active_cmd.store(false, Ordering::Release);
                    if let Some(ml) = mainloop_weak.upgrade() {
                        ml.quit();
                    }
                    log::info!("PW thread: quitting main loop");
                }
            }
        });

        // Run the main loop (blocks until quit).
        mainloop.run();
        log::info!("PW thread: main loop exited");
    }

    /// Build the SPA audio format pod used by streams.
    fn audio_format_pod() -> Result<Vec<u8>, PipeWireError> {
        let mut audio_info = AudioInfoRaw::new();
        audio_info.set_format(AudioFormat::F32LE);
        audio_info.set_rate(NODE_SAMPLE_RATE);
        audio_info.set_channels(NODE_CHANNELS);

        pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(pw::spa::pod::Object {
                type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
                id: ParamType::EnumFormat.as_raw(),
                properties: audio_info.into(),
            }),
        )
        .map_err(|e| {
            PipeWireError::NodeCreationFailed(format!("failed to serialize audio format: {e}"))
        })
        .map(|(cursor, _)| cursor.into_inner())
    }

    /// Build a stereo (2-channel FL+FR) format pod for the monitor playback stream.
    ///
    /// The processing pipeline is mono throughout; the monitor stream upmixes
    /// by duplicating each mono sample into both output channels in the process
    /// callback. Negotiating stereo here ensures PipeWire routes to both
    /// headphone channels rather than only the left channel.
    fn monitor_format_pod() -> Result<Vec<u8>, PipeWireError> {
        let mut audio_info = AudioInfoRaw::new();
        audio_info.set_format(AudioFormat::F32LE);
        audio_info.set_rate(NODE_SAMPLE_RATE);
        audio_info.set_channels(2);

        pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(pw::spa::pod::Object {
                type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
                id: ParamType::EnumFormat.as_raw(),
                properties: audio_info.into(),
            }),
        )
        .map_err(|e| {
            PipeWireError::NodeCreationFailed(format!(
                "failed to serialize monitor audio format: {e}"
            ))
        })
        .map(|(cursor, _)| cursor.into_inner())
    }

    /// Create the virtual mic node via `core.create_object()` using the
    /// `support.null-audio-sink` factory wrapped in an adapter.
    ///
    /// This creates a proper PipeWire device node that WirePlumber registers in
    /// the graph with visible ports (`capture_MONO` output, `input_MONO` input).
    fn create_virtual_mic_node(core: &pw::core::Core) -> Result<pw::node::Node, PipeWireError> {
        let channels_str = NODE_CHANNELS.to_string();

        // Build the audio position string based on channel count.
        let position = if NODE_CHANNELS == 1 {
            "[ MONO ]"
        } else {
            "[ FL FR ]"
        };

        let props = pw::properties::properties! {
            "factory.name" => "support.null-audio-sink",
            *pw::keys::NODE_NAME => NODE_NAME,
            *pw::keys::NODE_DESCRIPTION => "CleanMic Virtual Microphone",
            *pw::keys::MEDIA_CLASS => "Audio/Source/Virtual",
            "audio.position" => position,
            "audio.channels" => channels_str.as_str(),
            "object.linger" => "false",
        };

        let node: pw::node::Node = core.create_object("adapter", &props).map_err(|e| {
            PipeWireError::NodeCreationFailed(format!(
                "core.create_object(adapter) failed: {e}"
            ))
        })?;

        log::info!(
            "Created virtual mic node '{}' (media.class=Audio/Source/Virtual, {}ch, position={})",
            NODE_NAME,
            NODE_CHANNELS,
            position,
        );

        Ok(node)
    }

    /// Create the output (playback) stream that writes processed audio to the
    /// virtual mic node's input port.
    ///
    /// The stream uses `Stream/Output/Audio` media class with `target.object`
    /// pointing to the CleanMic virtual mic node, so PipeWire auto-links it.
    fn create_output_stream(
        core: &pw::core::Core,
        output_reader: RingBufReader,
        _virtual_mic_id: u32,
    ) -> Result<(Stream, pw::stream::StreamListener<()>), PipeWireError> {
        let props = pw::properties::properties! {
            *pw::keys::NODE_NAME => "CleanMic-output",
            *pw::keys::MEDIA_CLASS => "Stream/Output/Audio",
            *pw::keys::NODE_DESCRIPTION => "CleanMic Processed Output",
            // Don't auto-connect to default sink — we link manually to
            // CleanMic:input_MONO via pw-link after port registration.
            "node.autoconnect" => "false",
        };

        let stream = Stream::new(core, "CleanMic-output", props)
            .map_err(|e| PipeWireError::NodeCreationFailed(format!("Stream::new failed: {e}")))?;

        let listener = stream
            .add_local_listener()
            .state_changed(|_stream, _data: &mut (), old, new| {
                log::info!("CleanMic output stream state: {:?} -> {:?}", old, new);
            })
            .process(move |stream, _data: &mut ()| {
                unsafe {
                    let raw_buf = stream.dequeue_raw_buffer();
                    if raw_buf.is_null() {
                        return;
                    }
                    let buf = &mut *raw_buf;
                    if buf.buffer.is_null() {
                        stream.queue_raw_buffer(raw_buf);
                        return;
                    }
                    let spa_buf = &mut *buf.buffer;
                    if spa_buf.n_datas > 0 && !spa_buf.datas.is_null() {
                        let data = &mut *spa_buf.datas;
                        if !data.data.is_null() && data.maxsize > 0 {
                            let n_samples = data.maxsize as usize / std::mem::size_of::<f32>();
                            let slice =
                                std::slice::from_raw_parts_mut(data.data as *mut f32, n_samples);
                            let read = output_reader.read(slice);
                            for s in &mut slice[read..] {
                                *s = 0.0;
                            }
                            if !data.chunk.is_null() {
                                let chunk = &mut *data.chunk;
                                chunk.offset = 0;
                                chunk.stride = std::mem::size_of::<f32>() as i32;
                                chunk.size = (n_samples * std::mem::size_of::<f32>()) as u32;
                            }
                        }
                    }
                    stream.queue_raw_buffer(raw_buf);
                }
            })
            .register()
            .map_err(|e| {
                PipeWireError::NodeCreationFailed(format!(
                    "failed to register output listener: {e}"
                ))
            })?;

        let values = Self::audio_format_pod()?;
        let mut params = [Pod::from_bytes(&values).unwrap()];

        stream
            .connect(
                pw::spa::utils::Direction::Output,
                None,
                StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|e| {
                PipeWireError::NodeCreationFailed(format!("output stream.connect failed: {e}"))
            })?;

        Ok((stream, listener))
    }

    /// Create the capture stream (input direction — reads from the physical mic).
    ///
    /// `target_name` is informational only: `PW_KEY_TARGET_OBJECT` gets set as
    /// a hint, but we also disable `node.autoconnect` so WirePlumber never
    /// touches the link, and the caller links the capture port explicitly via
    /// `pw-link` (see `link_capture_to_target`). This is the same pattern we
    /// use for CleanMic-output → CleanMic:input_MONO and avoids WirePlumber's
    /// default-source policy clobbering `target.object` whenever CleanMic
    /// itself is set as the system default input.
    fn create_capture_stream(
        core: &pw::core::Core,
        capture_writer: RingBufWriter,
        target_name: Option<&str>,
    ) -> Result<(Stream, pw::stream::StreamListener<()>), PipeWireError> {
        let mut props = pw::properties::properties! {
            *pw::keys::NODE_NAME => "CleanMic-capture",
            *pw::keys::MEDIA_CLASS => "Stream/Input/Audio",
            *pw::keys::NODE_DESCRIPTION => "CleanMic Capture",
            "stream.dont-remix" => "true",
            // Do not let WirePlumber route this stream. We pw-link the
            // target port explicitly after creation.
            "node.autoconnect" => "false",
        };
        if let Some(name) = target_name {
            // Hint only — WirePlumber has historically honored this when its
            // default-source policy didn't clobber it. Harmless if ignored.
            props.insert("target.object", name);
        }

        let stream = Stream::new(core, "CleanMic-capture", props)
            .map_err(|e| PipeWireError::NodeCreationFailed(format!("Stream::new failed: {e}")))?;

        let listener = stream
            .add_local_listener()
            .state_changed(|_stream, _data: &mut (), old, new| {
                log::info!("CleanMic capture stream state: {:?} -> {:?}", old, new);
            })
            .process(move |stream, _data: &mut ()| {
                unsafe {
                    let raw_buf = stream.dequeue_raw_buffer();
                    if raw_buf.is_null() {
                        return;
                    }
                    let buf = &mut *raw_buf;
                    if buf.buffer.is_null() {
                        stream.queue_raw_buffer(raw_buf);
                        return;
                    }
                    let spa_buf = &mut *buf.buffer;
                    if spa_buf.n_datas > 0 && !spa_buf.datas.is_null() {
                        let data = &mut *spa_buf.datas;
                        if !data.data.is_null() && data.chunk.is_null() {
                            stream.queue_raw_buffer(raw_buf);
                            return;
                        }
                        if !data.data.is_null() && !data.chunk.is_null() {
                            let chunk = &*data.chunk;
                            let n_bytes = chunk.size as usize;
                            let n_samples = n_bytes / std::mem::size_of::<f32>();
                            let slice = std::slice::from_raw_parts(
                                (data.data as *const u8).add(chunk.offset as usize) as *const f32,
                                n_samples,
                            );
                            capture_writer.write(slice);
                        }
                    }
                    stream.queue_raw_buffer(raw_buf);
                }
            })
            .register()
            .map_err(|e| {
                PipeWireError::NodeCreationFailed(format!(
                    "failed to register capture listener: {e}"
                ))
            })?;

        let values = Self::audio_format_pod()?;
        let mut params = [Pod::from_bytes(&values).unwrap()];

        stream
            .connect(
                pw::spa::utils::Direction::Input,
                None,
                StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|e| {
                PipeWireError::NodeCreationFailed(format!("capture stream.connect failed: {e}"))
            })?;

        Ok((stream, listener))
    }

    /// Create a monitor playback stream (writes to the default audio output so
    /// the user can hear the processed mic signal).
    ///
    /// The stream is pinned to whatever node is listed as
    /// `default.configured.audio.sink` in pw-metadata (the user's GNOME
    /// Output choice). Without the pin, WirePlumber can route the stream
    /// to `default.audio.sink` instead — which is the system fallback and
    /// drifts away from the user's configured device whenever the preferred
    /// sink is temporarily unavailable (e.g. a Bluetooth headset that isn't
    /// connected yet). If no configured sink is readable, fall back to
    /// AUTOCONNECT without a target.
    fn create_monitor_stream(
        core: &pw::core::Core,
        monitor_reader: RingBufReader,
    ) -> Result<(Stream, pw::stream::StreamListener<()>), PipeWireError> {
        let mut props = pw::properties::properties! {
            *pw::keys::NODE_NAME => "CleanMic-monitor",
            *pw::keys::MEDIA_CLASS => "Stream/Output/Audio",
            *pw::keys::NODE_DESCRIPTION => "CleanMic Monitor",
        };
        if let Some(sink) = configured_default_sink() {
            log::info!("Pinning CleanMic monitor stream to configured sink {}", sink);
            props.insert("target.object", sink);
        } else {
            log::debug!(
                "No configured audio sink found — monitor stream will auto-connect"
            );
        }

        let stream = Stream::new(core, "CleanMic-monitor", props)
            .map_err(|e| PipeWireError::NodeCreationFailed(format!("Stream::new failed: {e}")))?;

        let listener = stream
            .add_local_listener()
            .state_changed(|_stream, _data: &mut (), old, new| {
                log::info!("CleanMic monitor stream state: {:?} -> {:?}", old, new);
            })
            .process(move |stream, _data: &mut ()| {
                // Monitor output is stereo (FL+FR): duplicate the processed mono
                // signal into both channels so both headphone ears receive audio.
                // The ring buffer contains mono f32 frames; we read into a temp
                // buffer then interleave each sample as [L, R] pairs in the
                // PipeWire output buffer.
                unsafe {
                    let raw_buf = stream.dequeue_raw_buffer();
                    if raw_buf.is_null() {
                        return;
                    }
                    let buf = &mut *raw_buf;
                    if buf.buffer.is_null() {
                        stream.queue_raw_buffer(raw_buf);
                        return;
                    }
                    let spa_buf = &mut *buf.buffer;
                    if spa_buf.n_datas > 0 && !spa_buf.datas.is_null() {
                        let data = &mut *spa_buf.datas;
                        if !data.data.is_null() && data.maxsize > 0 {
                            // Output buffer holds interleaved stereo f32 frames.
                            // Each stereo frame = 2 f32 samples (FL, FR).
                            let n_stereo_samples =
                                data.maxsize as usize / std::mem::size_of::<f32>();
                            let n_mono_frames = n_stereo_samples / 2;
                            let out = std::slice::from_raw_parts_mut(
                                data.data as *mut f32,
                                n_stereo_samples,
                            );

                            // Read mono frames into the first half of a stack
                            // scratch buffer, then interleave into the output.
                            let mut mono_buf = vec![0f32; n_mono_frames];
                            let read = monitor_reader.read(&mut mono_buf);
                            // Zero any unread frames (ring buffer underrun).
                            for s in &mut mono_buf[read..] {
                                *s = 0.0;
                            }
                            // Interleave: out[2*i] = FL, out[2*i+1] = FR
                            for (i, &sample) in mono_buf.iter().enumerate() {
                                out[2 * i] = sample;
                                out[2 * i + 1] = sample;
                            }

                            if !data.chunk.is_null() {
                                let chunk = &mut *data.chunk;
                                chunk.offset = 0;
                                // stride = bytes per frame = 2 channels * 4 bytes
                                chunk.stride = (2 * std::mem::size_of::<f32>()) as i32;
                                chunk.size =
                                    (n_stereo_samples * std::mem::size_of::<f32>()) as u32;
                            }
                        }
                    }
                    stream.queue_raw_buffer(raw_buf);
                }
            })
            .register()
            .map_err(|e| {
                PipeWireError::NodeCreationFailed(format!(
                    "failed to register monitor listener: {e}"
                ))
            })?;

        // Stereo format pod (FL + FR) for the monitor playback stream.
        // The virtual mic and processing pipeline stay mono; we upmix here
        // in the callback so the user hears audio in both headphone channels.
        let values = Self::monitor_format_pod()?;
        let mut params = [Pod::from_bytes(&values).unwrap()];

        stream
            .connect(
                pw::spa::utils::Direction::Output,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|e| {
                PipeWireError::NodeCreationFailed(format!("monitor stream.connect failed: {e}"))
            })?;

        Ok((stream, listener))
    }

    /// Enable monitor output by creating a playback stream connected to the
    /// default audio sink.
    pub fn enable_monitor(&self, monitor_reader: RingBufReader) -> Result<(), PipeWireError> {
        log::info!("Creating PipeWire monitor playback stream");
        self.loop_sender
            .send(PipeWireCommand::CreateMonitorStream { monitor_reader })
            .map_err(|_| {
                PipeWireError::NodeCreationFailed(
                    "failed to send CreateMonitorStream command to PW thread".into(),
                )
            })?;
        Ok(())
    }

    /// Disable monitor output by destroying the playback stream.
    pub fn disable_monitor(&self) -> Result<(), PipeWireError> {
        log::info!("Destroying PipeWire monitor playback stream");
        self.loop_sender
            .send(PipeWireCommand::DestroyMonitorStream)
            .map_err(|_| {
                PipeWireError::NodeDestructionFailed(
                    "failed to send DestroyMonitorStream command to PW thread".into(),
                )
            })?;
        Ok(())
    }

    /// Create the "CleanMic" virtual source node via PipeWire.
    ///
    /// `capture_target` pins the capture stream's source to a specific
    /// PipeWire node name (set via `PW_KEY_TARGET_OBJECT`). Always pin it so
    /// WirePlumber's default-source policy cannot re-route the capture stream.
    /// Pass `None` only when no physical mic is available on the system.
    pub fn create_virtual_mic(
        &mut self,
        capture_writer: RingBufWriter,
        output_reader: RingBufReader,
        capture_target: Option<String>,
    ) -> Result<(), PipeWireError> {
        log::info!(
            "Creating PipeWire virtual mic '{}' (null-audio-sink + streams, {}ch, {} Hz, f32, capture_target={:?})",
            NODE_NAME,
            NODE_CHANNELS,
            NODE_SAMPLE_RATE,
            capture_target,
        );
        self.loop_sender
            .send(PipeWireCommand::CreateStreams {
                capture_writer,
                output_reader,
                capture_target,
            })
            .map_err(|_| {
                PipeWireError::NodeCreationFailed(
                    "failed to send CreateStreams command to PW thread".into(),
                )
            })?;
        Ok(())
    }

    /// Retarget the capture stream to a different physical mic.
    ///
    /// Tears down the existing capture stream and creates a new one with
    /// `PW_KEY_TARGET_OBJECT` set to `target_name`. The caller owns lifecycle
    /// of the ring buffer: it passes the new writer here and must hand the
    /// matching new reader to the audio thread.
    pub fn set_capture_target(
        &self,
        target_name: Option<String>,
        capture_writer: RingBufWriter,
    ) -> Result<(), PipeWireError> {
        log::info!("Retargeting CleanMic capture stream to {:?}", target_name);
        self.loop_sender
            .send(PipeWireCommand::SetCaptureTarget {
                target_name,
                capture_writer,
            })
            .map_err(|_| {
                PipeWireError::NodeCreationFailed(
                    "failed to send SetCaptureTarget command to PW thread".into(),
                )
            })?;
        Ok(())
    }

    /// Destroy the "CleanMic" virtual source node.
    pub fn destroy_virtual_mic(&mut self) -> Result<(), PipeWireError> {
        log::info!("Destroying PipeWire node '{}'", NODE_NAME);
        self.loop_sender
            .send(PipeWireCommand::DestroyStream)
            .map_err(|_| {
                PipeWireError::NodeDestructionFailed(
                    "failed to send DestroyStream command to PW thread".into(),
                )
            })?;
        Ok(())
    }

    /// Scan for orphaned "CleanMic" nodes and remove them.
    pub fn cleanup_orphans(&self) -> Result<(), PipeWireError> {
        log::info!("Scanning for orphaned '{}' nodes", NODE_NAME);
        Ok(())
    }
}

impl Drop for LivePipeWireManager {
    fn drop(&mut self) {
        log::info!("LivePipeWireManager dropping — sending Quit to PW thread");
        if self.loop_sender.send(PipeWireCommand::Quit).is_err() {
            log::debug!("PipeWire loop channel closed - Quit command dropped");
        }

        if let Some(handle) = self.pw_thread.take()
            && let Err(e) = handle.join()
        {
            log::error!("PipeWire thread panicked: {:?}", e);
        }
    }
}

/// Explicitly link a physical mic node's capture port to
/// `CleanMic-capture:input_MONO` via `pw-link`.
///
/// We do this instead of relying on `target.object` + `AUTOCONNECT` because
/// WirePlumber's default-source policy will clobber `target.object` whenever
/// CleanMic is set as the system default input — it adds its own link from
/// `CleanMic:capture_MONO` to `CleanMic-capture:input_MONO`, creating the
/// exact self-loop we're trying to prevent. With `node.autoconnect = false`
/// on the capture stream and the link made manually, WirePlumber never
/// interferes.
///
/// ALSA source nodes typically expose either `capture_FL` / `capture_FR`
/// (stereo) or `capture_MONO`. We try `capture_FL` first (works for most
/// USB mics like the Razer Seiren X which presents a stereo interface), then
/// fall back to `capture_MONO`.
///
/// Runs in a background thread with a short delay to allow the ports to
/// finish registering with PipeWire.
fn link_capture_to_target(target_name: String) {
    std::thread::spawn(move || {
        // Retry a handful of times to survive late port registration —
        // ALSA nodes sometimes take longer than our fixed delay to expose
        // their capture_* ports, especially on the first selection after
        // a cold start.
        const CANDIDATE_PORTS: [&str; 2] = ["capture_FL", "capture_MONO"];
        for attempt in 0..10u32 {
            std::thread::sleep(std::time::Duration::from_millis(100 * (attempt + 1) as u64));
            for port in &CANDIDATE_PORTS {
                let src = format!("{target_name}:{port}");
                match std::process::Command::new("pw-link")
                    .arg(&src)
                    .arg("CleanMic-capture:input_MONO")
                    .output()
                {
                    Ok(out) if out.status.success() => {
                        log::info!("Linked {src} -> CleanMic-capture:input_MONO");
                        return;
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        let stderr = stderr.trim();
                        // "already exists" is a benign race with WirePlumber
                        // (shouldn't happen now but worth treating as success).
                        if stderr.contains("exists") || stderr.contains("existe") {
                            log::info!("Link {src} -> CleanMic-capture:input_MONO already present");
                            return;
                        }
                        log::debug!(
                            "pw-link {src} -> CleanMic-capture:input_MONO failed (attempt {attempt}): {stderr}"
                        );
                    }
                    Err(e) => {
                        log::error!("Failed to run pw-link: {e}");
                        return;
                    }
                }
            }
        }
        log::error!(
            "Gave up after 10 attempts to link {target_name}:capture_* -> CleanMic-capture:input_MONO"
        );
    });
}

/// Disconnect any existing links into `CleanMic-capture:input_MONO`.
///
/// Called before retargeting the capture stream so stale links to the
/// previous source (or the self-loop from WirePlumber's default-source
/// policy) don't linger and mix audio from two sources.
fn unlink_all_into_cleanmic_capture() {
    // pw-link has no "disconnect everything into X" mode, so we parse
    // `pw-link -l -I` for lines ending at CleanMic-capture:input_MONO
    // and disconnect each link id.
    let Ok(out) = std::process::Command::new("pw-link").arg("-l").arg("-I").output() else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Walk the output. The listing groups ports; we track the current port
    // header to know whether a `|<-` line belongs to CleanMic-capture.
    let mut in_cleanmic = false;
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('|') {
            // Port header line: "<id> <node>:<port>"
            in_cleanmic = line.contains("CleanMic-capture:input_MONO");
            continue;
        }
        if !in_cleanmic || !trimmed.starts_with("|<-") {
            continue;
        }
        // Line looks like: "  <link-id>   |<-   <src-node>:<src-port>"
        let Some(link_id) = line.split_whitespace().next() else { continue };
        let _ = std::process::Command::new("pw-link")
            .arg("-d")
            .arg(link_id)
            .output();
        log::debug!("Disconnected link {link_id} into CleanMic-capture:input_MONO");
    }
}

/// Read a single pw-metadata key from the "default" metadata object and parse
/// its `{"name": "..."}` JSON value.
///
/// Returns `None` if pw-metadata is unreachable, exits non-zero, or no
/// `update:` line for the key is present (which is what happens when the key
/// has never been written — e.g. a fresh GNOME install where the user hasn't
/// explicitly picked a default device).
fn pw_metadata_name(key: &str) -> Option<String> {
    let output = std::process::Command::new("pw-metadata")
        .arg("0")
        .arg(key)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // pw-metadata prints lines like:
    //   update: id:0 key:'<key>' value:'{"name":"<node>"}' type:'Spa:String:JSON'
    // Parse out the JSON value and extract "name".
    for line in stdout.lines() {
        let Some(start) = line.find("value:'") else { continue };
        let rest = &line[start + 7..];
        let Some(end) = rest.find('\'') else { continue };
        let json_str = &rest[..end];
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) else { continue };
        if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
            return Some(name.to_string());
        }
    }
    None
}

/// Read the default audio sink from PipeWire metadata.
///
/// Prefers `default.configured.audio.sink` (the user's explicit GNOME Sound
/// Settings choice, which doesn't drift if the device is temporarily
/// unavailable) and falls back to `default.audio.sink` (the runtime
/// auto-resolved default) when no configured sink has been written. The
/// fallback matters on fresh installs where the user has never opened Sound
/// Settings — in that case only the runtime key is populated.
///
/// Returns `None` if pw-metadata is unreachable or both keys are unset.
fn configured_default_sink() -> Option<String> {
    pw_metadata_name("default.configured.audio.sink")
        .or_else(|| pw_metadata_name("default.audio.sink"))
}

/// Read the default audio source from PipeWire metadata.
///
/// Prefers `default.configured.audio.source` (the user's explicit GNOME Sound
/// Settings choice) and falls back to `default.audio.source` (the runtime
/// auto-resolved default) when no configured source has been written. The
/// fallback is essential on fresh GNOME installs (e.g. Ubuntu 26.04 out of
/// the box) where the configured key is never populated until the user
/// touches the input picker, so without the fallback the mic-picker's
/// "Default (Mic)" row would silently disappear.
///
/// Self-loop safety: callers cross-check this name against
/// `list_input_devices()` (which excludes CleanMic), so even if the runtime
/// default points at CleanMic the upstream resolver discards it.
///
/// Returns `None` if pw-metadata is unreachable or both keys are unset.
pub(crate) fn configured_default_source() -> Option<String> {
    pw_metadata_name("default.configured.audio.source")
        .or_else(|| pw_metadata_name("default.audio.source"))
}

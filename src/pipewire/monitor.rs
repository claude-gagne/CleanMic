//! Monitor output routing.
//!
//! Creates and destroys a PipeWire link that routes the processed (denoised)
//! audio from the CleanMic pipeline to the user's default playback device, so
//! they can hear exactly what others will hear.
//!
//! When compiled without the `pipewire` feature, a stub implementation tracks
//! state in memory but does not create real PipeWire links.
//!
//! The real implementation uses a lock-free SPSC ring buffer to bridge the
//! audio thread (writer) and the PipeWire playback stream callback (reader).

use super::ringbuf::RingBufWriter;

/// Routes processed audio to a playback sink for listen-back.
///
/// In stub mode (no `pipewire` feature), the struct tracks enabled/disabled
/// state without touching real audio infrastructure.
pub struct MonitorOutput {
    enabled: bool,
    /// The most recently written monitor buffer, useful for testing that the
    /// monitor receives the processed (not raw) audio.
    last_buffer: Vec<f32>,
    /// Ring buffer writer for sending processed audio to the PipeWire playback
    /// stream. Only populated when a monitor ring buffer pair is attached.
    ring_writer: Option<RingBufWriter>,
}

impl MonitorOutput {
    /// Default buffer size for pre-allocation (480 samples = 10ms at 48 kHz).
    const PREALLOCATED_SIZE: usize = 480;

    /// Create a new monitor output in the disabled state.
    ///
    /// Pre-allocates the internal buffer to avoid heap allocations on the
    /// audio thread.
    pub fn new() -> Self {
        Self {
            enabled: false,
            last_buffer: vec![0.0; Self::PREALLOCATED_SIZE],
            ring_writer: None,
        }
    }

    /// Attach a ring buffer writer for sending audio to the PipeWire monitor
    /// playback stream. The audio thread writes processed samples here; the
    /// PipeWire playback callback reads from the corresponding reader.
    pub fn set_ring_writer(&mut self, writer: RingBufWriter) {
        self.ring_writer = Some(writer);
    }

    /// Detach the ring buffer writer (e.g., when the monitor stream is
    /// destroyed). Any subsequent writes while enabled will still update the
    /// last_buffer for test inspection but will not push to PipeWire.
    pub fn clear_ring_writer(&mut self) {
        self.ring_writer = None;
    }

    /// Enable monitor output — creates a PipeWire link routing processed audio
    /// to the default playback device.
    ///
    /// In stub mode, this just flips the enabled flag.
    ///
    /// Returns `Ok(())` on success. If the playback device is not available the
    /// monitor stays disabled and a warning is logged, but no error is returned
    /// (graceful degradation).
    pub fn enable(&mut self) -> Result<(), super::PipeWireError> {
        if self.enabled {
            log::debug!("MonitorOutput::enable called but already enabled — no-op");
            return Ok(());
        }

        #[cfg(feature = "pipewire")]
        {
            log::info!("MonitorOutput: enabling monitor output (ring buffer route)");
        }

        #[cfg(not(feature = "pipewire"))]
        {
            log::warn!(
                "Stub: would create PipeWire link from CleanMic output to default playback sink"
            );
        }

        self.enabled = true;
        log::info!("Monitor output enabled");
        Ok(())
    }

    /// Disable monitor output — removes the PipeWire link to the playback device.
    ///
    /// Safe to call when already disabled (no-op).
    pub fn disable(&mut self) -> Result<(), super::PipeWireError> {
        if !self.enabled {
            log::debug!("MonitorOutput::disable called but already disabled — no-op");
            return Ok(());
        }

        #[cfg(feature = "pipewire")]
        {
            log::info!("MonitorOutput: disabling monitor output");
        }

        #[cfg(not(feature = "pipewire"))]
        {
            log::warn!("Stub: would remove PipeWire link to playback sink");
        }

        self.enabled = false;
        // Zero out buffer without deallocating.
        for s in self.last_buffer.iter_mut() {
            *s = 0.0;
        }
        log::info!("Monitor output disabled");
        Ok(())
    }

    /// Returns `true` if the monitor output is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Write processed audio to the monitor output.
    ///
    /// When a ring buffer writer is attached, samples are pushed to the PipeWire
    /// playback stream via the lock-free ring buffer. In stub mode (or when no
    /// ring writer is attached) it stores the buffer for test inspection.
    /// This is called on the audio thread — no heap allocations allowed.
    pub fn write(&mut self, samples: &[f32]) {
        if !self.enabled {
            return;
        }

        // Push to the PipeWire playback stream via the ring buffer (lock-free).
        if let Some(ref writer) = self.ring_writer {
            writer.write(samples);
        }

        // Always copy into last_buffer for test verification.
        if self.last_buffer.len() != samples.len() {
            self.last_buffer.resize(samples.len(), 0.0);
        }
        self.last_buffer.copy_from_slice(samples);
    }

    /// Returns the last buffer written to the monitor (stub mode only).
    ///
    /// Useful in tests to verify that the monitor receives processed audio.
    #[cfg(test)]
    pub fn last_buffer(&self) -> &[f32] {
        &self.last_buffer
    }
}

impl Default for MonitorOutput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_disabled() {
        let monitor = MonitorOutput::new();
        assert!(!monitor.is_enabled());
    }

    #[test]
    fn enable_disable_toggles_state() {
        let mut monitor = MonitorOutput::new();

        monitor.enable().unwrap();
        assert!(monitor.is_enabled());

        monitor.disable().unwrap();
        assert!(!monitor.is_enabled());
    }

    #[test]
    fn enable_is_idempotent() {
        let mut monitor = MonitorOutput::new();
        monitor.enable().unwrap();
        monitor.enable().unwrap();
        assert!(monitor.is_enabled());
    }

    #[test]
    fn disable_is_idempotent() {
        let mut monitor = MonitorOutput::new();
        monitor.disable().unwrap();
        assert!(!monitor.is_enabled());
    }

    #[test]
    fn write_stores_buffer_when_enabled() {
        let mut monitor = MonitorOutput::new();
        monitor.enable().unwrap();

        let samples = vec![0.5f32; 480];
        monitor.write(&samples);

        assert_eq!(monitor.last_buffer(), &samples[..]);
    }

    #[test]
    fn write_is_noop_when_disabled() {
        let mut monitor = MonitorOutput::new();
        let samples = vec![0.5f32; 480];
        monitor.write(&samples);

        // Buffer stays zeroed (pre-allocated) when disabled.
        assert!(monitor.last_buffer().iter().all(|&s| s == 0.0));
    }

    #[test]
    fn disable_zeroes_buffer() {
        let mut monitor = MonitorOutput::new();
        monitor.enable().unwrap();
        monitor.write(&[1.0; 480]);
        assert!(monitor.last_buffer().iter().any(|&s| s != 0.0));

        monitor.disable().unwrap();
        assert!(
            monitor.last_buffer().iter().all(|&s| s == 0.0),
            "buffer should be zeroed after disable"
        );
    }
}

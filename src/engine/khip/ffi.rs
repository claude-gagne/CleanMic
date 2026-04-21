//! FFI declarations for the Khip noise-cancellation library.
//!
//! Khip processes audio at **32 kHz**, 480 samples per call.
//! The pipeline runs at 48 kHz, so the engine performs 48↔32 kHz conversion
//! internally (720 samples at 48 kHz ↔ 480 samples at 32 kHz).

/// Opaque Khip session handle.
#[repr(C)]
pub struct KhipSession {
    _opaque: [u8; 0],
}

/// Khip's native buffer size (480 samples at 32 kHz = 15 ms per call).
pub const BUF_SIZE: usize = 480;

/// Khip's required sample rate.
pub const SAMPLE_RATE: u32 = 32_000;

/// Equivalent buffer size in the pipeline's 48 kHz domain.
/// 480 × (48000 / 32000) = 720 samples.
pub const BUF_SIZE_48K: usize = BUF_SIZE * 48_000 / 32_000;

/// Function pointers resolved from `libkhip.so` at runtime.
///
/// All pointers are valid for the lifetime of the [`libloading::Library`]
/// stored alongside them in [`super::KhipEngine`].
pub struct KhipFunctions {
    /// Create a new Khip session. Returns null on allocation failure.
    pub create: unsafe extern "C" fn() -> *mut KhipSession,

    /// Destroy a session and release all resources.
    pub destroy: unsafe extern "C" fn(*mut KhipSession),

    /// Reset internal GRU state (forget context between segments).
    pub reset: unsafe extern "C" fn(*mut KhipSession),

    /// Process one 480-sample frame at 32 kHz.
    ///
    /// `attenuation`: 0.0 = bypass (no suppression), 1.0 = full cancellation.
    pub process: unsafe extern "C" fn(
        *mut KhipSession,
        input: *const f32,
        output: *mut f32,
        attenuation: f32,
    ),
}

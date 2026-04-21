//! CleanMic — a Linux desktop app providing a clean virtual microphone
//! with switchable noise suppression engines (RNNoise, DeepFilterNet, Khip).
//!
//! This crate contains the core logic for the audio pipeline, engine adapters,
//! configuration persistence, and optional GUI/tray components.

pub mod app;
pub mod audio;
pub mod config;
pub mod engine;
pub mod instance_lock;

pub mod pipewire;

pub mod ui;

pub mod tray;

pub mod autostart;

pub mod updater;

/// Shorthand for `gettextrs::gettext()` — translates a string literal at runtime.
///
/// Falls back to the original string if no translation is found, so this macro
/// is safe to use even when no locale files are present.
#[macro_export]
macro_rules! tr {
    ($s:literal) => {
        gettextrs::gettext($s)
    };
}

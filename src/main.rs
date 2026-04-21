//! CleanMic entry point.
//!
//! Sets up logging via `env_logger` (respects `RUST_LOG` environment variable,
//! defaults to `info` level) and delegates to [`cleanmic::app::run`] for the
//! full application lifecycle.

/// Constrain math-library thread pools before any shared library loads.
///
/// FFTW and OpenBLAS default to spawning O(num_cpus) threads, which causes
/// the Khip engine to saturate all CPU cores. Setting these vars to 1 enforces
/// single-threaded operation, which is always correct for 480-sample real-time audio.
fn limit_thread_pools() {
    // SAFETY: called before any threads are spawned; no other thread is
    // reading or writing these environment variables concurrently.
    unsafe {
        std::env::set_var("OPENBLAS_NUM_THREADS", "1");
        std::env::set_var("OMP_NUM_THREADS", "1");
        std::env::set_var("FFTW_NUM_THREADS", "1");
    }
    log::debug!(
        "thread pools limited: OPENBLAS_NUM_THREADS=1 OMP_NUM_THREADS=1 FFTW_NUM_THREADS=1"
    );
}

fn main() -> anyhow::Result<()> {
    limit_thread_pools();

    // Initialize logging. Default to "info" level if RUST_LOG is not set.
    //
    // Force the `df` (DeepFilterNet) crate's own logger to `error` level:
    // its `df::tract` layer emits "Possible clipping detected" at info/warn
    // for every 10 ms frame that saturates the input, which on a hot mic
    // at Medium/High strength fires ~100 times a second. env_logger writes
    // synchronously to stderr; that volume of log writes is enough to block
    // the audio thread and produce audible stuttering. Suppressing them here
    // keeps our own info-level logs intact while silencing the per-frame spam.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .filter_module("df", log::LevelFilter::Error)
        .init();

    // Install a global panic hook that logs the panic info and attempts a
    // best-effort GNOME notification (D-10). The hook must never panic itself.
    std::panic::set_hook(Box::new(|info| {
        log::error!("CleanMic panicked: {}", info);
        // Best-effort GNOME notification — GTK may not be initialized yet.
        #[cfg(feature = "gui")]
        if let Some(app) = gtk4::gio::Application::default() {
            use gtk4::prelude::ApplicationExt;
            let notif = gtk4::gio::Notification::new("CleanMic crashed unexpectedly");
            notif.set_body(Some(&format!("{}", info)));
            app.send_notification(Some("crash"), &notif);
        }
    }));

    log::info!("CleanMic v{} starting", env!("CARGO_PKG_VERSION"));
    cleanmic::app::run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_pool_vars_are_set_to_one() {
        limit_thread_pools();
        assert_eq!(std::env::var("OPENBLAS_NUM_THREADS").as_deref(), Ok("1"));
        assert_eq!(std::env::var("OMP_NUM_THREADS").as_deref(), Ok("1"));
        assert_eq!(std::env::var("FFTW_NUM_THREADS").as_deref(), Ok("1"));
    }
}

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
    // Hand-rolled --version / --help handlers. Run before logging init so
    // their stdout is not polluted by env_logger INFO lines. Mirrors the
    // existing --autostart flag-parsing pattern (no clap dep).
    let args: Vec<String> = std::env::args().collect();
    if parse_version_flag(&args) {
        println!("cleanmic {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }
    if parse_help_flag(&args) {
        println!(
            "CleanMic — clean virtual microphone with switchable noise suppression engines.\n\
             \n\
             USAGE:\n\
             \x20   cleanmic [FLAGS]\n\
             \n\
             FLAGS:\n\
             \x20   -h, --help         Print this help message and exit.\n\
             \x20   -V, --version      Print version information and exit.\n\
             \x20       --autostart    Internal flag set by the autostart .desktop file —\n\
             \x20                      signals that the binary was launched by the user's\n\
             \x20                      session autostart hook (vs a manual launcher click).\n\
             \n\
             For documentation, visit: https://github.com/claude-gagne/CleanMic"
        );
        std::process::exit(0);
    }

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

    // Hand-rolled `--autostart` detection to keep the project lean (no clap
    // dep). `--autostart` is the only flag the autostart `.desktop` Exec line
    // appends — it is the signal that the binary was launched by the system's
    // autostart hook (vs. a manual launcher click) and should switch to the
    // hide-if-tray policy in `run_with_gui`.
    let launched_via_autostart = parse_autostart_flag(std::env::args());
    if launched_via_autostart {
        log::info!("Launched via --autostart");
    }

    cleanmic::app::run(launched_via_autostart)
}

/// Return `true` if `--autostart` is present anywhere in the args iterator.
///
/// Matches the exact arg `--autostart` with no value form. Lifted into a
/// helper purely so it can be unit-tested without invoking the GTK runtime.
fn parse_autostart_flag<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|a| a.as_ref() == "--autostart")
}

/// Return `true` if `--version` or `-V` is present anywhere in the args iterator.
///
/// Matches the exact args `--version` or `-V` with no value form. Lifted into a
/// helper purely so it can be unit-tested without invoking the GTK runtime.
fn parse_version_flag<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|a| {
        let s = a.as_ref();
        s == "--version" || s == "-V"
    })
}

/// Return `true` if `--help` or `-h` is present anywhere in the args iterator.
///
/// Matches the exact args `--help` or `-h` with no value form. Lifted into a
/// helper purely so it can be unit-tested without invoking the GTK runtime.
fn parse_help_flag<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|a| {
        let s = a.as_ref();
        s == "--help" || s == "-h"
    })
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

    #[test]
    fn parse_autostart_flag_detects_the_flag() {
        assert!(parse_autostart_flag(["cleanmic", "--autostart"]));
    }

    #[test]
    fn parse_autostart_flag_returns_false_without_the_flag() {
        assert!(!parse_autostart_flag(["cleanmic"]));
    }

    #[test]
    fn parse_autostart_flag_does_not_match_substrings() {
        // `--autostart-something` should NOT match `--autostart`.
        assert!(!parse_autostart_flag(["cleanmic", "--autostart-foo"]));
        assert!(!parse_autostart_flag(["cleanmic", "autostart"]));
    }

    #[test]
    fn parse_autostart_flag_finds_flag_among_other_args() {
        // Future-proofing: even if we add other flags later, --autostart is found.
        assert!(parse_autostart_flag(["cleanmic", "--foo", "--autostart", "--bar"]));
    }

    #[test]
    fn parse_version_flag_detects_long_form() {
        assert!(parse_version_flag(["cleanmic", "--version"]));
    }

    #[test]
    fn parse_version_flag_detects_short_form() {
        assert!(parse_version_flag(["cleanmic", "-V"]));
    }

    #[test]
    fn parse_version_flag_returns_false_without() {
        assert!(!parse_version_flag(["cleanmic"]));
    }

    #[test]
    fn parse_version_flag_does_not_match_substrings() {
        assert!(!parse_version_flag(["cleanmic", "--versions"]));
        assert!(!parse_version_flag(["cleanmic", "version"]));
    }

    #[test]
    fn parse_help_flag_detects_long_form() {
        assert!(parse_help_flag(["cleanmic", "--help"]));
    }

    #[test]
    fn parse_help_flag_detects_short_form() {
        assert!(parse_help_flag(["cleanmic", "-h"]));
    }

    #[test]
    fn parse_help_flag_returns_false_without() {
        assert!(!parse_help_flag(["cleanmic"]));
    }
}

//! First-run welcome module.
//!
//! Detects whether this is the user's first launch (no config file exists)
//! and provides helpers to guide them through initial setup: selecting
//! "CleanMic" as their microphone in browser-based conferencing apps.

use crate::config::Config;

/// The URL opened by [`open_mic_test`] for the user to verify their setup.
pub const MIC_TEST_URL: &str = "https://webcammictest.com/check-microphone.html";

/// Returns `true` if this is the first time CleanMic is launched (no config
/// file on disk).
pub fn is_first_run() -> bool {
    Config::is_first_run().unwrap_or(true)
}

/// Log the first-run welcome message with instructions for the user.
pub fn log_first_run_instructions() {
    log::info!("Welcome to CleanMic! This appears to be your first run.");
    log::info!(
        "To use CleanMic: in Teams, Chrome, Meet, or Discord, select \"CleanMic\" as your microphone."
    );
    log::info!("You can test your setup by visiting: {}", MIC_TEST_URL);
}

/// Open a microphone test page in the default browser using `xdg-open`.
///
/// Returns `Ok(())` if the command was spawned successfully. The browser
/// opening is best-effort — failure is logged but not fatal.
pub fn open_mic_test() -> anyhow::Result<()> {
    log::info!("Opening mic test page: {}", MIC_TEST_URL);
    std::process::Command::new("xdg-open")
        .arg(MIC_TEST_URL)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to open browser with xdg-open: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn first_run_detected_when_config_absent() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let path = tmp.path().join("cleanmic").join("config.toml");
        assert!(
            Config::is_first_run_at(&path),
            "Should detect first run when config does not exist"
        );
    }

    #[test]
    fn subsequent_run_detected_when_config_exists() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let path = tmp.path().join("cleanmic").join("config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        let config = Config {
            enabled: true,
            ..Config::default()
        };
        config.save_to(&path).unwrap();

        assert!(
            !Config::is_first_run_at(&path),
            "Should detect subsequent run when config exists"
        );
    }

    #[test]
    fn subsequent_run_with_enabled_true() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let path = tmp.path().join("cleanmic").join("config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        let config = Config {
            enabled: true,
            ..Config::default()
        };
        config.save_to(&path).unwrap();

        let loaded = Config::load_from(&path).unwrap();
        assert!(loaded.enabled, "Config should have enabled=true");
        assert!(!Config::is_first_run_at(&path), "Should not be first run");
    }

    #[test]
    fn mic_test_url_is_valid() {
        assert!(MIC_TEST_URL.starts_with("https://"));
    }
}

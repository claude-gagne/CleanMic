//! Autostart management and desktop integration.
//!
//! Creates or removes an XDG autostart desktop entry in
//! `~/.config/autostart/com.cleanmic.CleanMic.desktop` so CleanMic launches
//! automatically on login when the user enables the autostart toggle.
//!
//! Also installs the desktop entry in `~/.local/share/applications/` and the
//! app icon in `~/.local/share/icons/` for app menu visibility and dock icon
//! matching. `install_desktop_integration()` is called on every startup so
//! fresh AppImage users get a correct dock icon without enabling autostart.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const APP_ID: &str = "com.cleanmic.CleanMic";
const DESKTOP_FILENAME: &str = "com.cleanmic.CleanMic.desktop";

/// Return the XDG autostart directory (`~/.config/autostart/`).
fn default_autostart_dir() -> PathBuf {
    let config_home = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".config")
        });
    config_home.join("autostart")
}

/// Return the XDG applications directory (`~/.local/share/applications/`).
fn default_applications_dir() -> PathBuf {
    let data_home = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local").join("share")
        });
    data_home.join("applications")
}

/// Generate the content of the `.desktop` file.
///
/// When running from an AppImage, `$APPIMAGE` is set by the runtime to the
/// path of the `.AppImage` file on disk — the persistent, correct path to use.
/// `current_exe()` would resolve to the ephemeral squashfs mount point
/// (`/tmp/.mount_*/usr/bin/cleanmic`) which disappears once the app exits,
/// causing "not found in path" errors when the launcher entry is clicked later.
fn desktop_entry_content() -> Result<String> {
    // Prefer $APPIMAGE (persistent .AppImage path) over current_exe() (ephemeral mount).
    let exec = std::env::var("APPIMAGE")
        .ok()
        .filter(|p| Path::new(p).exists())
        .map(Ok)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .context("failed to determine current executable path")
                .map(|p| p.display().to_string())
        })?;
    Ok(desktop_entry_content_with_exec(&exec))
}

/// Generate desktop entry content with a specific `Exec` path.
fn desktop_entry_content_with_exec(exec_path: &str) -> String {
    format!(
        "\
[Desktop Entry]
Type=Application
Name=CleanMic
Comment=Noise-free virtual microphone
Exec={exec_path}
Icon={APP_ID}
Categories=Audio;AudioVideo;
StartupNotify=false
StartupWMClass={APP_ID}
Terminal=false
"
    )
}

/// Install the desktop entry in a given directory, creating parent dirs as
/// needed.
fn install_desktop_entry(dir: &Path, content: &str) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create directory {}", dir.display()))?;
    let path = dir.join(DESKTOP_FILENAME);
    fs::write(&path, content)
        .with_context(|| format!("failed to write desktop entry at {}", path.display()))?;
    Ok(())
}

/// Return the XDG icons directory for the hicolor scalable apps slot.
/// (`~/.local/share/icons/hicolor/scalable/apps/`)
fn default_icons_dir() -> PathBuf {
    let data_home = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local").join("share")
        });
    data_home
        .join("icons")
        .join("hicolor")
        .join("scalable")
        .join("apps")
}

/// Locate the bundled SVG icon.
///
/// When running from an AppImage `$APPDIR` is set by the runtime and the icon
/// lives at `$APPDIR/usr/share/icons/hicolor/scalable/apps/com.cleanmic.CleanMic.svg`.
/// Falls back to `None` if the variable is unset (e.g. during `cargo run`).
fn bundled_icon_path() -> Option<PathBuf> {
    let appdir = std::env::var("APPDIR").ok()?;
    let path = PathBuf::from(appdir)
        .join("usr/share/icons/hicolor/scalable/apps")
        .join(format!("{APP_ID}.svg"));
    if path.exists() { Some(path) } else { None }
}

/// Install the app icon to `~/.local/share/icons/hicolor/scalable/apps/`.
///
/// Only copies the icon when running from an AppImage (i.e. `$APPDIR` is set
/// and the bundled icon file exists).  Silently skips when running from a
/// system install or `cargo run` because the icon is already in the system
/// theme.
fn install_icon(icons_dir: &Path) -> Result<()> {
    let Some(src) = bundled_icon_path() else {
        log::debug!("Skipping icon install: not running from AppImage or icon not found");
        return Ok(());
    };

    fs::create_dir_all(icons_dir)
        .with_context(|| format!("failed to create icons directory {}", icons_dir.display()))?;

    let dest = icons_dir.join(format!("{APP_ID}.svg"));
    fs::copy(&src, &dest)
        .with_context(|| format!("failed to copy icon from {} to {}", src.display(), dest.display()))?;

    log::debug!("Icon installed to {}", dest.display());
    Ok(())
}

/// Install the `.desktop` file and app icon into the user's XDG data directories
/// so that GNOME can match the running window to the correct icon and show it
/// in the application menu.
///
/// This is called on every startup (before the GTK window is shown) so that
/// fresh AppImage users get correct dock icon matching without having to enable
/// autostart first.  The function is idempotent and safe to call repeatedly.
///
/// Files written:
/// - `~/.local/share/applications/com.cleanmic.CleanMic.desktop`
/// - `~/.local/share/icons/hicolor/scalable/apps/com.cleanmic.CleanMic.svg`
///   (AppImage only; skipped for system installs where icon is in system theme)
pub fn install_desktop_integration() -> Result<()> {
    install_desktop_integration_in(&default_applications_dir(), &default_icons_dir())
}

/// Enable autostart by creating the desktop entry in both
/// `~/.config/autostart/` and `~/.local/share/applications/`.
///
/// This function is idempotent — calling it when already enabled simply
/// overwrites the files with the current content.
pub fn enable_autostart() -> Result<()> {
    enable_autostart_in(&default_autostart_dir(), &default_applications_dir())?;
    log::info!("autostart enabled");
    Ok(())
}

/// Disable autostart by removing the desktop entry from
/// `~/.config/autostart/`. The applications entry is left in place so the app
/// remains visible in the app menu.
pub fn disable_autostart() -> Result<()> {
    disable_autostart_in(&default_autostart_dir())
}

/// Check whether autostart is currently enabled by testing for the existence of
/// the desktop entry in `~/.config/autostart/`.
pub fn is_autostart_enabled() -> Result<bool> {
    Ok(is_autostart_enabled_in(&default_autostart_dir()))
}

// -- Internal helpers used by both the public API and tests ------------------

fn install_desktop_integration_in(applications_dir: &Path, icons_dir: &Path) -> Result<()> {
    let content = desktop_entry_content()?;
    install_desktop_entry(applications_dir, &content)?;
    install_icon(icons_dir)?;
    log::info!("Desktop integration installed");
    Ok(())
}

fn enable_autostart_in(autostart_dir: &Path, applications_dir: &Path) -> Result<()> {
    let content = desktop_entry_content()?;
    install_desktop_entry(autostart_dir, &content)?;
    install_desktop_entry(applications_dir, &content)?;
    Ok(())
}

fn disable_autostart_in(autostart_dir: &Path) -> Result<()> {
    let path = autostart_dir.join(DESKTOP_FILENAME);
    match fs::remove_file(&path) {
        Ok(()) => {
            log::info!("autostart disabled (removed {})", path.display());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::debug!("autostart already disabled (file not found)");
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to remove {}", path.display()));
        }
    }
    Ok(())
}

fn is_autostart_enabled_in(autostart_dir: &Path) -> bool {
    autostart_dir.join(DESKTOP_FILENAME).exists()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Create temp dirs that act as XDG_CONFIG_HOME/autostart and
    /// XDG_DATA_HOME/applications, returning the tempdir handle (keep alive),
    /// the autostart dir path, and the applications dir path.
    fn setup_temp_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let autostart = tmp.path().join("config").join("autostart");
        let applications = tmp.path().join("data").join("applications");
        (tmp, autostart, applications)
    }

    #[test]
    fn enable_creates_desktop_files() {
        let (_tmp, autostart, applications) = setup_temp_dirs();
        assert!(!autostart.join(DESKTOP_FILENAME).exists());
        assert!(!applications.join(DESKTOP_FILENAME).exists());

        enable_autostart_in(&autostart, &applications).expect("enable_autostart failed");

        assert!(autostart.join(DESKTOP_FILENAME).exists());
        assert!(applications.join(DESKTOP_FILENAME).exists());
    }

    #[test]
    fn disable_removes_autostart_file() {
        let (_tmp, autostart, applications) = setup_temp_dirs();
        enable_autostart_in(&autostart, &applications).expect("enable failed");
        assert!(autostart.join(DESKTOP_FILENAME).exists());

        disable_autostart_in(&autostart).expect("disable_autostart failed");

        assert!(!autostart.join(DESKTOP_FILENAME).exists());
        // Applications entry should remain.
        assert!(applications.join(DESKTOP_FILENAME).exists());
    }

    #[test]
    fn desktop_file_content_has_required_fields() {
        let (_tmp, autostart, applications) = setup_temp_dirs();
        enable_autostart_in(&autostart, &applications).expect("enable failed");

        let content = fs::read_to_string(autostart.join(DESKTOP_FILENAME))
            .expect("failed to read desktop file");

        assert!(content.contains("[Desktop Entry]"));
        assert!(content.contains("Type=Application"));
        assert!(content.contains("Name=CleanMic"));
        assert!(content.contains("Comment=Noise-free virtual microphone"));
        assert!(content.contains("Exec="));
        assert!(content.contains("Icon=com.cleanmic.CleanMic"));
        assert!(content.contains("Categories=Audio;AudioVideo;"));
    }

    #[test]
    fn is_autostart_enabled_detects_presence_and_absence() {
        let (_tmp, autostart, applications) = setup_temp_dirs();

        assert!(!is_autostart_enabled_in(&autostart));

        enable_autostart_in(&autostart, &applications).expect("enable failed");
        assert!(is_autostart_enabled_in(&autostart));

        disable_autostart_in(&autostart).expect("disable failed");
        assert!(!is_autostart_enabled_in(&autostart));
    }

    #[test]
    fn enable_is_idempotent() {
        let (_tmp, autostart, applications) = setup_temp_dirs();

        enable_autostart_in(&autostart, &applications).expect("first enable failed");
        let first_content = fs::read_to_string(autostart.join(DESKTOP_FILENAME))
            .expect("failed to read desktop file");

        enable_autostart_in(&autostart, &applications).expect("second enable failed");
        let second_content = fs::read_to_string(autostart.join(DESKTOP_FILENAME))
            .expect("failed to read desktop file");

        assert_eq!(first_content, second_content);
        assert!(autostart.join(DESKTOP_FILENAME).exists());
    }

    #[test]
    fn disable_when_not_enabled_is_ok() {
        let (_tmp, autostart, _applications) = setup_temp_dirs();

        // Should not error even if the file doesn't exist.
        disable_autostart_in(&autostart).expect("disable when not enabled should succeed");
    }

    #[test]
    fn desktop_entry_content_with_exec_generates_valid_entry() {
        let content = desktop_entry_content_with_exec("/usr/bin/cleanmic");
        assert!(content.contains("Exec=/usr/bin/cleanmic"));
        assert!(content.starts_with("[Desktop Entry]"));
    }

    #[test]
    fn install_desktop_integration_creates_applications_entry() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let applications = tmp.path().join("data").join("applications");
        let icons = tmp.path().join("data").join("icons").join("hicolor").join("scalable").join("apps");

        // APPDIR is not set in tests, so icon install is skipped silently.
        install_desktop_integration_in(&applications, &icons).expect("install failed");

        assert!(applications.join(DESKTOP_FILENAME).exists(),
            "desktop file should be installed to applications dir");
    }

    #[test]
    fn install_desktop_integration_is_idempotent() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let applications = tmp.path().join("data").join("applications");
        let icons = tmp.path().join("data").join("icons").join("hicolor").join("scalable").join("apps");

        install_desktop_integration_in(&applications, &icons).expect("first install failed");
        let content_first = fs::read_to_string(applications.join(DESKTOP_FILENAME))
            .expect("failed to read desktop file");

        install_desktop_integration_in(&applications, &icons).expect("second install failed");
        let content_second = fs::read_to_string(applications.join(DESKTOP_FILENAME))
            .expect("failed to read desktop file after second install");

        assert_eq!(content_first, content_second, "idempotent: both installs should produce the same file");
    }

    #[test]
    fn install_desktop_integration_desktop_entry_has_required_fields() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let applications = tmp.path().join("data").join("applications");
        let icons = tmp.path().join("data").join("icons");

        install_desktop_integration_in(&applications, &icons).expect("install failed");

        let content = fs::read_to_string(applications.join(DESKTOP_FILENAME))
            .expect("failed to read desktop file");

        assert!(content.contains("[Desktop Entry]"));
        assert!(content.contains("Icon=com.cleanmic.CleanMic"));
        assert!(content.contains("StartupWMClass=com.cleanmic.CleanMic"));
        assert!(content.contains("StartupNotify=false"));
    }
}

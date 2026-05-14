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
///
/// Used for the **manual launcher** entry that lives in
/// `~/.local/share/applications/`. Clicking the launcher icon should always
/// show the main window — so the `Exec=` line has no `--autostart` argument.
/// See [`desktop_entry_content_with_exec_autostart`] for the autostart variant.
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

/// Generate desktop entry content for the **autostart** entry that lives in
/// `~/.config/autostart/`.
///
/// Identical to [`desktop_entry_content_with_exec`] except the `Exec=` line
/// appends ` --autostart`. The CLI flag is parsed in `src/main.rs` and
/// threaded into `run_with_gui` so the activate closure can switch to the
/// hide-if-tray policy (see [`crate::app::should_hide_main_window_on_autostart`]).
///
/// Why a split helper: if the manual launcher entry also got `--autostart`,
/// clicking the app-menu icon would fire the autostart hidden-launch policy
/// (wrong UX — user expected the window to appear because they clicked).
fn desktop_entry_content_with_exec_autostart(exec_path: &str) -> String {
    format!(
        "\
[Desktop Entry]
Type=Application
Name=CleanMic
Comment=Noise-free virtual microphone
Exec={exec_path} --autostart
Icon={APP_ID}
Categories=Audio;AudioVideo;
StartupNotify=false
StartupWMClass={APP_ID}
Terminal=false
"
    )
}

/// Resolve the `Exec=` path the same way [`desktop_entry_content`] does, then
/// build the autostart-flavoured entry. See [`desktop_entry_content`] for the
/// `$APPIMAGE` vs `current_exe()` rationale.
fn desktop_entry_content_autostart() -> Result<String> {
    let exec = std::env::var("APPIMAGE")
        .ok()
        .filter(|p| Path::new(p).exists())
        .map(Ok)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .context("failed to determine current executable path")
                .map(|p| p.display().to_string())
        })?;
    Ok(desktop_entry_content_with_exec_autostart(&exec))
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
    fs::copy(&src, &dest).with_context(|| {
        format!(
            "failed to copy icon from {} to {}",
            src.display(),
            dest.display()
        )
    })?;

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

/// Reconcile the on-disk autostart state with the user's stated intent (config).
/// If they disagree, mutate the filesystem to match config. Logged at warn level
/// because drift is unexpected; the fix self-heals.
pub fn reconcile(config_value: bool, actual: bool) -> Result<()> {
    if config_value == actual {
        return Ok(());
    }
    log::warn!(
        "autostart drift detected (config={}, filesystem={}). Reconciling to match config.",
        config_value,
        actual
    );
    if config_value {
        enable_autostart()
    } else {
        disable_autostart()
    }
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
    // The autostart entry must include `--autostart` so the binary can
    // distinguish system-fired autostart launches (hide-if-tray policy)
    // from manual launcher clicks (always show window). The applications
    // entry deliberately does NOT include `--autostart` for the same reason.
    // See `desktop_entry_content_with_exec_autostart` for the rationale.
    let autostart_content = desktop_entry_content_autostart()?;
    let applications_content = desktop_entry_content()?;
    install_desktop_entry(autostart_dir, &autostart_content)?;
    install_desktop_entry(applications_dir, &applications_content)?;
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

#[cfg(test)]
fn reconcile_in(config_value: bool, autostart_dir: &Path, applications_dir: &Path) -> Result<()> {
    let actual = is_autostart_enabled_in(autostart_dir);
    if config_value == actual {
        return Ok(());
    }
    if config_value {
        enable_autostart_in(autostart_dir, applications_dir)
    } else {
        disable_autostart_in(autostart_dir)
    }
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
        // Autostart entry must include `--autostart` so the binary applies
        // the hide-if-tray policy (260508-k7q).
        assert!(
            content.contains("--autostart"),
            "autostart entry must include --autostart arg; got:\n{content}"
        );
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
    fn reconcile_noop_when_both_true() {
        let (_tmp, autostart, applications) = setup_temp_dirs();
        enable_autostart_in(&autostart, &applications).expect("enable failed");
        assert!(is_autostart_enabled_in(&autostart));

        reconcile_in(true, &autostart, &applications).expect("reconcile failed");

        // No change: file still present.
        assert!(is_autostart_enabled_in(&autostart));
    }

    #[test]
    fn reconcile_noop_when_both_false() {
        let (_tmp, autostart, applications) = setup_temp_dirs();
        assert!(!is_autostart_enabled_in(&autostart));

        reconcile_in(false, &autostart, &applications).expect("reconcile failed");

        // No change: file still absent.
        assert!(!is_autostart_enabled_in(&autostart));
    }

    #[test]
    fn reconcile_enables_when_config_true_filesystem_false() {
        let (_tmp, autostart, applications) = setup_temp_dirs();
        assert!(!is_autostart_enabled_in(&autostart));

        reconcile_in(true, &autostart, &applications).expect("reconcile failed");

        assert!(
            is_autostart_enabled_in(&autostart),
            "reconcile should have enabled autostart to match config"
        );
    }

    #[test]
    fn reconcile_disables_when_config_false_filesystem_true() {
        let (_tmp, autostart, applications) = setup_temp_dirs();
        // Simulate the drift bug: filesystem has the entry, config doesn't.
        enable_autostart_in(&autostart, &applications).expect("enable failed");
        assert!(is_autostart_enabled_in(&autostart));

        reconcile_in(false, &autostart, &applications).expect("reconcile failed");

        assert!(
            !is_autostart_enabled_in(&autostart),
            "reconcile should have removed the orphan autostart entry"
        );
    }

    #[test]
    fn desktop_entry_content_with_exec_generates_valid_entry() {
        let content = desktop_entry_content_with_exec("/usr/bin/cleanmic");
        assert!(content.contains("Exec=/usr/bin/cleanmic"));
        assert!(content.starts_with("[Desktop Entry]"));
    }

    /// The autostart-flavoured helper (~/.config/autostart/) MUST include
    /// `--autostart` on the Exec= line so the binary knows it was launched
    /// by the autostart hook and applies the hide-if-tray policy.
    #[test]
    fn autostart_entry_includes_autostart_arg() {
        let content = desktop_entry_content_with_exec_autostart("/usr/bin/cleanmic");
        assert!(
            content.contains("Exec=/usr/bin/cleanmic --autostart"),
            "expected single-space-separated --autostart; got:\n{content}"
        );
        // Must still produce a valid Desktop Entry stanza.
        assert!(content.starts_with("[Desktop Entry]"));
        assert!(content.contains("Icon=com.cleanmic.CleanMic"));
        assert!(content.contains("StartupWMClass=com.cleanmic.CleanMic"));
    }

    /// The applications-entry helper (~/.local/share/applications/) MUST NOT
    /// include `--autostart` — clicking the launcher icon is a manual
    /// invocation that should always show the main window.
    #[test]
    fn applications_entry_does_not_include_autostart_arg() {
        let content = desktop_entry_content_with_exec("/usr/bin/cleanmic");
        // Exec line is followed by a newline (no trailing args).
        assert!(
            content.contains("Exec=/usr/bin/cleanmic\n"),
            "expected bare Exec line with no args; got:\n{content}"
        );
        assert!(
            !content.contains("--autostart"),
            "applications entry must not include --autostart; got:\n{content}"
        );
    }

    #[test]
    fn install_desktop_integration_creates_applications_entry() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let applications = tmp.path().join("data").join("applications");
        let icons = tmp
            .path()
            .join("data")
            .join("icons")
            .join("hicolor")
            .join("scalable")
            .join("apps");

        // APPDIR is not set in tests, so icon install is skipped silently.
        install_desktop_integration_in(&applications, &icons).expect("install failed");

        assert!(
            applications.join(DESKTOP_FILENAME).exists(),
            "desktop file should be installed to applications dir"
        );
    }

    #[test]
    fn install_desktop_integration_is_idempotent() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let applications = tmp.path().join("data").join("applications");
        let icons = tmp
            .path()
            .join("data")
            .join("icons")
            .join("hicolor")
            .join("scalable")
            .join("apps");

        install_desktop_integration_in(&applications, &icons).expect("first install failed");
        let content_first = fs::read_to_string(applications.join(DESKTOP_FILENAME))
            .expect("failed to read desktop file");

        install_desktop_integration_in(&applications, &icons).expect("second install failed");
        let content_second = fs::read_to_string(applications.join(DESKTOP_FILENAME))
            .expect("failed to read desktop file after second install");

        assert_eq!(
            content_first, content_second,
            "idempotent: both installs should produce the same file"
        );
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
        // Sanity-check the split (260508-k7q): the applications entry must
        // NOT carry --autostart — that's reserved for the autostart entry only.
        assert!(
            !content.contains("--autostart"),
            "applications entry leaked --autostart; got:\n{content}"
        );
    }
}

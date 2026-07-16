//! Background update checker.
//!
//! Queries the GitHub Releases API to detect whether a newer CleanMic version
//! is available. Feature-gated behind `updater` — the rest of the crate can
//! call the stub when the feature is off.
//!
//! Per D-09: queries https://api.github.com/repos/claude-gagne/CleanMic/releases/latest
//! Per D-10: uses the `semver` crate to compare versions correctly
//! Per D-11: uses `ureq` (sync, no async runtime needed)
//! Per D-03: network failures return Ok(None) — silently ignored

/// GitHub Releases API URL for CleanMic.
const RELEASES_API: &str = "https://api.github.com/repos/claude-gagne/CleanMic/releases/latest";

/// GitHub Releases page URL opened by the "Download" banner button (per D-08).
pub const RELEASES_PAGE_URL: &str = "https://github.com/claude-gagne/CleanMic/releases/latest";

/// Check GitHub Releases for a version newer than the running binary.
///
/// Returns:
/// - `Ok(None)` — already on the latest version, or network/parse failure
///   (network failures are logged at debug level per D-03)
/// - `Ok(Some(tag))` — a newer version exists; `tag` is the raw tag_name
///   string from GitHub (e.g. "v1.2.0") for display in the UI
#[cfg(feature = "updater")]
pub fn check_for_update() -> anyhow::Result<Option<String>> {
    use semver::Version;

    let current_str = env!("CARGO_PKG_VERSION");
    let current = match Version::parse(current_str) {
        Ok(v) => v,
        Err(e) => {
            log::debug!("updater: failed to parse current version '{current_str}': {e}");
            return Ok(None);
        }
    };

    let mut response = match ureq::get(RELEASES_API)
        .header("User-Agent", &format!("CleanMic/{current_str}"))
        .call()
    {
        Ok(r) => r,
        Err(e) => {
            log::debug!("updater: GitHub API request failed: {e}");
            return Ok(None);
        }
    };

    let body: serde_json::Value = match response.body_mut().read_json() {
        Ok(v) => v,
        Err(e) => {
            log::debug!("updater: failed to parse GitHub API response: {e}");
            return Ok(None);
        }
    };

    let tag = match body.get("tag_name").and_then(|v| v.as_str()) {
        Some(t) => t.to_owned(),
        None => {
            log::debug!("updater: tag_name missing from GitHub API response");
            return Ok(None);
        }
    };

    // Strip leading 'v' for semver parsing ("v1.2.3" -> "1.2.3")
    let remote_str = tag.trim_start_matches('v');
    let remote = match Version::parse(remote_str) {
        Ok(v) => v,
        Err(e) => {
            log::debug!("updater: failed to parse remote version '{remote_str}': {e}");
            return Ok(None);
        }
    };

    if remote > current {
        log::info!("Update available: {tag} (current: v{current})");
        Ok(Some(tag))
    } else {
        log::debug!("updater: already up to date (remote={remote}, current={current})");
        Ok(None)
    }
}

/// Stub when the `updater` feature is disabled — always returns no update.
#[cfg(not(feature = "updater"))]
pub fn check_for_update() -> anyhow::Result<Option<String>> {
    Ok(None)
}

/// Decide whether a persisted "last seen update" tag still represents a genuine
/// update relative to the version currently running.
///
/// This is the single source of truth for "is this version tag newer than the
/// running binary?", shared by every place that surfaces an update indicator
/// from *persisted* state (as opposed to a fresh network check, which already
/// enforces this in [`check_for_update`]):
///
/// - `sync_tray_state()` in `src/app.rs` restores `Config::last_seen_update_version`
///   into the tray on startup and on every config-change sync.
/// - The poll-path in `src/app.rs` writes the same value into `TrayState` right
///   after a live check confirms an update — routed through here too so both
///   call sites agree and neither can regress independently.
///
/// Without this gate, a value persisted by an *older* installed version (e.g.
/// v1.0.6 detects v1.0.7 and saves `last_seen_update_version = "v1.0.7"`) would
/// keep resurfacing as "update available: v1.0.7" forever after the user
/// upgrades to v1.0.7, even though they are already on the latest version
/// (debug session: updater-false-positive).
///
/// Returns `Some(tag)` (the input string, unchanged, e.g. `"v1.2.0"`) only when
/// `tag` parses as a semver version strictly greater than `current`. Returns
/// `None` when `last_seen` is `None`, equal to or older than `current`, or
/// either string fails to parse (fail safe: an unparseable value is never
/// surfaced as an update).
#[cfg(feature = "updater")]
pub fn update_available_for(last_seen: Option<&str>, current: &str) -> Option<String> {
    use semver::Version;

    let tag = last_seen?;
    let current_v = Version::parse(current.trim_start_matches('v')).ok()?;
    let remote_v = Version::parse(tag.trim_start_matches('v')).ok()?;

    if remote_v > current_v {
        Some(tag.to_owned())
    } else {
        None
    }
}

/// Stub when the `updater` feature is disabled — never surfaces a persisted
/// value as an update, since there is no live check to have validated it.
#[cfg(not(feature = "updater"))]
pub fn update_available_for(_last_seen: Option<&str>, _current: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    // These tests exercise the version comparison logic without network calls.

    #[cfg(feature = "updater")]
    mod with_updater {
        use semver::Version;

        fn compare(current: &str, remote_tag: &str) -> Option<String> {
            let current_v = Version::parse(current).unwrap();
            let remote_str = remote_tag.trim_start_matches('v');
            let remote_v = Version::parse(remote_str).unwrap();
            if remote_v > current_v {
                Some(remote_tag.to_owned())
            } else {
                None
            }
        }

        #[test]
        fn same_version_returns_none() {
            assert_eq!(compare("1.0.0", "v1.0.0"), None);
        }

        #[test]
        fn newer_remote_returns_some() {
            assert_eq!(compare("1.0.0", "v1.2.0"), Some("v1.2.0".to_owned()));
        }

        #[test]
        fn older_remote_returns_none() {
            assert_eq!(compare("1.2.0", "v1.0.0"), None);
        }

        #[test]
        fn patch_bump_detected() {
            assert_eq!(compare("1.0.0", "v1.0.1"), Some("v1.0.1".to_owned()));
        }

        #[test]
        fn major_bump_detected() {
            assert_eq!(compare("1.0.0", "v2.0.0"), Some("v2.0.0".to_owned()));
        }

        // Regression tests for `update_available_for` — debug session
        // updater-false-positive. A persisted `last_seen_update_version` that
        // is <= the running version must never be surfaced as an update, in
        // either the tray restore path or the poll-path, once the user has
        // upgraded to (or past) that version.

        #[test]
        fn last_seen_equal_to_current_returns_none() {
            // Exact reproduction of the reported false positive: a v1.0.6
            // install persisted last_seen_update_version = "v1.0.7", then the
            // user upgraded to v1.0.7. The tray must not claim v1.0.7 is
            // available while v1.0.7 is what's running.
            assert_eq!(
                super::super::update_available_for(Some("v1.0.7"), "1.0.7"),
                None
            );
        }

        #[test]
        fn last_seen_greater_than_current_returns_some() {
            assert_eq!(
                super::super::update_available_for(Some("v1.2.0"), "1.0.0"),
                Some("v1.2.0".to_owned())
            );
        }

        #[test]
        fn last_seen_older_than_current_returns_none() {
            // e.g. a stale value from before the user upgraded past it.
            assert_eq!(
                super::super::update_available_for(Some("v1.0.0"), "1.2.0"),
                None
            );
        }

        #[test]
        fn last_seen_none_returns_none() {
            assert_eq!(super::super::update_available_for(None, "1.0.0"), None);
        }

        #[test]
        fn last_seen_unparseable_returns_none() {
            assert_eq!(
                super::super::update_available_for(Some("not-a-version"), "1.0.0"),
                None
            );
        }
    }

    #[cfg(not(feature = "updater"))]
    #[test]
    fn stub_returns_none() {
        assert_eq!(super::check_for_update().unwrap(), None);
    }

    #[cfg(not(feature = "updater"))]
    #[test]
    fn stub_update_available_for_always_none() {
        assert_eq!(super::update_available_for(Some("v9.9.9"), "1.0.0"), None);
    }
}

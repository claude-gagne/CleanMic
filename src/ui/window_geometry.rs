//! Pure, GTK-free main-window sizing policy (quick task 260927-mvb).
//!
//! The owner asked (2026-09-27, locked): "make the app less tall by default
//! and allow vertical resizing" and "width stays fixed and remember vertical
//! size between launches". This module holds the plain-data decision logic
//! for both halves — restoring a sensible height at window build time, and
//! clamping a resized height before it is persisted — kept separate from
//! `src/ui/window.rs`'s GTK widget construction so it is unit-testable
//! without a display server.
//!
//! # Width policy and its limits
//!
//! **Technique:** the window's default width is [`WINDOW_WIDTH`]; a
//! `set_size_request(WINDOW_WIDTH, MIN_WINDOW_HEIGHT)` call in
//! `src/ui/window.rs` makes the window manager refuse anything narrower (the
//! X11 `WM_NORMAL_HINTS` `PMinSize` hint, and the Wayland
//! `xdg_toplevel.set_min_size` request); the content column inside the window
//! is additionally hard-clamped to `WINDOW_WIDTH` via an `AdwClamp` with
//! `maximum_size == tightening_threshold == WINDOW_WIDTH` and
//! `unit = LengthUnit::Px` (see `src/ui/window.rs`'s Task 2 for the clamp
//! itself); and the width is never persisted — only [`height_to_remember`]
//! ever computes a value that is saved.
//!
//! **Limits (documented, not hidden):** GTK4 exposes no maximum-size hint for
//! a *resizable* toplevel — the X11 `PMaxSize` hint and the Wayland
//! `xdg_toplevel.set_max_size` request are both only sent by GTK when the
//! window is *not* resizable (`resizable(false)`), which this window cannot
//! be, since vertical resizing is the whole point of this task. So on X11 and
//! on GNOME Wayland (mutter), a user can still drag the window wider, tile
//! it, or maximize it via a header double-click or Super+Up; only empty
//! margins grow, since the content column stays clamped. The WM-less Xephyr
//! harness used for automated evidence enforces neither a minimum nor a
//! maximum at all, which is why [`height_to_remember`] clamps what is saved
//! rather than trusting the raw resized value.
//!
//! **Rejected alternatives:**
//! - Snapping the width back on `notify::default-width`: this creates a
//!   tug-of-war with the window manager during an interactive X11 drag (GTK
//!   requests one size, the WM's live drag reports another, GTK snaps back,
//!   repeat), and on Wayland the compositor's `configure` event wins during
//!   the drag anyway, producing a visible jump once the drag ends.
//! - Raw Xlib `WM_NORMAL_HINTS` manipulation: X11-only (the owner runs GNOME
//!   Wayland), and GTK rewrites the hints on every `present()` regardless.
//!
//! Wayland behavior above is derived from GTK4's documented toplevel-size
//! semantics; it was not machine-verified in this task, since the only
//! available harness (Xephyr) is X11.

/// The window's default and only-ever-set width, in logical px. Every
/// calibrated harness x coordinate in `scripts/nested-run-targets.tsv`
/// assumes this value; never persisted (see the module-level width-policy
/// doc above).
pub const WINDOW_WIDTH: i32 = 420;

/// The smallest height the window is ever built or remembered at: the
/// header, the whole Input group (the Enable row is centered at logical
/// y=197 per `scripts/nested-run-targets.tsv`), and the start of the engine
/// list — enough of a cue that the page scrolls for the rest.
pub const MIN_WINDOW_HEIGHT: i32 = 400;

/// The default height for a fresh install or an old config with no
/// remembered value: the header, Input, all five engine rows, and the
/// Strength row (centered at logical y=669 en / y=684 fr per the TSV) are
/// visible without scrolling; Mode/Levels/Settings — set-and-forget controls
/// — sit one scroll away. Fits the owner's 1920x1080 laptop panel (work area
/// about 1048 logical px under GNOME's 32px top bar) with more than 300px to
/// spare, where the previous natural height of about 1215 did not fit at
/// all.
pub const DEFAULT_WINDOW_HEIGHT: i32 = 720;

/// Reserved vertical space assumed unusable for the window itself: GNOME's
/// top bar (32px) plus a bottom dock or panel of up to 64px (e.g. Ubuntu
/// Dock at the bottom, or a KDE 44px panel). Subtracted from a monitor's
/// reported height before it is used as an upper bound.
pub const MONITOR_MARGIN: i32 = 96;

/// An upper bound on any restored or remembered height, guarding against a
/// corrupt or hand-edited config (`window_height = 999999999`, say): the
/// tallest common logical panel size (8K at display scale 1). Anything
/// larger cannot be a genuine remembered window height.
pub const MAX_WINDOW_HEIGHT: i32 = 4320;

/// How long to wait, after the last `default-height` change notification,
/// before persisting the new height — one config write per resize drag
/// rather than one per pixel. See `src/ui/window.rs`'s debounce
/// implementation for why this matters (the 30fps whole-`Config` comparison
/// and its unconditional tray `LayoutUpdated` signal).
pub const SAVE_DEBOUNCE_MS: u64 = 500;

/// Compute the height the main window should open at.
///
/// - `remembered`: `config.window_height`, if any. A non-positive value
///   (`<= 0`, including the sentinel some tests use to mean "unset") is
///   treated as if it were `None` — a corrupt or hand-edited zero/negative
///   value must never propagate.
/// - `natural`: the content's own natural (unclamped) height at
///   [`WINDOW_WIDTH`], measured from the built widget tree, if available.
///   The window is never opened taller than its own content.
/// - `monitor_height`: the smallest connected monitor's logical height, if
///   any. The window is never opened taller than that monitor minus
///   [`MONITOR_MARGIN`], since GTK cannot know in advance which monitor it
///   will map onto.
///
/// Rules, in order:
/// 1. `base` = `remembered` if `> 0`, else [`DEFAULT_WINDOW_HEIGHT`]; capped
///    at [`MAX_WINDOW_HEIGHT`] either way.
/// 2. `upper` = the minimum of `natural` (only if `> 0`) and
///    `monitor_height - MONITOR_MARGIN` (only if `monitor_height > 0`).
///    Either or both may be absent.
/// 3. `base` is limited by `upper`, if any upper bound exists, then raised to
///    at least [`MIN_WINDOW_HEIGHT`].
pub fn initial_window_height(
    remembered: Option<i32>,
    natural: Option<i32>,
    monitor_height: Option<i32>,
) -> i32 {
    let base = remembered
        .filter(|h| *h > 0)
        .unwrap_or(DEFAULT_WINDOW_HEIGHT)
        .min(MAX_WINDOW_HEIGHT);

    let natural_cap = natural.filter(|h| *h > 0);
    let monitor_cap = monitor_height
        .filter(|h| *h > 0)
        .map(|h| h - MONITOR_MARGIN);

    let upper = match (natural_cap, monitor_cap) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    let limited = match upper {
        Some(u) => base.min(u),
        None => base,
    };

    limited.max(MIN_WINDOW_HEIGHT)
}

/// Compute the value to persist as `config.window_height` for a
/// just-observed window height `current` (logical px), or `None` if nothing
/// should be persisted.
///
/// `current <= 0` (a nonsensical or not-yet-realized size) yields `None` —
/// callers must not overwrite the previously remembered value with garbage.
/// Otherwise the value is clamped to `[MIN_WINDOW_HEIGHT, MAX_WINDOW_HEIGHT]`
/// before being returned — a hand-editable config field is never trusted
/// with a raw, unclamped number, even on the write path.
pub fn height_to_remember(current: i32) -> Option<i32> {
    if current <= 0 {
        return None;
    }
    Some(current.clamp(MIN_WINDOW_HEIGHT, MAX_WINDOW_HEIGHT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_window_height_table() {
        let cases: &[(Option<i32>, Option<i32>, Option<i32>, i32)] = &[
            (None, None, None, DEFAULT_WINDOW_HEIGHT),
            (None, Some(1215), Some(1080), DEFAULT_WINDOW_HEIGHT),
            (Some(560), Some(1215), Some(1080), 560),
            // Monitor cap: 1080 - 96 = 984.
            (Some(1100), Some(1215), Some(1080), 984),
            // Natural cap: never taller than the content itself.
            (Some(1100), Some(1050), Some(1440), 1050),
            // A default taller than the content is cut to the content.
            (None, Some(600), Some(1080), 600),
            // MIN_WINDOW_HEIGHT floor.
            (Some(200), Some(1215), Some(1080), MIN_WINDOW_HEIGHT),
            // Non-positive remembered values mean unset.
            (Some(0), None, None, DEFAULT_WINDOW_HEIGHT),
            (Some(-5), None, None, DEFAULT_WINDOW_HEIGHT),
            // MAX_WINDOW_HEIGHT guard against a corrupt config.
            (Some(i32::MAX), None, None, MAX_WINDOW_HEIGHT),
            // The minimum wins over a tiny monitor cap.
            (None, None, Some(400), MIN_WINDOW_HEIGHT),
            // Non-positive caps are ignored.
            (None, Some(0), Some(0), DEFAULT_WINDOW_HEIGHT),
        ];

        for (remembered, natural, monitor_height, expected) in cases.iter().copied() {
            assert_eq!(
                initial_window_height(remembered, natural, monitor_height),
                expected,
                "initial_window_height({remembered:?}, {natural:?}, {monitor_height:?})"
            );
        }
    }

    #[test]
    fn height_to_remember_table() {
        assert_eq!(height_to_remember(0), None);
        assert_eq!(height_to_remember(-3), None);
        assert_eq!(height_to_remember(250), Some(MIN_WINDOW_HEIGHT));
        assert_eq!(height_to_remember(610), Some(610));
        assert_eq!(height_to_remember(9000), Some(MAX_WINDOW_HEIGHT));
    }
}

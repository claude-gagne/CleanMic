#!/usr/bin/env bash
#
# pre-commit-i18n-check.sh — guard against shipping untranslated UI strings.
#
# Two failure classes are detected on the STAGED diff (added lines only):
#
#   A) Wrapped-but-untranslated:
#      A new  gettext("X")  or  tr!("X")  call site whose msgid is missing
#      from locale/fr/LC_MESSAGES/cleanmic.po, or present with an empty msgstr.
#
#   B) Unwrapped user-facing string:
#      A raw string literal (or format!("...")) that looks like UI prose,
#      added in a user-facing file (src/tray.rs, src/ui/*.rs, src/app.rs),
#      that is NOT wrapped in gettext()/tr!(). This is the class that caused
#      the v1.0.6 "Update available" regression — the string was never wrapped,
#      so a translation-coverage check alone would not have caught it.
#
# Both classes are heuristic-bounded to keep false positives low; see the
# allowlist below. Genuine exceptions can be silenced inline with a trailing
#   // i18n-ignore
# comment, or the whole commit bypassed with  git commit --no-verify
# (use sparingly — see the "i18n Discipline" section of CLAUDE.md).
#
# Exit 0 = clean, exit 1 = violations found (commit blocked).

set -euo pipefail

# Optional first arg: a git diff range (e.g. "origin/master...HEAD") for CI use.
# When empty (the pre-commit case), the staged index is inspected instead.
DIFF_RANGE="${1:-}"

PO="locale/fr/LC_MESSAGES/cleanmic.po"
# Files whose added string literals are treated as user-facing (Class B).
UI_GLOBS=("src/tray.rs" "src/ui/*.rs" "src/app.rs")

fail=0
RED=""; YEL=""; RST=""
if [ -t 2 ]; then RED=$'\033[31m'; YEL=$'\033[33m'; RST=$'\033[0m'; fi

err()  { printf '%s\n' "${RED}i18n-guard: $*${RST}" >&2; }
note() { printf '%s\n' "${YEL}  $*${RST}" >&2; }

if [ ! -f "$PO" ]; then
    err "translation catalog not found at $PO"
    exit 1
fi

# ----------------------------------------------------------------------------
# Helper: does $PO contain msgid "<id>" with a NON-EMPTY msgstr?
# Prints "OK", "EMPTY", or "MISSING".
# ----------------------------------------------------------------------------
po_status() {
    awk -v want="$1" '
        function unesc(s){ gsub(/\\"/,"\"",s); return s }
        /^msgid "/ {
            cur = $0
            sub(/^msgid "/, "", cur); sub(/"[[:space:]]*$/, "", cur)
            id = unesc(cur); inmsg = (id == want); next
        }
        inmsg && /^msgstr "/ {
            ms = $0; sub(/^msgstr "/, "", ms); sub(/"[[:space:]]*$/, "", ms)
            print (ms == "" ? "EMPTY" : "OK"); found = 1; exit
        }
        END { if (!found) print "MISSING" }
    ' "$PO"
}

# Added lines (no +++ header) for a pathspec; strips the leading '+'.
# Uses DIFF_RANGE when set (CI), otherwise the staged index (pre-commit).
staged_added() {
    if [ -n "$DIFF_RANGE" ]; then
        git diff -U0 --no-color "$DIFF_RANGE" -- "$@" | grep -E '^\+[^+]' | sed 's/^\+//' || true
    else
        git diff --cached -U0 --no-color -- "$@" | grep -E '^\+[^+]' | sed 's/^\+//' || true
    fi
}

# ----------------------------------------------------------------------------
# CLASS A — new gettext()/tr!() call sites must be translated in fr.
# ----------------------------------------------------------------------------
a_lines="$(staged_added '*.rs' || true)"
if [ -n "$a_lines" ]; then
    # Extract msgids from gettext("…") and tr!("…"). Simple literals only.
    ids="$(printf '%s\n' "$a_lines" \
        | grep -oE '(gettext|tr!)\("([^"\\]|\\.)*"' \
        | sed -E 's/^(gettext|tr!)\("//; s/"$//' \
        | sort -u || true)"
    while IFS= read -r id; do
        [ -z "$id" ] && continue
        st="$(po_status "$id")"
        if [ "$st" = "MISSING" ]; then
            err "Class A — new translatable string has no French entry:"
            note "msgid \"$id\"  → add it to $PO (with msgstr), then: make mo"
            fail=1
        elif [ "$st" = "EMPTY" ]; then
            err "Class A — French translation is empty:"
            note "msgid \"$id\"  → fill the msgstr in $PO, then: make mo"
            fail=1
        fi
    done <<EOF
$ids
EOF
fi

# ----------------------------------------------------------------------------
# CLASS B — unwrapped user-facing string literals in UI files.
#
# Heuristic: an added line in a UI file that contains a string literal of UI
# prose (starts with a capital letter and contains a space, OR a format! whose
# template is such prose) but does NOT call gettext(/tr!( and is not in an
# obviously non-UI context (logging, asserts, errors, comments, attributes,
# dotted property keys, URLs). Catches the cross-line MenuItem::action(\n
# format!("Prose")) pattern because the format!/literal line itself is flagged.
# ----------------------------------------------------------------------------
b_lines="$(staged_added "${UI_GLOBS[@]}" || true)"
if [ -n "$b_lines" ]; then
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        # Inline opt-out.
        case "$line" in *"i18n-ignore"*) continue;; esac
        # Allowlist of non-UI contexts.
        case "$line" in
            *"log::"*|*"println!"*|*"eprintln!"*|*"//"*|*"assert"*|*"debug_assert"*) continue;;
            *".context("*|*"with_context"*|*".expect("*|*"panic!"*|*"unreachable!"*) continue;;
            *"#["*) continue;;
        esac
        # A wrapped string on the line is fine.
        case "$line" in *"gettext(\""*|*"tr!(\""*) continue;; esac
        # Does the line contain UI-prose: a quoted literal starting with a
        # capital letter and containing a space? (excludes "{}: {}", keys, etc.)
        if printf '%s' "$line" | grep -qE '"[A-Z][A-Za-z0-9]*([ ][^"]*)+"'; then
            # Exclude dotted property keys / URLs even if capitalized.
            case "$line" in *"://"*) continue;; esac
            err "Class B — possible unwrapped user-facing string:"
            note "$(printf '%s' "$line" | sed 's/^[[:space:]]*//')"
            note "wrap it in gettext(\"…\") (or add // i18n-ignore if truly not user-facing)"
            fail=1
        fi
    done <<EOF
$b_lines
EOF
fi

if [ "$fail" -ne 0 ]; then
    err "commit blocked — see above. Bypass (sparingly): git commit --no-verify"
    exit 1
fi
exit 0

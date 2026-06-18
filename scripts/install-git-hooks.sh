#!/usr/bin/env bash
#
# install-git-hooks.sh -- Install CleanMic's local git hooks into .git/hooks/.
#
# Currently installs:
#   * pre-commit -> runs scripts/pre-commit-i18n-check.sh (i18n guard)
#
# The pre-commit hook blocks commits that introduce untranslated or unwrapped
# user-facing strings. See the "i18n Discipline" section of CLAUDE.md.
#
# Usage: bash scripts/install-git-hooks.sh
#
# To bypass the hook for a single commit (use sparingly):
#   git commit --no-verify
# This is an honor-system caveat — the hook is convenience, not security. The
# CI backstop (.github/workflows/) runs the same check on pull requests.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOK_DEST="$PROJECT_ROOT/.git/hooks/pre-commit"

info()  { printf '\033[1;34m==> %s\033[0m\n' "$*"; }
error() { printf '\033[1;31m==> ERROR: %s\033[0m\n' "$*" >&2; exit 1; }

if [ ! -d "$PROJECT_ROOT/.git" ]; then
    error "not inside a git repo (no $PROJECT_ROOT/.git directory)"
fi

if [ -f "$HOOK_DEST" ] && ! grep -q "pre-commit-i18n-check.sh" "$HOOK_DEST" 2>/dev/null; then
    info ".git/hooks/pre-commit already exists and is not ours — not overwriting."
    info "    Add this line to it manually:"
    info "      bash \"\$(git rev-parse --show-toplevel)/scripts/pre-commit-i18n-check.sh\""
    exit 0
fi

info "Installing i18n pre-commit hook..."
cat > "$HOOK_DEST" << 'HOOK_EOF'
#!/bin/sh
# CleanMic i18n guard — see scripts/pre-commit-i18n-check.sh and CLAUDE.md.
ROOT="$(git rev-parse --show-toplevel)"
exec bash "$ROOT/scripts/pre-commit-i18n-check.sh"
HOOK_EOF

chmod +x "$HOOK_DEST"
info "Installed $HOOK_DEST"
info "Bypass a single commit with: git commit --no-verify"

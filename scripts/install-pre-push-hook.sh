#!/usr/bin/env bash
#
# install-pre-push-hook.sh -- Install the commit-timing pre-push hook into
#                             this repo's .git/hooks/ directory.
#
# The hook blocks `git push origin master` on weekdays between 09:00 and 14:30
# America/Toronto. Pushes at other times, and pushes to other branches or
# remotes, pass through unchanged.
#
# Usage: bash scripts/install-pre-push-hook.sh
#
# To bypass the hook temporarily (e.g. emergency fix), use:
#   git push --no-verify origin master
# This is an honor-system caveat — the hook is convenience, not security.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOK_DEST="$PROJECT_ROOT/.git/hooks/pre-push"

info()  { printf '\033[1;34m==> %s\033[0m\n' "$*"; }
warn()  { printf '\033[1;33m==> %s\033[0m\n' "$*"; }
error() { printf '\033[1;31m==> ERROR: %s\033[0m\n' "$*" >&2; exit 1; }

if [ ! -d "$PROJECT_ROOT/.git" ]; then
    error "not inside a git repo (no $PROJECT_ROOT/.git directory)"
fi

if [ -f "$HOOK_DEST" ]; then
    info ".git/hooks/pre-push already exists — not overwriting."
    info "    Remove it first if you want to reinstall:"
    info "      rm $HOOK_DEST && bash $0"
    exit 0
fi

info "Installing commit-timing pre-push hook..."

cat > "$HOOK_DEST" << 'HOOK_EOF'
#!/bin/sh
#
# pre-push hook -- block pushes to claude-gagne/CleanMic master on
# weekdays 09:00-14:30 America/Toronto.
#
# Called by "git push" after remote status check, before anything is pushed.
# If this hook exits non-zero, nothing is pushed.
#
# Parameters:
#   $1 -- remote name
#   $2 -- remote URL
#
# stdin format (one line per ref being pushed):
#   <local_ref> <local_oid> <remote_ref> <remote_oid>

remote_url="$2"

# Only enforce on the public repo. Other remotes (forks, backups, mirrors)
# pass through unchanged.
case "$remote_url" in
    *claude-gagne/CleanMic*) ;;   # enforce — fall through to next check
    *) exit 0 ;;
esac

# Only gate pushes to master.
gated=0
while read -r _local_ref _local_oid remote_ref _remote_oid; do
    if [ "$remote_ref" = "refs/heads/master" ]; then
        gated=1
    fi
done

[ "$gated" -eq 0 ] && exit 0

# Local time window check (America/Toronto).
dow=$(TZ=America/Toronto date +%u)    # 1=Mon … 7=Sun
hm=$(TZ=America/Toronto  date +%H%M)  # e.g. 1345

if [ "$dow" -le 5 ] && [ "$hm" -ge 0900 ] && [ "$hm" -lt 1430 ]; then
    echo >&2 "pre-push: blocked — weekday 09:00-14:30 America/Toronto window."
    echo >&2 "          retry after 14:30 local, or push on a weekend."
    echo >&2 "          bypass (use sparingly): git push --no-verify"
    exit 1
fi

exit 0
HOOK_EOF

chmod +x "$HOOK_DEST"
info "Installed $HOOK_DEST"
info "To remove: rm $HOOK_DEST"

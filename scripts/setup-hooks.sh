#!/bin/bash
# Setup git hooks for Carina

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOOKS_DIR="$(git -C "$SCRIPT_DIR" rev-parse --path-format=absolute --git-common-dir)/hooks"
HOOK_PATH="$HOOKS_DIR/pre-commit"

echo "Installing git hooks..."

mkdir -p "$HOOKS_DIR"
ENTRY_FILE=$(mktemp "$HOOKS_DIR/.pre-commit.XXXXXX")
trap 'rm -f "$ENTRY_FILE"' EXIT
cat >"$ENTRY_FILE" <<'HOOK'
#!/bin/bash
# carina-pre-commit-entry v1
set -e

TRACKED_HOOK="$(git rev-parse --show-toplevel)/scripts/pre-commit"
if [ ! -f "$TRACKED_HOOK" ]; then
    echo "ERROR: Missing tracked pre-commit script: $TRACKED_HOOK" >&2
    exit 1
fi
"$TRACKED_HOOK" "$@"

LOCAL_HOOK="$(git rev-parse --git-common-dir)/hooks/pre-commit.local"
if [ -x "$LOCAL_HOOK" ]; then
    "$LOCAL_HOOK" "$@"
fi
HOOK
chmod +x "$ENTRY_FILE"

if { [ -e "$HOOK_PATH" ] || [ -L "$HOOK_PATH" ]; } && ! cmp -s "$HOOK_PATH" "$ENTRY_FILE"; then
    BACKUP_BASE="$HOOK_PATH.backup-$(date -u +%Y%m%dT%H%M%SZ)"
    BACKUP_PATH="$BACKUP_BASE"
    BACKUP_SUFFIX=0
    while [ -e "$BACKUP_PATH" ] || [ -L "$BACKUP_PATH" ]; do
        BACKUP_SUFFIX=$((BACKUP_SUFFIX + 1))
        BACKUP_PATH="$BACKUP_BASE-$BACKUP_SUFFIX"
    done
    mv "$HOOK_PATH" "$BACKUP_PATH"
    echo "The existing hook differs from the carina entry; moved it to: $BACKUP_PATH"
    echo "Local additions belong in $HOOKS_DIR/pre-commit.local."
    echo "Make it executable with: chmod +x \"$HOOKS_DIR/pre-commit.local\""
fi

# Replace the entry atomically, without following an existing hook symlink.
mv -f "$ENTRY_FILE" "$HOOK_PATH"

echo "Git hooks installed successfully!"
echo "Pre-commit hook runs this checkout's scripts/pre-commit: static checks, fmt, clippy; then pre-commit.local if present and executable."
echo "Existing clones must re-run scripts/setup-hooks.sh once to install this entry point."

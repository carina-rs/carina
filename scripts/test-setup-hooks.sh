#!/bin/bash
# Exercise installation and real commits only in a throwaway repository.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
scratch_dir=$(mktemp -d "${TMPDIR:-/tmp}/carina-setup-hooks.XXXXXX")
trap 'rm -rf "$scratch_dir"' EXIT
scratch_dir=$(cd "$scratch_dir" && pwd -P)
repo="$scratch_dir/main checkout"
worktree="$scratch_dir/linked worktree"

# Ignore user/system configuration and any repository/config overrides from
# the caller, so no commit can use the developer's real hooks or identity.
unset GIT_DIR GIT_COMMON_DIR GIT_WORK_TREE GIT_INDEX_FILE
unset GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES
unset GIT_CONFIG GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME='Carina hook test' GIT_AUTHOR_EMAIL=hooks@example.invalid
export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME" GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"
export HOOK_TEST_LOG="$scratch_dir/hook.log"
unset HOOK_TEST_LOCAL_STATUS

fail() {
    echo "Assertion failed: $*" >&2
    exit 1
}

assert_equals() {
    if [ "$1" != "$2" ]; then
        fail "$3 (expected '$1', got '$2')"
    fi
}

assert_contains() {
    case "$1" in
        *"$2"*) ;;
        *) fail "missing output: $2" ;;
    esac
}

commit_in() {
    git -C "$1" -c commit.gpgSign=false commit --quiet --allow-empty -m 'Hook fixture commit'
}

assert_commit_fails() {
    local previous_head
    previous_head=$(git -C "$1" rev-parse HEAD)
    if output=$(commit_in "$1" 2>&1); then
        fail "$2"
    fi
    assert_equals "$previous_head" "$(git -C "$1" rev-parse HEAD)" "failed hook must prevent the commit"
}

assert_local_unchanged() {
    cmp "$scratch_dir/original-local" "$local_hook" || fail 'setup changed pre-commit.local'
    [ -x "$local_hook" ] || fail 'setup changed pre-commit.local permissions'
}

git init --quiet --template= -b hook-main "$repo"
hooks_dir="$(git -C "$repo" rev-parse --path-format=absolute --git-common-dir)/hooks"
# Pin this fixture's hook path, including for its linked worktree.
git -C "$repo" config --local core.hooksPath "$hooks_dir"
mkdir -p "$repo/scripts" "$hooks_dir"
cp "$SCRIPT_DIR/setup-hooks.sh" "$repo/scripts/setup-hooks.sh"
cp "$SCRIPT_DIR/pre-commit" "$repo/scripts/pre-commit"
# Replace the tracked hook before any commit: no Rust tools can run here.
cat >"$repo/scripts/pre-commit" <<'SH'
#!/bin/bash
printf 'tracked:%s\n' "$0" >> "$HOOK_TEST_LOG"
if [ "$#" -gt 0 ]; then
    printf 'tracked-arg:<%s>\n' "$@" >> "$HOOK_TEST_LOG"
fi
SH
chmod +x "$repo/scripts/pre-commit"

hook="$hooks_dir/pre-commit"
local_hook="$hooks_dir/pre-commit.local"
cat >"$hook" <<'SH'
#!/bin/bash
echo 'old hook must be backed up, never run' >&2
exit 99
SH
chmod +x "$hook"
cp "$hook" "$scratch_dir/original-hook"
cat >"$local_hook" <<'SH'
#!/bin/bash
printf 'local\n' >> "$HOOK_TEST_LOG"
if [ "$#" -gt 0 ]; then
    printf 'local-arg:<%s>\n' "$@" >> "$HOOK_TEST_LOG"
fi
exit "${HOOK_TEST_LOCAL_STATUS:-0}"
SH
chmod +x "$local_hook"
cp "$local_hook" "$scratch_dir/original-local"

# Setup must locate the repository from its script, even from another cwd.
setup_output=$(cd "$scratch_dir" && /bin/bash "$repo/scripts/setup-hooks.sh")
shopt -s nullglob
backups=("$hooks_dir"/pre-commit.backup-*)
assert_equals 1 "${#backups[@]}" 'old hook must have exactly one backup'
cmp "$scratch_dir/original-hook" "${backups[0]}" || fail 'backup lost the original hook'
[ -x "${backups[0]}" ] || fail 'backup lost executable permissions'
assert_contains "$setup_output" "${backups[0]}"
assert_contains "$setup_output" 'existing hook differs from the carina entry; moved it to:'
assert_contains "$setup_output" "Local additions belong in $local_hook."
assert_contains "$setup_output" "$local_hook"
assert_contains "$setup_output" 'chmod +x'
assert_contains "$setup_output" 'static checks, fmt, clippy; then pre-commit.local'
assert_contains "$setup_output" 'Existing clones must re-run scripts/setup-hooks.sh once'
assert_equals '#!/bin/bash' "$(sed -n '1p' "$hook")" 'entry must use macOS Bash'
assert_equals '# carina-pre-commit-entry v1' "$(sed -n '2p' "$hook")" 'entry must carry its fixed marker'
[ -x "$hook" ] || fail 'entry must be executable'
assert_local_unchanged

: >"$HOOK_TEST_LOG"
git -C "$repo" add scripts
commit_in "$repo"
assert_equals "$(printf 'tracked:%s\nlocal' "$repo/scripts/pre-commit")" \
    "$(cat "$HOOK_TEST_LOG")" 'main checkout must run its tracked hook before the local hook'

git -C "$repo" worktree add --quiet -b hook-worktree "$worktree"
: >"$HOOK_TEST_LOG"
commit_in "$worktree"
assert_equals "$(printf 'tracked:%s\nlocal' "$worktree/scripts/pre-commit")" \
    "$(cat "$HOOK_TEST_LOG")" 'linked worktree must run its own tracked hook before the local hook'

# Reinstall from the linked worktree into the shared hook directory.
setup_output=$(/bin/bash "$worktree/scripts/setup-hooks.sh")
new_backups=("$hooks_dir"/pre-commit.backup-*)
assert_equals "${backups[*]}" "${new_backups[*]}" 'reinstall must not create another backup'
case "$setup_output" in
    *'differs from the carina entry'*|*'moved it to:'*) fail 'reinstall must not repeat the migration warning' ;;
esac
assert_local_unchanged

# An edited entry still has the marker, but its local additions must survive.
cp "$hook" "$scratch_dir/original-entry"
printf '\nexit 73\n' >>"$hook"
cp "$hook" "$scratch_dir/edited-entry"
setup_output=$(/bin/bash "$worktree/scripts/setup-hooks.sh")
new_backups=("$hooks_dir"/pre-commit.backup-*)
assert_equals 2 "${#new_backups[@]}" 'an edited entry must have its own backup'
cmp "$scratch_dir/original-hook" "${backups[0]}" || fail 'reinstall changed the first backup'
for backup_path in "${new_backups[@]}"; do
    [ "$backup_path" != "${backups[0]}" ] || continue
    cmp "$scratch_dir/edited-entry" "$backup_path" || fail 'backup lost the edited entry'
    [ -x "$backup_path" ] || fail 'backup lost the edited entry permissions'
    assert_contains "$setup_output" "$backup_path"
done
assert_contains "$setup_output" 'existing hook differs from the carina entry; moved it to:'
cmp "$scratch_dir/original-entry" "$hook" || fail 'setup did not restore the exact entry'
assert_local_unchanged

# A tracked-script edit takes effect immediately, without another install.
printf '\nexit 23\n' >>"$worktree/scripts/pre-commit"
: >"$HOOK_TEST_LOG"
assert_commit_fails "$worktree" 'a failing tracked hook must fail the commit'
assert_equals "tracked:$worktree/scripts/pre-commit" "$(cat "$HOOK_TEST_LOG")" \
    'a failing tracked hook must stop before the local hook'
hook_status=0
(cd "$worktree" && "$hook") || hook_status=$?
assert_equals 23 "$hook_status" 'entry must propagate the tracked hook status'
cp "$repo/scripts/pre-commit" "$worktree/scripts/pre-commit"

: >"$HOOK_TEST_LOG"
export HOOK_TEST_LOCAL_STATUS=29
assert_commit_fails "$worktree" 'a failing local hook must fail the commit'
assert_equals "$(printf 'tracked:%s\nlocal' "$worktree/scripts/pre-commit")" \
    "$(cat "$HOOK_TEST_LOG")" 'the failing local hook must run after the tracked hook'
hook_status=0
(cd "$worktree" && "$hook") || hook_status=$?
assert_equals 29 "$hook_status" 'entry must propagate the local hook status'
unset HOOK_TEST_LOCAL_STATUS

: >"$HOOK_TEST_LOG"
mv "$worktree/scripts/pre-commit" "$scratch_dir/saved-pre-commit"
assert_commit_fails "$worktree" 'a missing tracked hook must fail the commit'
assert_contains "$output" "Missing tracked pre-commit script: $worktree/scripts/pre-commit"
assert_equals '' "$(cat "$HOOK_TEST_LOG")" 'missing tracked hook must stop before the local hook'
mv "$scratch_dir/saved-pre-commit" "$worktree/scripts/pre-commit"

: >"$HOOK_TEST_LOG"
(cd "$repo" && "$hook" 'first argument' 'second argument')
assert_equals "$(printf 'tracked:%s\ntracked-arg:<first argument>\ntracked-arg:<second argument>\nlocal\nlocal-arg:<first argument>\nlocal-arg:<second argument>' "$repo/scripts/pre-commit")" \
    "$(cat "$HOOK_TEST_LOG")" 'entry must forward arguments to both hooks'

chmod -x "$local_hook"
: >"$HOOK_TEST_LOG"
commit_in "$repo"
assert_equals "tracked:$repo/scripts/pre-commit" "$(cat "$HOOK_TEST_LOG")" \
    'a non-executable local hook must be skipped'
/bin/bash "$repo/scripts/setup-hooks.sh" >/dev/null
[ ! -x "$local_hook" ] || fail 'setup made pre-commit.local executable'
cmp "$scratch_dir/original-local" "$local_hook" || fail 'setup changed the non-executable local hook'

rm "$local_hook"
/bin/bash "$worktree/scripts/setup-hooks.sh" >/dev/null
[ ! -e "$local_hook" ] || fail 'setup created pre-commit.local'
: >"$HOOK_TEST_LOG"
commit_in "$worktree"
assert_equals "tracked:$worktree/scripts/pre-commit" "$(cat "$HOOK_TEST_LOG")" \
    'an absent local hook must be skipped'

echo 'setup-hooks tests passed'

#!/bin/sh
# Claude Code `WorktreeRemove` hook (spec §2, consumer contract 2).
#
# Claude Code sends one JSON object on stdin that names the worktree in
# `worktree_path`. The hook removes the klon with `gh klon rm`, which renames
# the tree into the trash directory, prunes the git entry, and deletes the
# copy in the background. A non-zero exit reports the failure to Claude Code.
#
# The hook appends one line to the file named by $KLON_HOOK_LOG when that
# variable is set. The S3 spike reads the log to learn whether the hook ran.

set -u

log() {
    [ -n "${KLON_HOOK_LOG:-}" ] || return 0
    stamp=$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || echo '-')
    printf '%s %s\n' "$stamp" "$1" >>"$KLON_HOOK_LOG" 2>/dev/null || true
}

fail() {
    log "$1"
    echo "klon hook: $1" >&2
    exit 1
}

# Print the string value of field $1 from the JSON object on stdin. See the
# create hook for the jq and sed branches.
json_field() {
    field=$1
    if [ -z "${KLON_HOOK_NO_JQ:-}" ] && command -v jq >/dev/null 2>&1; then
        jq -r --arg f "$field" '.[$f] // empty'
    else
        sed -n "s/.*\"$field\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p"
    fi
}

input=$(cat)

path=$(printf '%s' "$input" | json_field worktree_path)
[ -n "$path" ] || fail "the hook input has no worktree_path"

# The worktree is already gone: the removal is done, and git forgets the
# stale entry at the next `gh klon prune`.
if [ ! -d "$path" ]; then
    log "WorktreeRemove skip path=$path reason=absent"
    exit 0
fi

# Run from the main worktree, never from the directory that `rm` removes.
common=$(git -C "$path" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) ||
    fail "$path is not inside a git repository"
repo=$(dirname "$common")

json=$(cd "$repo" && gh klon rm --path "$path" --force --json)
status=$?
[ "$status" -eq 0 ] || fail "gh klon rm failed for $path (exit $status)"

log "WorktreeRemove ok path=$path"

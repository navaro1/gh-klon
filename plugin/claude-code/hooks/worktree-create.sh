#!/bin/sh
# Claude Code `WorktreeCreate` hook (spec §2, consumer contract 2).
#
# Claude Code sends one JSON object on stdin with at least `cwd` and `name`.
# The hook creates a klon at `<cwd>/.claude/worktrees/<name>` with the branch
# `worktree-<name>` and prints that path on stdout. Claude Code replaces its
# own `git worktree add` with this hook, so any non-zero exit aborts the
# worktree creation and the error must reach stderr.
#
# The hook appends one line to the file named by $KLON_HOOK_LOG when that
# variable is set. The S3 spike reads the log to learn whether the hook ran.
# Set KLON_HOOK_NO_JQ=1 to force the sed fallback; the tests use it.

set -u

# One log line with a UTC timestamp. A broken log file never fails the hook.
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

# Print the string value of field $1 from the JSON object on stdin.
# jq is the exact parser. The fallback sed reads a `"field":"value"` pair on
# one line and keeps JSON escapes as they are; worktree names and klon paths
# never need escapes.
json_field() {
    field=$1
    if [ -z "${KLON_HOOK_NO_JQ:-}" ] && command -v jq >/dev/null 2>&1; then
        jq -r --arg f "$field" '.[$f] // empty'
    else
        sed -n "s/.*\"$field\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p"
    fi
}

input=$(cat)

cwd=$(printf '%s' "$input" | json_field cwd)
name=$(printf '%s' "$input" | json_field name)

[ -n "$cwd" ] || fail "the hook input has no cwd"
[ -n "$name" ] || fail "the hook input has no name"

# A name is one path component. `.` and `..` would escape the worktrees
# directory, and a `/` would nest the klon under an untracked subdirectory.
case $name in
    .* | */* | *\\*) fail "refuses the worktree name '$name'" ;;
esac

dest="$cwd/.claude/worktrees/$name"

# The hook inherits stderr, so a failed `add` reports its own error there.
json=$(cd "$cwd" && gh klon add "worktree-$name" --path "$dest" --json)
status=$?
[ "$status" -eq 0 ] || fail "gh klon add failed for worktree-$name (exit $status)"

path=$(printf '%s' "$json" | json_field path)
[ -n "$path" ] || fail "the gh klon add output has no path field"

printf '%s\n' "$path"
log "WorktreeCreate ok name=$name path=$path"

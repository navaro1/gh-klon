# klon plugin for Claude Code

The plugin replaces the `git worktree add` of Claude Code with `gh klon add`.
A `WorktreeCreate` hook creates a klon at `<repo>/.claude/worktrees/<name>`
with the branch `worktree-<name>` and prints the path. A `WorktreeRemove`
hook removes the klon at session end. The hook contract is spec §2,
consumer contract 2: the hook JSON arrives on stdin, the create hook prints
the path only, and any non-zero exit aborts the worktree creation.

## Install

1. Install the `gh` extension, so `gh klon` works:

       gh extension install navaro1/gh-klon

2. Copy this directory to a stable place, for example `~/.claude/klon-plugin`:

       cp -r plugin/claude-code ~/.claude/klon-plugin
       chmod +x ~/.claude/klon-plugin/hooks/*.sh

3. Copy the hook entries of `settings.json` into your project settings
   (`.claude/settings.json`) or your user settings (`~/.claude/settings.json`).
   Replace `${CLAUDE_PLUGIN_ROOT}` with the directory from step 2:

       "command": "/home/you/.claude/klon-plugin/hooks/worktree-create.sh"
       "command": "/home/you/.claude/klon-plugin/hooks/worktree-remove.sh"

   When you install the directory as a Claude Code plugin instead, keep
   `${CLAUDE_PLUGIN_ROOT}`: the harness substitutes it. A plugin reads the
   same JSON from its own `hooks/hooks.json`, so the fragment works there
   unchanged.

## Try it

Start Claude Code in a repository and run:

    claude --worktree test -p 'pwd'

The session root is `<repo>/.claude/worktrees/test`, and `git worktree list`
shows it as a klon. The hook appends one line to `$KLON_HOOK_LOG` when that
variable is set; the S3 spike uses the log to detect whether the hook ran.

## Remove a subagent klon by hand

Claude Code makes a worktree through three entry points, and all three run the
create hook: the `--worktree` flag, the `EnterWorktree` tool, and a subagent
with `isolation: "worktree"`. Creation needs no extra work.

Removal has a gap. Claude Code removes only a worktree that it made with git
itself, because it marks each one with a `CLAUDE_BASE` file in the git
metadata. The hook makes the klon, so the klon has no mark. Claude Code
therefore leaves a finished subagent's klon on disk and never calls the
`WorktreeRemove` hook for it. Its periodic sweep skips the klon for the same
reason, so nothing removes it later.

List the leftovers and remove each one:

    gh klon list
    gh klon rm --path <repo>/.claude/worktrees/<name> --force

A leftover klon is never locked, so a plain `git worktree remove` works too.
This affects subagent klons only. A `-p` session also keeps its klon, which is
what Claude Code does for a plain git worktree as well. `docs/spikes/2026-claude-enterworktree.md`
holds the measurements and a draft upstream issue.

## Notes

- The hook needs `jq` for exact JSON parsing and falls back to a `sed` one
  line parse without it.
- `add --path-mode {sibling,claude,t3,codex}` sets the same path templates by
  hand (research record §19). The `claude` mode renames the branch to
  `worktree-<name>`.
- Without the plugin, `gh klon rm --path <path> --force` removes a klon that
  Claude Code made with plain `git worktree add`.

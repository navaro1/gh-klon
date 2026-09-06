# S3 spike: does Claude Code `EnterWorktree` bypass `WorktreeCreate`?

Date: 2026-09-06. Issue: #34. Spec: `docs/klon-spec.md` §7 S3. Plugin: `docs/klon-spec.md` §7 C28.

This report answers one question: does a worktree that Claude Code makes through a path
other than the `--worktree` flag still run the klon `WorktreeCreate` hook? It records
every command and its result.

## 1. Summary

| Question | Answer |
|---|---|
| Does the `--worktree` flag run the `WorktreeCreate` hook? | Yes. |
| Does the `EnterWorktree` tool run the `WorktreeCreate` hook? | **Yes.** It does not bypass the hook. |
| Does a subagent with `isolation: "worktree"` run the `WorktreeCreate` hook? | **Yes.** |
| Is each resulting path a klon? | Yes. All three hold `.klon/env`, and `git worktree list` shows each one. |
| Does Claude Code remove a hook-made subagent worktree when the subagent finishes? | **No.** The worktree stays on disk. |
| Does the `WorktreeRemove` hook run when that subagent finishes? | **No.** The hook log stays empty. |
| Does Claude Code remove a git-made subagent worktree in the same repository? | Yes. A control run removed it. |

**Verdict: creation is complete. Removal has a gap.**

All three entry points run the create hook, so klon covers every documented way that
Claude Code makes a worktree. The gap is on the other side. Claude Code marks each
worktree that it makes with git, and it removes only a marked worktree. A hook makes
an unmarked worktree, so Claude Code never removes it and never calls the
`WorktreeRemove` hook for it. A long session that spawns many isolated subagents
leaks one klon per subagent.

Section 7 states the decision: document the gap in the plugin README, and give the
user the one command that clears the leak. Section 8 holds the text of an upstream
issue. This spike does not file that issue.

## 2. Host and versions

| Item | Value |
|---|---|
| OS | Ubuntu 22.04, kernel 6.2.0-36-generic |
| Filesystem for `$HOME` | ext4 |
| Claude Code | 2.1.263 |
| git | 2.34.1 |
| jq | 1.6 |
| klon | `gh-klon 0.1.0`, built from this branch with `cargo build --release` |
| Model for each nested run | `claude-sonnet-5` |

The spec asks for Claude Code 2.1.259 or newer. The host has 2.1.263.

## 3. Setup

### 3.1 The temporary repository

The spike used a repository outside the project tree, because a klon must live in the
repository that owns it:

    /home/navaro/.local/share/klon/s3-spike/repo

The repository holds two commits: one `README.md`, and one `.claude/settings.json`.
The settings file holds the C28 hook fragment with an absolute path to each hook
script of this branch:

```json
{
  "hooks": {
    "WorktreeCreate": [
      { "hooks": [ { "type": "command", "command": "<repo>/plugin/claude-code/hooks/worktree-create.sh" } ] }
    ],
    "WorktreeRemove": [
      { "hooks": [ { "type": "command", "command": "<repo>/plugin/claude-code/hooks/worktree-remove.sh" } ] }
    ]
  }
}
```

`KLON_HOOK_LOG` pointed at `<repo>/hook.log`. Each hook script appends one line to
that file. An empty or absent file means the hook did not run.

### 3.2 The `gh` shim

A shim at `/home/navaro/.local/share/klon/s3-spike/bin/gh` stood first on `PATH`. It
forwards `gh klon ...` to the binary of this branch and every other call to the real
`gh`. `tests/plugin.rs` uses the same pattern.

```sh
#!/bin/sh
if [ "$1" = klon ]; then
    shift
    exec <repo>/target/release/gh-klon "$@"
fi
exec /home/navaro/.local/bin/gh "$@"
```

The shim needs the execute bit. Without it, `gh klon` reaches the `gh-klon` extension
that the host already has installed, whose symlink points at the main checkout's
build. That build is older and rejects `--json`, so the hook fails with
`error: unexpected argument '--json' found`. Section 4.0 shows that first failure.

### 3.3 The child environment

This spike ran inside a Claude Code session. A nested `claude` refuses to start when
the parent variables stay in the environment. Each run script unsets them first:

    CLAUDECODE CLAUDE_CODE_CHILD_SESSION CLAUDE_CODE_SESSION_ID CLAUDE_PID
    CLAUDE_EFFORT CLAUDE_CODE_MESSAGING_SOCKET CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS
    CLAUDE_CODE_ENTRYPOINT CLAUDE_CODE_EXECPATH CLAUDE_CODE_MESSAGING_TOKEN

With those unset, every nested run started. `env -i` was not needed.

Each run used `--output-format json`, `--model claude-sonnet-5`, and an `--allowedTools`
list with only the tools that the entry point needs. Print mode skips the workspace
trust check, so no trust dialog appeared.

## 4. Results

The table holds one row per entry point. Each run started from the repository root
with a clean `hook.log` and no worktree.

| # | Entry point | Hook ran | Evidence line from `hook.log` | Worktree path | klon or plain |
|---|---|---|---|---|---|
| 0 | Baseline: the hook driven by hand | Yes | `WorktreeCreate ok name=baseline path=<repo>/.claude/worktrees/baseline` | `<repo>/.claude/worktrees/baseline` | klon |
| 1 | `claude --worktree s3flag -p ...` | **Yes** | `WorktreeCreate ok name=s3flag path=<repo>/.claude/worktrees/s3flag` | `<repo>/.claude/worktrees/s3flag` | klon |
| 2 | `EnterWorktree` tool | **Yes** | `WorktreeCreate ok name=s3enter path=<repo>/.claude/worktrees/s3enter` | `<repo>/.claude/worktrees/s3enter` | klon |
| 3 | Subagent `isolation: "worktree"` | **Yes** | `WorktreeCreate ok name=agent-a1f25247ba820c6ab path=<repo>/.claude/worktrees/agent-a1f25247ba820c6ab` | `<repo>/.claude/worktrees/agent-a1f25247ba820c6ab` | klon |

"klon" means two facts hold: the path holds a `.klon/env` file, and
`git worktree list --porcelain` shows the path with the branch `worktree-<name>`.

No run wrote a `WorktreeRemove` line. Section 5 explains why, and separates the
expected case from the gap.

### 4.0 Baseline: the hook driven by hand

The baseline proves the harness before the nested runs. A failed baseline would make
a later "the hook did not run" result meaningless.

The first attempt failed, because the shim had no execute bit:

```
$ .../plugin/claude-code/hooks/worktree-create.sh < baseline.json
error: unexpected argument '--json' found

  tip: to pass '--json' as a value, use '-- --json'

Usage: gh-klon add --path <PATH> <BRANCH>

For more information, try '--help'.
klon hook: gh klon add failed for worktree-baseline (exit 2)
exit=1
```

After `chmod +x` on the shim:

```
$ .../plugin/claude-code/hooks/worktree-create.sh < baseline.json
/home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/baseline
exit=0

$ cat hook.log
2026-09-06T06:40:59Z gh klon add failed for worktree-baseline (exit 2)
2026-09-06T06:41:45Z WorktreeCreate ok name=baseline path=/home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/baseline

$ git worktree list --porcelain
worktree /home/navaro/.local/share/klon/s3-spike/repo
HEAD f1d84fa2f12cafd2a72856461f8bb8cae220aa11
branch refs/heads/main

worktree /home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/baseline
HEAD f1d84fa2f12cafd2a72856461f8bb8cae220aa11
branch refs/heads/worktree-baseline

$ cat .claude/worktrees/baseline/.klon/env
KLON_NAME=worktree-baseline
KLON_IP=127.0.0.2
HOST=127.0.0.2
TMPDIR=/home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/baseline/.klon/tmp
KLON_JOBSERVER=
GIT_CONFIG_COUNT=1
GIT_CONFIG_KEY_0=core.hooksPath
GIT_CONFIG_VALUE_0=/home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/baseline/.klon/hooks
```

The klon was then removed with `gh klon rm --path ... --force`, exit 0, and the log
was cleared.

### 4.1 Entry point 1: the `--worktree` flag

The flag exists in 2.1.263. It took the name as a positional value.

```
$ claude --worktree s3flag -p 'Print the working directory with pwd and stop.' \
      --model claude-sonnet-5 --output-format json --allowedTools Bash
{... "result":"The working directory is `/home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/s3flag`.",
 "is_error":false, "num_turns":2, "subtype":"success", ...}
CLAUDE_EXIT=0
```

```
$ cat hook.log
2026-09-06T06:42:26Z WorktreeCreate ok name=s3flag path=/home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/s3flag

$ git worktree list --porcelain
worktree /home/navaro/.local/share/klon/s3-spike/repo
HEAD f1d84fa2f12cafd2a72856461f8bb8cae220aa11
branch refs/heads/main

worktree /home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/s3flag
HEAD f1d84fa2f12cafd2a72856461f8bb8cae220aa11
branch refs/heads/worktree-s3flag

$ ls -la .claude/worktrees/
drwxrwxr-x 4 navaro navaro 4096 wrz  6 08:42 s3flag

$ ls -la .claude/worktrees/s3flag/.klon/env
-rw-rw-r-- 1 navaro navaro 314 wrz  6 08:42 .../s3flag/.klon/env
```

The session root is the klon. This confirms the first C28 acceptance line on a live
Claude Code, not only in the hook harness.

### 4.2 Entry point 2: the `EnterWorktree` tool

This is the question of the issue title. The tool did **not** bypass the hook.

```
$ claude -p 'Call the EnterWorktree tool with name s3enter, then run pwd with Bash, then stop.' \
      --allowedTools EnterWorktree,Bash --model claude-sonnet-5 --output-format json
{... "result":"The worktree \"s3enter\" exists. The session now works in
 `/home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/s3enter`.",
 "is_error":false, "num_turns":3, "subtype":"success", "permission_denials":[], ...}
CLAUDE_EXIT=0
```

```
$ cat hook.log
2026-09-06T06:43:35Z WorktreeCreate ok name=s3enter path=/home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/s3enter

$ git worktree list --porcelain
worktree /home/navaro/.local/share/klon/s3-spike/repo
HEAD f1d84fa2f12cafd2a72856461f8bb8cae220aa11
branch refs/heads/main

worktree /home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/s3enter
HEAD f1d84fa2f12cafd2a72856461f8bb8cae220aa11
branch refs/heads/worktree-s3enter

$ ls -a .claude/worktrees/s3enter/
.  ..  .claude  .git  .klon  README.md
```

The klon holds `.klon/env`, `.klon/hooks`, and `.klon/tmp`. The branch is
`worktree-s3enter`, which is the klon naming rule of C28.

### 4.3 Entry point 3: a subagent with `isolation: "worktree"`

```
$ claude -p 'Use the Agent tool with subagent_type general-purpose and isolation "worktree" to run pwd with Bash and report the output, then stop.' \
      --allowedTools Agent,Bash --model claude-sonnet-5 --output-format json
{... "result":"The agent ran `pwd` in its isolated worktree. The output:\n\n```\n
 /home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/agent-a1f25247ba820c6ab\n```\n\n
 The agent made no changes. The worktree cleanup is automatic, so no worktree remains. Task complete.",
 "subagent_stats":{"spawned":1,"completed":1,"failed":0,"by_type":{"general-purpose":1}},
 "is_error":false, "subtype":"success", ...}
CLAUDE_EXIT=0
```

The parent session states that no worktree remains. That statement is wrong. It is
the model's own summary, not a measurement. The measurement follows.

```
$ cat hook.log
2026-09-06T06:44:42Z WorktreeCreate ok name=agent-a1f25247ba820c6ab path=/home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/agent-a1f25247ba820c6ab
(end)

$ git worktree list --porcelain
worktree /home/navaro/.local/share/klon/s3-spike/repo
HEAD f1d84fa2f12cafd2a72856461f8bb8cae220aa11
branch refs/heads/main

worktree /home/navaro/.local/share/klon/s3-spike/repo/.claude/worktrees/agent-a1f25247ba820c6ab
HEAD f1d84fa2f12cafd2a72856461f8bb8cae220aa11
branch refs/heads/worktree-agent-a1f25247ba820c6ab

$ ls -la .claude/worktrees/
drwxrwxr-x 4 navaro navaro 4096 wrz  6 08:44 agent-a1f25247ba820c6ab
```

The create hook ran. The klon named the subagent, so klon covers this entry point too.
The log holds no `WorktreeRemove` line. A second check 70 seconds later showed the same
log and the same directory, so the removal is not merely late.

## 5. The removal gap

### 5.1 What the docs promise

The Claude Code hooks reference lists the triggers of each event:

- `WorktreeCreate`: "When a worktree is being created via `--worktree`,
  `isolation: "worktree"`, or for a background session. Replaces default git behavior".
- `WorktreeRemove`: "When a worktree is being removed at session exit, when a subagent
  finishes, or when you delete a background session".

The worktrees page adds that a subagent worktree is removed "automatically when the
subagent finishes without changes".

Two notes on the create list. It does not name `EnterWorktree`, yet section 4.2 shows
that `EnterWorktree` runs the hook. The docs undersell the coverage. That is a
documentation gap, not a klon gap, and it works in klon's favour.

### 5.2 What happened

Entry point 3 finished with a clean worktree:

```
$ cd .claude/worktrees/agent-a1f25247ba820c6ab && git status --porcelain
(no output)
```

The tree was clean, so the documented rule asks Claude Code to remove it. Claude Code
did not remove it, and did not call the `WorktreeRemove` hook.

### 5.3 The control

The same prompt ran in a second repository with **no hooks**, at
`/home/navaro/.local/share/klon/s3-spike/control`:

```
$ claude -p 'Use the Agent tool with subagent_type general-purpose and isolation "worktree" ...' \
      --allowedTools Agent,Bash --model claude-sonnet-5 --output-format json
{... "result":"The agent ran `pwd` in its worktree. The output is:\n\n```\n
 /home/navaro/.local/share/klon/s3-spike/control/.claude/worktrees/agent-a6597ba0b03a822bd\n```\n\n
 The agent made no changes, so the worktree cleanup happened automatically.", ...}
CLAUDE_EXIT=0

$ git worktree list --porcelain
worktree /home/navaro/.local/share/klon/s3-spike/control
HEAD 220d445f7295c5ff6b5d7f2873cc532c375248da
branch refs/heads/main

$ ls -la .claude/worktrees/
total 8
drwxrwxr-x 2 navaro navaro 4096 wrz  6 08:47 .
drwxrwxr-x 3 navaro navaro 4096 wrz  6 08:46 ..
```

Without the hook, Claude Code removed the worktree. With the hook, it did not. The
hook is the only difference between the two runs.

### 5.4 The cause

Claude Code marks each worktree that it makes with git. The worktrees page says so:
"Claude Code writes a marker into the git metadata of every worktree it creates with
git, and the sweep keeps any worktree without one, including a worktree a
`WorktreeCreate` hook created."

A third control named the marker. `claude --worktree ctrlmark -p ...` ran in the
control repository, which has no hooks, so Claude Code made the worktree with git:

```
$ ls -la control/.git/worktrees/ctrlmark/
-rw-rw-r-- 1 navaro navaro   40 wrz  6 08:48 CLAUDE_BASE
-rw-rw-r-- 1 navaro navaro    6 wrz  6 08:48 commondir
-rw-rw-r-- 1 navaro navaro   80 wrz  6 08:48 gitdir
-rw-rw-r-- 1 navaro navaro   34 wrz  6 08:48 HEAD
-rw-rw-r-- 1 navaro navaro  137 wrz  6 08:48 index
-rw-rw-r-- 1 navaro navaro   54 wrz  6 08:48 locked
drwxrwxr-x 2 navaro navaro 4096 wrz  6 08:48 logs
-rw-rw-r-- 1 navaro navaro   41 wrz  6 08:48 ORIG_HEAD

$ cat control/.git/worktrees/ctrlmark/CLAUDE_BASE
220d445f7295c5ff6b5d7f2873cc532c375248da

$ cat control/.git/worktrees/ctrlmark/locked
claude session ctrlmark (pid 2540987 start 298641136)
```

The hook-made worktree of entry point 3 has neither file:

```
$ ls -la repo/.git/worktrees/agent-a1f25247ba820c6ab/
-rw-rw-r-- 1 navaro navaro    6 wrz  6 08:44 commondir
-rw-rw-r-- 1 navaro navaro   92 wrz  6 08:44 gitdir
-rw-rw-r-- 1 navaro navaro   49 wrz  6 08:44 HEAD
-rw-rw-r-- 1 navaro navaro  504 wrz  6 08:44 index
drwxrwxr-x 2 navaro navaro 4096 wrz  6 08:44 logs

$ find repo/.git/worktrees -name locked
(no output)
```

`CLAUDE_BASE` holds the base commit. `locked` holds the session that owns the tree.
A hook makes the worktree, so klon writes neither file, and Claude Code cannot make
them: the hook contract returns a path on stdout and nothing else.

This is the likely mechanism, stated as an inference from three facts: the marker is
absent, the removal did not happen, and the same removal happened when the marker was
present. The spike did not read the Claude Code source, so it cannot prove the code
path.

### 5.5 What is expected, and what is the gap

| Case | `WorktreeRemove` ran | Verdict |
|---|---|---|
| Entry points 1 and 2, print mode | No | **Expected.** The docs say print mode has no exit prompt, so "Claude doesn't clean up their worktrees". This holds with git too. |
| Entry point 3, subagent finish | No | **The gap.** The docs promise removal here, and the control shows it happens without the hook. |
| The periodic sweep | Not reached | **The gap widens.** The sweep keeps every worktree without the marker, so it never retries. |

The leak is bounded per session but unbounded over time. One long session that spawns
twenty isolated subagents leaves twenty klons. Each klon holds a warm copy of the
ignored files, so the disk cost is real.

### 5.6 The workaround

`gh klon rm --path <path>` removes a leaked klon. `gh klon list` finds it. Neither
needs the plugin.

The user must pick the path with care, because `gh klon list` shows every klon of the
repository, not only the leaked ones. A leaked subagent klon carries the name
`agent-<id>`, which Claude Code assigns; a klon from `--worktree` or `EnterWorktree`
carries the user's own name.

`rm` must run **without** `--force`. Plain `rm` refuses a dirty klon and a klon with a
live process, and it names the reason (`src/cli/rm.rs` lines 98 and 115). That refusal
is the safety net. `--force` removes both checks, so it belongs only on a klon the user
has looked at and decided to discard.

The leaked klons are not locked, because Claude Code writes the `locked` file only for
a worktree it makes with git. A plain `git worktree remove` also works. This is a small
mercy: the user never needs `git worktree unlock` first.

The spike itself used `--force`, because its klons were throwaway and it removed them
in a script. The README must not teach that habit.

## 6. What this means for klon

1. **No create-side work is needed.** All three entry points reach the hook. C28 covers
   every documented way that Claude Code makes a worktree, and one undocumented way
   (`EnterWorktree`).
2. **The C28 acceptance line "with the hook installed, `claude --worktree test -p 'pwd'`
   prints a path under `.claude/worktrees/` that `git worktree list` shows as a klon"
   holds on a live Claude Code**, not only in the hook harness. Section 4.1 is the
   evidence.
3. **klon must not depend on `WorktreeRemove` for subagent worktrees.** The `rm` and
   `prune` commands stay the only reliable cleanup for that case.
4. **A future `gh klon prune` improvement could help**, because the leaked klons sit
   under one predictable directory with one predictable branch prefix. This spike does
   not ask for that work. It records the option.

## 7. Decision

**Document the gap in `plugin/claude-code/README.md`.** The same pull request adds the
paragraph. The reasons:

- The gap has a one-command workaround that klon already ships.
- klon cannot fix the gap. Only Claude Code can write its own marker.
- The user meets the gap the first time a subagent finishes, so the note belongs in the
  install document, not in a spike report that the user never reads.

**Do not file the upstream issue from this spike.** Section 8 holds the text. The
maintainer decides whether to file it. The spike has no mandate to open an issue in
another project.

## 8. Draft upstream issue (not filed)

> **Title:** A `WorktreeCreate` hook stops subagent worktree cleanup, and `WorktreeRemove` never fires
>
> **Version:** Claude Code 2.1.263 on Ubuntu 22.04, git 2.34.1.
>
> **What happens**
>
> With a `WorktreeCreate` hook installed, a subagent that runs with
> `isolation: "worktree"` leaves its worktree on disk after it finishes. The
> `WorktreeRemove` hook never runs for that worktree.
>
> The docs say the opposite in two places. The hooks reference lists "when a subagent
> finishes" as a `WorktreeRemove` trigger. The worktrees page says Claude Code removes a
> subagent worktree "automatically when the subagent finishes without changes".
>
> **How to reproduce**
>
> 1. Make a git repository with one commit.
> 2. Add a `WorktreeCreate` hook and a `WorktreeRemove` hook to `.claude/settings.json`.
>    A create hook that runs `git worktree add` itself and prints the path is enough.
>    Make each hook append one line to a log file.
> 3. Run, from the repository root:
>
>        claude -p 'Use the Agent tool with subagent_type general-purpose and isolation "worktree" to run pwd with Bash and report the output, then stop.' --allowedTools Agent,Bash --output-format json
>
> 4. Read the log. It holds the create line and no remove line.
> 5. Run `git worktree list`. The subagent worktree is still there. The tree is clean:
>    `git status --porcelain` inside it prints nothing.
>
> **The control**
>
> The same prompt in the same repository with the two hook entries removed does remove
> the worktree. The hook is the only difference.
>
> **The likely cause**
>
> Claude Code writes a `CLAUDE_BASE` file and a `locked` file into
> `.git/worktrees/<name>/` for every worktree it makes with git. A hook-made worktree
> has neither. The worktrees page already says the periodic sweep keeps a worktree
> without the marker. The subagent-finish path appears to use the same test, so a
> hook-made worktree is never removed and the `WorktreeRemove` hook is never called.
>
> **Why this matters**
>
> A `WorktreeCreate` hook is the documented way to use Claude Code with a non-git
> version control system, and the documented way to place worktrees elsewhere. Every
> such user leaks one worktree per isolated subagent, with no automatic recovery,
> because the sweep skips them too.
>
> **Suggested fix**
>
> Call the `WorktreeRemove` hook whenever a worktree the session owns is finished with,
> whatever made it. The session already knows the path: it received the path from the
> create hook. Alternatively, write the marker for a hook-made worktree as well.

## 9. Limits of this spike

1. Each nested run used print mode. An interactive session was not tested. Print mode
   skips the workspace trust check and the exit cleanup prompt, so an interactive
   session may clean up differently.
2. The background-session entry point was not tested. The docs list it as a third
   `WorktreeCreate` trigger. It is the one documented trigger this spike leaves open.
3. Each run used one model, `claude-sonnet-5`. The entry point does not depend on the
   model, so this is not a real limit.
4. The cause in section 5.4 is an inference from observed files, not a source reading.
5. The spike ran on one host and one Claude Code version.

## 10. Cleanup

The spike removed each klon with `gh klon rm --path ... --force`, exit 0 each time,
and deleted the whole temporary tree under `/home/navaro/.local/share/klon/s3-spike`
at the end. The project repository holds no leftover state.

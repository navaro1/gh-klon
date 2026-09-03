# Handoff notes: gh-klon, 2026-09-03

This note tells a fresh session where the project stands and what to do next. Read it first.

## State on main

| Item | Location | Role |
|---|---|---|
| Design, revision 3 | `docs/klon-handoff.md` | The authoritative design. Wins on design questions. |
| Specification | `docs/klon-spec.md` | Requirements R1 to R40, chunks C0 to C32, spikes S1 to S3, goal sessions G1 to G4. Wins on acceptance questions. |
| Research record | `docs/klon-research-2026-09-03.md` | Revision 2 (PR #1), verbatim. Measurements and sources. |
| Evidence record | `docs/klon-evidence.md` | From PR #2, verbatim. Pinned competitor commits. |
| Proposal record | `docs/proposals/2026-09-03-evidence-gated-workspaces.md` | The receipt architecture from PR #2, verbatim. Kept for v2. |
| Tickets | GitHub issues #3 to #42 | One issue per chunk, spike, and goal. Labels `type:chunk`, `type:spike`, `type:goal`, `platform:*`. Milestones v1.0 to v1.3 and release. |

Both design PRs are merged. No open pull request exists.

## What the reconciliation decided

Handoff §2 has the full table. The short form:

1. The copy-on-write clone of the whole directory is the product. A byte copy is the fallback, never the default.
2. Git creates the worktree admin entry. klon replaces only the working directory. Verified on git 2.34.1.
3. A journal plus `doctor --repair` protects every `add` and `rm`.
4. Repository-supplied commands in `.klon.toml` need one approval per content hash.
5. The envelope (Landlock or Seatbelt fence, systemd scope, jobserver, loopback address) is core. Each part degrades with a message when the host lacks it.
6. Receipts, `verify`, and SQLite from PR #2 are deferred to v2. A light `check` receipt and `claim` ship in v1.2.
7. The jobserver uses the pipe-style handshake. The fifo style is a fatal error on make 4.3.

## Facts verified on the development laptop (2026-09-03)

Handoff §11 has the two tables. The facts that shape the first chunks:

- Ubuntu 22.04, ext4, git 2.34.1, make 4.3, cargo 1.92, no `btrfs-progs`, no `pasta`.
- Landlock ABI 3 fences writes without privileges.
- `systemd-run --user --scope` applies `MemoryHigh` and `TasksMax` with no password. `CPUWeight` is ignored.
- The register-then-fill order for `add` works. `git checkout` moves the tree in 0.31 s on 100k files.
- The first `git status` costs 0.45 s with `core.checkStat=minimal`; later calls 0.1 s.
- `udisksctl loop-setup` and `mount` need no password. The mount root is root-owned; seeded files keep their owner.

## How to start

1. Install the toolchain: `cargo 1.92` is present. Nothing else is needed for C0.
2. Take the first open issue in milestone v1.0 (C0). Its body holds the build line and the acceptance list. `docs/klon-spec.md` §7 has the same text.
3. Work in a branch, open a pull request, and close the issue from the pull request.
4. Run `python3 ~/.claude/skills/writing-specs/scripts/validate_spec.py docs/klon-spec.md` after any spec edit.
5. For a goal ticket (G1 to G4), open Claude Code in the repository and run `/goal` with the condition from the issue body. The bench JSON must appear in the transcript.

## Spikes that need another host

- S1 (btrfs loop volume) needs `btrfs-progs`: `sudo apt install btrfs-progs` on this laptop, or any btrfs host.
- S2 (jetsam) needs a Mac.
- S3 (Claude Code `EnterWorktree`) needs the C28 plugin first.

## Local artefacts, not committed

- `/tmp/klon-git-check` (895 MB): the git 2.34.1 fixture and the grafted worktree `wt/x`. Safe to delete.
- `/tmp/klon-reconcile`: the staging copies used for the merge. Safe to delete.

## Review

A separate Opus reviewer read the handoff and the specification before the tickets were created. It verified its claims with commands on the development laptop. It found 6 blockers, 10 majors, and 3 minors. All 19 are applied. A confirmation pass marked all 19 resolved and gave 5 small follow-ups, also applied. The final verdict was "ready".

The blockers, for the record:

1. `git checkout` aborts on a dirty golden. `add` now uses `git checkout --force` then `git clean -fdq`.
2. The fence blocked every git write. It now allows the git object, ref, and log directories and the klon's own worktree directory, never the `<common>` root.
3. `git worktree remove` deletes inline. `rm` now renames, drops the `.git` file, and runs `git worktree prune`.
4. `doctor --repair` could not remove a locked worktree. It now unlocks first.
5. A null index trailer fails `git fsck`. G4 now writes a correct SHA-1 trailer.
6. `.klon/` showed as untracked. `add` now adds `/.klon/` to `<common>/info/exclude`.

## Ticket map

| Milestone | Issues |
|---|---|
| v1.0 local worktree replacement | #3 C0, #4 C1, #5 C2, #6 C3, #7 C4, #8 C5, #9 C6, #10 C7, #11 C8, #12 C9, #13 C10, #14 C11, #15 C12, #16 C13, #17 C14, #18 S1, #19 C15 |
| v1.1 envelope | #20 C16, #21 C17, #22 C18, #23 C19, #24 C20, #25 S2, #26 C21, #27 C22, #28 C23 |
| v1.2 integration certainty | #29 C24, #30 C25, #31 C26, #32 C27, #33 C28, #34 S3, #35 C29, #36 C30 |
| v1.3 performance goals | #37 C31, #38 G1, #39 G2, #40 G3, #41 G4 |
| release | #42 C32 |

Each issue body lists its dependencies as issue links. Start with #3.

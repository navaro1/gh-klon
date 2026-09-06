# gh klon: implementation specification

Date: 2026-09-03 · Status: Draft · Scope: the full `gh klon` product from an empty repository to a released `gh` extension · Predecessors: `docs/klon-handoff.md` (revision 3)

This specification turns the handoff into requirements and dependency-ordered chunks. Each chunk becomes one GitHub issue. The handoff holds the design reasons. This document holds the build order and the acceptance checks. Where the two differ, the handoff wins on design and this document wins on acceptance.

Terms:
- **golden**: the main checkout on the base branch.
- **klon**: a linked worktree that klon created.
- **common**: the output of `git rev-parse --git-common-dir`.
- **backend**: one implementation of the whole-directory clone (`copy`, `reflink-walk`, `apfs-clone`, `btrfs-snapshot`).
- **fixture**: a generated repository with a fixed seed, N tracked files, one ignored directory, and one feature branch.
- **oracle**: the result of the installed `git` for the same operation.
- **manifest**: a sorted list of (path, type, size, mode, mtime, symlink target, content hash) for a directory tree.

---

## 1. Objective & Non-Goals

**Objective.** Build `gh klon`, a `git worktree` replacement that spawns a warm, fenced, mergeable copy of a project for each coding agent, on Linux first and macOS last, with zero `sudo` on the default path. The first milestone runs on the development laptop through `gh extension install .`. The macOS backend and envelope are the last milestone.

**What NOT to build (non-goals):**
1. **A daemon.** Background work is a detached child process that one command starts (handoff §1).
2. **A virtual filesystem, a VM, or FUSE.** Rejected in handoff §10.
3. **Content-bound proof receipts with execution manifests, `verify`, or SQLite.** Deferred to v2 (handoff §2).
4. **Agent orchestration.** klon gives a directory, an env file, and `--json`.
5. **Windows.** Out of v1.
6. **A reimplementation of any git plumbing.** klon shells to `git` for every repository state change.

---

## 2. Context & Sources (grounding)

**Host facts (2026-09-03).**
- The repository `navaro1/gh-klon` on `main` at commit `0818cc5` holds only `docs/`. No `Cargo.toml`, no source, no CI exists (`ls -la` on 2026-09-03).
- The development laptop: Ubuntu 22.04.5, kernel 6.2.0-36, 20 CPUs, 62 GiB RAM, one ext4 partition, git 2.34.1, systemd 249.11, make 4.3, cargo 1.92.0, rustc 1.92.0, pnpm 10.28.2, node 18.17.1, uv 0.12.5, gh 2.74.0 (snap), Claude Code 2.1.259 (handoff §11).
- Absent on the laptop: `btrfs-progs`, `pasta`, `ninja`, `dotnet`, `go` (handoff §11).
- Landlock ABI 3 works without privileges. `systemd-run --user --scope` applies `MemoryHigh` and `TasksMax` with no password. `CPUWeight` is ignored on systemd 249 (handoff §11).
- A fifo-style jobserver is a fatal error on make 4.3. The pipe style works on make 4.3 and cargo 1.92 (handoff §11).
- `udisksctl loop-setup` and `mount` need no password. The mount root is `root:root`. Seeded files keep their owner (handoff §11).
- `gh extension search klon` returns nothing. The name is free (handoff §11).
- Snap `gh` cannot read `/tmp` or `~/.t3`. Tests that call `gh` must run from a path under `$HOME`.
- Git 2.34.1 experiment on a 100k-file fixture (handoff §11): `git worktree add --no-checkout --detach --lock` writes no index and accepts a replaced working directory; `git checkout` moves the tree in 0.31 s; the first `git status` costs 0.45 s with `core.checkStat=minimal` and 2.63 s without; `git worktree remove` deletes inline in 4.28 s; `git worktree add` refuses a non-empty path even with `--force`; the legacy `merge-tree` form detects a conflict in 0.01 s.

**Existing code / consumer contracts.**
- No existing code. Every module in §4 is new.
- Consumer contract 1: `git worktree list --porcelain`, `git worktree remove`, `git worktree prune`, and `git worktree repair` must keep working on a klon (handoff §3).
- Consumer contract 2: Claude Code `WorktreeCreate` hook: stdin JSON with `hook_event_name`, `cwd`, `name`; stdout is the path only; a non-zero exit aborts. `WorktreeRemove` gets `worktree_path` (research record §19).
- Consumer contract 3: the `/goal` evaluator reads only the transcript. A goal ticket must name a command whose printed output proves the condition (https://code.claude.com/docs/en/goal).

**Pinned versions (from `cargo search` on 2026-09-03).**
`clap 4.6.6`, `serde_json 1.0.151`, `toml 1.1.5`, `rayon 1.12.0`, `reflink-copy 0.1.30`, `ignore 0.4.33`, `grep-searcher 0.1.17`, `landlock 0.4.7`, `jobserver 0.1.35`, `nix 0.31.3`, `libc 0.2` (latest stable; `1.0.0-alpha` is a prerelease), `walkdir 2.5.0`, `filetime 0.2.29`, `tempfile 3.27.0`, `assert_cmd 2.2.2`. Rust edition 2021. MSRV 1.85.

**External references.**
- `docs/klon-handoff.md` revision 3: design, decisions, metrics, backends, envelope, rejected designs.
- `docs/klon-research-2026-09-03.md`: measurements (§15), sources (§20), harness conventions (§19).
- `docs/klon-evidence.md`: pinned competitor commits; Apple and Linux clone facts.
- https://code.claude.com/docs/en/goal: `/goal` condition shape (measurable end state, check command, constraints, turn cap).
- https://github.com/cli/gh-extension-precompile: release asset naming and workflow.

---

## 3. Requirements & Acceptance Criteria

Functional requirements. Each uses the form WHEN / WHILE / IF / shall.

**Core (v0)**
- **R1** — WHEN the user runs `gh klon add <branch>`, klon shall create a linked worktree that `git worktree list --porcelain` reports with the path, HEAD, and branch, without a klon process.
- **R2** — WHEN `add` completes, the tracked files of the klon shall equal the tree of `<branch>`: `git status --porcelain` prints nothing and `git diff --quiet HEAD` exits 0.
- **R3** — WHEN `add` completes, the ignored files of the klon shall equal the manifest of golden's ignored files, minus `.git`, `.klonignore` matches, and the delete list (`.next/cache`, `.ninja_log`, `.ninja_deps`).
- **R4** — klon shall NOT create a writable hardlink between a klon and golden, a spare, or another klon.
- **R5** — WHEN `doctor` probes a backend, klon shall select the backend only if a fixture clone passes the manifest test, and shall report the selected backend and the reason in `--json`.
- **R6** — IF `add` or `rm` stops before completion, THEN a repeated command or `doctor --repair` shall reach the prior valid state or the completed state, and shall leave no half-registered worktree.
- **R7** — `rm` shall refuse the repository root, the home directory, an unresolved path template, a dirty tree without `--force`, and a tree with live processes without `--force`.
- **R8** — WHEN `rm` accepts a target, it shall return within 100 ms on the 10k fixture and on the 100k fixture, and shall delete in a background process at low priority.
- **R9** — `add`, `list`, `rm`, `doctor`, and `bench` shall give JSON with `--json` that carries a `schema` field, and a schema test shall fail on an undocumented incompatible change.
- **R10** — WHEN golden has a completed `cargo build` or `pnpm install`, the first `cargo build` or `pnpm install --frozen-lockfile --offline` in a fresh klon shall compile zero units and download zero bytes.
- **R11** — WHEN the user runs `git status` in a fresh klon of the 100k fixture with a warm cache, the first call shall complete in at most 500 ms at p50 and the second call in at most 150 ms at p50.
- **R12** — WHILE a spare exists that matches golden's HEAD, or golden is a btrfs subvolume, `add` on the 100k fixture shall complete in at most 1 s at p50.
- **R13** — WHEN `add` runs on ext4 without a volume, it shall succeed with at most one warning line and zero `sudo` prompts.
- **R14** — `bench` shall write a versioned manifest, the raw samples, p50 and p95, and an environment record, and shall void the timing of any cell with a correctness mismatch.
- **R15** — Path fixup shall rewrite golden's absolute path only in files that are text, at most 1 MB, valid UTF-8, and outside the skip list, and shall log each rewritten path.
- **R16** — klon shall NOT run a command from `.klon.toml` (`warm.steps`, `proof.steps`, `copy.reinstall`) before the user approves that file's content hash once, and `--yes` shall approve in a non-interactive run.
- **R29** — `up` shall refuse a dirty golden or a golden not on the base branch, shall fetch and fast-forward, shall run approved `[warm] steps`, and shall start a spare.
- **R30** — `add` shall resolve a local branch, then `origin/<name>`, then a new branch from base; and shall accept `origin/<name>`, `--pr <n>`, and `--issue <n>`. `sync` shall fast-forward, rebase with `--autostash`, or `--merge`, and shall refuse a force-pushed upstream unless no unique local commits exist or `--force`.
- **R31** — `doctor` shall report the backend, git version, fence ABI, cgroup delegation, inotify limits, make and ninja versions, pasta, `btrfs-progs`, and stale journal entries; repeated runs shall give the same result.
- **R32** — WHEN golden is a user-owned btrfs subvolume, the `btrfs-snapshot` backend shall clone with one snapshot call; `init` shall convert a plain golden directory into a subvolume under a journal entry, after a printed plan and a `y` answer; and `init --undo` shall restore the prior layout.
- **R33** — `init --volume <size>` shall create a btrfs loop volume through `udisksctl` with zero `sudo` prompts in an active local session, with a user-owned working directory.
- **R34** — The `apfs-clone` backend shall NOT call `clonefile` on the repository root; it shall clone each top-level ignored directory with one call and tracked files per file.
- **R35** — The `reflink-walk` backend shall use `FICLONE` per file with 4 workers and shall restore the source mtime on each clone.
- **R36** — The `copy` backend shall copy big-file directories and re-install approved small-file directories, and shall land warm directories through an atomic rename.
- **R39** — klon shall read `.klon.toml`, `.klonignore` (gitignore syntax), and `.worktreeinclude` (additive include), and shall exclude nested `.git` directories and submodule paths by default.
- **R41** — WHEN the selected backend copies bytes, `add` shall refuse before any change when the free space is below 1.2 times the estimated size, shall print the shortfall, and shall print a progress line on a TTY that `--json` suppresses.
- **R40** — WHEN `add`, `up`, or `rm` completes, klon shall start one detached low-priority process that prepares `.spare/`, and `add` shall use the spare only when its recorded HEAD equals golden's HEAD and its tear check passed.

**Envelope (v0.1)**
- **R17** — WHILE a process runs under `run`, `shell`, or `add -- cmd`, a write outside the klon, the git object, ref, and log directories, the klon's own worktree directory, `TMPDIR`, `/tmp`, the declared caches, and `[fence] allow` shall fail with `EACCES` on Linux or `EPERM` on macOS; a write inside shall succeed; `git commit` inside the klon shall succeed; and `<common>/hooks` and `<common>/config` shall stay read-only.
- **R18** — WHILE a process runs under `run` on Linux with systemd ≥ 249, its cgroup shall carry `memory.high` = total / (N+1) and `pids.max`; on macOS klon shall poll the footprint and send SIGTERM above a threshold.
- **R19** — WHILE N klons build under `run`, the count of concurrent jobserver-aware compile processes shall not exceed the token count plus N, and a build under make 4.3 shall not fail because of the jobserver.
- **R20** — WHEN an agent edits a hook inside a klon, that hook shall NOT run in golden or a sibling klon.
- **R21** — `run` shall export `KLON_IP=127.0.0.N` unique per live klon, and on Linux a bind to that address shall succeed; `run --netns` shall map host `127.0.0.N:<port>` into a `pasta` namespace when `pasta` is present; on macOS `gh klon lo0` shall print the alias command.
- **R22** — `stop` shall end every process in the klon's process tree within 5 s.

**Integration certainty (v0.2)**
- **R23** — `list` shall show conflicts vs base and vs each sibling from `git merge-tree --write-tree` on git ≥ 2.38, or from the legacy `git merge-tree <base> <a> <b>` form below 2.38, within 40 ms per pair on the 100k fixture.
- **R24** — `merge` shall stop when the `pre_merge` hook fails, shall use mergiraf when installed, shall fast-forward base, shall remove the klon, and shall NOT push.
- **R25** — `check` shall refuse a dirty tree, shall run approved `[proof] steps` inside the envelope, and shall write a receipt bound to the commit; `merge` shall refuse without a receipt for HEAD unless `--no-check`.
- **R26** — WHEN two `claim` calls overlap concurrently, exactly one shall succeed; a prefix claim shall respect path component boundaries.
- **R27** — WHEN Claude Code runs with the klon plugin, `WorktreeCreate` shall create a klon and print its path, and `WorktreeRemove` shall remove it.
- **R28** — `hibernate` then `wake` shall restore the tracked diff and the untracked non-ignored files exactly; a hibernated klon shall use at most 1 MB on disk outside the object store; and `disk_budget` shall refuse `add` and name the candidate by default, and shall hibernate only with `--evict` or `disk_budget_action = "hibernate"`.
- **R38** — `list` shall show the disk delta, RSS, live process count, PR number, and check status, with PR data cached for 60 s.

**Release**
- **R37** — A release shall publish assets for macOS arm64, macOS x86-64, Linux x86-64 glibc, and Linux arm64 through `gh-extension-precompile`, and `gh extension install navaro1/gh-klon` shall work on a clean host.

Non-functional and edge cases:
- Every missing host feature degrades with one stderr line and a `doctor` entry. Silence is not allowed.
- A JSON schema change increments the `schema` version suffix.
- The 100k fixture: 100,000 tracked files in 1,000 directories, one ignored directory with 10,000 files, a feature branch with 20 changed and 2 added files, seed 1.
- The 10k fixture: the same shape at one tenth.

Acceptance (representative, Given / When / Then):
- *Given* golden on the 10k fixture with a completed build, *when* `gh klon add feature`, *then* `git -C <klon> status --porcelain` prints nothing, `git worktree list --porcelain` lists the klon, and the ignored manifest equals golden's.
- *Given* a klon with a modified tracked file, *when* `gh klon rm feature`, *then* the command exits non-zero with `dirty` in the message and the tree still exists.
- *Given* `add` killed with SIGKILL after registration, *when* `gh klon doctor --repair`, *then* `git worktree list` shows no klon and the journal is empty.
- *Given* a process under `gh klon run`, *when* it writes to golden, *then* the write fails with `EACCES`.

---

## 4. Design (HOW)

**Architecture.** One Rust binary. Commands are thin. Every git state change is a `git` subprocess. Each host feature sits behind a probe.

```
gh-klon/
  Cargo.toml                 package gh-klon, bin gh-klon, edition 2021, MSRV 1.85
  src/main.rs                clap dispatcher, --json, exit codes
  src/git.rs                 subprocess wrapper: worktree add/unlock/remove/list --porcelain, checkout, status, config, rev-parse
  src/paths.rs               path template, managed-path resolution, common dir
  src/journal.rs             <common>/klon/journal/<name>.json, state machine, repair
  src/config.rs              .klon.toml, .klonignore, .worktreeinclude, approvals
  src/backend/mod.rs         trait Backend { probe, clone, delete }, manifest, probe cache
  src/backend/copy.rs        std::fs copy, per-directory strategy, background warm
  src/backend/reflink.rs     FICLONE walk, rayon 4 workers, utimensat
  src/backend/apfs.rs        clonefile per top-level ignored dir, clonefileat per tracked file
  src/backend/btrfs.rs       subvolume snapshot, init convert, loop volume
  src/spare.rs               detached spare builder, tear check, claim by rename
  src/fixup.rs               ignore + grep-searcher text rewrite with rails
  src/cli/{add,rm,list,prune,doctor,bench,up,sync,pr,init,run,shell,stop,merge,check,claim,hibernate,lo0}.rs
  src/envelope/env.rs        .klon/env, loopback slot allocation
  src/envelope/jobserver.rs  fifo token store, pipe-style handshake, top-up
  src/envelope/fence_linux.rs   landlock crate ruleset
  src/envelope/fence_macos.rs   Seatbelt profile + sandbox-exec
  src/envelope/scope_linux.rs   systemd-run --user --scope, cgroupfs fallback, nice fallback
  src/envelope/scope_macos.rs   posix_spawnattr QoS clamp, setsid, footprint poll
  src/envelope/netns.rs      pasta wrapper
  src/radar.rs               merge-tree runner + cache
  src/receipt.rs             check receipts
  src/claims.rs              claims.json under flock
  src/bench/{manifest,fixture,runner,report}.rs
  tests/common/mod.rs        fixture generator, manifest, oracle helpers
  tests/*.rs                 integration tests per chunk
  plugin/claude-code/        hooks for WorktreeCreate / WorktreeRemove
  .github/workflows/ci.yml   ubuntu-22.04, ubuntu-24.04 (btrfs, xfs loop), macos-14
  .github/workflows/release.yml   gh-extension-precompile
```

**The `add` transaction** follows handoff §4 step by step: journal, `git worktree add --no-checkout --detach --lock`, clone into the registered path (the destination and every registered worktree excluded), rewrite `.git`, index copy with a fresh mtime, `git checkout -q --force`, `git clean -fdq`, path fixup, env, unlock, spare. `/.klon/` sits in `<common>/info/exclude`.

**Extension axis (OCP).** Three axes:
1. Filesystems: one file per `Backend`. A new filesystem adds a file and a probe. It never edits `add`.
2. Envelope parts: fence, scope, jobserver, and netns are independent modules with a `Capability` probe each. `run` composes the ones that are present.
3. Commands: one file per command under `src/cli/`. A new command adds a file and one dispatcher line.

**State on disk.** Under `<common>/klon/`: `journal/`, `probe.json`, `radar/`, `receipts/`, `claims.json`, `slots.json`. In each klon: `.klon/env`, `.klon/hooks/`, `.klon/tmp/`. Every file has a `version` field. An unknown future version fails closed.

**Error handling and degradation.** Each host probe returns `Present`, `Absent(reason)`, or `Broken(reason)`. `run` prints one line per absent feature and continues. `add` never refuses because of an absent feature. `doctor` lists every absent feature with the fix. A `git` subprocess failure returns the git exit code and the git stderr unchanged.

**JSON.** Each object has `"schema": "klon.<command>/<n>"`. Tests hold the documented schema and fail on a removed or retyped field.

**Test tiers.** Per change: `cargo test` on ubuntu-22.04 and macos-14 with the 10k fixture. Nightly: the 100k fixture, btrfs and xfs loop jobs on ubuntu-24.04, the bench cells. Release: both git floors, both macOS architectures.

---

## 5. Boundaries

Always:
- klon shells to `git` for every repository state change.
- klon writes a journal entry before each state change, including `init`.
- Every host feature is optional. klon degrades with one message when a feature is absent.
- klon logs each path-fixup rewrite.
- `git worktree` commands keep working on a klon.

Ask first:
- The first run of any command from `.klon.toml`.
- `rm` of a dirty tree or of a tree with live processes.
- `init` and `init --volume`. They move golden. klon prints the plan with both paths and waits for `y`.
- A package install during a spike.

Never:
- A writable hardlink to golden.
- A `clonefile` call on the repository root.
- `sudo` on the default path.
- A push from `merge`.
- A daemon.
- A hardcoded path from the development laptop in the source.
- A branch delete from `rm` without `--merged` or `--delete-branch`.
- `git worktree remove` on the `rm` hot path. It deletes inline. The one exception is the cross-filesystem fallback in C3.

---

## 6. Open Questions

All design questions have a decision or a spike. See handoff §12. The spikes are S1 (btrfs loop volume), S2 (macOS jetsam), and S3 (Claude Code `EnterWorktree`). No question blocks C0 to C14.

---

## 7. Chunks and Acceptance Criteria

Delivery policy: each chunk is one GitHub issue and one pull request. Each chunk includes its own acceptance tests. The milestones are v0, v0.1, v0.2, v0.3, release, and v0.4 macOS improvements. A size estimate follows each title. Split a chunk when it goes above 400 lines.

### C0 — First end-to-end path: `add` with the copy backend (~380 lines)
**Status:** `[ ]` pending
**Build:** Create the Cargo project and the `gh-klon` binary. Implement `gh klon add <branch> [--path <p>]` for an existing local branch with these steps:
1. Refuse a path that is not empty. Refuse a path inside golden unless it is under `.claude/worktrees`, `.t3`, or a path that `.klonignore` excludes.
2. Run `git worktree add --no-checkout --detach --lock <path>`.
3. Append `/.klon/` to `<common>/info/exclude` once, with a duplicate guard.
4. Copy golden into `<path>` with `std::fs` on one thread. Exclude `.git`, the destination path, and every path from `git worktree list --porcelain`.
5. Rewrite `<path>/.git` to `gitdir: <common>/worktrees/<name>`.
6. Copy golden's index into `<common>/worktrees/<name>/index` and give it a fresh mtime. `--no-checkout` writes no index.
7. Set `core.checkStat=minimal`, `core.untrackedCache=true`, and `index.version=4` in the shared config.
8. Run `git checkout -q --force <branch>`. This resets golden's dirty tracked paths and moves the tree in one step.
9. Run `git clean -fdq` to remove untracked non-ignored paths. Ignored paths stay.
10. Run `git worktree unlock <path>`.
11. On a failure after step 2, run `git worktree unlock` and `git worktree remove --force`, then exit with the git error.

Add `README.md` with the local install line: `cargo build --release && ln -sf target/release/gh-klon gh-klon && gh extension install .`. Add `/gh-klon` to `.gitignore`.
**AC:**
- On a generated 10k fixture with a `build/` ignored directory, `gh klon add feature` exits 0 and `git -C <klon> status --porcelain` prints nothing.
- `git worktree list --porcelain` lists the klon path with `branch refs/heads/feature`.
- `git -C <klon> rev-parse HEAD^{tree}` equals `git rev-parse feature^{tree}`.
- The manifest of `<klon>/build` equals the manifest of `golden/build` (content hash, size, mode, mtime).
- Only the differing files of `feature` have an mtime newer than the copy time.
- Given golden with a modified `f2.txt` that also differs on `feature`, `add feature` exits 0 and `git -C <klon> status --porcelain` prints nothing.
- `add` of a branch that golden has checked out exits non-zero with `already checked out`, and `git worktree list` shows no new entry.
- `add --path <golden>/.claude/worktrees/x feature` terminates and produces a klon with no `.claude/worktrees` inside it.
- After the documented install line, `gh klon --version` prints the crate version.
- A second `gh klon add feature` on the same path exits non-zero with `path not empty` and changes nothing.
**Depends on:** — · **Traces to:** R1, R2, R3

### C1 — Fixture generator, manifest, and oracle harness (~250 lines)
**Status:** `[ ]` pending
**Build:** `tests/common/mod.rs`: `Fixture::generate(seed, tracked_files, dirs, ignored_files, diff_paths)` that builds golden with `main` and `feature`; `manifest(dir) -> Vec<Entry>`; `oracle_worktree_add(branch)` that runs plain `git worktree add` for comparison; `assert_worktree_parity(klon, oracle)` that compares `git worktree list --porcelain` shape, HEAD, branch, and tracked tree hash. Move the C0 test onto it. Add the 100k profile behind an environment variable `KLON_FIXTURE=100k`.
**AC:**
- The same seed produces the same manifest twice (byte-equal after removing timestamps).
- `assert_worktree_parity` fails when a tracked file in the klon differs from the branch tree by one byte.
- `manifest` reports a symlink target, a mode change, and an mtime change as three distinct differences.
- The C0 test passes unchanged on the harness.
- `KLON_FIXTURE=100k cargo test add_100k` generates 100,000 files and passes in under 5 minutes on the development laptop.
- In that test, the first `git status` in the klon completes in under 500 ms and the second in under 150 ms.
**Depends on:** C0 · **Traces to:** R2, R3, R11, R14

### C2 — CI: build, lint, and test on two operating systems (~80 lines)
**Status:** `[ ]` pending
**Build:** `.github/workflows/ci.yml` with jobs on `ubuntu-22.04` and `macos-14`: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`. Cache the cargo registry. Add a `nightly` job on a schedule that runs with `KLON_FIXTURE=100k`.
**AC:**
- A pull request with a `clippy` warning fails CI.
- Both operating-system jobs run the C0 and C1 tests and pass.
- The nightly job has `workflow_dispatch`, and one recorded manual run of it passes with the 100k test.
**Depends on:** C1 · **Traces to:** R37

### C3 — `rm`, `prune`, and basic `list` (~320 lines)
**Status:** `[ ]` pending
**Build:** Implement `gh klon rm (<branch> | --path <path>) [--force]`, `gh klon prune`, and `gh klon list` with these steps for `rm`:
1. Resolve the target to a registered worktree path. Refuse the repository root, `$HOME`, and an unresolved template.
2. Refuse a dirty tree without `--force`.
3. Refuse a tree with live processes without `--force`. Linux: scan `/proc/*/cwd`. macOS: `lsof -F`.
4. Rename the directory into `../<repo>.wt/.trash/<name>-<ts>`. When `.trash` is on another filesystem, fall back to `git worktree remove --force` and warn.
5. Delete the `.git` file in the trash copy. Run `git worktree prune`.
6. Start a detached `rm -rf` under `nice -n 19 ionice -c 3` (Linux) or `PRIO_DARWIN_BG` (macOS).

`prune` runs `git worktree prune` and drains `.trash`. `list` prints path, branch, HEAD, and a dirty flag from `git worktree list --porcelain` plus `git status --porcelain` per klon. `rm` never deletes the branch without `--merged` (C13) or `--delete-branch`.
**AC:**
- `rm` of a dirty klon exits non-zero with `dirty` in stderr and the tree still exists; `rm --force` removes it.
- `rm` with a path template that resolves to `$HOME` or the repository root exits non-zero before any change.
- `rm` of a klon with a running `sleep` process whose cwd is the klon exits non-zero without `--force`.
- `rm` returns in under 100 ms on the 10k fixture, and in under 100 ms on the 100k fixture in the nightly job; the directory is gone from `.trash` within 30 s.
- After `rm`, `git worktree list` no longer shows the klon and `git branch` still shows the branch.
- `rm --path <path>` removes the same klon as `rm <branch>`.
- `list` shows every registered klon with a `*` dirty flag for one that has a modified file.
**Depends on:** C0 · **Traces to:** R7, R8

### C4 — Journal, `doctor`, and JSON schemas (~300 lines)
**Status:** `[ ]` pending
**Build:** `src/journal.rs`: `<common>/klon/journal/<name>.json` with `version`, `state` (`planned`, `registered`, `cloned`, `checked-out`, `ready`, `removing`), `path`, `branch`, `started`. `add` and `rm` write each transition. `gh klon doctor [--json] [--repair]`: report git version, filesystem type of golden, `btrfs-progs`, inotify limits, make and ninja versions, `pasta`, journal entries; `--repair` moves each stale entry to the prior valid state (unregister a `registered` klon with `git worktree unlock` then `git worktree remove --force`, or complete a `checked-out` one). `init` writes a journal entry too (C7, C15). `--json` on `add`, `list`, `rm`, and `doctor` with `schema` fields `klon.add/1`, `klon.list/1`, `klon.rm/1`, `klon.doctor/1`. A schema test in `tests/schema.rs` holds the documented field sets.
**AC:**
- `add` killed with SIGKILL between registration and checkout leaves a `registered` journal entry and a locked worktree; `doctor --repair` unlocks and removes the worktree and the entry; `git worktree list` shows no klon.
- A repeated `add` after the kill completes the klon and the journal entry is gone.
- `doctor` run twice gives byte-equal JSON except the timestamp.
- A journal file with `"version": 99` makes `doctor` exit non-zero with `unknown journal version` and change nothing.
- Removing the `path` field from `klon.add/1` output makes `tests/schema.rs` fail.
**Depends on:** C3 · **Traces to:** R6, R9, R31

### C5 — Backend trait, probe, and the `reflink-walk` backend (~350 lines)
**Status:** `[ ]` pending
**Build:** `src/backend/mod.rs`: `trait Backend { fn name(); fn probe(golden) -> Capability; fn clone(src, dst, excludes) -> Timing; fn delete(dst) }`; `probe` clones a 200-file fixture into a temp dir and runs the manifest test; the result is cached in `<common>/klon/probe.json`. Move the C0 copy into `backend/copy.rs`. Add `backend/reflink.rs`: `reflink-copy` per file, `rayon` with 4 workers over directory entries, `filetime` to restore mtime after `FICLONE`, symlinks recreated, directories with mode kept. `add --backend <name>` overrides the probe. Add a CI job on `ubuntu-24.04` that creates a btrfs and an xfs loop filesystem with `sudo` and runs the backend tests on them.
**AC:**
- On ext4, `doctor --json` reports `backend: "copy"` with reason `reflink unsupported`.
- On the CI xfs loop, `doctor --json` reports `backend: "reflink-walk"` and `add` produces an ignored manifest equal to golden's, including mtimes.
- A backend whose `clone` drops one file fails the probe and `doctor` reports `probe failed: manifest mismatch`.
- No file in the klon shares an inode with a file in golden (`stat` comparison over the manifest).
- The reflink walk of the 100k fixture on the xfs loop takes under 10 s.
**Depends on:** C1, C4 · **Traces to:** R4, R5, R35

### C7 — `btrfs-snapshot` backend and `init` (~250 lines)
**Status:** `[ ]` pending
**Build:** `src/backend/btrfs.rs`: probe = golden is a subvolume (`btrfs subvolume show` or the inode-256 check) owned by the user; `clone` = `btrfs subvolume snapshot golden dst`; `delete` = `btrfs subvolume delete` when `user_subvol_rm_allowed` is in the mount options, else the background `rm -rf`. `gh klon init` converts a plain golden directory on btrfs into a subvolume: print the plan with both paths and wait for `y` (or `--yes`); write a journal entry; create `golden.klon-sub`; reflink-copy the content; swap with one `mv` pair; update the journal; print the result. `gh klon init --undo` reverses the swap. `doctor --repair` completes or reverts an interrupted `init`. The CI btrfs loop job mounts with `user_subvol_rm_allowed`.
**AC:**
- On the CI btrfs loop, after `init`, `doctor --json` reports `backend: "btrfs-snapshot"`.
- `add` on the 100k fixture completes in under 200 ms.
- The ignored manifest of the klon equals golden's.
- `rm` returns in under 100 ms and the subvolume is gone within 30 s.
- `init` on a golden that is already a subvolume exits 0 and changes nothing.
- `init` on ext4 exits non-zero with `not btrfs` and changes nothing.
- `init` killed with SIGKILL between the two `mv` calls, then `doctor --repair`, leaves golden at its original path with a byte-equal manifest.
- `init --undo` after a completed `init` restores a plain directory with a byte-equal manifest.
**Depends on:** C5 · **Traces to:** R6, R32

### C8 — `bench` v0: manifest, M1, M4, M6 (~350 lines)
**Status:** `[ ]` pending
**Build:** `src/bench/`: a versioned manifest (`bench/manifests/v1.toml`) that fixes the fixture seed and shape, the cells, the run counts (10 warm, 5 cold; `--release` uses 30 and 10), the timer points, and the pass rule. Cells: `m1-add-10k`, `m1-add-100k`, `m4-status-100k` (reports `first_p50_ms` and `steady_p50_ms` separately), `m6-rm-100k`, each for the selected backend and for the `git worktree add` baseline; each cell record carries `backend` and a `spare` boolean. Cold runs drop the page cache only when `KLON_BENCH_DROP_CACHES` names a command that can do it; else they are marked `warm-only`. `gh klon bench [--cell <name>] [--json] [--release]` writes `bench/results/<date>-<host>.json` with the raw samples, p50, p95, the environment record (hardware, OS, filesystem, mount options, git version, klon commit, fixture hash), and a `correctness` field from the manifest test; a mismatch sets `timing_valid: false`.
**AC:**
- `gh klon bench --cell m1-add-10k --json` prints `schema: klon.bench/1`, 10 raw samples, `p50_ms`, `p95_ms`, and the environment record.
- A cell with an injected manifest mismatch reports `timing_valid: false`.
- The baseline `git worktree add` cell runs with the same fixture and prints its own samples.
- Changing the manifest seed changes the `fixture_hash` in the report.
- The run order is random and recorded in the report.
**Depends on:** C1, C5 · **Traces to:** R14

### C9 — Hot spare (~250 lines)
**Status:** `[ ]` pending
**Build:** `src/spare.rs`: after `add`, `up`, and `rm`, start a detached process (`setsid`, `nice -n 19`, `ionice -c 3` or `PRIO_DARWIN_BG`) that clones golden into `../<repo>.wt/.spare.tmp`, copies the index with a fresh mtime, records `{version, head, status_hash, top_mtimes_before, top_mtimes_after}` in `.spare.tmp/.klon/spare.json`, and renames to `.spare`. `add` uses the spare when its `head` equals golden's HEAD and the tear check passed: rename `.spare` to the target path, then continue at the `.git` rewrite step. `.klon.toml` `spare = 0` disables it. A stale spare (different HEAD) is still used; a torn spare is deleted and `add` clones directly with a warning.
**AC:**
- After `add`, a `.spare` directory appears within 60 s on the 10k fixture with a valid `spare.json`.
- With a valid spare, `add` on the 100k fixture completes in under 1 s at p50 on ext4 (bench cell `m1-add-100k` with `spare: true`).
- A spare whose `top_mtimes_after` differs from `top_mtimes_before` is deleted and `add` prints `spare torn`.
- `spare = 0` results in no `.spare` directory after `add`.
- Two concurrent `add` calls use the spare at most once (the second clones directly).
**Depends on:** C4, C5, C8 · **Traces to:** R12, R40

### C10 — `.klon.toml` loader and command approvals (~200 lines)
**Status:** `[ ]` pending
**Build:** `src/config.rs`: parse `.klon.toml` (`base`, `path`, `disk_budget`, `spare`, `[warm]`, `[proof]`, `[fence]`, `[copy]`, `[fixup]`, `[hardlink]`) with `toml`. Approvals: before any command-bearing key is used, print the commands, ask `Approve? [y/N]`, and store the file's SHA-256 in `~/.config/klon/approvals.toml`; `--yes` approves without a prompt; a non-interactive run without `--yes` refuses with `needs approval`.
**AC:**
- A `.klon.toml` with `[warm] steps` and no approval makes `gh klon up` exit non-zero with `needs approval` and run nothing.
- `gh klon up --yes` runs the steps and writes the hash to `approvals.toml`.
- A one-byte change to `.klon.toml` invalidates the approval.
- A `.klon.toml` with an unknown key produces one warning and does not fail.
- `path = "/"` makes `add` exit non-zero with `refuses path template` before any change.
**Depends on:** C0 · **Traces to:** R16, R39

### C11 — `.klonignore`, `.worktreeinclude`, path fixup, and zero-compile tests (~350 lines)
**Status:** `[ ]` pending
**Build:** Backends take an exclude set built from `.klonignore` (gitignore syntax through `ignore`), the defaults (nested `.git`, submodule paths from `.gitmodules`, the destination path, every registered worktree path), and `.worktreeinclude` as an additive include. `src/fixup.rs`: search golden's absolute path with `grep-searcher` over the ignored directories; rewrite in files that are text, at most 1 MB, valid UTF-8, and outside the skip list; rewrite symlink targets that point into golden; delete `.next/cache`, `.ninja_log`, `.ninja_deps`; log each rewrite to `.klon/fixup.log`; `--no-fixup` and `[fixup] skip`. Zero-compile tests: a Rust fixture (`cargo build` in golden, then in the klon expects zero `Compiling` lines) and a pnpm fixture (`pnpm install --frozen-lockfile --offline` in the klon expects no change to `node_modules` and exit 0 after the `.modules.yaml` rewrite).
**AC:**
- A path listed in `.klonignore` is absent from the klon.
- A nested `.git` directory inside an ignored directory is absent from the klon.
- `cargo build` in a fresh klon of the Rust fixture prints zero `Compiling` lines.
- `pnpm install --frozen-lockfile --offline` in a fresh klon exits 0 and `.modules.yaml` holds the klon path.
- A 2 MB text file that holds golden's path is not rewritten; a 100 KB one is, and `.klon/fixup.log` names it.
- A `.sqlite` file that holds golden's path is not rewritten.
**Depends on:** C5, C10 · **Traces to:** R3, R10, R15, R39

### C12 — `copy` backend strategy, free-space check, progress, and background warm (~370 lines)
**Status:** `[ ]` pending
**Build:** `backend/copy.rs`: a per-directory strategy for top-level ignored directories: `copy` for big-file directories; `reinstall` for directories named in `[copy] reinstall` (after approval); the klon is usable after the tracked checkout, and each warm directory lands through a copy into `<dir>.klon-warming` then one rename; `add` prints once per repository: `run gh klon init --volume for instant spawns`. `list` shows `warming` for a klon with a pending directory. Before any change, `add` estimates the bytes the backend will write (`Backend::estimate_bytes`) and refuses when the free space on the target filesystem is below 1.2 times that estimate, with the shortfall printed. On a TTY, the copy prints one progress line (bytes copied, files remaining) that `--json` suppresses.
**AC:**
- On ext4, `add` on the 10k fixture returns before the ignored directory copy completes, and `list` shows `warming` until the rename.
- After the rename, the ignored manifest equals golden's.
- A `[copy] reinstall` entry for `node_modules` runs the approved command inside the klon instead of a copy.
- The warning line about `init --volume` appears exactly once across two `add` calls.
- No `sudo` prompt appears (the test runs with `SUDO_ASKPASS=/bin/false`).
- With a target filesystem whose free space is below 1.2 times the estimate (a small loop image in the test), `add` exits non-zero with the shortfall in bytes and creates no worktree.
- On a pseudo-TTY the copy prints a progress line; with `--json` it prints none.
**Depends on:** C10, C11 · **Traces to:** R13, R36, R41

### C13 — Branch forms, `--pr`, `--issue`, `pr`, `rm --merged` (~300 lines)
**Status:** `[ ]` pending
**Build:** Branch resolution in `add`: local branch; else `origin/<name>` after `git fetch origin <name>`; else a new branch from `base`. Explicit `origin/<name>`. `--pr <n>`: fetch `refs/pull/<n>/head` into `pr/<n>` through `gh api` for the fork owner and branch name. `--issue <n>`: branch name from the issue title through `gh api`, slugified. `gh klon pr <branch>`: `gh pr create` with `--head` from inside the klon. `rm --merged`: refuse unless `git merge-base --is-ancestor <branch> <base>` or the PR is merged.
**AC:**
- `add` of a name that exists only on `origin` creates a tracking branch with `branch.<name>.remote=origin`.
- `add` of an unknown name creates a branch from `base` and exits 0.
- `add --pr <n>` on a fork PR checks out the PR head commit (test against a recorded `gh api` response).
- `add --issue <n>` names the branch `<n>-<slug>` from a recorded issue title.
- `rm --merged` of an unmerged branch exits non-zero with `not merged`.
**Depends on:** C4 · **Traces to:** R30

### C14 — `up` and `sync` (~300 lines)
**Status:** `[ ]` pending
**Build:** Implement `gh klon up` with these steps:
1. Refuse a dirty golden or a golden not on `base`.
2. Run `git fetch origin`.
3. Run `git merge --ff-only`.
4. Run the approved `[warm] steps`.
5. Start a spare.

Implement `gh klon sync <branch> [--merge|--onto <base>|--fresh|--all|--check]` with these steps:
1. Fetch once for the common directory.
2. Fast-forward when the branch has no local divergence.
3. Else run `rebase --autostash`, or `merge` with `--merge`.
4. Detect a force-pushed upstream with `merge-base --is-ancestor`. Refuse unless no unique local commits exist or `--force` is given.
5. `--fresh` runs `rm` then `add` with the same branch. `--all` loops over the klons. `--check` is a dry run through the radar (C24) and prints `n/a` before C24 lands.
**AC:**
- `up` on a dirty golden exits non-zero with `dirty` and fetches nothing.
- `up` on a golden behind `origin/main` fast-forwards and starts a spare.
- `sync` of a klon behind its upstream with no local commits fast-forwards.
- `sync` of a klon whose upstream was force-pushed and that has one unique local commit exits non-zero with `force-pushed`.
- `sync --fresh` gives a klon on the same branch with the same HEAD and a manifest equal to golden's ignored state.
**Depends on:** C9, C10, C13 · **Traces to:** R29, R30

### S1 — Spike: sudo-free btrfs loop volume through udisks (report only)
**Status:** `[ ]` pending
**Build:** On a host with `btrfs-progs`: create a sparse image; `mkfs.btrfs --rootdir <dir>` where `<dir>` holds an empty user-owned `klon/` directory; `udisksctl loop-setup -f`; `udisksctl mount -b`; record the owner of the mount root and of `klon/`; as the user, `btrfs subvolume create klon/golden`, `btrfs subvolume snapshot klon/golden klon/x`, `btrfs subvolume delete klon/x`; test `udisksctl mount -o user_subvol_rm_allowed`; test what happens after a reboot (`loop-setup` again). Write `docs/spikes/2026-btrfs-loop-volume.md` with each command, its result, and a decision for Q1 and Q2 (bundle `mkfs.btrfs` or print the install line).
**AC:**
- The report answers, with command output: root ownership after mount; `klon/` ownership; unprivileged create, snapshot, and delete results; whether udisks accepts `user_subvol_rm_allowed`; the re-attach steps after a reboot.
- The report states a decision for Q1 and Q2.
**Depends on:** — · **Traces to:** R33

### C15 — `init --volume` (~250 lines)
**Status:** `[ ]` pending
**Build:** `gh klon init --volume <size>`: refuse without `btrfs-progs` and print the install line; print the plan with the image path, the mount path, and both golden paths, and wait for `y` (or `--yes`); write a journal entry; create `~/.local/share/klon/<repo>.img` sparse; `mkfs.btrfs --rootdir` with a user-owned `klon/`; `udisksctl loop-setup -f`; `udisksctl mount -b`; move golden into `<mount>/klon/<repo>` as a subvolume; leave a symlink at the old path; record the image path in `<common>/klon/volume.json`. `init --volume --undo` moves golden back and removes the symlink and the image record. On the first `add` after a reboot, re-run `loop-setup` and `mount` when the image is not mounted. Apply the S1 decisions.
**AC:**
- On a host with `btrfs-progs`, `init --volume 4G` completes with zero `sudo` prompts (`SUDO_ASKPASS=/bin/false`) and `doctor --json` reports `backend: "btrfs-snapshot"`.
- `add` after `init --volume` completes in under 1 s on the 100k fixture.
- After `udisksctl loop-delete`, the next `add` re-attaches and mounts the image and succeeds.
- Without `btrfs-progs`, `init --volume` exits non-zero with the install line and changes nothing.
- `init --volume` on a golden with uncommitted changes exits non-zero with `dirty` and changes nothing.
- `init --volume` killed with SIGKILL after the move, then `doctor --repair`, leaves golden reachable at its original path with a byte-equal manifest.
- `init --volume --undo` restores golden on ext4 and `doctor` reports `backend: copy`.
**Depends on:** C7, S1 · **Traces to:** R6, R33

### C16 — Env file, loopback slots, `run`, `shell`, `stop` (~300 lines)
**Status:** `[ ]` pending
**Build:** Implement the env file and three commands:
1. `src/envelope/env.rs` writes `.klon/env` at `add` with `KLON_NAME`, `KLON_IP`, `HOST`, `TMPDIR`, `KLON_JOBSERVER`, and `GIT_CONFIG_*` for `core.hooksPath`.
2. `add` allocates `127.0.0.N` from `<common>/klon/slots.json` under `flock`. `rm` releases it.
3. `gh klon run <branch> -- <cmd>` calls `setsid`, exports the env plus `gc.auto=0` through `GIT_CONFIG_*`, tags the process with `KLON_ID`, and `exec`s the command.
4. `gh klon shell <branch>` runs `$SHELL` through `run`.
5. `gh klon stop <branch>` enumerates the process group (Linux: a `/proc` scan for `KLON_ID` in `environ`; macOS: `proc_listpgrppids`), sends SIGTERM, waits 3 s, then sends SIGKILL.
6. `add -- <cmd>` runs `run` after `add`.
**AC:**
- `gh klon run x -- sh -c 'echo $KLON_IP'` prints a `127.0.0.N` that no other live klon holds.
- Inside `run`, `python3 -c "import os,socket; s=socket.socket(); s.bind((os.environ['KLON_IP'],3000)); print('ok')"` prints `ok` on Linux.
- After `add`, `git -C <klon> status --porcelain` prints nothing although `.klon/env` exists.
- `stop` ends a `run` that started `sleep 1000` in a subshell within 5 s, including the grandchild.
- `rm` releases the slot and the next `add` reuses it.
- `add x -- true` exits with the exit code of `true` after the klon exists.
**Depends on:** C4 · **Traces to:** R21, R22

### C17 — Jobserver with the pipe-style handshake (~200 lines)
**Status:** `[ ]` pending
**Build:** `src/envelope/jobserver.rs`: create `<XDG_RUNTIME_DIR or ~/.klon>/jobserver` fifo once; fill with `nproc-2` tokens; `run` opens it read-write, keeps two descriptors open across `exec`, and exports `MAKEFLAGS=-j --jobserver-auth=R,W`; a top-up routine compares the token count with `nproc-2` when no klon runs and writes back the missing tokens; `KLON_NO_JOBSERVER=1` skips the export; `doctor` reports the make version and the token count.
**AC:**
- Under `run` on make 4.3, a Makefile with 8 one-second jobs and 2 tokens completes in 3 to 5 s and prints no `jobserver` error.
- Under `run`, `cargo build` of 4 independent crates with 2 tokens shows at most 3 concurrent `rustc` or build-script processes.
- After a client is killed with SIGKILL while it holds a token, `doctor` reports the shortfall and the top-up restores the count.
- `KLON_NO_JOBSERVER=1 gh klon run x -- sh -c 'echo $MAKEFLAGS'` prints an empty line.
**Depends on:** C16 · **Traces to:** R19

### C18 — Linux write fence with Landlock (~200 lines)
**Status:** `[ ]` pending
**Build:** `src/envelope/fence_linux.rs` with the `landlock` crate: a ruleset with read everywhere; write, create, delete, and truncate (ABI ≥ 3) only under the klon, `<common>/objects`, `<common>/refs`, `<common>/logs`, `<common>/rr-cache`, `<common>/klon`, `<common>/worktrees/<name>`, the file `<common>/packed-refs` (never the `<common>` root, so `hooks/` and `config` stay read-only; `run` sets `gc.auto=0` through `GIT_CONFIG_*` so no `packed-refs.lock` is needed), `TMPDIR`, `/tmp`, `/var/tmp`, `$XDG_RUNTIME_DIR`, `~/.cache`, `~/.cargo`, `~/.npm`, the pnpm store, `~/.nuget`, `GOCACHE`, the uv cache, `/dev/null`, `/dev/shm`, `/dev/tty`, and `[fence] allow`; `prctl(PR_SET_NO_NEW_PRIVS)`; `landlock_restrict_self` before `exec`. `--no-fence`. `doctor` reports the ABI. Absent Landlock prints one line and continues.
**AC:**
- Under `run`, `touch <golden>/x` fails with `EACCES`; `touch <sibling>/x` fails; `touch ~/.ssh/x` fails.
- Under `run`, `touch <klon>/x`, `touch $TMPDIR/x`, and a write under `~/.cargo` succeed.
- Under `run`, `cargo build` of the Rust fixture exits 0.
- `run --no-fence` allows the write to golden.
- Under `run`, `git -C <klon> commit --allow-empty -m x` exits 0, and `touch <golden>/src/x` still fails with `EACCES`.
- Under `run`, `touch <common>/hooks/x` and `git config --local user.name x` fail with `EACCES`, and `doctor` lists `refs/heads/<base>` as writable under the fence (a documented residual).
- A unit test that builds the ruleset with a forced ABI of 2 asserts that `TRUNCATE` is absent and `WRITE_FILE` is present.
**Depends on:** C16 · **Traces to:** R17

### C20 — Linux resource scope (~200 lines)
**Status:** `[ ]` pending
**Build:** `src/envelope/scope_linux.rs`: `run` wraps the command in `systemd-run --user --scope -p MemoryHigh=<total/(N+1)> -p TasksMax=<n> [-p CPUWeight=<w> when systemd ≥ 252]`, where N is the count of live klons from `slots.json`; a cgroupfs fallback creates `<user cgroup>/klon-<name>` and writes `memory.high`; a `nice -n 10` fallback when neither exists; `stop` uses `cgroup.kill` when present. `doctor` reports the delegated controllers.
**AC:**
- Under `run` on systemd 249 (laptop-only test, marked `#[ignore]` in CI, which has no user D-Bus session), `cat /sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)/memory.high` prints `total/(N+1)` within 1 %.
- With two live klons, a new `run` gets `total/3`.
- `stop` on a scope ends every process, including one that called `setsid`.
- On a host without `systemd-run` (test by `PATH` manipulation), `run` prints one line about the fallback and the command still runs.
**Depends on:** C16 · **Traces to:** R18

### C22 — Per-tree hooks and `[warm] steps` in `up` (~150 lines)
**Status:** `[ ]` pending
**Build:** At `add`, copy the repository hooks directory (or `core.hooksPath`) into `<klon>/.klon/hooks`; `.klon/env` and `run` export `core.hooksPath` through `GIT_CONFIG_*`; `git` outside `run` also sees it because `add` sets `core.hooksPath` in `<common>/worktrees/<name>/config.worktree` only when `extensions.worktreeConfig` is already on, else documents the `run` limitation in `doctor`. `up` runs approved `[warm] steps` under the jobserver and the scope.
**AC:**
- A `pre-commit` hook edited inside a klon runs on `git commit` inside `run` in that klon and does not run on `git commit` in golden.
- A hook edited in golden after `add` does not change the klon's copy.
- `up` with an approved `[warm] steps = ["true"]` runs it and exits 0; with `["false"]` it exits non-zero and reports the step.
**Depends on:** C10, C14, C16 · **Traces to:** R20, R29

### C23 — `run --netns` with pasta (~120 lines)
**Status:** `[ ]` pending
**Build:** `src/envelope/netns.rs`: when `pasta` is present and `--netns` is given, wrap the command in `pasta --config-net -t <KLON_IP>/auto -- <cmd>`; absent `pasta` prints one line and runs without the namespace. `doctor` reports `pasta`. Test on `ubuntu-24.04` in CI.
**AC:**
- On `ubuntu-24.04`, under `run --netns`, a server bound to `0.0.0.0:3000` inside is reachable from the host at `<KLON_IP>:3000`.
- Two klons under `run --netns` both bind `0.0.0.0:3000` without `EADDRINUSE`.
- Inside the namespace, `curl https://example.com` exits 0.
- On `ubuntu-22.04` without `pasta`, `run --netns` prints `pasta absent` and runs the command.
**Depends on:** C16 · **Traces to:** R21

### C24 — Conflict radar and `sync --check` (~250 lines)
**Status:** `[ ]` pending
**Build:** `src/radar.rs`: for each klon, `git merge-tree --write-tree --quiet <base> <head>` on git ≥ 2.38 (pairwise with siblings through `--stdin`; conflict paths from `--name-only -z`), or below 2.38 the legacy `git merge-tree $(git merge-base a b) a b` form with the conflict paths from the `changed in both` lines; cache in `<common>/klon/radar/<tuple-hash>.json`; `list` gains `vs-base`, `vs-siblings`, and `behind` columns; `doctor` names which form is in use. `sync --check` prints the radar row for one klon.
**AC:**
- Two klons that edit the same line of the same file show `1 conflict` vs each other in `list` on git ≥ 2.38.
- A klon that edits a file untouched by base shows `clean` vs base.
- On git 2.34.1 the legacy form finds the same conflict and `doctor` reports `radar: legacy merge-tree`.
- The radar for 5 klons on the 100k fixture completes in under 400 ms warm.
- A second `list` with unchanged HEADs reads the cache and makes no `merge-tree` call.
**Depends on:** C4 · **Traces to:** R23

### C25 — `merge` with a test gate and mergiraf (~250 lines)
**Status:** `[ ]` pending
**Build:** Implement `gh klon merge <branch>` with these steps:
1. Refuse a dirty golden.
2. Run `git fetch`.
3. Run the `pre_merge` hook from `.klon/hooks`, or the approved `[proof] steps` when present. Stop on failure. (C26 supersedes the second half: where `[proof] steps` are present, `merge` reads the `check` receipt of the klon's HEAD instead of running the steps again.)
4. Configure `merge.mergiraf.driver` and a generated `<common>/info/attributes` when `mergiraf` is present.
5. Run `git merge --no-ff` or `--ff-only` per `.klon.toml`. On a conflict, stop and print the paths.
6. On success, fast-forward `base`, then run `rm`.

`merge` never runs `git push`. It sets `merge.conflictStyle=zdiff3` and `rerere.enabled=true` in the shared config.
**AC:**
- `merge` with a failing `pre_merge` hook exits non-zero and base's HEAD is unchanged.
- `merge` of a clean klon fast-forwards base and removes the klon.
- With `mergiraf` present, a non-overlapping same-file edit merges without a conflict marker.
- No `git push` runs (test by a fake `git` on `PATH` that logs arguments).
- `merge` on a dirty golden exits non-zero with `dirty`.
**Depends on:** C22, C24 · **Traces to:** R24

### C26 — `check` receipts and the `merge` gate (~200 lines)
**Status:** `[ ]` pending
**Build:** `gh klon check <branch>`: refuse a dirty tree; run approved `[proof] steps` under `run`; write `<common>/klon/receipts/<commit>.json` with `{version, commit, tree, steps_hash, results: [{cmd, status, duration_ms}], duration_ms, created}`; `merge` requires a receipt whose `commit` equals the klon's HEAD and whose `steps_hash` equals the current steps, unless `--no-check`. `list` shows a `✓` for a klon with a fresh receipt.
**AC:**
- `check` on a dirty klon exits non-zero with `dirty` and writes no receipt.
- `check` with a failing step writes a receipt with `status: failed` and `merge` refuses with `receipt failed`.
- After one more commit, `merge` refuses with `receipt stale`; `merge --no-check` proceeds.
- The receipt holds no environment values (the test greps for a canary exported in the environment).
**Depends on:** C25 · **Traces to:** R25

### C27 — `claim` (~200 lines)
**Status:** `[ ]` pending
**Build:** `gh klon claim <branch> <paths...> [--release]`: normalize each path (reject `..`, a symlink ancestor, an absolute path outside the tree); under `flock` on `<common>/klon/claims.json`, reject an exact or component-boundary prefix overlap with another klon's claim; record `{version, claims: [{klon, path, kind, created}]}`. `list` flags an overlap with `!`. `check` reports changed paths outside the klon's claims as `claim escape` and marks the receipt. `rm` releases the claims.
**AC:**
- Two concurrent `claim` calls for `src/a` from two klons: exactly one exits 0 (a test with 20 iterations).
- A claim on `src/app` does not conflict with a claim on `src/apple`.
- A claim on `src/app` conflicts with a claim on `src/app/main.rs`.
- `check` on a klon that changed a file outside its claims writes a receipt with `claim_escape: [...]`.
- `rm` removes the klon's claims.
**Depends on:** C26 · **Traces to:** R26

### C28 — Claude Code plugin and harness path modes (~150 lines)
**Status:** `[ ]` pending
**Build:** `plugin/claude-code/`: a `WorktreeCreate` hook script that reads the stdin JSON, runs `gh klon add worktree-<name> --path <repo>/.claude/worktrees/<name> --json`, and prints the path; a `WorktreeRemove` hook that runs `gh klon rm --path <worktree_path> --force --json`; a `settings.json` fragment and an install note. `add --path-mode {sibling,claude,t3,codex}` sets the path template from the research record §19.
**AC:**
- With the hook installed, `claude --worktree test -p 'pwd'` (or the documented hook harness) prints a path under `.claude/worktrees/` that `git worktree list` shows as a klon.
- `WorktreeRemove` removes the klon and `git worktree list` no longer shows it.
- A hook exit code of non-zero on a failed `add` propagates.
- `add --path-mode claude x` creates `<repo>/.claude/worktrees/x` with branch `worktree-x`.
**Depends on:** C13 · **Traces to:** R27

### S3 — Spike: does Claude Code `EnterWorktree` bypass `WorktreeCreate`? (report only)
**Status:** `[ ]` pending
**Build:** With the C28 plugin installed on Claude Code 2.1.259 or newer: trigger `EnterWorktree` and `isolation: "worktree"` on a subagent; record whether the hook ran (a log line in the hook script). Write `docs/spikes/2026-claude-enterworktree.md` with the result and a decision (document the gap, or file an issue upstream).
**AC:**
- The report states, for each of the two entry points, whether the hook ran, with the log.
**Depends on:** C28 · **Traces to:** R27

### C29 — `hibernate`, `wake`, and `disk_budget` (~250 lines)
**Status:** `[ ]` pending
**Build:** `gh klon hibernate <branch>`: refuse with live processes; `git stash create` plus `git add -A` of untracked non-ignored files into a commit on `refs/klon/hibernate/<name>`; record `{version, head, stash, path, created}` in `<common>/klon/hibernate/<name>.json`; `rm --force` the tree. `gh klon wake <branch>`: `add` to the same path, then apply the stash commit and reset the index. `disk_budget`: at `add`, when the sum of unique bytes (count of klons times the ignored size, or `btrfs fi du -s` when present) exceeds the budget, refuse the `add` and name the least recently used klon without live processes as the candidate. Only `add --evict` or `disk_budget_action = "hibernate"` in `.klon.toml` hibernates that candidate first.
**AC:**
- `hibernate` then `wake` on a klon with a modified tracked file and a new untracked file restores both byte-for-byte.
- A hibernated klon uses under 1 MB outside the object store (`du` of its metadata).
- `hibernate` on a klon with a live process exits non-zero.
- With `disk_budget = "1G"` and two 600 MB klons, a third `add` exits non-zero, names the candidate, and changes nothing.
- The same `add --evict` hibernates the candidate first and then succeeds.
**Depends on:** C4, C9 · **Traces to:** R28

### C30 — `list` extras: disk delta, RSS, processes, PR, checks (~200 lines)
**Status:** `[ ]` pending
**Build:** `list` adds: disk delta (`btrfs fi du -s` when present, else the ignored-directory size as an upper bound), RSS and live process count from the scope or the `KLON_ID` scan, PR number and check status through `gh api` cached in `<common>/klon/gh-cache.json` for 60 s. `--json` extends `klon.list/1` to `klon.list/2` additively.
**AC:**
- `list` for a klon with a running `run` shows a process count of at least 1 and an RSS above 0.
- Two `list` calls within 60 s make one `gh api` call (a fake `gh` on `PATH` counts calls).
- `list --json` validates against `klon.list/2` and still holds every `klon.list/1` field.
**Depends on:** C13, C16, C20, C24 · **Traces to:** R38

### C31 — `bench` v2: M2, M3, M5, M12 and the baseline runner (~300 lines)
**Status:** `[ ]` pending
**Build:** Cells `m2-warm-100k` (time from `add` to a complete ignored manifest), `m3-zero-compile-rust` and `m3-zero-compile-pnpm` (count of compiled units), `m5-disk-100k` (unique bytes per idle klon, filesystem-specific), `m12-throughput-n6` (6 klons build the Rust fixture concurrently under `run`; `ratio = (6 × T_solo) / T_wall6`, where `T_solo` is the median wall time of one build alone and `T_wall6` is the wall time from the first start to the last finish of the 6 concurrent builds; 0.80 means at most 25 % lost to contention). The baseline runner drives `git worktree add` plus the same build for each cell.
**AC:**
- `bench --cell m3-zero-compile-rust --json` reports `units_compiled: 0` on a warm golden.
- `bench --cell m12-throughput-n6 --json` reports a `ratio` field and the per-klon build times.
- `bench --cell m5-disk-100k --json` reports `unique_bytes` on btrfs from `btrfs fi du` and marks ext4 as `upper-bound`.
- Every v2 cell has a baseline row with the same fixture.
**Depends on:** C8, C11, C17, C20 · **Traces to:** R14

### G1 — Goal: M1 spawn p50 ≤ 1 s on ext4 with a spare and ≤ 200 ms on btrfs (goal session)
**Status:** `[ ]` pending
**Build:** A Claude Code `/goal` session. Condition: `gh klon bench --cell m1-add-100k --json` printed in the transcript reports `p50_ms <= 1000` and `timing_valid: true` with `spare: true` on ext4, and `p50_ms <= 200` on the CI btrfs loop; constraints: the manifest seed and run counts stay unchanged, `cargo test` stays green, no change under `bench/manifests/`, and no change under `src/bench/` without a justification in the report; turn cap 25. Permitted changes: `index.skipHash`, the checkout path, the spare claim, the journal write count. Independent evidence that the transcript must show: (a) three runs of `/usr/bin/time -f %e gh klon add bench-<i>` on the same fixture; (b) after the timed run, `git -C <klon> status --porcelain` with empty output and a `diff` of `find <dir> -printf '%P %s %T@\n' | sort` between golden's ignored directory and the klon's; (c) `git diff --stat origin/main -- src/bench/ bench/`, with a written justification for every hunk; (d) the last 20 lines of `cargo test`.
**AC:**
- The bench output in the transcript shows both cells with `timing_valid: true` under the limits.
- The three `/usr/bin/time` lines in the transcript are each under 1.2 s on ext4 with a spare.
- The `status` output is empty and the `find` diff is empty in the transcript.
- The last 20 lines of `cargo test` in the transcript show `test result: ok` and no failure.
- `bench/manifests/v1.toml` is unchanged; every hunk under `src/bench/` has a justification.
**Depends on:** C7, C8, C9 · **Traces to:** R12

### G2 — Goal: M4 first `git status` p50 ≤ 150 ms on the 100k fixture (goal session)
**Status:** `[ ]` pending
**Build:** A `/goal` session. Condition: `gh klon bench --cell m4-status-100k --json` reports `first_p50_ms <= 500`, `steady_p50_ms <= 150`, and `timing_valid: true`; constraints as G1; turn cap 20. Permitted changes: the index copy, `core.untrackedCache`, `index.version`, the fresh-mtime rule. Independent evidence that the transcript must show: three runs of `/usr/bin/time -f %e git -C <klon> status` for a fresh klon and three for a second call; the `git diff --stat origin/main -- src/bench/ bench/` output with a justification for every hunk; the last 20 lines of `cargo test`.
**AC:**
- The bench output in the transcript shows the cell under the limits with `timing_valid: true`.
- The independent `/usr/bin/time` lines in the transcript are under 0.6 s for the first call and under 0.2 s for the second.
- The last 20 lines of `cargo test` in the transcript show `test result: ok`.
**Depends on:** C8 · **Traces to:** R11

### G3 — Goal: M12 throughput ≥ 80 % at N=6 (goal session)
**Status:** `[ ]` pending
**Build:** A `/goal` session. Condition: `gh klon bench --cell m12-throughput-n6 --json` reports `ratio >= 0.80` on the development laptop; constraints: the token count policy may change, `cargo test` stays green, no manifest change, no change under `src/bench/` without a justification; turn cap 25. Permitted changes: the token count, the `MemoryHigh` formula, the jobserver top-up timing, `nice` values. Independent evidence that the transcript must show: three `/usr/bin/time -f %e` lines for a solo build; for the 6-way run, one `date +%s.%N` line before the loop, six per-build `/usr/bin/time -f %e` lines, and one `date +%s.%N` line after the last build ends, so a reader can recompute the ratio; the `git diff --stat origin/main -- src/bench/ bench/` output; and the last 20 lines of `cargo test`.
**AC:**
- The bench output in the transcript shows `ratio >= 0.80`.
- The `date` boundaries and the six `/usr/bin/time` lines in the transcript give a ratio of at least 0.75 when the reader recomputes it.
- The last 20 lines of `cargo test` in the transcript show `test result: ok`.
**Depends on:** C31 · **Traces to:** R19

### G4 — Goal: M1 ≤ 100 ms through an index byte-splice, only if G1 shows checkout dominates (goal session, conditional)
**Status:** `[ ]` pending
**Build:** Open only when the G1 report shows `git checkout` above 60 % of the spawn time. A `/goal` session. Condition: `gh klon bench --cell m1-add-100k --json` reports `p50_ms <= 100` with `spare: true`, `timing_valid: true`, and the tracked tree still equals the branch tree (the correctness gate); implementation: copy the unchanged index bytes, patch the differing entries, and write a correct SHA-1 trailer over the spliced bytes (git verifies it in `fsck`; `index.skipHash` is not on git 2.34); turn cap 30. Independent evidence that the transcript must show: three runs of `/usr/bin/time -f %e gh klon add bench-<i>`, `git -C <klon> fsck` output, `git -C <klon> status --porcelain` output, the `find` diff of the ignored directory, and the last 20 lines of `cargo test`.
**AC:**
- The bench output in the transcript shows the cell under 100 ms with `timing_valid: true`.
- `git fsck` and `git status` on the produced klon report nothing on git 2.34.1 and on the newest git in CI, and the transcript shows both outputs.
- The three `/usr/bin/time` lines in the transcript are each under 0.15 s.
- The last 20 lines of `cargo test` in the transcript show `test result: ok`.
**Depends on:** G1 · **Traces to:** R12

### C32 — Release: precompiled assets and install docs (~120 lines)
**Status:** `[ ]` pending
**Build:** `.github/workflows/release.yml` with `cli/gh-extension-precompile@v2` and a `build_script_override` that runs `cargo build --release` for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, and `aarch64-apple-darwin`; assets named `gh-klon_v<ver>_<os>-<arch>`; `LICENSE-MIT` and `LICENSE-APACHE`; README install, quick start, `doctor` output, and the known limitations from the handoff, including the `core.checkStat=minimal` blind spot (an edit that keeps size and mtime is invisible to `git status`).
**AC:**
- A tag `v0.1.0` produces four assets with the documented names.
- `gh extension install navaro1/gh-klon` on a clean `ubuntu-22.04` and `macos-14` runner installs and `gh klon doctor` exits 0 (on macOS it reports `backend: copy` until v0.4).
- A tag `v0.1.0-rc1` produces a prerelease.
**Depends on:** C2, C4, C7 · **Traces to:** R37

---

The four items below need a Mac to develop and test. They form the last milestone, v0.4 macOS improvements, by request on 2026-09-03. Until they land, klon on macOS uses the `copy` backend and runs without a fence and a scope; `doctor` reports each absent part. Their identifiers keep their original numbers because the issues already use them.

### C6 — `apfs-clone` backend (~200 lines)
**Status:** `[ ]` pending
**Build:** `src/backend/apfs.rs`: for each top-level ignored directory, one `clonefile(2)` call, in parallel; tracked files and other entries through `reflink-copy` per file; never a `clonefile` on the repository root. Probe on macOS only. Add the macOS backend tests to the `macos-14` CI job.
**AC:**
- On `macos-14`, `doctor --json` reports `backend: "apfs-clone"`.
- `add` on the 10k fixture produces an ignored manifest equal to golden's, including mtimes.
- A test that intercepts the `clonefile` call list shows no call with the repository root as the source.
- No file in the klon shares an inode with a file in golden.
- `add` on the 100k fixture completes in under 15 s on `macos-14`.
**Depends on:** C5 · **Traces to:** R5, R34

### C19 — macOS write fence with Seatbelt (~200 lines)
**Status:** `[ ]` pending
**Build:** `src/envelope/fence_macos.rs`: generate a profile `(version 1) (deny default) (allow file-read*) (allow file-write* (subpath "<klon>") (subpath "<TMPDIR>") ...) (allow network*) (allow process-exec*) (allow process-fork) (allow sysctl-read) (allow mach-lookup) (allow signal)` with the same allow set as C18 (the git subdirectories, never the `<common>` root), plus `/tmp` and `/private/tmp`; run `sandbox-exec -f <profile> <cmd>`. `--no-fence`. `doctor` reports `sandbox-exec` presence.
**AC:**
- On `macos-14` under `run`, `touch <golden>/x` fails with `EPERM`; `touch <klon>/x` succeeds; `git -C <klon> commit --allow-empty -m x` exits 0.
- Under `run`, `cargo build` of the Rust fixture exits 0 and `curl https://example.com` exits 0.
- `run --no-fence` allows the write to golden.
- The deprecation warning from `sandbox-exec` is filtered from stderr.
**Depends on:** C16 · **Traces to:** R17

### S2 — Spike: macOS jetsam limit from user space (report only)
**Status:** `[ ]` pending
**Build:** On a Mac: a small program that spawns a child with `posix_spawnattr_setjetsam_ext` and a memory limit, then allocates past it; record whether the child is killed, what signal, and whether an entitlement is needed. Write `docs/spikes/2026-macos-jetsam.md`.
**AC:**
- The report states, with output, whether the limit kills the child on macOS 14 or 15 and which API call was used.
- The report gives a decision for Q3: use jetsam, or the footprint poll only.
**Depends on:** — · **Traces to:** R18

### C21 — macOS QoS clamp, process group, and footprint poll (~200 lines)
**Status:** `[ ]` pending
**Build:** `src/envelope/scope_macos.rs`: spawn through `posix_spawn` with `POSIX_SPAWN_SETSID` and `posix_spawnattr_set_qos_clamp_np(UTILITY)`; a poll thread reads `proc_pid_rusage` footprint of the group every 2 s and sends SIGTERM above `total/(N+1)`; apply the S2 decision for jetsam. `stop` uses `proc_listpgrppids` and `killpg`. `gh klon lo0` prints the `ifconfig lo0 alias` command for every allocated slot and the LaunchDaemon one-liner.
**AC:**
- On `macos-14` under `run`, `sysctl`-visible QoS of the child is `utility` (read through `taskpolicy` or `proc_pidinfo`).
- A child that allocates past the threshold receives SIGTERM within 5 s.
- `curl` under `run` is not throttled (a 10 MB download under `run` takes at most 1.5x the time outside `run`).
- `stop` ends the whole group.
- `gh klon lo0` prints one `sudo ifconfig lo0 alias 127.0.0.N up` line per allocated slot and exits 0 without a prompt.
**Depends on:** C16 · **Traces to:** R18, R21, R22

## Definition of Done

- `python3 scripts/validate_spec.py docs/klon-spec.md` exits 0 with zero open clarification markers.
- Every requirement R1 to R40 is traced by at least one chunk; every chunk traces to at least one requirement.
- Every `Depends on` points to a chunk listed above it.
- Each chunk has a mechanically verifiable acceptance list.
- A separate reviewer (Opus) has reviewed this specification and its findings are applied or recorded.
- One GitHub issue exists per chunk, spike, and goal, with `Depends on` links and a milestone.

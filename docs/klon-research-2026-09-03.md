> **Record.** This file is revision 2 of the handoff (PR #1, 2026-09-03), kept verbatim as the research record. The authoritative design is `docs/klon-handoff.md` (revision 3). Where the two differ, revision 3 wins. Section references from revision 3 (§15.3, §16, §19, §20) point into this file.

# klon — handoff document

`gh klon`: a `git worktree` replacement for developers running many coding agents (Codex, Claude Code, others) on one laptop. Linux + macOS. No VMs, no daemon, no setup wizard, no sudo.

Original design session: 2026-09-03, Piotr. **Revision 2 (2026-09-03, same day)**: research pass with six parallel investigations (competitors, filesystems, git plumbing, envelope, academic literature, ecosystems), one local benchmark on an ext4 laptop, and one udisks spike. Sections marked *(revised)* or *(new)* changed in this pass. Sources are in §20. Claims are marked **V** (verified with a source or an experiment) or **B** (believed) where it matters.

---

## 0. TL;DR *(revised)*

- **Problem**: `git worktree add` materialises the *tracked* files (cheap, regenerable) and drops all *untracked* build state (`target/`, `node_modules/`, `bin/obj`, `.venv`, `.next` …) — the expensive thing. Every agent starts cold and every tree costs a full copy of the build cache. Agents also share hooks, ports, and RAM with no fence and no budget, and nobody tells the developer which trees will conflict before a merge.
- **Core idea**: clone the **directory as it is on disk** — tracked and untracked — with a copy-on-write filesystem primitive, register it as a normal git worktree, and wrap it in an envelope (write fence, resource scope, loopback identity, build slots). Disk cost = diff only. Build cache warm from second zero.
- **Phase transition** (what makes this more than a faster worktree):
  1. **O(1) spawn.** A btrfs snapshot where the filesystem allows it (native btrfs, or a sudo-free btrfs loop volume on ext4). A pre-cloned **hot spare** everywhere else (APFS, XFS, ext4 copy). "Spawn is a rename."
  2. **Guarantees instead of hope.** Kernel write fence (Landlock / Seatbelt), per-tree hooks, `MemoryHigh` scope, own loopback IP on Linux, jobserver build slots.
  3. **Integration certainty.** Every `list` runs `git merge-tree` dry-runs against base and against sibling klons, shows overlap, and `merge` is test-gated with structured merge.
- **Mental model**: *a klon is a folder that's a full copy of your project, already built, with its own localhost, that cannot hurt its neighbours, and that tells you whether it merges.* Folder / built / localhost / fenced / mergeable. Every feature must serve one of those five words.
- **Shape**: a gh extension (`gh-klon` repo, Rust binary), verb-for-verb the same as `git worktree`, plus `--pr`, `sync`, `up`, `run`, `merge`, `init`, `doctor`, `bench`. Writes git's own worktree metadata so all existing tooling sees ordinary worktrees.
- **What klon writes itself** (~2.5–3k lines Rust): backend probe + parallel reflink/clonefile walk + btrfs snapshot/volume driver, hot spare, worktree metadata (3 files), index copy, jobserver fifo, Landlock fence + Seatbelt profile, macOS QoS/setsid FFI, envelope, path fixup, conflict radar, async delete, CLI. Everything else is linked (`landlock`, `reflink-copy`, `ignore`, `grep-searcher`, `rayon`) or shelled to (`git`, `gh`, `systemd-run`, `udisksctl`, `btrfs`, `sandbox-exec`, `pasta`, `mergiraf`). **gitoxide is dropped** (§13).
- **Corrected assumptions** (details in §15): APFS directory `clonefile` is 3.5–6 s per 100k files, not 100–300 ms, and Apple discourages it; `git read-tree -m -u` is slower than `git checkout`; `extensions.worktreeConfig` is unnecessary; ext4 is the majority Linux desktop filesystem and must be first-class; a full copy on ext4 costs 40–100 s per tree.
- **Name**: `klon` — Polish for *maple* **and** *clone*. Repo `gh-klon`, command `gh klon`. `gh extension search klon` returns nothing and no `gh-klon` repo exists on GitHub (**V**, 2026-09-03).

---

## 1. Goals and non-goals *(revised)*

### Goals
1. Drop-in worktree replacement: same verbs, same git metadata, same GitHub workflows (branches, push, PRs, hooks, CI). GitHub never learns anything changed.
2. Maximal performance and throughput for N concurrent agents on a modern laptop (12–20 cores, 32–64 GB, NVMe). Spawn is O(1) from the user's point of view.
3. Lightweight: disk per tree ≈ diff from the source checkout; near-zero for idle trees.
4. Language-agnostic by construction (Rust, TS, C#, Go, Python, docs, polyglot monorepos, anything on GitHub). No per-ecosystem knowledge required for *correctness*; ecosystem knowledge only improves warmth (§14).
5. Linux and macOS, **fully operational from the first release on APFS, btrfs, XFS, and ext4**. Zero sudo prompts on the default path.
6. Certainty: an agent in one klon cannot write outside it; a klon's hooks and config cannot affect golden or siblings; the developer sees conflicts before merging.

### Non-goals (stated so they stay out)
- No VMs, no kernel extensions, no FUSE/NFS virtual filesystem (§11).
- No daemon. Background work is fire-and-forget child processes started by a command (async delete, hot spare).
- No build configuration (linkers, debuginfo, cranelift) — the repo's business.
- No agent orchestration — tmux / Claude Squad / t3 code / scripts do that. klon provides a directory + env + `--json`.
- No auto-setup wizard. `gh klon init --volume` is one explicit optional command.
- Windows: out of scope for v1 (ReFS/Dev Drive have block cloning; git-sprout supports it — possible later).

---

## 2. Prior art *(revised, research 2026-09-03)*

### Direct competitors: copy-on-write worktree tools

| Tool | Spawn model | Untracked build state | Platforms | Fallback | License / maturity |
|---|---|---|---|---|---|
| **worktrunk** v0.76.0 (2026-09-01), Rust, ~6.8k stars | plain `git worktree add`; path templates | `wt step copy-ignored`: post-hoc **per-file reflink walk, 4 threads**, all gitignored files by default, narrowed by `.worktreeinclude`. 14 GB `target/` ≈ 20 s. ext4 "very slow" (issue #3384) | mac, Linux, Windows | copy | **MIT OR Apache-2.0** (**V**). Homebrew, cargo, winget, conda, Nix |
| **git-sprout** v0.1.0 (2026-08-20), Rust | reflink of **tracked** files that the index marks unmodified; re-hashes in parallel to write stat data | not copied | APFS, btrfs, XFS, bcachefs, ReFS | plain worktree on ext4/NTFS or with `autocrlf` | MIT, 17 stars |
| **coworktree** (GallagherCommaJack), Go | `clonefile()` of the **whole project dir**, then register as linked worktree; rewrites absolute paths in ignored files | copied | **APFS only**; Linux "coming soon" via overlayfs | silent plain worktree | MIT, 8 stars, no releases. Closest design to klon |
| **cow** (joeinnes), Rust | `clonefile(2)` of the whole dir; **default is a separate full copy incl. `.git`**, `--worktree` opt-in; symlinks top-level packages in `node_modules`/`.venv` | copied or symlinked | APFS; Linux `cp --reflink=always` | copy | MIT, 13 stars. Has `cow sync`, MCP server. Turbopack cannot resolve symlinked deps |
| **git-cow-worktree** (josharian), Go | `--no-checkout`, reflink tracked blobs with equal SHA, then `git checkout` | not copied | APFS, FICLONE | checkout | "vibe-coded" (author); sprout's precursor |
| **lane** (lukeed), 2026 | reflinks all git-ignored paths | copied | APFS, Linux reflink | — | 12,283 files / 1358 MiB in 4.0 s; after one rebuild 106 MiB private, 1252 MiB shared |

None of them gives a per-tree resource, network, or fence envelope. None does O(1) spawn.

### worktrunk details worth borrowing
- Hooks: `pre/post-switch`, `-start`, `-commit`, `-merge`, `-remove`; project hooks need first-run approval (`approvals.toml`); hooks get variables as JSON on stdin; `hash_port` filter (10000–19999).
- `wt list`: Branch, Status, HEAD±, main↕, main…±, Remote⇅, Commit, Age, Message; `--full` adds CI + LLM summary; JSON schema v2 with `--print-schema`; agent-activity markers from git config.
- `wt merge main`: LLM commit message, squash, rebase, fast-forward via `update-ref` + `read-tree -m -u`, remove worktree and branch. Never pushes.
- Claude Code plugin: `WorktreeCreate` / `WorktreeRemove` hooks reroute Claude's `isolation: worktree` through `wt`. klon must ship the same (§19).
- Background removal under `taskpolicy -b` (macOS) / `nice -n 19` + `ionice -c 3` (Linux). Reuse.

### Orchestrators and adjacent tools (2026)

| Tool | Spawn | Isolation | Status |
|---|---|---|---|
| Claude Code native `--worktree` | `git worktree add` under `.claude/worktrees/`, branch `worktree-<name>`; `.worktreeinclude` copies gitignored files; `WorktreeCreate` hook replaces creation | tool-level only: blocks edits and `git -C` into the main checkout | `-p` runs leave worktrees behind |
| Conductor | worktree + `setup` script, `CONDUCTOR_PORT` | port env var | proprietary, macOS only |
| Emdash | worktree, pre-warmed pool (third-party report), `preservePatterns` | `EMDASH_PORT` | Apache-2.0, 5.6k stars |
| Superset | worktree per workspace | port detection | ELv2, 13.7k stars |
| vibe-kanban / Crystal | worktree + scripts | none | sunsetting / deprecated |
| claude-squad, cmux | worktree + tmux | none | AGPL |
| Dagger container-use | Docker container + branch | container | little 2026 activity |
| Fly Sprites | Firecracker microVM, CoW checkpoints | full VM | cloud |
| jj workspaces | separate working copy; **not** a git worktree (issue #8052 open) | none | slower than git for agents in GitButler's vcbench |
| GitButler | no worktrees, virtual branches in one dir | none; overlaps "lead to races" | $22M Series A |
| Sapling / EdenFS | lazy VFS | — | "not supported for external usage" |
| Fletch | `git clone --shared` per agent to isolate refs/config/hooks/stash | — | benchmark: same cost as worktree |

### Developer pain points (GitHub issues, HN, Reddit, blogs 2025–2026)
1. Cold deps and builds per tree (68 s vs 3 s for Rust with `target/` copied; "10-minute yarn install").
2. Disk: 9.82 GB for two worktrees of a 2 GB repo.
3. Ports: `EADDRINUSE` on 3000/5432; agents misread it as a code bug and "rewrite application logic".
4. Missing `.env` files (now mostly solved by include lists).
5. **Shared `.git` state**: an agent planted a `pre-commit` hook that ran in the main repo.
6. Editor confusion: VS Code does not detect relative worktrees; grep across nested trees.
7. **Resource exhaustion**: 3 sessions on 16 GB "is the cliff"; 20 subagents caused an I/O storm; users wrap Claude in `systemd-run` scopes; static `memory.max` wastes >90% or kills bursts (AgentCgroup, arXiv 2602.09345).
8. Cleanup: crashed sessions leave worktrees and locks.
9. Merge: lockfile churn; two agents refactor the same helper.

### Gaps no tool fills (klon's novelty, precisely)
1. Whole-directory CoW spawn **with** linked-worktree registration **on Linux reflink filesystems** (coworktree is APFS-only; cow defaults to a full clone; worktrunk copies after the fact; sprout excludes build state).
2. O(1) spawn (snapshot or hot spare).
3. A measured ext4 strategy.
4. Per-tree resource envelope; shared jobserver; loopback IP per tree.
5. Kernel-level write fence and per-tree hooks/config isolation.
6. Conflict radar before merge; structured merge.

---

## 3. Metrics to beat *(new)*

`gh klon bench` runs these on a 100k-file repo with a 10 GB build directory and prints a table.

| # | Metric | git worktree | git-sprout | worktrunk | **klon target** |
|---|---|---|---|---|---|
| M1 | Spawn to editable tree, p50 | 2–5 s | 0.5–1 s | 2–5 s | **≤ 1 s** v1 (snapshot or spare + `checkout`); **≤ 100 ms** v1.1 (index byte-splice) |
| M2 | Spawn to warm build state | never | never | +20 s (14 GB reflink walk); minutes on ext4 | **0 s** (snapshot or spare); background on cold ext4 copy |
| M3 | Build units compiled on first build | all | all | 0 if listed | **0** for cargo, npm/pnpm, Vite, TS, Go; documented exceptions (§14) |
| M4 | First `git status` | n/a | fast after re-hash | normal | **≤ 150 ms** warm |
| M5 | Unique disk per idle tree | src + build | diff, no build | diff | **diff + inode metadata**; KBs when hibernated |
| M6 | Delete latency | seconds–minutes | same | async | **≤ 10 ms** + background |
| M7 | Cross-tree writes blocked under `run` | 0% | 0% | 0% | **100%** (Landlock / Seatbelt) |
| M8 | Hooks/config isolated per tree | no | no | no | **yes** |
| M9 | Port collisions across N trees | yes | yes | `hash_port` | **0** on Linux (own IP); env contract on macOS |
| M10 | RAM bound per tree | none | none | none | `MemoryHigh` (Linux); footprint poll (macOS) |
| M11 | Conflict warning lead time | none | none | none | **every `list`**, ≤ 40 ms per pair |
| M12 | Build throughput at N=6 concurrent builders vs ideal | thrash | thrash | thrash | **≥ 80%** with jobserver + cpu weight |
| M13 | Commands to first klon / sudo prompts | 1 / 0 | 1 / 0 | 1 / 0 | **1 / 0** (also on ext4) |
| M14 | Daemon | no | no | no | **no** |

M1, M2, M7, M11 are the phase-transition metrics. The rest are parity or table stakes.

---

## 4. The primitive: `add` *(revised)*

```
gh klon add <branch>
  0. spare?   if ../<repo>.wt/.spare exists and matches golden's HEAD: rename → target (O(1)); skip step 1
  1. clone    golden → ../<repo>.wt/<branch>   (backend table below)
  2. .git     hand-write $GIT_COMMON_DIR/worktrees/<name>/{HEAD,commondir,gitdir} + <tree>/.git   (~1 ms, no git call)
  3. index    copy golden's index with a FRESH mtime (~5 ms)
  4. tree     git checkout -q <branch>   (370–900 ms on 100k files; touches only differing paths)
  5. fix      revert golden's dirty tracked paths; remove untracked non-ignored files; path fixup (§14)
  6. env      write .klon/env; allocate loopback slot; copy hooks dir; (Linux) prepare scope name
  7. spare    spawn a detached low-priority process that prepares the next spare
```

### 4.1 Backends, ranked by O(1)-ness

```rust
trait Backend {
    fn probe(golden) -> Capability;   // once per repo, cached in .klon/
    fn clone(golden, dst) -> Timing;  // whole dir incl. ignored, minus .klonignore
    fn delete(dst);                   // rename to .trash, then background
}
```

| Backend | When | Spawn (100k files) | Delete | Notes |
|---|---|---|---|---|
| `btrfs-snapshot` | Linux; golden dir is a btrfs subvolume owned by the user | **~5 ms** (**B**; O(1) **V**) | O(1) with `user_subvol_rm_allowed`; else background `rm -rf` (works since kernel 4.18, slow) | Best backend. `gh klon init` converts the golden dir into a subvolume (one `mv`). Unprivileged snapshot needs the source to be a subvolume the user owns (kernel ≥ 3.13) **V** |
| `btrfs-volume` | Linux on ext4 or any FS; one-time `gh klon init --volume 60G` | as above | as above | Sparse image + `mkfs.btrfs --rootdir <empty user dir>` (user-owned root, **B**) + `udisksctl loop-setup -f` + `udisksctl mount`. **No password in an active local session** (**V**, polkit `allow_active=yes`, verified on this laptop). Needs `btrfs-progs`: bundle a static `mkfs.btrfs` or print the install line. Golden moves into the volume once. Prefer `losetup --direct-io=on` to avoid double caching. udisks may not allow `user_subvol_rm_allowed` → background `rm -rf` |
| `apfs-clone` | macOS | 3.5–6 s (**V**: 17k–29k files/s, Apple DTS) | background `rm -rf` | **One `clonefile` per top-level ignored directory**, in parallel, never on the repo root: Apple says the kernel locks the source hierarchy for the whole call and a large tree "can stall the system"; it "strongly discourages" directory clones. Tracked files via per-file `clonefileat`. The hot spare hides the latency |
| `reflink-walk` | Linux XFS (`reflink=1`, default since 2019), bcachefs, btrfs plain dir, ZFS ≥ 2.2 (block cloning off by default until 2.2.6+) | ~3 s (**V**: 116k files, 4 threads 3.3 s; 10 threads 9 s) | background | Parallel FICLONE + `utimensat` (FICLONE sets mtime to now, **V**), **4 workers**. Hot spare |
| `copy` | ext4 without a volume | 40–100 s (**V**, local) | background | Per-directory strategy: **copy** big-file dirs (`target/`, `obj/`, `.next`); **re-install** small-file dirs that have a per-user store (pnpm 5 s from warm store vs 95 s copy; uv; Go). Warm dirs land in the background via atomic rename `target.klon-warming → target`. Hot spare at idle IO priority. Prints once: "run `gh klon init --volume` for instant spawns". **Never hardlink mutable dirs** (§11) |
| `overlay` (v2, Linux) | headless agent farms | O(1) | O(1) | overlayfs in a user+mount namespace (kernel ≥ 5.11, **V**): shares the lower page cache (**V**), works on ext4. Mount is visible **only inside** `gh klon run` (**V**). Lower layer must be a frozen golden generation. Opt-in only |

### 4.2 Hot spare *(new)*
After every `add`, `up`, and `rm`, klon spawns a detached low-priority process (same pattern as async delete) that prepares `../<repo>.wt/.spare/` from golden: clone + index copy with fresh mtime + dirty-path revert. `add` then renames the spare into place (36–62 ms measured, **V**), writes metadata, runs `git checkout <branch>`, and starts the next spare. Pool depth 1; `.klon.toml` `spare = 0` disables it. A spare cloned before an `up` is still warm for unchanged units; the checkout diff is just larger. No daemon.

### 4.3 Golden policy
Golden = your main checkout, on the base branch. A klon = **tracked files of its branch + ignored files of golden**. Dirty tracked files in golden are reverted in the clone (list from `git status`); untracked non-ignored files are removed from the clone. `.klonignore` (gitignore syntax) excludes paths from the clone: datasets, logs, nested `.git`, submodules by default.

### 4.4 Git details (all **V** on git 2.55 unless noted)
- **Why copy the index**: git compares inode/ctime/dev per entry by default; a clone changes them and the first `git status` re-hashes every file (20,403 file opens on a 20k repo). With `core.checkStat=minimal` git compares only size and whole-second mtime → 202 opens (directories only), clean. `core.trustctime=false` is redundant. **The index file must get a fresh mtime**: when all file mtimes equal the index mtime, racy-git re-hashes everything.
- **Why `git checkout`, not `read-tree -m -u` or `reset --hard`** (100k files, 20 differing paths): `checkout` 0.37–0.90 s (threaded lstat preload 60–160 ms, cache-tree kept); `read-tree -m -u` 0.9–1.7 s (frees the cache-tree unconditionally and rebuilds it); `reset --hard` 0.9–2.0 s. The cost is CPU on the index, not syscalls. `index.skipHash=true` (git ≥ 2.40) saves 40–100 ms per index write.
- **Worktree metadata by hand**: `HEAD`, `commondir`, `gitdir`, plus the `.git` file in the tree. `git worktree list` and `git status` accept it; jj does the same. `git worktree add --no-checkout --detach` refuses a non-empty dir, so hand-writing is required anyway.
- **Config**: `config.worktree` is ignored without `extensions.worktreeConfig`. Not needed: put `core.checkStat=minimal`, `core.untrackedCache=true`, `index.skipHash=true`, `index.version=4`, `merge.conflictStyle=zdiff3`, `rerere.enabled=true` in the shared repo config (harmless for golden; `untrackedCache` cut `status` from 0.35 s to 0.15 s) or pass via `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n` (git ≥ 2.31).
- **fsmonitor**: Linux inotify support arrived in git 2.55 (2026-06). One daemon per worktree; one inotify watch per directory; on a hot cache `status` was slower with it (0.49 s vs 0.35 s). Codex saw hangs with `core.fsmonitor=true` during `worktree add`. **Off by default.** Split index hit a `BUG:` abort in testing; do not use.
- **Branch resolution (git DWIM)**: local branch → use; else `origin/<name>` exists → tracking branch (targeted `git fetch origin <name>`); else new branch from base. Explicit: `origin/<name>`, `--pr <n>` (`refs/pull/<n>/head`, works for forks), `--issue <n>` (branch name from issue title).
- **Minimum git 2.38** (`merge-tree --write-tree`); recommended ≥ 2.44 (mergiraf). Ubuntu 22.04 ships 2.34; Ubuntu 24.04 ships 2.43; Homebrew ships current. `gh klon doctor` reports.

---

## 5. CLI surface *(revised)*

```
gh klon add <branch>                  # ≡ git worktree add, O(1) + warm; DWIM branch resolution
gh klon add origin/<branch>           # explicit remote-tracking
gh klon add --pr 123                  # PR (incl. forks) into a warm tree
gh klon add --issue 45                # branch from issue title
gh klon add <branch> -- <cmd...>      # spawn + run a command inside the envelope
gh klon list [--json]                 # path · branch · disk delta · RSS · procs · PR # · checks · vs-base · vs-siblings · behind
gh klon rm <branch> [--merged]        # ≡ git worktree remove (+ branch); async delete
gh klon prune
gh klon pr <branch>                   # gh pr create from that tree
gh klon sync <branch> [--merge|--onto main|--fresh|--all|--check]
gh klon merge <branch>                # fetch → pre_merge hook → structured merge → ff base → rm   (never pushes)
gh klon claim <branch> <paths...>     # v1.1: record owned paths; list flags overlaps
gh klon run <branch> -- <cmd...>      # exec inside envelope (fence + scope + env)
gh klon shell <branch>
gh klon stop <branch>                 # kill the whole process tree
gh klon up                            # update golden (main): fetch, ff-only, optional [warm] steps, re-spare
gh klon hibernate <branch> / wake <branch>
gh klon init [--volume <size>]        # one-time: make golden a btrfs subvolume, or create a sudo-free btrfs loop volume
gh klon doctor                        # backend, git version, fence ABI, cgroup delegation, inotify limits, make/ninja versions, pasta
gh klon bench                         # measure M1–M12 on this repo
gh klon lo0                           # macOS: print the lo0 alias + LaunchDaemon one-liner
```

- Path defaults to a sibling dir, worktree convention: `../<repo>.wt/<branch>`. `--path` for parity. Path modes for harnesses in §19.
- `git worktree list/remove/prune` keep working on klons (same metadata).
- `sync`: fetch (once for the common dir); fast-forward if no local divergence; else `rebase --autostash` (or `--merge`); detect force-push via `merge-base --is-ancestor` and refuse unless no unique local commits or `--force`. `--fresh` = `rm` + `add` + switch branch in. `--onto main` also rebases onto base. `--check` = dry-run via `merge-tree`.
- `hibernate`: stash tracked changes to `refs/klon/<name>` (+ untracked *non-ignored* files), delete the folder. `wake` = `add` + apply. Over `disk_budget`, `add` hibernates the LRU klon first (lazy, at `add` time). Never hibernate a tree with live processes.
- `list` data: disk delta (btrfs: `btrfs fi du -s`; else `du` of the tree minus shared — v1: count + trash `du`), RSS and live processes (cgroup / process group), PR + checks via `gh api` cached in `.klon/` with a short TTL, radar columns (§7).

Packaging: repo `gh-klon`, executable `gh-klon`; precompiled release assets `gh-klon_v<ver>_<os>-<arch>`; `gh extension create --precompiled=other klon`; `cli/gh-extension-precompile@v2` with `build_script_override`; tags with `-` become prereleases (**V**). Token via `gh auth token`; PR-shaped things via `gh api`. License: MIT OR Apache-2.0 (matches worktrunk; allows borrowing).

---

## 6. The envelope *(revised)*

`.klon/env` in each tree:

```
KLON_NAME=<branch>
KLON_IP=127.0.0.N          # per-klon loopback address (slot N)
HOST=127.0.0.N             # env-contract fallback for tools that honor it
TMPDIR=<tree>/.klon/tmp
MAKEFLAGS=-j --jobserver-auth=fifo:<XDG_RUNTIME_DIR or ~/.klon>/jobserver     # bare -j, not -jN (make warns otherwise)
GIT_CONFIG_COUNT=1  GIT_CONFIG_KEY_0=core.hooksPath  GIT_CONFIG_VALUE_0=<tree>/.klon/hooks
```

`gh klon run` sources it, applies the fence, and joins the resource group. `HOME` stays shared on purpose (per-user caches: cargo registry, pnpm store, NuGet, GOCACHE, uv cache).

| Feature | Linux | macOS | Default |
|---|---|---|---|
| **Write fence** | **Landlock** (kernel ≥ 5.13; ABI 3 = kernel 6.2 adds TRUNCATE; ABI 4 = 6.7 adds TCP bind/connect). Read everywhere; write only in the tree, `TMPDIR`, `~/.cache`, `~/.cargo`, `~/.npm`, pnpm store, `~/.nuget`, `GOCACHE`, uv cache, and `.klon.toml` `[fence] allow`. Always pair WRITE_FILE with TRUNCATE. No user namespace → Ubuntu 24.04 AppArmor rules (which block bwrap) do not apply. Rust crate `landlock` 0.4.7 (2026-07) is mature. **V** | `sandbox-exec` with a generated Seatbelt profile: `(deny default) (allow file-read*) (allow file-write* (subpath …)) (allow network*)` plus process/sysctl/mach allows. Works on macOS 15 and 26 with a deprecation warning (**V**). Same approach as Anthropic sandbox-runtime and Codex | **on** under `run`/`shell`/`add -- cmd`; `--no-fence` |
| **Memory / pids** | `systemd-run --user --scope -p MemoryHigh=<total/(N+1)> -p TasksMax=<n>`; recomputed at each `run` from live klon count (no daemon). memory + pids delegated on all systemd versions (**V**). D-Bus-free fallback: `mkdir` under the user's `user@UID.service` cgroup and write `memory.high` (**V**) | No cap. Poll `proc_pid_rusage` footprint of the group; SIGTERM at threshold. Jetsam priority via `posix_spawnattr_setjetsam_ext` (private, no privilege check in `kern_exec.c`, **V**; kill behaviour needs a test). `RLIMIT_AS` breaks JVM/.NET — do not use | on |
| **CPU** | `CPUWeight` works with systemd ≥ 252 (Ubuntu 24.04 = 255, Fedora 42 = 257, Ubuntu 26.04 = 259, **V**); Ubuntu 22.04 (249) silently ignores it → `nice`. `cpuset`/`io` need a root drop-in `Delegate=pids memory cpu cpuset io` in `/etc/systemd/system/user@.service.d/` — document only | QoS clamp UTILITY via `posix_spawnattr_set_qos_clamp_np` (SPI in libSystem, no privilege check, what `taskpolicy -c` uses, **V**). **Never** `PRIO_DARWIN_BG` for agents: it throttles network for the process's sockets (**V**, XNU `do_background_socket`). BG only for deletes | on |
| **Build slots** | fifo jobserver (make 4.4 protocol): `nproc-2` tokens; read a byte = acquire, write it back = release; klon tops up lost tokens (SIGKILLed clients). Clients: cargo/rustc (`jobserver` crate parses `fifo:`), GNU make ≥ 4.4, ninja ≥ 1.13 (client only, fifo on POSIX, disabled by `-jN`), LLVM `--offload-jobs=jobserver`. Not: CMake itself, Bazel, Go, tsc, esbuild, vite, dotnet, gradle, pytest. Ubuntu 24.04 ships make 4.3 (pipe style only) → export `fifo:` anyway; make 4.3 ignores it with a warning (**B**) | same; macOS `/usr/bin/make` is 3.81; Homebrew make 4.4.1 | on; `KLON_NO_JOBSERVER=1` |
| **Network identity** | `lo` owns all of `127/8`: bind to `127.0.0.N` with no config (**V**). `run --netns`: `pasta --config-net -t 127.0.0.N/auto -- <cmd>` maps host `127.0.0.N:<port>` to the same port in a rootless namespace with egress; `/auto` rescans bound ports every 1 s; ~250 ms startup (**V**, 224–327 ms). Packaged on Ubuntu 24.04+, Fedora, Arch, Debian 13; **not on Ubuntu 22.04**. Go binaries with a pure resolver may need `--dns-forward` (**B**). `unshare -Un` = private loopback, no egress, near-zero cost | **No sudo-free mechanism** (macOS 15/26). `lo0` holds only `127.0.0.1`; alias needs root; a `RunAtLoad` LaunchDaemon makes it persistent. `pfctl` rdr also needs root. The DYLD interposer is dead: hardened-runtime binaries (node, python.org, .NET) ignore `DYLD_*`. macOS 26 Containerization is VM-based. → env contract only (`HOST`, `PORT`, `KLON_IP`); `gh klon lo0` prints the one-liner | env on; netns opt-in |
| **Process tree** | `cgroup.kill` on the scope | `POSIX_SPAWN_SETSID`, enumerate with `proc_listpgrppids`, `killpg`; escapees via `proc_listchildpids` or `KLON_ID` env tag | `gh klon stop` |
| **Hooks/config** | per-tree `core.hooksPath` → `<tree>/.klon/hooks` (copy of the repo hooks). An agent that edits a hook cannot affect golden or siblings | same | on |

Services: state lives in the tree. If golden contains a seeded `pgdata/`, every klon clones a pre-seeded Postgres in zero time; with a per-klon IP it is isolated automatically. Shared-stack alternative: one Postgres, `CREATE DATABASE <klon> TEMPLATE golden`, exported as `DATABASE_URL`.

### Delete
Rename into `../<repo>.wt/.trash/` (36–62 ms measured on ext4, **V**) → background delete under `PRIO_DARWIN_BG` (macOS) / `nice -n 19` + `ionice -c 3` (Linux). Measured background `rm -rf` on ext4: 4.5 s for a 6 GB / 11k-file tree, 10 s for 4 GB / 82k files. btrfs subvolume delete is O(1) with `user_subvol_rm_allowed`.

---

## 7. Integration certainty *(new)*

The literature says coordination, not parallelism, carries the gain (§15.3). klon gives the developer the facts before a merge and keeps merges deterministic.

- **Radar** in `gh klon list`, computed with `git merge-tree --write-tree --quiet` (10–40 ms per pair on 100k files; cost scales with changed paths, not tree size; **V**): **vs base** (clean / N conflicts, `--name-only -z` for paths), **vs siblings** (pairwise, batched with `--stdin`; klons that touch the same files), **behind** (commits behind base). Cached in `.klon/radar` keyed by the tuple of HEADs.
- **Claims** (v1.1): `gh klon claim <branch> <paths...>` records owned paths in `.klon/claims`; `list` flags overlaps; `add --claim` seeds them. This is the file-claim mechanism that AgentRoom's ablation identifies as the load-bearing part, and what weave's MCP server does at the entity level.
- **`gh klon merge <branch>`**: fetch → `pre_merge` hook (tests; Glite ARF shows deterministic verifier gates at ~1% overhead) → structured merge with **mergiraf** when installed (GPLv3, separate process, `merge.mergiraf.driver` + `.gitattributes`; 29 languages; 42% fewer false negatives than Spork; median < 1 s) → fast-forward base → `rm`. Never pushes. Refuses on a dirty golden. LLM conflict resolution is out of scope (< 60% of hunks, non-deterministic).
- `merge.conflictStyle=zdiff3` and `rerere` on in the shared config.

---

## 8. `gh klon up` (golden refresh, on demand)

In the main checkout: `git fetch origin` → `git merge --ff-only` → optional `[warm]` steps from `.klon.toml` run under the jobserver → re-spare in the background. Refuses if main is dirty or not on the base branch. Same contract as `git pull` today, no daemon. Golden is as fresh as your last `up`; new klons clone whatever state it has.

---

## 9. Config (`.klon.toml`, all optional) *(revised)*

```toml
base = "main"                       # golden branch
path = "../{repo}.wt/{branch}"      # path template
disk_budget = "40G"                 # LRU-hibernate above this (checked at `add`)
spare = 1                           # hot-spare pool depth; 0 disables

[warm]                              # run by `gh klon up` after ff
steps = ["cargo build", "cargo nextest run --no-run", "pnpm install --frozen-lockfile"]

[fence]                             # extra writable paths under `run`
allow = ["~/.local/share/pnpm"]

[copy]                              # ext4 `copy` backend only: per-directory strategy overrides
reinstall = { "node_modules" = "pnpm install --offline --frozen-lockfile" }

[hardlink]                          # v2, opt-in: paths hardlinked instead of cloned (page-cache sharing)
paths = ["target/debug/deps", "target/debug/.fingerprint"]
```

`.klonignore` (gitignore syntax): paths excluded from the clone. `.worktreeinclude` (Claude Code / worktrunk convention) is honoured as an additive include when present.

---

## 10. Throughput model *(revised)*

- **CPU**: kernel fair-share (`cpu.weight` is work-conserving; QoS clamp on macOS) + jobserver for jobserver-aware tools. No userspace scheduler. `make -l` load-average scheduling oscillates — do not use.
- **RAM — the real ceiling**: CoW shares *disk blocks, not page cache*. Reflinked and cloned files have distinct inodes and distinct page caches on btrfs, XFS, and APFS (**V**); hardlinks share one; overlayfs shares the lower layer's (**V**). Mitigations:
  - v2 opt-in **hardlink split** for content-addressed artifacts whose filename contains the input hash (registry crates, pnpm store, Go build cache): one inode → one page-cache copy across all trees. howardjohn measured < 1 s and 0 GB for hardlinked immutable deps vs 2m19s and 127 GB for a copy. Unsafe for anything rewritten in place; the repo declares paths.
  - Ecosystems that already share per user need nothing: pnpm, NuGet, Go, Gradle cache, ccache/sccache, Turborepo (cache is shared across worktrees), Nx (`~/.nx`).
  - `MemoryHigh` per tree on Linux, **computed as total/(N+1) at each `run`** (AgentCgroup: static caps waste >90% or kill bursts). macOS has no equivalent → fewer concurrent trees there.
  - Editors/LSPs are the real RAM ceiling on "many klons" (1–3 GB per window); `list` shows RSS per tree.
- **Git**: one object store → one fetch serves all trees; `git maintenance` (commit-graph, multi-pack-index, `worktree-prune` task since 2.50) keeps status/log fast across dozens of worktrees.
- **Inotify**: `max_user_watches` is 1% of low memory in [8192, 1048576] since kernel 5.11; this laptop has 65,536 and 128 instances. 10 trees × `node_modules` exhausts it. `doctor` reports; document the sysctl. macOS FSEvents has no per-watch limit.
- **Ballpark** (20 cores / 62 GB / NVMe, Rust monorepo): worktrees today — three agents building thrash (3× page cache, cold builds). klon with hardlinked deps + jobserver + `MemoryHigh` — five or six agents building concurrently at ~3 cores each, no thrash, `add` ≤ 1 s, disk a few hundred MB per tree.

---

## 11. Rejected designs (and why) *(revised)* — don't re-litigate without new data

| Rejected | Why |
|---|---|
| Virtual filesystem (FUSE / NFS loopback / EdenFS-style lazy materialisation) | Daemon in the hot path of every `stat`; VFS for Git died when Apple deprecated kexts; Scalar became sparse-checkout + fsmonitor; EdenFS is "not supported for external usage". Below a few hundred thousand files a CoW clone is cheaper and compatible with every tool |
| Reimplementing git's object store | 100 % cost, 0 % gain; it *is* GitHub compatibility |
| **gitoxide on the hot path** | Cannot create a linked worktree, cannot add/remove index entries, index write is slower than git's; a gix two-tree update is 1.2–2x at best, not ≥ 2x (**V**, §15.2) |
| **`read-tree -m -u` two-tree update** | Frees the cache-tree unconditionally; slower than `checkout` (**V**) |
| **APFS `clonefile` on the repo root** | 3.5–6 s per 100k files; locks the source hierarchy; Apple discourages it (**V**). Per-top-level-dir clones + hot spare instead |
| **Full copy as the routine ext4 path** | 95 s per node_modules and 32 s to delete on a real laptop (**V**) |
| **Hardlink copy of mutable dirs** (`cp -al`) | cargo, MSBuild, pnpm `.modules.yaml`, npm, ninja open outputs with truncate → corrupts golden (BuildStream saw artifact corruption; pnpm warns) |
| **overlayfs as the default Linux backend** | Mount is invisible outside the namespace; editors cannot open the tree; macOS has no equivalent. v2 opt-in for headless farms only |
| **Static `memory.max`** | Wastes >90 % or kills bursts (AgentCgroup). Use soft `MemoryHigh`, recomputed per `run` |
| **bwrap as the Linux fence** | Needs user namespaces; Ubuntu 24.04 AppArmor blocks it by default. Landlock needs none |
| **jj as the agent-facing VCS** | Slower than git for agents in GitButler's vcbench; workspaces are not git worktrees (issue #8052 open) |
| **CRDT shared workspace / shared mutable dir** | AgentRoom ablation: coordination, not CRDT merge, bears the load; character merges produce semantic conflicts |
| **LLM-only conflict resolution** | < 60 % of real hunks; non-deterministic (word overlap < 0.6 in 67 % of reruns) |
| Separate golden tree + refresh daemon | Main checkout is golden; `gh klon up` replaces the daemon |
| Userspace governor (slots, SRPT, RSS models, priority jobserver) | Kernel fair-share + jobserver covers it; per-command slots can't catch an agent's own `bash` calls, a cgroup/QoS on the process tree can |
| Observation layer (fanotify/FSEvents write attribution, learned immutability, cost tables) | Whole-klon LRU hibernate makes it unnecessary |
| Three-state eviction, GreedyDual-Size | Two states suffice |
| Per-ecosystem unit DAG, `[ports]` templates, `depends_on` | Same-port-everywhere localhost removes port templating; tools' own incrementality removes affected-set computation |
| Padded paths (conda trick) for binary relocation | Text rewrite covers ~95 %; rest is debuginfo |
| macOS `bind()` interposer | Hardened runtime strips `DYLD_*`; notarized node/dotnet/python are hardened (**V**) |
| Auto-follow, recorded command replay, CI-inferred recipes | Manual `sync`, optional `[warm]` list |
| Hardlinking everything with read-only bit | Cargo rewrites fingerprint files in place → EACCES → build fails |
| Naive best-of-N above K=8 | Gains plateau (R2E-Gym ~43 % at K=8) |

---

## 12. Theory notes (for context; not implementation guidance)

- **Information floor**: build artifacts are a deterministic function of sources (given reproducible builds), so K(build | src, toolchain) = O(1). The minimum state per idle tree is the compressed source patch (KBs) — which is what `hibernate` stores. N×diff (block CoW, 100 MB–GB) is a time-space trade, not information.
- **Time-space frontier**: pebbling on the build DAG; practical policy would be "keep `.rmeta`, drop codegen" — deliberately not implemented.
- **Below block CoW without recompute**: content-defined-chunk dedup across agents — requires a content-addressed store, i.e. leaving "real files on disk". Not for klon.
- **Metadata cost**: N×diff is really N×(diff + inode table). APFS `clonefile` duplicates inodes (~100 B × files); btrfs snapshot is O(1) (shared B-tree). This is *why* only snapshots are O(1) and why the hot spare exists.

---

## 13. Component decision table — build, link, or shell (the 2x rule) *(revised)*

| Component | Decision | Why |
|---|---|---|
| Two-tree update | **shell** `git checkout` | gix cannot; ≤ 2x possible; 0.4–0.9 s is acceptable behind a spare |
| Index byte-splice editor | **build, v1.1** | Only path to M1 ≤ 100 ms: copy unchanged index bytes, patch ~20 entries, write a null trailer (git 2.55 `verify_hdr` accepts it, **V**). Do it after `bench` proves the need |
| Worktree metadata | **build** (3 files + `.git`) | ~1 ms; no git call; `worktree add` refuses a non-empty dir |
| Parallel reflink / clonefile walk | **build** (rayon, 4 workers) | `cp` is single-threaded; 2.6x measured with 4 threads; 10 threads is slower |
| btrfs snapshot / volume | **shell** `btrfs`, `udisksctl`; bundle static `mkfs.btrfs` | O(1) primitive; never reimplement |
| Hot spare | **build** (~150 lines) | Novel; makes every backend O(1) at claim time |
| Jobserver server | **build** (~50 lines) | No machine-wide owner exists; top-up on token loss; Gentoo's CUSE variant is the reference |
| Landlock fence | **link** `landlock` crate | In-process, no userns, no subprocess |
| Seatbelt fence | **build** the profile; **shell** `sandbox-exec` | Same as Anthropic sandbox-runtime |
| macOS QoS / setsid / pgrp / jetsam | **build** (FFI, ~40 lines) | No privilege checks; in-process |
| cgroup scope | **shell** `systemd-run`; cgroupfs fallback | Standard |
| pasta | **shell**, opt-in | Rootless netns with egress; nothing to reimplement |
| Conflict radar | **shell** `git merge-tree --write-tree` | 10–40 ms; exact |
| Structured merge | **shell** mergiraf (GPLv3, separate process) | 42 % fewer false negatives; never reimplement |
| Path fixup | **link** `ignore` + `grep-searcher` | ~80 lines |
| Index validity | **reuse** `core.checkStat=minimal` + fresh index mtime + `untrackedCache` | copied index valid instantly (**V**) |
| GitHub | **shell** `gh` | auth, `gh api`, PR refs |
| gitoxide | **drop** from v1 | see §11 |

---

## 14. Ecosystem notes *(revised, verified 2026-09)*

| Ecosystem | Already shared per user | Per tree | Relocation after a whole-dir copy (mtimes preserved) |
|---|---|---|---|
| Rust / cargo | registry sources | `target/` (2–11 GB) | **Relocatable as-is**: fingerprints hash relative paths by design (**V**). rustc tracks `working_dir`, so the first *edit* after a move recompiles that one crate non-incrementally. `build.build-dir` stable since 1.91; layout v2 re-stabilised Aug 2026; `trim-paths` still nightly; cross-workspace cache is a 2026 nightly goal |
| TS/Node (pnpm) | pnpm store (hardlinks) | symlink farm, `.next`, `dist`, `.tsbuildinfo` | Runtime works. **`node_modules/.modules.yaml` stores absolute `storeDir`/`virtualStoreDir`** → next `pnpm install` raises `ERR_PNPM_UNEXPECTED_VIRTUAL_STORE` (**V**). Path fixup rewrites two keys |
| TS/Node (npm, Yarn PnP, Bun) | Bun cache | full `node_modules` | Relocatable (relative keys / relative symlinks, **V**). npm has no page-cache sharing (recommend pnpm) |
| Vite, TypeScript `.tsbuildinfo`, Turborepo, Nx | Turbo/Nx caches shared across worktrees | outputs | Relocatable (**V**) |
| Next.js `.next/cache` | — | webpack cache | **Not relocatable** (webpack stores absolute paths, **V**) → delete on clone |
| C# / .NET | NuGet `~/.nuget/packages`, Roslyn server | `bin/`, `obj/` | **`obj/*.nuget.g.props` and `project.assets.json` hold absolute paths** (**V**, sdk#10046); `dotnet build` re-restores (dgspec hash has paths) and CoreCompile reruns (**B**). Path fixup + `dotnet restore` (offline, cheap) |
| Go | `GOCACHE` | ~nothing | Relocatable with `-trimpath`; without it the package dir is in the action ID (**V**) |
| Python (uv) | uv cache | `.venv` | `uv venv --relocatable` fixes shebangs; non-relocatable venvs record `uv-venv-path` and uv recreates them (**V**). Path fixup for plain venvs |
| C/C++ (CMake + Ninja) | ccache | build dir | **`CMakeCache.txt` absolute** (CMake errors on mismatch, **V**); `.ninja_deps`/`.ninja_log` hold old paths → fixup + `cmake .`, or delete the ninja logs |
| JVM (Gradle) | build cache, deps | `build/`, `.gradle/` | Build cache relocatable only for RELATIVE-sensitivity tasks; configuration cache not relocatable (**V**). One JVM daemon per active tree (RAM) |
| Maven | `~/.m2` | `target/maven-status` | `inputFiles.lst` absolute → one full recompile (**V**) |
| Bazel / Buck2 | Bazel output base keyed by workspace path | `bazel-*` links | Bazel: empty output base after a move unless `--output_base` pinned (**V**). Buck2: full rebuild (**B**) |
| Docs (mkdocs/docusaurus/mdbook) | — | `site/`, `build/` | none |

Language-agnostic facts: toolchain pins (`rust-toolchain.toml`, `.nvmrc`, `global.json`) travel with the tree; installs are per-user, side-by-side. Docker image builds share BuildKit's layer cache per daemon. `.env` files clone with the tree (override `PORT`/`HOST`/`DATABASE_URL` via env contract). In-place writers (unsafe to hardlink): cargo, MSBuild, pnpm `.modules.yaml`, npm, ninja.

**Path fixup** (language-agnostic relocation): fixed-string search for golden's absolute path over gitignored dirs (`ignore` + `grep-searcher`), rewrite hits in **text files only**. Covers pnpm `.modules.yaml`, .NET `obj/`, `CMakeCache.txt`, `pyvenv.cfg`, venv shebangs, `.bin` shims. Binaries left alone. Delete `.next/cache` and `.ninja_log`/`.ninja_deps`. A klon's path is stable for its lifetime (wake re-clones to the same path) so whatever isn't rewritten is paid once.

---

## 15. Measurements and research *(new)*

### 15.1 Local benchmark (2026-09-03, Ubuntu 22.04, ext4 NVMe 87 % full, warm cache, 20 cores, 62 GB)

| Directory | `cp -al` (metadata walk) | `cp -a` (full copy) | 16-way parallel copy | `rm -rf` of copy | rename to trash | `rm -rf` under nice/ionice |
|---|---|---|---|---|---|---|
| node_modules, 4039 MB, 81,874 files | 9.3 s | **94.8 s** | 328.7 s (worse) | 32.0 s | 62 ms | 10.0 s |
| Rust target, 6028 MB, 10,855 files | 0.8 s | **37.5 s** | 61.2 s (worse) | 5.7 s | 36 ms | 4.5 s |

Lessons: file count is the enemy for CoW walks (~9–14k files/s single-threaded metadata rate); bytes are the enemy for ext4 copies; parallelism hurts on an IOPS-bound disk; rename is O(1). A pnpm re-install from the warm store (~5 s per pnpm's own benchmark) beats copying node_modules on ext4.

Also verified locally: `udisksctl loop-setup` needs no password in an active x11 session; Landlock ABI 3; cgroup delegation `memory pids` (systemd 249); inotify 65,536 / 128; git 2.34.1 (too old for `merge-tree --write-tree`); no `pasta`, no `btrfs-progs`; `bwrap` present.

### 15.2 Git plumbing experiments (git 2.55 built from source; 20k and 100k-file repos; **V**)

| Command (100k files, 20 paths differ) | `newfstatat` calls | Wall time |
|---|---|---|
| `git read-tree -m -u golden target` | 270–290 | 0.9–1.7 s (frees cache-tree, rebuilds it) |
| `git checkout <branch>` | 101,401 (threaded preload) | 0.44–0.90 s; 0.37 s with `index.skipHash` |
| `git reset --hard` | ~100k | 0.9–2.0 s |
| `git status` (copied index, `checkStat=minimal`, `untrackedCache`) | dirs only | 0.15 s |
| `git merge-tree --write-tree --quiet A B` | — | 10–40 ms; 5,000 changed files also 40 ms |
| `git worktree add --no-checkout --detach <empty>` | — | 10 ms; hand-written metadata ~1 ms |

gix-index 0.48 on 100k entries: read 26–30 ms (hash skipped), write 185–200 ms without hash, 520–560 ms with. The `gix` CLI was slower than git for `status`, `diff tree`, and `merge tree`.

### 15.3 Literature (2024–2026) that shaped the design

| Finding | Source | klon consequence |
|---|---|---|
| Two agents on one repo score ~30 % lower together than alone; failures are coordination failures | CooperBench, arXiv 2601.13295 | Radar + claims |
| "Coordination, not parallelism or CRDT-merge, bears the load"; file-level claims work | AgentRoom, arXiv 2608.23740 | `claim` |
| Dependency-aware partitioning with isolated hub files: 2.10x wall-clock, +14 % pass, −35 % cost; naive per-file split gives almost nothing | Co-Coder, arXiv 2606.00953 | Radar surfaces hub-file overlap |
| Branch-and-merge with test-gated merge is the central coordination primitive (+25.6 % PaperBench) | CAID, arXiv 2603.21489 | `merge` with `pre_merge` gate |
| Write-time conflict detection beats a worktree baseline by +18.7 | STORM, arXiv 2605.20563 | Radar at every `list` |
| Fixed serialisation order + speculative writes + saga undo: 1.4x; 2PL/OCC lose all concurrency; LLM merges are non-deterministic | CoAgent 2606.15376, S-Bus 2605.17076 | No locks; deterministic structured merge |
| 27.7 % of agent PRs conflict with base; 42 % of conflicts are structural | arXiv 2607.04697, 2604.03551 | Structured merge matters |
| Mergiraf: 42 % fewer false negatives than Spork, median < 1 s; semistructured merge cuts spurious conflicts | arXiv 2507.19687, 2608.11345 | mergiraf driver |
| Best LLMs resolve < 60 % of real hunks | Merge-Bench 2605.25890 | No LLM merge |
| Twelve agents on one 48 GB Mac, deterministic verifier gates at ~1 % overhead | Glite ARF, arXiv 2606.27416 | `pre_merge` tests |
| Static `memory.max` wastes > 90 % or kills bursts | AgentCgroup, arXiv 2602.09345 | Soft `MemoryHigh`, recomputed |
| Hardlinked immutable deps: < 1 s and 0 GB vs 2m19s and 127 GB | howardjohn 2026 | v2 hardlink split |
| Machine-wide jobserver: CUSE fifo reclaims lost tokens; cgroup `cpu.weight` is work-conserving; `make -l` oscillates | Górny 2025, guildmaster | Token top-up; no load-average scheduling |
| Best-of-N plateaus at K≈8; branching from mid-trajectory snapshots cuts cost 17 % | R2E-Gym 2504.07164, SWE-Replay 2601.22129 | Cheap snapshots enable retries; not a klon feature |
| VFS for Git died with kexts; Scalar = sparse-checkout + fsmonitor | GitHub "The Story of Scalar" | No VFS |
| Anthropic and OpenAI sandboxes: Seatbelt on macOS, bwrap/Landlock on Linux, proxy for network | sandbox-runtime, Willison 2025 | Same fence design |

---

## 16. Open questions *(revised)*

### Resolved in this pass
- q1 (APFS mtime): the man page says the clone gets "its own copy of attributes and extended attributes" identical to the source (**V** wording; **B** that this covers times). Keep a test in the suite.
- q2 (Linux default): snapshot when golden is a user-owned subvolume; else reflink walk with 4 workers; `init` can convert.
- q3 (walk speed): ~35k files/s with 4 threads on XFS (**V**); more threads is slower.
- q4 (ext4): never hardlink mutable dirs; per-directory copy/reinstall + hot spare; recommend the udisks btrfs volume.
- q5 (nested repos / submodules): excluded by default via `.klonignore` defaults.
- q7 (gix): dropped; metadata by hand; `checkout` for the tree.
- q8 (two-tree semantics): `checkout` handles it; dirty golden paths reverted first; ignored content inside deleted directories is kept by git.
- q9 (`checkStat`): `minimal` is enough; index file needs a fresh mtime.
- q10 (`worktreeConfig`): not needed; shared config or `GIT_CONFIG_*`.
- q12–13 (macOS APIs): `posix_spawnattr_set_qos_clamp_np` (process-wide, inherited); jetsam via `posix_spawnattr_setjetsam_ext`; no privilege checks.
- q14 (systemd): ≥ 252 delegates cpu; memory/pids everywhere; drop-in documented only.
- q15 (jobserver): blocking on an empty fifo is correct; `KLON_NO_JOBSERVER=1`; `nproc-2` tokens; top up lost tokens.
- q16 (pasta): Ubuntu 24.04+, Fedora, Arch, Debian 13; not Ubuntu 22.04; ~250 ms start.
- q17 (lo0): user-managed; `gh klon lo0` prints the command.
- q18 (name): free.
- q19 (worktrunk license): MIT OR Apache-2.0.
- q21 (`list` PR data): cache with TTL in `.klon/`.

### Still open
1. Does `mkfs.btrfs --rootdir <user-owned empty dir>` produce a user-owned root inode? Does udisks accept `user_subvol_rm_allowed`? If not, snapshot delete is background `rm -rf` (kernel ≥ 4.18). Spike on a machine with `btrfs-progs`.
2. Bundle a static `mkfs.btrfs` (GPLv2, ~1–2 MB) in the release asset, or only print the install line?
3. Does a jetsam memlimit set via `posix_spawnattr_setjetsam_ext` actually kill on macOS (CONFIG_JETSAM off)? Test.
4. Hot spare + dirty golden: revert at spare creation or at claim? (Proposal: record dirty paths at creation, revert at claim.)
5. `add -- <cmd>` in tmux: exec only, or open a pane when tmux is present? (Proposal: exec; `--json` gives orchestrators what they need.)
6. `--only <dirs>` sparse-checkout cone for focused klons (editor/LSP/inotify load) — v2?
7. `disk_budget` accounting: `btrfs fi du -s` where available; else count + trash `du`.
8. Symlinks inside the tree pointing at golden's absolute path: rewrite targets in path fixup?
9. Claude Code's `EnterWorktree` / `isolation: worktree` may bypass `WorktreeCreate` (issue #36205). Test.

---

## 17. Verification plan *(revised)* — must pass before calling v1 done

1. **Zero-compile test**: `gh klon add x` from a built main → `cargo build` / `pnpm build` / `dotnet build` in the klon compiles nothing. Run on a Rust workspace, a pnpm workspace, a .NET solution (M3).
2. **Relocation test**: clone golden to a random path; `cargo build` → 0 "Compiling"; `pnpm install --frozen-lockfile --offline` → no-op after `.modules.yaml` fixup; `dotnet build` → no compile after fixup + restore.
3. **Instant status**: `git status` in a fresh klon returns clean in ≤ 150 ms warm, ≤ 1 s cold (M4).
4. **Spawn latency**: `add` ≤ 1 s on 100k files on btrfs (snapshot) and on APFS/XFS/ext4 (spare); report per-step timings (M1, M2). `bench` prints the table.
5. **Worktree compatibility**: `git worktree list` shows klons; `git worktree remove` works; hooks fire from the per-tree hooks dir; `gh pr create` from inside a klon works; CI unchanged.
6. **Envelope**: N klons building concurrently → total rustc processes ≤ jobserver tokens; `MemoryHigh` enforced on Linux; agent API calls not throttled on macOS (utility clamp, not BG); `stop` kills the whole tree (M10, M12).
7. **Fence**: under `run`, a write to golden, a sibling klon, or `~/.ssh` fails with EACCES on Linux and EPERM on macOS; writes to the tree, `TMPDIR`, and declared caches succeed (M7).
8. **Hooks isolation**: a hook edited inside a klon does not run in golden (M8).
9. **Radar**: two klons editing the same function show a sibling conflict at the next `list`; `merge` refuses until resolved; mergiraf resolves a non-overlapping same-file edit (M11).
10. **Delete**: `rm` returns ≤ 10 ms; trash drained in background at low priority (M6).
11. **ext4 without setup**: `add` works with a single warning line and background warming; `init --volume` then gives ≤ 1 s spawns with zero sudo prompts (M13).
12. **APFS mtime**: cloned files keep mtime; cargo and `checkStat=minimal` stay warm.

---

## 18. Build order *(revised)*

1. Backends `btrfs-snapshot`, `reflink-walk`, `apfs-clone`, `copy` + metadata + index copy + `checkout` + async `rm` + `list` + `bench` + `doctor`. → a usable, measured worktree replacement.
2. Hot spare + `btrfs-volume` (`init --volume`, `mkfs.btrfs` bundling decision). → O(1) spawn on every platform.
3. Envelope: env file, jobserver, Landlock / Seatbelt fence, scope / QoS, per-tree hooks, `run` / `shell` / `stop`.
4. Radar + `merge` + `sync --check` + mergiraf driver + Claude Code plugin + `--json`.
5. Path fixup, `.klonignore`, `--pr`, `--issue`, `pr`, `up`, `rm --merged`, hibernate / wake, `disk_budget`.
6. v1.1: index byte-splice editor (M1 ≤ 100 ms), `claim`, pasta `--netns`, hardlink split for declared immutable paths (page cache).
7. v2: `overlay` backend, `--only`, Windows/ReFS.

---

## 19. Harness integration *(new)*

| Harness | Path convention | Custom create? | Env / stdin |
|---|---|---|---|
| Claude Code | `<repo>/.claude/worktrees/<name>`, branch `worktree-<name>`, `worktree.baseRef` = `fresh` / `head` | **`WorktreeCreate` hook** replaces git: stdin JSON (`hook_event_name`, `cwd`, `name`), stdout = path only, non-zero aborts. `WorktreeRemove` gets `worktree_path`. `.worktreeinclude` is skipped when a hook exists (**V**) | `CLAUDE_PROJECT_DIR` stays at the launch dir |
| Codex app / CLI | `$CODEX_HOME/worktrees`, detached HEAD, `codex/` prefix, keeps 15 | no; setup scripts only | none |
| Cursor | undocumented root; `cursor.worktreeMaxCount` = 25 | no; `.cursor/worktrees.json` `setup-worktree` after create | `ROOT_WORKTREE_PATH` |
| t3 code | `~/.t3/worktrees/<repo>/t3code-<8hex>`, branch `t3code/<8hex>` (**V**, this session) | no | none |
| OpenCode | `~/.local/share/opencode/worktree/<project>/<branch>` via plugins | plugin API | `worktree`, `directory` |
| Conductor | `~/conductor/workspaces/<project>/<city>` | no; `setup` script | `CONDUCTOR_WORKSPACE_PATH`, `CONDUCTOR_ROOT_PATH`, `CONDUCTOR_PORT` |

klon ships: a Claude Code plugin (`WorktreeCreate` / `WorktreeRemove`); `--path` modes for the conventions above; `--json` on `add`, `list`, `rm`; `.worktreeinclude` honoured as an additive include. Because klons are ordinary worktrees, harnesses that only run a post-create script work by pointing that script at `gh klon warm <path>` (v1.1: warm an existing worktree in place).

---

## 20. References *(revised)*

**Competitors**
- worktrunk: https://github.com/max-sixty/worktrunk · https://worktrunk.dev/step/ · https://worktrunk.dev/hook/ · https://worktrunk.dev/claude-code/ · https://github.com/max-sixty/worktrunk/issues/3384
- git-sprout: https://github.com/alltuner/git-sprout · git-cow-worktree: https://github.com/josharian/git-cow-worktree · coworktree: https://github.com/GallagherCommaJack/coworktree · cow: https://github.com/joeinnes/cow · lane: https://lane.lukeed.com/ · clonetree: https://github.com/cortesi/clonetree
- Claude Code worktrees: https://code.claude.com/docs/en/worktrees · Codex worktrees: https://learn.chatgpt.com/docs/environments/git-worktrees · Cursor: https://cursor.com/docs/configuration/worktrees · Conductor: https://www.conductor.build/docs/reference/scripts/setup · Emdash: https://docs.emdash.sh/project-config · Superset: https://github.com/superset-sh/superset · Fletch: https://fletch.sh/blog/git-worktrees-vs-clones-for-ai-agents/ · GitButler: https://docs.gitbutler.com/ai-agents/parallel-agents · vcbench: https://github.com/gitbutlerapp/version-control-bench · Sapling: https://engineering.fb.com/2025/10/16/developer-tools/branching-in-a-sapling-monorepo/ · https://blog.ezyang.com/2026/03/parallel-agents-heart-sapling/
- Pain points: https://github.com/anthropics/claude-code/issues/15487 · https://github.com/anthropics/claude-code/issues/20905 · https://github.com/anthropics/claude-code/issues/36205 · https://trigger.dev/blog/parallel-agents-gitbutler · https://news.ycombinator.com/item?id=49110389 · https://github.com/microsoft/vscode/issues/320749

**Filesystems**
- APFS `clonefile(2)`: https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2 · Apple DTS on directory clones: https://developer.apple.com/forums/thread/784446 · https://developer.apple.com/forums/thread/786595 · https://eclecticlight.co/2024/03/20/apfs-files-and-clones/
- Reflink benchmark: https://www.tunbury.org/2025/07/15/reflink-copy/ · FICLONE mtime: https://lkml.rescloud.iu.edu/1704.0/03512.html · `reflink-copy`: https://crates.io/crates/reflink-copy · `xcp`: https://github.com/tarka/xcp · `fcp`: https://github.com/Svetlitski/fcp · io_uring copier: https://dev.to/vincentdu2021/building-a-file-copier-4x-faster-than-cp-using-iouring-4b5n
- btrfs: https://btrfs.readthedocs.io/en/latest/btrfs-subvolume.html · unprivileged snapshot (3.13): https://lkml.iu.edu/1402.0/01696.html · `user_subvol_rm_allowed`: https://github.com/moby/moby/pull/42253 · XFS reflink default: https://www.man7.org/linux/man-pages/man8/mkfs.xfs.8.html · OpenZFS block cloning: https://blog.linux-ng.de/2024/11/14/openzfs-and-the-state-of-block-cloning/
- Page cache: https://lkml.iu.edu/hypermail/linux/kernel/1605.2/02806.html · overlayfs: https://lwn.net/Articles/636943/ · user namespaces: https://manpages.debian.org/bookworm/manpages/user_namespaces.7.en.html
- udisks polkit: https://raw.githubusercontent.com/storaged-project/udisks/master/data/org.freedesktop.UDisks2.policy.in · loop direct-io: https://lwn.net/Articles/654701/ · https://www.phoronix.com/news/Linux-6.19-Faster-Loop-Block
- Hardlink hazards: https://gitlab.com/BuildStream/buildstream/-/issues/19 · https://pnpm.io/settings/node-modules
- Distro defaults: https://fedoraproject.org/wiki/Changes/BtrfsByDefault · https://en.opensuse.org/SDB:BTRFS · Ubuntu 26.04 install guide (ext4)

**Git**
- `git merge-tree`: https://git-scm.com/docs/git-merge-tree · fsmonitor: https://git-scm.com/docs/git-fsmonitor--daemon · git 2.55 (Linux fsmonitor): https://github.blog/open-source/git/highlights-from-git-2-55/ · git 2.50 (`worktree-prune`): https://github.blog/open-source/git/highlights-from-git-2-50/ · relative worktrees: https://github.com/libgit2/libgit2/issues/7210
- gitoxide: https://github.com/GitoxideLabs/gitoxide/blob/main/crate-status.md · https://github.com/Byron/gitoxide/discussions/978
- Mergiraf: https://mergiraf.org/ · https://lwn.net/Articles/1042355/ · weave: https://github.com/ataraxy-labs/weave
- jj: https://docs.jj-vcs.dev/latest/working-copy/ · https://docs.jj-vcs.dev/latest/git-compatibility/ · https://github.com/jj-vcs/jj/issues/8052
- Scalar: https://github.blog/open-source/git/the-story-of-scalar/

**Envelope**
- systemd: NEWS v252 (`Delegate=pids memory cpu` for `user@.service`) · systemd.resource-control(5) · https://wiki.archlinux.org/title/Cgroups
- Landlock: https://docs.kernel.org/userspace-api/landlock.html · https://docs.rs/landlock · sandbox-runtime: https://github.com/anthropics/sandbox-runtime · Claude Code sandboxing: https://code.claude.com/docs/en/sandboxing · Codex sandbox: https://simonwillison.net/2025/Nov/9/codex-sandbox-investigation/ · Bazel sandboxing: https://bazel.build/docs/sandboxing
- macOS: `taskpolicy(8)`: https://keith.github.io/xcode-man-pages/taskpolicy.8.html · XNU `bsd/man/man2/getpriority.2`, `bsd/kern/kern_resource.c`, `bsd/kern/kern_exec.c`, `bsd/kern/kern_memorystatus.c`, `bsd/kern/uipc_socket.c`, `osfmk/kern/task_policy.c`, `libsyscall/wrappers/spawn/spawn_private.h`, `libproc.h` · DYLD: https://theevilbit.github.io/posts/dyld_insert_libraries_dylib_injection_in_macos_osx_deep_dive/ · lo0 alias LaunchDaemon: https://gist.github.com/brandt/c2f9e8277c90a1c284770c7ca7966226 · Containerization: https://github.com/apple/containerization
- pasta/passt: https://passt.top/ · https://man.archlinux.org/man/passt.1.en
- Jobserver: GNU make manual, "POSIX Jobserver Interaction" · ninja 1.13.0: https://github.com/ninja-build/ninja/releases/tag/v1.13.0 · rust-lang/jobserver-rs · https://github.com/golang/go/issues/36868 · https://github.com/bazelbuild/bazel/issues/10443 · Yocto series: https://patchwork.yoctoproject.org/project/oe-core/cover/20240403070204.367470-1-martin@geanix.com/ · https://blogs.gentoo.org/mgorny/2025/11/30/one-jobserver-to-rule-them-all/ · https://codeberg.org/amonakov/guildmaster

**Research (arXiv unless noted)**
- CooperBench 2601.13295 · Co-Coder 2606.00953 · CAID 2603.21489 · STORM 2605.20563 · CoAgent 2606.15376 · S-Bus 2605.17076 · AgentRoom 2608.23740 · agent PR conflicts 2607.04697, AgenticFlict 2604.03551 · CodeMonkeys 2501.14723 · SWE-Replay 2601.22129 · R2E-Gym 2504.07164 · Glite ARF 2606.27416 · LastMerge 2507.19687 · MergirafSemi 2608.11345 · Merge-Bench 2605.25890 · LLM-judge merges 2607.27674 · Rover 2605.17279 · AgentCgroup 2602.09345 · Speculative Actions 2510.04371 · SPAgent 2511.20048 · Eg-walker 2409.14252 · conflict prediction: Alfayez et al., JSEP 2025, https://onlinelibrary.wiley.com/doi/10.1002/smr.70047
- Shared Rust builds: https://blog.howardjohn.info/posts/shared-rust-build/ · Cargo cross-workspace cache: https://rust-lang.github.io/rust-project-goals/2026/cargo-cross-workspace-cache.html · fast builds roadmap: https://rust-lang.github.io/rust-project-goals/2026/roadmap-fast-builds.html · build-dir layout v2: https://github.com/rust-lang/cargo/pull/17354 · Bazel disk cache: https://bazel.build/remote/caching · https://github.com/bazelbuild/bazel/issues/27913

**Ecosystems**
- cargo fingerprints: https://doc.rust-lang.org/beta/nightly-rustc/cargo/core/compiler/fingerprint/index.html · pnpm: https://github.com/pnpm/pnpm/issues/2335 · https://github.com/pnpm/pnpm/issues/12307 · https://pnpm.io/benchmarks · npm lock: https://docs.npmjs.com/cli/v11/configuring-npm/package-lock-json/ · Bun isolated: https://bun.sh/docs/pm/isolated-installs · webpack cache: https://webpack.js.org/configuration/cache/ · Vite: https://vite.dev/guide/dep-pre-bundling · TS 32023: https://github.com/microsoft/TypeScript/issues/32023 · Turborepo: https://turborepo.dev/docs/crafting-your-repository/caching · Nx: https://nx.dev/docs/reference/inputs · .NET: https://github.com/dotnet/sdk/issues/10046 · https://github.com/dotnet/project-system/issues/1538 · uv: https://github.com/astral-sh/uv/pull/5515 · https://github.com/astral-sh/uv/pull/11168 · CMake: https://gitlab.kitware.com/cmake/cmake/-/merge_requests/6143 · Go trimpath: https://github.com/golang/go/commit/8c8a881688ea386070ee9f6646b9ef7af52ad5ba · Gradle: https://docs.gradle.org/current/userguide/build_cache_concepts.html · Bazel output base: https://bazel.build/remote/output-directories · inotify default: https://github.com/torvalds/linux/commit/92890123749bafc317bbfacbe0a62ce08d78efb7
- gh extensions: https://github.com/cli/gh-extension-precompile · https://docs.github.com/en/github-cli/github-cli/creating-github-cli-extensions

**Local artefacts**: `/tmp/klon-bench.log` (ext4 copy calibration); `/tmp/klon-exp` (git experiments); `/tmp/klon-research` (envelope tests). Not committed.

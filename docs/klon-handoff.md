# klon — handoff document

`gh klon`: a `git worktree` replacement for developers running many coding agents (Codex, Claude Code, others) on one laptop. Linux + macOS. No VMs, no daemon, no setup wizard.

Date: 2026-09-03. Author of the design session: Piotr. This document is the full state of the design so a fresh session (Codex CLI) can start implementing without re-deriving anything.

---

## 0. TL;DR

- **Problem**: `git worktree add` materialises the *tracked* files (cheap, regenerable) and drops all *untracked* build state (`target/`, `node_modules/`, `bin/obj`, `.venv`, `.next` …) — the expensive thing. Every agent starts cold and every tree costs a full copy of the build cache.
- **Core idea**: clone the **directory as it is on disk** — tracked and untracked — with a copy-on-write filesystem clone (APFS `clonefile`, btrfs/XFS reflink), then register it as a normal git worktree. Sub-second spawn, warm build cache, disk cost = diff only.
- **Mental model**: *a klon is a folder that's a full copy of your project, already built, with its own localhost.* Folder / built / localhost. Every feature must serve one of those three words.
- **Shape**: a gh extension (`gh-klon` repo, Rust binary), verb-for-verb the same as `git worktree`, plus `--pr`, `sync`, `up`, `run`. Writes git's own worktree metadata so all existing tooling sees ordinary worktrees.
- **What klon writes itself** (~1.5–2k lines Rust): parallel reflink walk, two-tree update onto a cloned tree, worktree entry, jobserver fifo, envelope (env file + exec wrapper), path fixup, async delete, CLI. Everything else is linked (`gix`, `reflink-copy`, `ignore`, `rayon`) or shelled to (`git`, `gh`, `systemd-run`, `pasta`).
- **Name**: `klon` — Polish for *maple* **and** *clone*. A tree, and the tool clones trees. Repo `gh-klon`, command `gh klon`. (Check name availability: `gh extension search klon`.)

---

## 1. Goals and non-goals

### Goals
1. Drop-in worktree replacement: same verbs, same git metadata, same GitHub workflows (branches, push, PRs, hooks, CI). GitHub never learns anything changed.
2. Maximal performance and throughput for N concurrent agents on a modern laptop (12–16 cores, 32–64 GB, NVMe).
3. Lightweight: disk per tree ≈ diff from the source checkout; near-zero for idle trees.
4. Language-agnostic by construction (Rust, TS, C#, Go, Python, docs, polyglot monorepos, anything on GitHub). No per-ecosystem knowledge required for correctness.
5. Linux and macOS.

### Non-goals (stated so they stay out)
- No VMs, no kernel extensions, no FUSE/NFS virtual filesystem (see §9).
- No daemon. No background scheduler, observer, or eviction engine.
- No build configuration (linkers, debuginfo, cranelift) — the repo's business.
- No agent orchestration — tmux / Claude Squad / scripts do that. klon provides a directory + env.
- No auto-setup wizard. If the filesystem can't CoW, klon degrades (hardlink copy), never refuses.
- Windows: out of scope for v1 (ReFS/Dev Drive have block cloning; git-sprout supports it — possible later).

---

## 2. Prior art (research, 2026-09-03)

Two tools already occupy parts of this space. Study both before writing code.

### worktrunk — https://github.com/max-sixty/worktrunk (docs: https://worktrunk.dev)
- "A CLI for Git worktree management, designed for parallel AI agent workflows." Mature (v0.6x, on conda-forge, Homebrew). Rust.
- Core: `switch` / `list` / `remove`; hooks in `.worktrunk.toml` (`on_create`, `pre_merge`, `post_merge`); path templates; LLM commit messages; Claude Code plugin; agent-activity indicators in `list`.
- **`wt step copy-ignored`**: copies gitignored paths listed in a `.worktreeinclude` file (must also be gitignored) into the new worktree, using reflink with fallback. This is a *post-step that copies listed paths file-by-file*, not a whole-directory clone.
- Background removal runs under `taskpolicy -b` (macOS) / `nice -n 19` + `ionice -c 3` (Linux) — reuse this exact policy.
- **What to borrow**: UX conventions, `list` output, hooks concept, the delete-priority policy. Check its license before copying code.

### git-sprout — https://github.com/alltuner/git-sprout (MIT, v0.1.0, 2026-08-20)
- "Drop-in replacement for `git worktree add` that shares disk blocks instead of copying your tree. On the Linux kernel, 1816 MB becomes 36 MB."
- Clones the **tracked** files from an existing checkout via block clones (APFS `clonefile`, btrfs/XFS/bcachefs reflink, ReFS on Windows), verifies against the index, falls through to plain `git worktree add` on ext4/NTFS or when checkout filters (autocrlf, `text=auto eol=crlf`) would rewrite bytes.
- On macOS it calls `clonefile(2)` directly on directories ("copies a whole hierarchy in one call"); files go through `reflink-copy`.
- **What to borrow**: argument parsing of the `git worktree add` surface, the index-based verification approach, the filter-attribute guard. MIT, so code reuse is fine.

### What neither does (klon's actual novelty — be precise about it)
1. Clone the **entire directory including untracked/ignored build state** as one operation (one `clonefile` on macOS, one btrfs snapshot or one parallel reflink walk on Linux).
2. In-process git for the spawn path (no `git` subprocesses on the hot path).
3. A **resource/network envelope** per tree (cgroup or QoS clamp, jobserver, loopback IP, `TMPDIR`).
4. Hibernate/wake (idle tree → patch only).

---

## 3. The primitive: `add`

Five steps, target < 1 s total (~5 ms btrfs snapshot, ~100–300 ms APFS for 100k files).

```
gh klon add <branch>
  1. clone    golden → ../<repo>.wt/<branch>
                Linux:  btrfs subvolume snapshot if golden is a subvolume,
                        else parallel FICLONE walk (reflink-copy + rayon)
                macOS:  one clonefile(2) on the directory root
                fallback (ext4/NTFS): hardlink copy of files (cp -al style), warn once
  2. .git     write `gitdir:` pointer + $GIT_COMMON_DIR/worktrees/<name>/{HEAD,commondir,gitdir,index}
  3. index    copy golden's index verbatim; set worktree-local
              core.checkStat=minimal, core.untrackedCache=true, core.fsmonitor=true, index.version=4
  4. tree     two-tree update golden-tree → target-tree (touch only differing paths),
              after reverting golden's dirty paths (known from golden's index/fsmonitor)
  5. env      write .klon/env; allocate loopback slot; (Linux) prepare cgroup scope name
```

Key details:
- **Golden = your main checkout.** No separate golden tree, no refresh daemon. `gh klon up` (§6) keeps it fresh. It should stay on the base branch; all work happens in klons.
- **Why copy the index**: git's index stores inode/ctime/dev per entry; a clone changes inode and ctime, so a default git would re-hash every file on the first `git status`. With `core.checkStat=minimal` git compares only size and whole-second mtime, which CoW clones preserve → the copied index is valid instantly.
- **Why two-tree update instead of `git reset --hard`**: `reset --hard` lstat's every file (100k stats on a cold dcache). The clone is known to equal golden's tree, so apply only `diff(golden-tree, target-tree)` — dozens of files for a typical branch. Implement with `gix` (tree diff + entry write + index patch). This is the *only* git logic klon owns (~200 lines).
- **Branch resolution (git DWIM)**: local branch → use; else `origin/<name>` exists → tracking branch from it (targeted `git fetch origin <name>`); else new branch from base. Explicit forms: `origin/<name>`, `--pr <n>` (fetches `refs/pull/<n>/head`, works for forks), `--issue <n>` (branch name from issue title).
- **Path fixup** (language-agnostic relocation): fixed-string search for golden's absolute path over gitignored dirs (`ignore` + `grep-searcher` crates), rewrite hits in **text files only**. Covers `.NET obj/project.assets.json`, `CMakeCache.txt`, `pyvenv.cfg`, `.bin` shims. Binaries left alone (debuginfo paths are cosmetic). A klon's path is stable for its lifetime (wake re-clones to the same path) so whatever isn't rewritten is paid once.
- **`.klonignore`**: paths *not* to clone (huge datasets, logs, nested `.git`s). Inverse of worktrunk's include list — klon clones everything by default.
- Worktree-local config requires `extensions.worktreeConfig=true` in the common repo (one-time, on first `add`; document it).

---

## 4. CLI surface

```
gh klon add <branch>                  # ≡ git worktree add, instant + warm; DWIM branch resolution
gh klon add origin/<branch>           # explicit remote-tracking
gh klon add --pr 123                  # PR (incl. forks) into a warm tree
gh klon add --issue 45                # branch from issue title
gh klon add <branch> -- <cmd...>      # spawn + run a command inside the envelope (e.g. codex "...")
gh klon list                          # path · branch · disk delta · live procs · PR # · checks
gh klon rm <branch> [--merged]        # ≡ git worktree remove (+ branch); async delete
gh klon prune
gh klon pr <branch>                   # gh pr create from that tree
gh klon sync <branch> [--merge|--onto main|--fresh|--all]
gh klon run <branch> -- <cmd...>      # exec inside envelope
gh klon shell <branch>
gh klon up                            # update golden (main): fetch, ff-only, optional [warm] steps
gh klon hibernate <branch> / wake <branch>
```

- Path defaults to a sibling dir, worktree convention: `../<repo>.wt/<branch>`. `--path` for parity.
- `git worktree list/remove/prune` keep working on klons (same metadata).
- `sync`: fetch (once for the common dir); fast-forward if no local divergence; else `rebase --autostash` (or `--merge`); detect force-push via `merge-base --is-ancestor` and refuse unless no unique local commits or `--force`. `--fresh` = `rm` + `add` + switch branch in (generic "rebase as respawn": only the branch's own crates recompile). `--onto main` also rebases onto base.
- `hibernate`: stash tracked changes to `refs/klon/<name>` (+ untracked *non-ignored* files), delete the folder. `wake` = `add` + apply. A hibernated klon is KBs. Over `disk_budget`, `add` hibernates the LRU klon first (lazy, at `add` time; no daemon).
- `list` data: disk delta (du of clone minus shared — or just "bytes unique"), live processes (from cgroup / process group), PR + checks via `gh api`.

Packaging: repo `gh-klon`, executable `gh-klon`; precompiled release assets named `gh-klon_v<ver>_<os>-<arch>`; `gh extension create --precompiled=other` scaffold; `gh extension install <you>/gh-klon`. Token via `gh auth token`; PR-shaped things via `gh api`.

---

## 5. The envelope

`.klon/env` in each tree:

```
KLON_NAME=<branch>
KLON_IP=127.0.0.N          # per-klon loopback address (slot N)
HOST=127.0.0.N             # env-contract fallback for tools that honor it
TMPDIR=<tree>/.klon/tmp
MAKEFLAGS=--jobserver-auth=fifo:<XDG_RUNTIME_DIR or ~/.klon>/jobserver
```

`gh klon run` sources it and joins the resource group. `HOME` stays shared on purpose (that's where per-user caches live: cargo registry, pnpm store, NuGet, GOCACHE, uv cache).

### CPU budget: GNU jobserver (reimplement server, ~30 lines)
- A fifo containing `nproc` single-byte tokens; export `MAKEFLAGS=--jobserver-auth=fifo:PATH`. Machine-wide, no daemon (the fifo is filled once; klon can top it up / drain it on demand).
- Clients that self-limit automatically: cargo, rustc, GNU make, **ninja ≥ 1.13** (auto-joins a GNU Make jobserver as client; fifo style requires make 4.4 semantics, which is just the protocol) → CMake builds too. Yocto runs exactly this as a machine-wide shared jobserver service (precedent).
- Tools that don't speak it (node, dotnet, gradle, jest) get the kernel's fair share via the resource group below. Oversubscription costs ~10–20 % in context switching — acceptable; the jobserver claws it back for the tools that matter most.

### Resource group
- **Linux**: `systemd-run --user --scope -p MemoryHigh=<x> [-p CPUWeight=<w>]` per klon. As an unprivileged user, **memory and pids controllers work with no setup**; **cpu and io require delegation** (`/etc/systemd/system/user@<uid>.service.d/delegate.conf` → `[Service] Delegate=cpu cpuset io`). Without delegation, use `nice`. The whole process tree an agent spawns inherits the scope — including the agent's own `bash` tool calls.
- **macOS**: no cgroups. In-process libc calls (no shelling to `taskpolicy`):
  - QoS clamp **utility** for agent trees (`taskpolicy -c utility` equivalent).
  - **Do not** use `PRIO_DARWIN_BG` (`taskpolicy -b`) for agent trees: it throttles network I/O for sockets opened afterwards → agent API calls suffer. Use it only for background deletes.
  - Jetsam priority (`taskpolicy -j <pri>` equivalent) is the one real memory lever: klon processes get lower jetsam priority so memory pressure kills a build before the editor. Verify API availability from user space.
  - Process group per klon so `gh klon stop` can kill everything.

### Network identity ("its own localhost")
- Design: each klon gets a loopback IP `127.0.0.N`; **same ports in every klon** (3000 is 3000 everywhere); open `http://127.0.0.7:3000` or `http://<branch>.klon:3000` with a hosts entry.
- **Linux**: `pasta` (passt package, in Ubuntu; Podman rootless uses it by default). Rootless network namespace with egress: `pasta --config-net -t 127.0.0.N/auto -- <cmd>` forwards host `127.0.0.N:<port>` to the same port inside the namespace; agents keep internet access. All of `127/8` is on `lo` natively. Shell to it; nothing to reimplement. Optional (`run --netns`).
- **macOS**: **no generic mechanism.** The DYLD interposer idea (`bind()` hook via `DYLD_INSERT_LIBRARIES`) is dead: modern macOS strips `DYLD_*` variables for hardened-runtime binaries and library validation refuses inserted dylibs; official node/dotnet builds are notarized → hardened. Decision: env contract only (`HOST`, `PORT`, `KLON_IP`). Loopback aliases (`sudo ifconfig lo0 alias 127.0.0.N`) are optional and user-managed; without them ports collide exactly as with worktrees today (no regression, no setup demanded).
- Services: state lives in the tree. If golden contains a seeded `pgdata/`, every klon clones a pre-seeded Postgres in zero time; with a per-klon IP it's isolated automatically. Shared-stack alternative: one Postgres, `CREATE DATABASE <klon> TEMPLATE golden`, exported as `DATABASE_URL`.

### Delete
Rename into `../<repo>.wt/.trash/` (instant) → background delete under `PRIO_DARWIN_BG` (macOS: `nice` only touches CPU on Darwin; `taskpolicy -b` also throttles disk) / `nice -n 19` + `ionice -c 3` (Linux). btrfs: subvolume delete ioctl is O(1) (needs `user_subvol_rm_allowed` for rootless `rm -rf` compatibility — otherwise prefer reflink walk over snapshots).

---

## 6. `gh klon up` (golden refresh, on demand)

In the main checkout: `git fetch origin` → `git merge --ff-only` → optional `[warm]` steps from `.klon.toml` run under the jobserver. Refuses if main is dirty or not on the base branch. Same contract as `git pull` today, no daemon. Golden is as fresh as your last `up`; new klons clone whatever state it has.

---

## 7. Config (`.klon.toml`, all optional)

```toml
base = "main"                       # golden branch
path = "../{repo}.wt/{branch}"      # path template
disk_budget = "40G"                 # LRU-hibernate above this (checked at `add`)

[warm]                              # run by `gh klon up` after ff
steps = ["cargo build", "cargo nextest run --no-run", "pnpm install --frozen-lockfile"]

[hardlink]                          # v2, opt-in: paths hardlinked instead of cloned (page-cache sharing)
paths = ["target/debug/deps", "target/debug/.fingerprint"]
```

`.klonignore` (gitignore syntax): paths excluded from the clone.

---

## 8. Throughput model (why this is fast under N trees)

- **CPU**: kernel fair-share (cgroup / QoS) + jobserver for jobserver-aware tools. No userspace scheduler.
- **RAM — the real ceiling**: CoW shares *disk blocks, not page cache* (cache is keyed per inode). N clones read the same dependency artifacts N times and hold N copies. Mitigations:
  - v2 opt-in **hardlink split** for content-addressed artifacts whose filename contains the input hash (registry crates, pnpm store, Go build cache): one inode → one page-cache copy across all trees. Unsafe for anything rewritten in place; the repo declares paths, klon never guesses.
  - Ecosystems that already share per user need nothing: pnpm (hardlinked store), NuGet (`~/.nuget/packages`), Go (`GOCACHE`), Gradle build cache, ccache/sccache. Rust and npm are the outliers with per-tree everything — which is why the pain was felt in Rust.
  - `MemoryHigh` per tree on Linux prevents one runaway build from evicting everyone's page cache; macOS has no equivalent → fewer concurrent trees there.
- **Git**: one object store → one fetch serves all trees; `git maintenance` (commit-graph, multi-pack-index) keeps status/log fast across dozens of worktrees.
- **Ballpark** (16 cores / 64 GB / NVMe, Rust monorepo): worktrees today — three agents building thrash (3× page cache, cold builds). klon with hardlinked deps + jobserver — five or six agents building concurrently at ~3 cores each, no thrash, `add` < 1 s, disk a few hundred MB per tree.

---

## 9. Rejected designs (and why) — don't re-litigate without new data

| Rejected | Why |
|---|---|
| Virtual filesystem (FUSE / NFS loopback / EdenFS-style lazy materialisation) | Daemon in the hot path of every `stat`; cargo stats the whole tree per invocation; macOS without kext = NFS loopback; payoff only at millions of files. Below a few hundred thousand files a CoW clone is cheaper and compatible with every tool. Revisit only for Meta-sized repos. |
| Reimplementing git's object store | 100 % cost, 0 % gain; it *is* GitHub compatibility. Keep git formats (index v4, worktree dir, `.git` pointer). |
| Separate golden tree + refresh daemon | Main checkout is golden; `gh klon up` replaces the daemon. |
| Userspace governor (slots, SRPT, RSS models, priority jobserver) | Kernel fair-share + jobserver covers it; per-command slots can't catch an agent's own `bash` calls, a cgroup/QoS on the process tree can. |
| Observation layer (fanotify/FSEvents write attribution, learned immutability, cost tables) | Existed to rank artifacts for partial eviction; whole-klon LRU hibernate makes it unnecessary. |
| Three-state eviction (awake / dozing / hibernating), GreedyDual-Size | Two states suffice. |
| Per-ecosystem unit DAG, `[ports]` templates, `depends_on` | Same-port-everywhere localhost removes port templating; tools' own incrementality removes affected-set computation. Optional precision for later, never required. |
| Padded paths (conda trick) for binary relocation | Text rewrite covers ~95 %; rest is debuginfo. |
| macOS `bind()` interposer | Hardened runtime strips `DYLD_*`; notarized node/dotnet are hardened. |
| Auto-follow, recorded command replay, CI-inferred recipes | Manual `sync`, optional `[warm]` list. |
| Hardlinking everything with read-only bit | Cargo rewrites fingerprint files in place → EACCES → build fails. Hardlink only declared content-addressed paths (v2). |

---

## 10. Theory notes (for context; not implementation guidance)

- **Information floor**: build artifacts are a deterministic function of sources (given reproducible builds), so K(build | src, toolchain) = O(1). The minimum state per idle tree is the compressed source patch (KBs) — which is what `hibernate` stores. N×diff (block CoW, 100 MB–GB) is a time-space trade, not information.
- **Time-space frontier**: pebbling on the build DAG; practical policy would be "keep `.rmeta`, drop codegen" — deliberately not implemented (v1 hibernates whole trees).
- **Below block CoW without recompute**: content-defined-chunk dedup across agents (near-identical downstream rlibs) — requires a content-addressed store, i.e. leaving "real files on disk". Not for klon.
- **Physics**: Landauer says holding bits is free, erasing costs; recompute is ~10⁸× above the thermodynamic floor. Physics favours caching; economics (disk) doesn't. Irrelevant to engineering choices.
- **Metadata cost**: N×diff is really N×(diff + inode table). APFS `clonefile` duplicates inodes (~100 B × files); btrfs snapshot is O(1) (shared B-tree). Prefer snapshots on Linux when the subvolume is yours.

---

## 11. Component decision table (research-backed)

| Component | Native tool | Smart part | Decision |
|---|---|---|---|
| CoW clone | APFS `clonefile(2)`; Linux `FICLONE` | one clonefile on the root dir on macOS; per-file reflink walk elsewhere | link `reflink-copy`; write parallel walk (rayon); optional btrfs snapshot ioctl |
| Git plumbing | gitoxide (`gix`) | plumbing covers index mutation, status, tree diff, low-level checkout; **missing**: checkout/switch/reset orchestration, merge, rebase | link `gix` for hot path; **reimplement** two-tree update onto cloned tree; shell `git` for fetch/rebase/merge/push |
| Index validity | `core.checkStat=minimal`, builtin fsmonitor (macOS + Linux via inotify, since git ≥ 2.4x; ~8192 default inotify watches/user) | copied index valid instantly; status O(changes) | reuse; set in worktree-local config |
| CPU budget | GNU jobserver protocol | fifo with N tokens; clients cargo/rustc/make/ninja ≥ 1.13 | **reimplement server** (~30 lines) |
| Envelope, Linux | `systemd-run --user --scope`, cgroup v2 | memory/pids need no setup; cpu/io need `Delegate=` drop-in | shell to `systemd-run`; `nice` fallback |
| Envelope, macOS | `setpriority` / `setiopolicy_np` / QoS clamp / jetsam pri | utility clamp for agents; `PRIO_DARWIN_BG` only for deletes (throttles network) | **reimplement in-process** (libc calls) |
| Network, Linux | `pasta` | `-t 127.0.0.N/auto` maps a host loopback address to the namespace; egress via `--config-net` | shell to `pasta` (optional flag) |
| Network, macOS | none | interposer defeated by hardened runtime | env contract only |
| Delete | rename + bg rm | worktrunk's priority policy | reuse pattern in-process |
| Path fixup | ripgrep libs (`ignore`, `grep-searcher`) | fixed-string search over ignored dirs, rewrite text hits | link; ~80 lines |
| GitHub | `gh` | auth, `gh api`, PR refs | shell to `gh` |

---

## 12. Ecosystem notes (for the relocation test and docs, not for code)

| Ecosystem | Already shared per user | Per tree | Relocation gotcha |
|---|---|---|---|
| Rust | registry sources | `target/` | none; cargo hashes are workspace-relative. Incremental caches embed paths → first edit after move compiles that crate non-incrementally once |
| TS/Node (pnpm) | pnpm store (hardlinks) | symlink farm (~free), `.next`, `dist`, `.tsbuildinfo` | none (relative links, relative tsbuildinfo) |
| TS/Node (npm) | nothing | full `node_modules` | fine; no page-cache sharing (recommend pnpm) |
| Turborepo / Nx | local content-keyed cache | outputs | point `TURBO_CACHE_DIR` at one shared dir → klons share task results |
| C# / .NET | NuGet `~/.nuget/packages`, Roslyn server | `bin/`, `obj/` | **`obj/project.assets.json` absolute paths** → path fixup, else re-restore (cheap) |
| Go | `GOCACHE` | ~nothing | none |
| Python (uv) | uv cache | `.venv` | `uv venv --relocatable`; plain venvs break silently → path fixup |
| C/C++ (CMake) | ccache | build dir | **`CMakeCache.txt` absolute** → path fixup / reconfigure |
| JVM (Gradle) | build cache, deps | `build/` | fine; one JVM daemon per active tree (RAM) |
| Docs (mkdocs/docusaurus/mdbook) | — | `site/`, `build/` | none |

Language-agnostic facts: toolchain pins (`rust-toolchain.toml`, `.nvmrc`, `global.json`) travel with the tree; installs are per-user, side-by-side. Docker image builds share BuildKit's layer cache per daemon. `.env` files clone with the tree (override `PORT`/`HOST`/`DATABASE_URL` via env contract). Linux inotify limits: 20 klons × 100k files exhausts defaults — document `fs.inotify.max_user_watches` / `max_user_instances`. Editors/LSPs are the real RAM ceiling on "many klons" (1–3 GB per window); `list` should show RSS per tree; never hibernate a tree with live processes.

---

## 13. Open questions

**Filesystem / clone**
1. Does APFS `clonefile(2)` preserve mtime on all entries? (Believed yes — `CLONE_NOOWNERCOPY` implies metadata copied by default — but *verify*; cargo freshness and `core.checkStat=minimal` both depend on it.)
2. Linux default: btrfs snapshot (O(1), but `rm -rf` needs `user_subvol_rm_allowed`) vs. reflink walk (O(files), universally removable). Proposal: walk by default, snapshot when golden is a subvolume *and* the mount allows rootless delete.
3. How fast is the parallel FICLONE walk on XFS/btrfs for 100k–500k files? Benchmark; rayon over directory entries.
4. ext4 fallback: hardlink copy (`cp -al` semantics) — any tool writing in place then corrupts golden. Is a loud warning enough, or should ext4 fall back to plain `git worktree add` (as git-sprout does) plus a copy of gitignored dirs?
5. Nested git repos / submodules inside the clone: exclude via `.klonignore` by default? Sprout ignores submodules; decide.
6. Symlinks inside the tree pointing at golden's absolute path (e.g. `node_modules/.bin` in some setups): relink or leave? Path fixup could rewrite symlink targets too.

**Git**
7. `gix` maturity for writing a worktree entry and patching the index: prototype; fallback is `git worktree add --no-checkout --detach <tmp>` + move `.git` file + `git worktree repair` + `git reset -q` (mixed).
8. Two-tree update: exact semantics when golden is dirty (revert dirty paths first, then diff), when the target tree deletes a directory that has ignored content inside (keep ignored content), and for files with checkout filters/LFS (sprout guards on attributes — replicate).
9. Is `core.checkStat=minimal` enough, or do we also need `core.trustctime=false` / racy-git handling for sub-second mtimes? (Test: clone, immediate `git status` must be clean and instant.)
10. `extensions.worktreeConfig=true` must be set in the common repo — acceptable one-time change? Alternative: pass config via `GIT_CONFIG_*` env in `.klon/env` only (then plain `git` outside `run` wouldn't get it).
11. `hibernate`: stash-object approach vs. a real commit on a hidden ref; include untracked non-ignored files (`git stash -u` semantics); confirm ignored build state is dropped by design.

**Envelope**
12. Jetsam-priority API from user space on macOS (`taskpolicy -j` exists; which syscall/sysctl, and does it need entitlement?).
13. QoS clamp from a non-Apple binary: `pthread_set_qos_class_self_np` / `setpriority(PRIO_DARWIN_PROCESS, …)`? Confirm which call sets a *process-wide* clamp inherited by children.
14. Linux: ship the `Delegate=cpu cpuset io` drop-in as documentation only (never auto-write to `/etc`). Confirm `systemd-run --user --scope` works on Ubuntu 24.04/26.04 out of the box for `MemoryHigh`.
15. Jobserver: what happens when a tool inherits `MAKEFLAGS` but the fifo has no free tokens and the user runs a build manually outside `run`? (It blocks — document; consider `KLON_NO_JOBSERVER=1` escape hatch.) Token count policy: `nproc` vs `nproc - 2` to keep the editor responsive.
16. `pasta` availability across distros (Ubuntu has `passt`; Fedora yes; Arch yes) and minimum kernel version for address-scoped forwarding (man page says "since Linux 5.7" for interface-scoped binds).
17. macOS loopback aliases: leave entirely to the user (documented one-liner + optional launchd plist in README) — confirm this is acceptable UX, or offer `gh klon lo0` that prints the exact sudo command.

**Product**
18. Name availability: `gh extension search klon`; repo `gh-klon`; possible confusion with `gh repo clone` (accepted; subcommands are worktree-shaped).
19. worktrunk license — for borrowing UX/code. (git-sprout is MIT.)
20. Should `add -- <cmd>` open in a tmux window / pane when tmux is present, or just exec? (Claude Squad integration implies tmux.) Keep klon orchestration-free; but `run` printing the tree path + env for tmux scripts is cheap.
21. `list`: PR/checks via `gh api` on every call may be slow / rate-limited; cache with short TTL in `.klon/`?
22. `--only <dirs>` (sparse-checkout cone for focused klons to reduce editor/LSP/inotify load) — v2?
23. `disk_budget` semantics: measure unique bytes per clone (requires fs-specific accounting: `btrfs fi du -s`, APFS has no cheap equivalent) or just count klons? Simplest v1: LRU by count + total `du` of trash.

---

## 14. Verification plan (must pass before calling v1 done)

1. **Zero-compile test**: `gh klon add x` from a built main → `cargo build` / `pnpm build` / `dotnet build` in the klon compiles nothing (0 units). Run on a Rust workspace, a pnpm workspace, a .NET solution.
2. **Relocation test**: clone golden to a random path; `cargo build` → 0 "Compiling"; `pnpm install --frozen-lockfile --offline` → no-op; `dotnet build` → no restore after path fixup.
3. **Instant status**: `git status` in a fresh klon returns clean in < 50 ms with a warm index (checkStat=minimal), < 1 s cold.
4. **Spawn latency**: `add` < 1 s on 100k files, APFS and btrfs; report per-step timings.
5. **Worktree compatibility**: `git worktree list` shows klons; `git worktree remove` works; hooks fire; `gh pr create` from inside a klon works; CI unchanged.
6. **Envelope**: N klons building concurrently → total rustc processes ≤ jobserver tokens; `MemoryHigh` enforced on Linux; agent API calls not throttled on macOS (utility clamp, not BG).
7. **Delete**: `rm` returns < 10 ms; trash drained in background at low priority.
8. **Fallback**: on ext4 `add` still works (hardlink copy) with a single warning line.

---

## 15. Suggested build order

1. `add` pipeline (clone + worktree entry + index copy + two-tree update) with `git` subprocess fallback; `rm` with async delete; `list`. → already a usable worktree replacement.
2. Envelope: env file, jobserver fifo, `run`/`shell`, Linux `systemd-run`, macOS QoS clamp + process group.
3. `sync` (ff/rebase/--fresh), `up`, `--pr`, `--issue`, `pr`, `rm --merged`.
4. Path fixup, `.klonignore`, `hibernate`/`wake`, `disk_budget`.
5. Linux `pasta` integration behind `run --netns`.
6. v2: hardlink split, `--only`, btrfs snapshot backend, Windows.

---

## 16. References

- git-sprout: https://github.com/alltuner/git-sprout (MIT; APFS/btrfs/XFS/bcachefs/ReFS block clones for tracked files)
- worktrunk: https://github.com/max-sixty/worktrunk · https://worktrunk.dev (`.worktreeinclude`, hooks, delete priority policy in `src/priority.rs`)
- reflink-copy crate: https://docs.rs/reflink-copy (FICLONE on Linux, `clonefile` on macOS; `create_new` semantics); clonetree crate (single `clonefile` call strategy on macOS): https://github.com/cortesi/clonetree
- gitoxide status: https://github.com/GitoxideLabs/gitoxide/blob/main/crate-status.md (plumbing: index mutation, status, tree diff, low-level checkout; missing: reset/switch orchestration, merge, rebase)
- ninja 1.13 jobserver client: https://github.com/ninja-build/ninja/releases/tag/v1.13.0
- Yocto shared jobserver precedent: https://patchwork.yoctoproject.org/project/oe-core/cover/20240403070204.367470-1-martin@geanix.com/
- cgroup v2 as unprivileged user (memory/pids ok; cpu/io need delegation): https://wiki.archlinux.org/title/Cgroups ; systemd.resource-control(5)
- macOS `taskpolicy(8)`: QoS clamp `-c`, `-b` (PRIO_DARWIN_BG throttles CPU+disk+network), `-j` jetsam priority: https://keith.github.io/xcode-man-pages/taskpolicy.8.html
- pasta/passt: https://passt.top/ ; man page with `-t 127.0.0.6/all:…` address-scoped forwarding: https://man.archlinux.org/man/passt.1.en
- DYLD_INSERT_LIBRARIES vs hardened runtime: https://theevilbit.github.io/posts/dyld_insert_libraries_dylib_injection_in_macos_osx_deep_dive/ ; https://www.macinternals.app/en/blog/dyld-in-depth
- git builtin fsmonitor (macOS + Linux/inotify caveats): https://man.archlinux.org/man/git-fsmonitor--daemon.1.en
- Cargo cross-workspace cache project goal (2026; MVP caches only immutable registry/git crates, not path crates): https://rust-lang.github.io/rust-project-goals/2026/cargo-cross-workspace-cache.html ; build-dir stabilised in cargo 1.91; new build-dir layout call for testing (2026-03): https://blog.rust-lang.org/2026/03/13/call-for-testing-build-dir-layout-v2
- Shared Rust build cache experiments (sccache keys on cwd; hardlinks for deps): https://blog.howardjohn.info/posts/shared-rust-build/

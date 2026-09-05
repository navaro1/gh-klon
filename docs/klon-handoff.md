# klon: handoff document

Revision 3, 2026-09-03. Status: Ready for the specification and the tickets.

`gh klon` is a `git worktree` replacement for a developer who runs many coding agents on one laptop. It runs on Linux and macOS. It has no daemon, no virtual machine, no FUSE, and no `sudo` on the default path.

This revision reconciles two same-day proposals:

| Record | Content | Location |
|---|---|---|
| Revision 2 (PR #1) | Research pass: competitors, filesystems, git plumbing, envelope, literature, ecosystems. Metrics M1 to M14. | `docs/klon-research-2026-09-03.md` |
| Evidence-gated proposal (PR #2) | Correctness-first architecture: Git as the oracle, transactions, immutable generations, claims, receipts. | `docs/proposals/2026-09-03-evidence-gated-workspaces.md` and `docs/klon-evidence.md` |

Where this document and a record differ, this document wins. The records stay as the source of the measurements and the citations.

Claims carry a label. **V** = verified with a source or a local experiment. **B** = believed; a test must confirm it.

---

## 0. Summary

- **Problem.** `git worktree add` copies the tracked files and drops the ignored build state (`target/`, `node_modules/`, `obj/`, `.venv`, `.next`). Each agent starts cold. Agents also share ports, hooks, and RAM with no fence.
- **Core idea.** Clone the directory as it is on disk, tracked and ignored, with a copy-on-write (CoW) primitive. Register the clone as an ordinary linked worktree. Wrap it in an envelope: a write fence, a memory scope, build slots, and its own loopback address.
- **Mental model.** A klon is a folder that is a full copy of your project, already built, with its own localhost, that cannot hurt its neighbours, and that tells you whether it merges. Folder, built, localhost, fenced, mergeable. Each feature must serve one of these five words.
- **What is new against the competitors.** Whole-directory CoW spawn with linked-worktree registration on Linux and macOS. O(1) spawn through a snapshot or a hot spare. A measured ext4 strategy. A per-tree envelope. A merge radar.
- **Shape.** A `gh` extension. One Rust binary. The same verbs as `git worktree`, plus `up`, `run`, `sync`, `merge`, `init`, `doctor`, and `bench`.
- **Priority order.** Correctness first. Time from spawn to a warm build second. Portability third; ext4 is first-class. Resource guarantees fourth. Raw spawn latency fifth. A klon that reports wrong git state is worse than a slow klon.

---

## 1. Goals and non-goals

### Goals
1. Drop-in worktree replacement. Same verbs, same git metadata, same GitHub workflows. GitHub sees nothing new.
2. Warm from the first second. The first build in a klon compiles zero units when golden is built (M3).
3. O(1) spawn from the user's view (M1). A snapshot where the filesystem gives one. A hot spare everywhere else.
4. Light on disk. Disk per tree is about the diff. An idle tree costs kilobytes when hibernated (M5).
5. Language-agnostic for correctness. Ecosystem knowledge only improves warmth.
6. Linux first, macOS last. Fully operational on btrfs, XFS, and ext4 from the first release. macOS runs from the first release with the `copy` backend and a degraded envelope; the APFS backend, the Seatbelt fence, and the QoS scope land in the last milestone. Zero `sudo` prompts on the default path (M13).
7. Certainty. An agent in one klon cannot write outside it. A klon's hooks cannot touch golden or a sibling. The developer sees conflicts before a merge (M7, M8, M11).

### Non-goals
- No virtual machines, kernel extensions, FUSE, or NFS.
- No daemon. Background work is a detached child process that one command starts.
- No build configuration. That belongs to the repository.
- No agent orchestration. tmux, Claude Code, t3 code, and scripts do that. klon gives a directory, an env file, and `--json`.
- No setup wizard. `gh klon init` is one explicit optional command.
- No requirement for administrator access, systemd, a network namespace, or a specific filesystem. klon uses each of these when the host has it. It degrades with a message when the host does not.
- No full proof-receipt system in v1. See §6 and §12.
- No Windows in v1.

---

## 2. Reconciliation decisions

The two proposals agreed on these points: Apple discourages a directory `clonefile`; `FICLONE` clones one file, and a walk is not a snapshot; a writable hardlink fallback is unsafe; published competitor timings need local reproduction; `git checkout` beats `read-tree -m -u`; gitoxide leaves the hot path.

They disagreed on the points below. The table gives the decision.

| Topic | Revision 2 (PR #1) | Evidence-gated proposal (PR #2) | Decision | Why |
|---|---|---|---|---|
| Who writes the worktree metadata | klon writes 3 files by hand (about 1 ms) | The installed Git creates the worktree | **Git creates the admin entry.** `git worktree add --no-checkout --detach --lock` on an empty path. klon then replaces the working directory content and rewrites the `.git` file. | The cost is 10 ms. klon holds zero format knowledge. `list`, `remove`, `prune`, `repair`, reftable, and relative paths keep working when Git changes. |
| Default clone source | golden (the main checkout), possibly dirty | An immutable, verified generation | **v0: golden through a hot spare, with a tear check. v0.2: `warm` generations as an opt-in source.** | A snapshot is atomic. A walk is not. The spare is made when golden is quiet. After a clone, `git status` plus `git checkout -- <paths>` restores each tracked file exactly. A torn ignored file costs one rebuild of that unit. |
| Default copy backend | CoW clone; `copy` only on ext4 | Regular byte copy; clone adapters off until evidence | **The CoW clone is the product.** `copy` stays as the universal fallback. Every backend must pass one manifest-equality test before `doctor` selects it. | The test is adopted. The default is not. A tool that copies 4 GB in 95 s is not the product. |
| Transactions and repair | Not covered | A marker journal for every state change; `doctor` repairs | **Adopted.** `add` and `rm` write a journal under the common git directory. A repeated command reaches the prior state or the completed state. | Crashed agents leave half-trees (pain point 8). The journal is cheap to build. |
| Repository-supplied commands | `.klon.toml` `[warm] steps` run by `up` | A repository policy needs consent per content hash | **Adopted.** Command-bearing keys in `.klon.toml` need one approval per content hash. klon stores the approval in `~/.config/klon/approvals.toml`. | A repository can be untrusted. worktrunk uses the same pattern. |
| Envelope (fence, scope, jobserver, loopback) | A core feature, on by default under `run` | Outside the portable core | **A core feature.** Each part degrades with a message when the host lacks it. | Pain points 3, 5, and 7 are the product reason. klon does not *require* these host features. klon *uses* them. |
| Claims (owned paths) | v0.1, a file under `.klon/` | An atomic SQLite ledger with collision keys | **v0.2, one JSON file under the common git directory with `flock`.** | One laptop. A file lock gives the atomic overlap check. SQLite is a later option if a test shows a need. |
| Receipts, `ready`, `verify` | `merge` with a `pre_merge` test gate | A content-bound receipt from a private proof worktree | **v0.2 light receipt.** `gh klon check` runs `[proof] steps` at a clean HEAD and records the commit, tree, steps, status, and duration. `merge` needs a fresh receipt unless the user passes `--no-check`. The private proof worktree and the execution manifest are a v2 candidate. | The test gate is the load-bearing part (CAID, Glite). The full manifest machinery has no demand evidence yet. |
| Benchmark rigor | `gh klon bench` prints a table | 100 warm and 100 cold runs per cell, bootstrap intervals | **Adopted at a smaller scale.** A versioned manifest, seeded fixtures, raw samples, p50 and p95, an environment record. 10 warm and 5 cold runs for development. 30 and 10 for a release claim. A correctness mismatch voids the timing. | The rigor is what a `/goal` session needs as its verifier. The counts fit a laptop. |
| Path fixup | A generic text rewrite of golden's path in ignored directories | Never rewrite an unknown artifact | **A generic rewrite with rails.** Text files only, at most 1 MB, valid UTF-8, not in a skip list (`.db`, `.sqlite*`, `.pack`, `.bin`, `.o`, `.a`, `.so`, `.dylib`, `.class`, `.pyc`, `.wasm`). klon logs each rewrite. `--no-fixup` and `[fixup] skip` exist. | Language-agnostic relocation is a goal. The rails remove the known hazards. |
| Support floor | git ≥ 2.38 for `merge-tree --write-tree` | git ≥ 2.34.1, macOS 13+, kernel 5.15+ | **Core: git ≥ 2.34.1, macOS 13+, Linux kernel ≥ 5.15. The radar uses `merge-tree --write-tree` on git ≥ 2.38 and the legacy three-argument form below it (**V**, §11).** | Ubuntu 22.04 ships git 2.34.1 and is the development laptop. |
| Writing style | Dense notes | ASD-STE100 | **ASD-STE100 for this document and the specification.** The records keep their original style. | The owner reads in STE. |

---

## 3. Product shape

### Command surface

| Command | Meaning |
|---|---|
| `gh klon add <branch>` | Same as `git worktree add`. O(1) when a spare or a snapshot exists. Warm. Branch resolution follows git DWIM. |
| `gh klon add origin/<branch>` | An explicit remote-tracking branch. |
| `gh klon add --pr <n>` | A pull request, forks included, from `refs/pull/<n>/head`. |
| `gh klon add --issue <n>` | A new branch named from the issue title. |
| `gh klon add <branch> -- <cmd...>` | Spawn, then run a command inside the envelope. |
| `gh klon list [--json]` | Path, branch, disk delta, RSS, live processes, PR number, checks, vs-base, vs-siblings, behind. |
| `gh klon rm (<branch> \| --path <p>) [--merged] [--delete-branch] [--force]` | Same as `git worktree remove`. Deletes the branch only with `--merged` or `--delete-branch`. Async delete. Refuses a dirty tree or a tree with live processes without `--force`. |
| `gh klon prune` | Same as `git worktree prune`, plus journal cleanup. |
| `gh klon pr <branch>` | `gh pr create` from that tree. |
| `gh klon sync <branch> [--merge\|--onto <base>\|--fresh\|--all\|--check]` | Fetch, then fast-forward or rebase. `--check` is a dry run through `merge-tree`. |
| `gh klon merge <branch>` | Fetch, `pre_merge` hook, structured merge, fast-forward base, remove. Never pushes. |
| `gh klon check <branch>` | v0.2. Run `[proof] steps` at a clean HEAD and record a receipt. |
| `gh klon claim <branch> <paths...>` | v0.2. Record owned paths. `list` flags overlaps. |
| `gh klon run <branch> -- <cmd...>` | Execute inside the envelope: fence, scope, env. |
| `gh klon shell <branch>` | An interactive shell inside the envelope. |
| `gh klon stop <branch>` | Kill the whole process tree of that klon. |
| `gh klon up` | In golden: fetch, `merge --ff-only`, approved `[warm] steps`, then a new spare. |
| `gh klon hibernate <branch>` / `wake <branch>` | Stash the diff to `refs/klon/<name>` and delete the folder. `wake` is `add` plus apply. `add` never hibernates a klon on its own; `disk_budget` makes `add` refuse and name the candidate, and `add --evict` hibernates it. |
| `gh klon init [--volume <size>] [--undo]` | One time. Make golden a btrfs subvolume, or create a sudo-free btrfs loop volume. Prints the plan and waits for `y`. `--undo` reverses it. |
| `gh klon doctor [--json] [--repair]` | Backend, git version, fence ABI, cgroup delegation, inotify limits, make and ninja versions, pasta, journal repair. |
| `gh klon bench [--json]` | Measure M1 to M12 on this repository against a manifest. |
| `gh klon lo0` | macOS. Print the `lo0` alias command and the LaunchDaemon one-liner. |

Rules:
- The default path is `../<repo>.wt/<branch>`. `--path` overrides it. The research record §19 lists the harness path conventions.
- `git worktree list`, `remove`, `prune`, and `repair` keep working on a klon.
- `add`, `list`, `rm`, and `doctor` give versioned JSON with `--json`. A schema test rejects an undocumented breaking change.

### Packaging
- Repository `gh-klon`. Executable `gh-klon`. Release assets `gh-klon_v<ver>_<os>-<arch>` through `cli/gh-extension-precompile`.
- Local install for development: `cargo build --release && ln -sf target/release/gh-klon gh-klon && gh extension install .` from the repository root. `gh` needs the executable in the repository root (**V**, `gh extension install --help`). `/gh-klon` is in `.gitignore`.
- License: MIT OR Apache-2.0. This matches worktrunk and allows borrowing.

### Configuration: `.klon.toml` (all keys optional)

```toml
base = "main"                       # golden branch
path = "../{repo}.wt/{branch}"      # path template
disk_budget = "40G"                 # above this, `add` refuses and names the LRU klon; `add --evict` hibernates it
disk_budget_action = "refuse"       # or "hibernate"
spare = 1                           # hot-spare pool depth; 0 disables

[warm]                              # run by `gh klon up` after the fast-forward; needs approval
steps = ["cargo build", "pnpm install --frozen-lockfile"]

[proof]                             # v0.2: run by `gh klon check`; needs approval
steps = ["cargo nextest run"]

[fence]                             # extra writable paths under `run`
allow = ["~/.local/share/pnpm"]

[copy]                              # ext4 `copy` backend: per-directory strategy; needs approval
reinstall = { "node_modules" = "pnpm install --offline --frozen-lockfile" }

[fixup]
skip = ["**/*.sqlite"]

[hardlink]                          # v2, opt-in: hardlink instead of clone for page-cache sharing
paths = ["target/debug/deps"]
```

`.klonignore` uses gitignore syntax and lists paths that klon does not clone. Defaults: nested `.git` directories and submodule paths. klon honours `.worktreeinclude` as an additive include when present.

Approval rule: the first time klon reads a `.klon.toml` with a command-bearing key (`warm.steps`, `proof.steps`, `copy.reinstall`), it prints the commands and asks once. It stores the file's content hash in `~/.config/klon/approvals.toml`. A changed file asks again. `--yes` approves in a non-interactive run.

---

## 4. The `add` transaction

```
gh klon add <branch>
  0. journal   write <common>/klon/journal/<name>.json   state=planned
  1. register  git worktree add --no-checkout --detach --lock <path>   (empty dir; ~10 ms)
               add "/.klon/" to <common>/info/exclude once
               state=registered
  2. clone     spare present?  rename ../<repo>.wt/.spare -> <path>   (O(1), 40-60 ms)
               else            backend.clone(golden -> <path>), excluding golden/.git, .klonignore,
                               the destination, and every registered worktree path
               rewrite <path>/.git as  "gitdir: <common>/worktrees/<name>"
               state=cloned
  3. index     copy golden's index to <common>/worktrees/<name>/index; give it a fresh mtime
  4. tree      git checkout -q --force <branch>   (resets golden's dirty tracked paths; touches only the differing paths)
  5. repair    git clean -fdq                     (removes untracked non-ignored paths; ignored paths stay)
               path fixup on the ignored directories
               state=checked-out
  6. env       write .klon/env; allocate a loopback slot; copy the hooks to .klon/hooks
  7. unlock    git worktree unlock <name>; state=ready; remove the journal entry
  8. spare     start a detached low-priority process that prepares the next spare
```

Step 1 gives Git the ownership of `<common>/worktrees/<name>/`. Step 2 replaces only the working directory. `git worktree add` refuses a non-empty directory, also with `--force`, so this order is fixed (**V**). Step 4 needs `--force`: a plain `checkout` aborts when a dirty golden file also differs on the branch (**V**, review 2026-09-03). A failure after step 1 unlocks and removes the registered worktree.

### Backends

`doctor` probes once per repository and caches the result in `<common>/klon/probe.json`. The probe clones a small fixture and compares a manifest (type, size, mode, mtime, symlink target, content hash) between the source and the destination. A backend that fails the test is never selected.

| Backend | When | Spawn, 100k files | Delete | Notes |
|---|---|---|---|---|
| `btrfs-snapshot` | Linux; golden is a user-owned btrfs subvolume | about 5 ms (**B**; O(1) **V**) | O(1) with `user_subvol_rm_allowed`; else a background `rm -rf` | `gh klon init` converts golden into a subvolume with one `mv`. An unprivileged snapshot needs a user-owned source subvolume, kernel ≥ 3.13 (**V**). |
| `btrfs-volume` | Linux on ext4 or any filesystem; one-time `gh klon init --volume 60G` | as above | as above | A sparse image, `mkfs.btrfs --rootdir <empty user dir>`, `udisksctl loop-setup -f`, `udisksctl mount`. No password in an active local session (**V**, polkit `allow_active=yes`, verified on the development laptop). Needs `btrfs-progs`. The image must seed a user-owned directory (for example `klon/`) through `--rootdir`; the mount root itself is `root:root` under udisks (**V** with an ext4 substitute, §11). Golden moves into that directory once. After a reboot the first `add` re-runs `loop-setup` and `mount` (about 1 s, no password). See §12 Q1 and Q2. |
| `apfs-clone` | macOS | 3.5-6 s (**V**, 17k-29k files/s, Apple DTS). gh-wt measured 19 s for 174k files; `git worktree add` took 15.5 s on the same fixture (**V**, published) | background `rm -rf` | One `clonefile` per top-level ignored directory, in parallel, never on the repository root. Apple says the kernel locks the source hierarchy for the whole call and strongly discourages directory clones. Tracked files go through per-file `clonefileat`. The spare hides the latency. |
| `reflink-walk` | Linux XFS (`reflink=1`), bcachefs, a btrfs plain directory, ZFS ≥ 2.2.6 | about 3 s (**V**, 116k files: 4 threads 3.3 s; 10 threads 9 s) | background | Parallel `FICLONE` plus `utimensat`. `FICLONE` sets the mtime to now (**V**). 4 workers. Spare. |
| `copy` | ext4 without a volume | 40-100 s (**V**, local) | background | A per-directory strategy. Copy big-file directories (`target/`, `obj/`, `.next`). Re-install small-file directories that have a per-user store (pnpm: 5 s from a warm store against 95 s for a copy). Warm directories land in the background through an atomic rename. The spare runs at idle I/O priority. Prints once: run `gh klon init --volume` for instant spawns. Never hardlinks a mutable directory. |
| `overlay` (v2) | Headless Linux agent farms | O(1) | O(1) | overlayfs in a user and mount namespace (kernel ≥ 5.11). Shares the lower page cache. Visible only inside `gh klon run`. Opt-in only. |

### Hot spare
After each `add`, `up`, and `rm`, klon starts a detached low-priority process. That process prepares `../<repo>.wt/.spare/` from golden: the clone, the index copy with a fresh mtime, and a record of golden's HEAD and dirty paths. `add` renames the spare into place, rewrites the `.git` file, runs `git checkout <branch>`, and starts the next spare. The pool depth is 1. `spare = 0` disables it. A spare made before an `up` is still warm for the unchanged units. No daemon.

Tear check: the spare process records golden's HEAD, the hash of `git status --porcelain`, and the mtimes of the top-level ignored directories before and after the clone. A mismatch marks the spare as torn. `add` then clones directly and warns.

### Golden policy
Golden is the main checkout on the base branch. A klon holds the tracked files of its branch plus the ignored files of golden. `git checkout --force` resets golden's dirty tracked files in the clone. `git clean -fdq` removes untracked non-ignored files from the clone. The user should not run a build in golden during `add`. The spare makes this rare.

### Git details (**V** on git 2.55 unless noted; §11 gives the git 2.34.1 results)
- **Index copy.** Git compares the inode, ctime, and device per entry by default. A clone changes them, so the first `git status` re-hashes every file. With `core.checkStat=minimal`, git compares only the size and the whole-second mtime, which the clone keeps. The index file must get a fresh mtime. The rule: the index mtime must be newer than every working-tree file mtime, else racy-git re-hashes everything (**V**, §11). Known blind spot: an edit that keeps the byte count and the mtime is invisible under `minimal`; the README documents it.
- **`git checkout`, not `read-tree -m -u` or `reset --hard`.** On 100k files with 20 differing paths: `checkout` 0.37-0.90 s; `read-tree -m -u` 0.9-1.7 s; `reset --hard` 0.9-2.0 s. On git 2.34.1: 0.31 s, 0.66 s, and 0.71 s (**V**, §11). `index.skipHash=true` (git ≥ 2.40) saves 40-100 ms per index write.
- **Config.** `extensions.worktreeConfig` is not needed. Put `core.checkStat=minimal`, `core.untrackedCache=true`, `index.version=4`, `merge.conflictStyle=zdiff3`, and `rerere.enabled=true` in the shared repository config, or pass them through `GIT_CONFIG_COUNT` (git ≥ 2.31). `untrackedCache` cut `status` from 0.35 s to 0.15 s.
- **fsmonitor** stays off. Linux support arrived in git 2.55. It made a hot-cache `status` slower and caused hangs in Codex during `worktree add`. The split index hit a `BUG:` abort. Do not use either.
- **Branch resolution.** klon uses a local branch when one exists. Else an existing `origin/<name>` gives a tracking branch after a targeted fetch. Else klon makes a new branch from base.

---

## 5. The envelope

`.klon/env` in each tree:

```
KLON_NAME=<branch>
KLON_IP=127.0.0.N
HOST=127.0.0.N
TMPDIR=<tree>/.klon/tmp
KLON_JOBSERVER=<XDG_RUNTIME_DIR or ~/.klon>/jobserver     # `run` adds MAKEFLAGS=-j --jobserver-auth=R,W with inherited descriptors
GIT_CONFIG_COUNT=1  GIT_CONFIG_KEY_0=core.hooksPath  GIT_CONFIG_VALUE_0=<tree>/.klon/hooks
```

`gh klon run` sources it, applies the fence, and joins the resource scope. `HOME` stays shared on purpose. That is where the per-user caches live.

| Feature | Linux | macOS | Default |
|---|---|---|---|
| Write fence | Landlock (kernel ≥ 5.13; ABI 3 = kernel 6.2 adds TRUNCATE). Read everywhere. Write only in the tree; in `<common>/objects`, `refs`, `logs`, `rr-cache`, `klon`, `worktrees/<name>`, and the file `packed-refs` (git writes there on commit, **V**; never the `<common>` root, so `hooks/` and `config` stay read-only; `run` sets `gc.auto=0`); in `TMPDIR`, `/tmp`, `/var/tmp`, `$XDG_RUNTIME_DIR`, `~/.cache`, `~/.cargo`, `~/.npm`, the pnpm store, `~/.nuget`, `GOCACHE`, the uv cache, `/dev/null`, `/dev/shm`, `/dev/tty`, and `[fence] allow`. Always pair WRITE_FILE with TRUNCATE. No user namespace, so the Ubuntu 24.04 AppArmor rules do not apply. Crate `landlock` 0.4.x (**V**). | `sandbox-exec` with a generated Seatbelt profile: deny default, allow read, allow write under the listed paths, allow network. Works on macOS 15 and 26 with a deprecation warning (**V**). The same approach as Anthropic sandbox-runtime and Codex. | on under `run`, `shell`, and `add -- cmd`; `--no-fence` |
| Memory and pids | `systemd-run --user --scope -p MemoryHigh=<total/(N+1)> -p TasksMax=<n>`, recomputed at each `run` from the live klon count. memory and pids are delegated on all systemd versions (**V**). D-Bus-free fallback: `mkdir` under the user's `user@UID.service` cgroup and write `memory.high` (**V**). | No cap. Poll the `proc_pid_rusage` footprint of the group; SIGTERM at a threshold. Jetsam priority through `posix_spawnattr_setjetsam_ext` (**V** that it has no privilege check; **B** that it kills). Never `RLIMIT_AS`; it breaks the JVM and .NET. | on |
| CPU | `CPUWeight` works with systemd ≥ 252 (Ubuntu 24.04 = 255, **V**). Ubuntu 22.04 (249) ignores it; use `nice`. `cpuset` and `io` need a root drop-in; document only. | QoS clamp UTILITY through `posix_spawnattr_set_qos_clamp_np` (**V**, what `taskpolicy -c` uses). Never `PRIO_DARWIN_BG` for agents; it throttles the network sockets (**V**). BG only for deletes. | on |
| Build slots | A fifo jobserver with the **pipe-style** handshake: `run` opens the fifo read-write, keeps the two descriptors open across `exec`, and exports `MAKEFLAGS=-j --jobserver-auth=R,W`. The fifo holds `nproc-2` tokens. Read a byte to acquire, write it back to release. klon tops up lost tokens. The fifo style (`fifo:<path>`) is a **fatal error** on make 4.3, which Ubuntu 22.04 and 24.04 ship (**V**, §11). The pipe style works on make 4.3 and cargo 1.92 (**V**). Clients: cargo and rustc, GNU make, ninja ≥ 1.13, LLVM. Not: CMake itself, Bazel, Go, tsc, esbuild, vite, dotnet, gradle, pytest. | Same. `/usr/bin/make` is 3.81; Homebrew make is 4.4.1. Both accept the pipe style. | on; `KLON_NO_JOBSERVER=1` |
| Network identity | `lo` owns all of `127/8`; a bind to `127.0.0.N` needs no config (**V**). `run --netns`: `pasta --config-net -t 127.0.0.N/auto -- <cmd>` maps host `127.0.0.N:<port>` to the same port in a rootless namespace with egress, about 250 ms startup (**V**). Packaged on Ubuntu 24.04+, Fedora, Arch, Debian 13; not Ubuntu 22.04. | No sudo-free mechanism (**V**). `lo0` holds only `127.0.0.1`. The DYLD interposer is dead under the hardened runtime. Env contract only. `gh klon lo0` prints the one-liner. | env on; netns opt-in |
| Process tree | `cgroup.kill` on the scope | `POSIX_SPAWN_SETSID`, `proc_listpgrppids`, `killpg` | `gh klon stop` |
| Hooks and config | Per-tree `core.hooksPath` to `<tree>/.klon/hooks`, a copy of the repository hooks. An agent that edits a hook cannot affect golden or a sibling. The fence keeps `<common>/hooks` and `<common>/config` read-only. Residual: `refs/heads/<base>` must stay writable for git, so the fence cannot stop a ref update to base; `doctor` reports it. | Same | on |

Delete: rename into `../<repo>.wt/.trash/` (36-62 ms on ext4, **V**), then a background `rm -rf` under `nice -n 19` and `ionice -c 3` on Linux, or `PRIO_DARWIN_BG` on macOS. Measured: 4.5 s for 6 GB in 11k files; 10 s for 4 GB in 82k files.

---

## 6. Integration certainty

The literature says that coordination, not parallelism, carries the gain (research record §15.3).

- **Radar** in `gh klon list`, through `git merge-tree --write-tree --quiet` on git ≥ 2.38 (10-40 ms per pair on 100k files, **V**), or the legacy `git merge-tree <merge-base> <a> <b>` form below 2.38 (0.01 s per pair; conflict paths from the `changed in both` lines, **V**). Columns: vs base (clean or N conflicts), vs siblings (pairwise), behind (commits behind base). Cached in `<common>/klon/radar` keyed by the tuple of HEADs.
- **`gh klon merge <branch>`**: fetch, `pre_merge` hook, a structured merge with mergiraf when installed (GPLv3, a separate process, `merge.mergiraf.driver` plus `.gitattributes`), fast-forward base, `rm`. Never pushes. Refuses on a dirty golden. LLM conflict resolution is out of scope: the best models resolve under 60 % of real hunks.
- **`gh klon check <branch>`** (v0.2): refuses a dirty tree. Runs the approved `[proof] steps` in the klon inside the envelope. Writes a receipt `{version, commit, tree, steps_hash, results[], duration, created}` to `<common>/klon/receipts/<commit>.json`. `merge` needs a receipt for the exact HEAD unless the user passes `--no-check`. A receipt for another commit is stale.
- **`gh klon claim <branch> <paths...>`** (v0.2): records owned paths in `<common>/klon/claims.json` under `flock`. A claim names an exact file or a directory prefix. The overlap check runs inside the lock. `list` flags overlaps. `check` reports changed paths outside the klon's claims.
- `merge.conflictStyle=zdiff3` and `rerere` are on in the shared config.

---

## 7. Lifecycle safety

- **Journal.** `add`, `rm`, and `init` write `<common>/klon/journal/<name>.json` with a state and a timestamp. A repeated command, or `doctor --repair`, moves each entry to the prior valid state or the completed state. A journal entry contains no repository content.
- **`rm` safeguards.** Resolve the target to a managed path under the configured template or to a registered worktree. Refuse the repository root, the home directory, or an unresolved template. Refuse a dirty tree without `--force`. Refuse a tree with live processes without `--force`. Rename to `.trash`, drop the `.git` file, and run `git worktree prune`; never `git worktree remove`, which deletes inline (4.3 s on 110k files, **V**). The background delete follows.
- **`init` safety.** `init` prints the plan with the source and target paths and waits for `y`. It writes a journal entry before the move. `init --undo` reverses it. `doctor --repair` completes or reverts an interrupted `init`.
- **`doctor`.** Reports the backend, git version, Landlock ABI or Seatbelt, cgroup delegation, inotify limits, make and ninja versions, pasta, `btrfs-progs`, and stale journal entries. `--repair` completes or reverts them. Repeated runs give the same result.
- **Formats.** The journal, probe cache, radar cache, receipts, and claims carry a `version` field. An unknown future version fails closed.

---

## 8. Metrics and benchmark rules

`gh klon bench` measures a repository against a manifest and prints a table or JSON.

| # | Metric | git worktree | git-sprout | worktrunk | klon target |
|---|---|---|---|---|---|
| M1 | Spawn to editable tree, p50 | 2-5 s | 0.5-1 s | 2-5 s | ≤ 1 s v0 (snapshot or spare plus `checkout`); ≤ 100 ms v0.3 (index byte-splice) |
| M2 | Spawn to warm build state | never | never | +20 s (14 GB reflink walk); minutes on ext4 | 0 s (snapshot or spare); background on a cold ext4 copy |
| M3 | Build units compiled on the first build | all | all | 0 if listed | 0 for cargo, npm and pnpm, Vite, TS, Go; documented exceptions in §9 |
| M4 | `git status` in a fresh klon | n/a | fast after a re-hash | normal | first call ≤ 500 ms; later calls ≤ 150 ms (100k files, warm) |
| M5 | Unique disk per idle tree | src + build | diff, no build | diff | diff plus inode metadata; KBs when hibernated |
| M6 | Delete latency | seconds to minutes | same | async | ≤ 100 ms to return (a rename measured 36-62 ms on ext4), then background |
| M7 | Cross-tree writes blocked under `run` | 0 % | 0 % | 0 % | 100 % |
| M8 | Hooks and config isolated per tree | no | no | no | yes |
| M9 | Port collisions across N trees | yes | yes | `hash_port` | 0 on Linux; env contract on macOS |
| M10 | RAM bound per tree | none | none | none | `MemoryHigh` on Linux; a footprint poll on macOS |
| M11 | Conflict warning lead time | none | none | none | every `list`, ≤ 40 ms per pair |
| M12 | Build throughput at N=6 builders against ideal | thrash | thrash | thrash | ≥ 80 % |
| M13 | Commands to the first klon / sudo prompts | 1 / 0 | 1 / 0 | 1 / 0 | 1 / 0, also on ext4 |
| M14 | Daemon | no | no | no | no |

Benchmark rules (from the evidence-gated proposal, scaled to a laptop):
- A versioned manifest fixes the fixture seed, the file counts, the size distribution, the branch diff size, the commands, the timer points, and the pass rule before a run.
- Fixtures: 10k and 100k tracked files; ignored state of 250 MB and 2 GB. A 500k-file and 14 GB cell is optional.
- Development runs: 10 warm and 5 cold. Release claims: 30 warm and 10 cold. Random order, recorded.
- The report gives p50, p95, the raw samples, and the environment: hardware, OS, filesystem, mount options, git version, klon commit, fixture hash.
- A correctness mismatch in the same cell voids its timing result.
- A warm unchanged build must compile zero units and download zero bytes.
- The same runner drives every compared tool with the same final tree and the same commands.

---

## 9. Ecosystem notes and path fixup

| Ecosystem | Shared per user | Per tree | Relocation after a whole-directory clone with mtimes kept |
|---|---|---|---|
| Rust / cargo | registry sources | `target/` (2-11 GB) | Relocatable as-is; fingerprints hash relative paths (**V**). The first edit after a move recompiles that one crate non-incrementally. |
| TS/Node (pnpm) | pnpm store (hardlinks) | symlink farm, `.next`, `dist`, `.tsbuildinfo` | The runtime works. `node_modules/.modules.yaml` stores an absolute `storeDir` and `virtualStoreDir`; the next `pnpm install` fails with `ERR_PNPM_UNEXPECTED_VIRTUAL_STORE` (**V**). Path fixup rewrites two keys. |
| TS/Node (npm, Yarn PnP, Bun) | Bun cache | full `node_modules` | Relocatable (**V**). No page-cache sharing with npm. |
| Vite, `.tsbuildinfo`, Turborepo, Nx | Turbo and Nx caches | outputs | Relocatable (**V**). |
| Next.js `.next/cache` | none | webpack cache | Not relocatable; absolute paths (**V**). Delete on clone. |
| C# / .NET | NuGet `~/.nuget/packages` | `bin/`, `obj/` | `obj/*.nuget.g.props` and `project.assets.json` hold absolute paths (**V**). Path fixup, then `dotnet restore` offline. |
| Go | `GOCACHE` | almost nothing | Relocatable with `-trimpath` (**V**). |
| Python (uv) | uv cache | `.venv` | `uv venv --relocatable` fixes the shebangs. Path fixup for plain venvs (`pyvenv.cfg`, shebangs). |
| C/C++ (CMake + Ninja) | ccache | build dir | `CMakeCache.txt` is absolute (**V**). Fixup plus `cmake .`; delete `.ninja_log` and `.ninja_deps`. |
| JVM (Gradle) | build cache | `build/`, `.gradle/` | The configuration cache is not relocatable (**V**). One JVM daemon per active tree. |
| Maven | `~/.m2` | `target/maven-status` | `inputFiles.lst` is absolute; one full recompile (**V**). |
| Bazel / Buck2 | output base keyed by the workspace path | `bazel-*` links | An empty output base after a move unless pinned (**V**). |

Path fixup: a fixed-string search for golden's absolute path over the ignored directories (`ignore` plus `grep-searcher` crates). klon rewrites the hits in text files only, with the rails from §2. klon also rewrites symlink targets that point into golden (§12 Q8). klon deletes `.next/cache`, `.ninja_log`, and `.ninja_deps`. A klon's path is stable for its lifetime, so whatever is not rewritten is paid once.

In-place writers, never safe to hardlink: cargo, MSBuild, pnpm `.modules.yaml`, npm, ninja.

---

## 10. Rejected designs

Do not re-open one of these without new data.

| Rejected | Why |
|---|---|
| A virtual filesystem (FUSE, NFS loopback, EdenFS) | A daemon on the hot path of every `stat`. VFS for Git died with kernel extensions. EdenFS is not supported outside Meta. |
| A reimplementation of git's object store | 100 % cost, 0 % gain. |
| gitoxide on the hot path | It cannot create a linked worktree or add index entries. A gix two-tree update is 1.2-2x at best, under the 2x rule (**V**). |
| Hand-written worktree metadata | It works (**V**), but Git does it in 10 ms and owns the format. |
| `read-tree -m -u` as the two-tree update | It frees the cache-tree and rebuilds it. Slower than `checkout` (**V**). |
| APFS `clonefile` on the repository root | 3.5-6 s per 100k files. It locks the source hierarchy. Apple discourages it (**V**). |
| A full copy as the routine ext4 path | 95 s per `node_modules` (**V**). |
| A hardlink copy of mutable directories | cargo, MSBuild, pnpm, npm, and ninja open outputs with truncate and corrupt golden. |
| A regular byte copy as the default backend | The CoW clone is the product. The copy is the fallback. |
| An immutable generation as the only clone source in v1 | It doubles the disk and delays warmth. The spare plus the tear check gives most of the safety. Generations return in v0.2 as an option. |
| A SQLite state store in v1 | One laptop. A file under `flock` gives the same atomic check. |
| Full proof receipts with execution manifests in v1 | No demand evidence. The test gate is the load-bearing part. |
| overlayfs as the default Linux backend | The mount is invisible outside the namespace. Editors cannot open the tree. |
| A static `memory.max` | It wastes over 90 % or kills bursts (AgentCgroup). Use a soft `MemoryHigh`, recomputed. |
| bwrap as the Linux fence | It needs user namespaces. Ubuntu 24.04 AppArmor blocks it. Landlock needs none. |
| jj as the agent-facing VCS | Slower than git for agents in vcbench. Its workspaces are not git worktrees. |
| A CRDT shared workspace | Coordination, not merge, bears the load (AgentRoom). |
| LLM-only conflict resolution | Under 60 % of real hunks; non-deterministic. |
| A separate golden tree plus a refresh daemon | The main checkout is golden. `up` replaces the daemon. |
| A userspace scheduler, an observation layer, three-state eviction | Kernel fair-share plus the jobserver covers CPU. Whole-klon hibernate covers disk. |
| A macOS `bind()` interposer | The hardened runtime strips `DYLD_*` (**V**). |
| Hardlink everything read-only | Cargo rewrites fingerprint files in place. The build fails. |

---

## 11. Support floor and host facts

Support floor:

| Item | Core | Radar and `merge` | Fence | Scope |
|---|---|---|---|---|
| git | ≥ 2.34.1 | ≥ 2.34.1 (legacy form); ≥ 2.38 for `--write-tree` | - | - |
| Linux | kernel ≥ 5.15, glibc ≥ 2.31 or musl | - | Landlock ABI ≥ 1 (kernel ≥ 5.13); ABI 3 preferred | systemd ≥ 249 for memory and pids; ≥ 252 for CPU weight |
| macOS | 13+, arm64 and x86-64 | - | `sandbox-exec` present | - |
| Release tests | the oldest and the current git on each OS | | | |

### Development laptop, probed 2026-09-03 (a `gh klon doctor` dry run)

| Item | Result |
|---|---|
| Host | Ubuntu 22.04.5, kernel 6.2.0-36, 20 CPUs, 62 GiB RAM, systemd 249.11 |
| Filesystem | One ext4 partition for `/`, `/home`, and `/tmp`: 938 GB, 88 % used, 116 GB free. `cp --reflink=always` fails with "Operation not supported" (**V**) |
| git | 2.34.1. This is the core support floor. No `merge-tree --write-tree` |
| Landlock | LSM enabled; ABI 3. An unprivileged ruleset denied a write outside the allowed path with errno 13 and allowed a write inside it (**V**) |
| systemd user scope | `systemd-run --user --scope -p MemoryHigh=100M -p TasksMax=50` runs with no password. `memory.high` and `pids.max` are applied (**V**). `CPUWeight=50` returns success and does nothing. Only `memory pids` are delegated (**V**) |
| inotify | 65,536 watches, 128 instances |
| udisks | `loop-setup` and `mount` need no password (**V**). The mount root is `root:root` and the user cannot write to it. Files seeded through `mkfs -d` keep their owner (**V** on ext4 as a substitute). The detach verb is `loop-delete`. GNOME mounts a loop device again after `unmount` |
| btrfs-progs | Absent. The volume spike (Q1) did not run here |
| make | 4.3. A fifo-style `--jobserver-auth` is a **fatal error**: `internal error: invalid --jobserver-auth string`. The pipe style `R,W` works: 8 one-second jobs took 3.03 s with 2 tokens (**V**) |
| cargo | 1.92.0. Honours both the fifo and the pipe style. 4 build scripts peaked at 3 concurrent under 2 tokens (**V**) |
| Loopback | Binds to `127.0.0.7:3000` and `127.0.0.250:3000` succeed with no config (**V**) |
| gh | 2.74.0 (snap), authenticated. `gh extension search klon` returns nothing (**V**). No extensions installed. Snap `gh` cannot read `/tmp` or `~/.t3` |
| Claude Code | 2.1.259. Has `-w, --worktree` and `--tmux`. No `WorktreeCreate` hook in the user settings |
| Present | cargo 1.92, pnpm 10.28, node 18.17, uv 0.12, python 3.10, cmake 3.22, bwrap 0.6.1, strace, gcc 11.4, docker 29.1, tmux 3.2, opencode 1.18 |
| Absent | ninja, dotnet, go, pasta, passt, mkfs.btrfs, btrfs, codex |

Consequences:
- The jobserver uses the pipe style (§5). The fifo file stays the token store.
- The daily local path is the `copy` backend plus the hot spare, until the user installs `btrfs-progs`. Then `init --volume` applies.
- The `init --volume` image seeds a user-owned `klon/` directory through `--rootdir`. Golden and the klons live below it.
- Zero-compile tests in v0 use Rust and pnpm fixtures. .NET runs in CI only.

### Git 2.34.1 pipeline check, 2026-09-03 (development laptop, ext4, warm cache)

Fixture: 100,001 tracked files, 10,000 ignored files in `build/` (40 MB), branch `feature` with 20 edits and 2 new files. Left in place at `/tmp/klon-git-check`.

| Step | Result |
|---|---|
| `git worktree add --no-checkout --detach --lock <empty path>` | 0.02 s. Writes `HEAD`, `commondir`, `gitdir`, `locked`, `logs/HEAD` under `<common>/worktrees/x/`. Writes **no index**. The path holds only the `.git` file (**V**) |
| Replace the working directory with a copy of golden minus `.git`, rewrite `.git`, copy the index, `touch` it | Git accepts it. `git worktree repair` changes nothing (**V**) |
| `git checkout -q feature` | 0.31-0.34 s. Exactly the 22 differing files get a new mtime (**V**) |
| `git read-tree -m -u main feature` / `git reset --hard feature` | 0.66 s / 0.71 s. `checkout` wins by 2.1x (**V**) |
| First `git status` with `checkStat=minimal` | 0.45 s (103k `stat`, 1k `open`). Without `minimal`: 2.63 s (102k `open`, a full re-hash). Later `status` calls: 0.09-0.13 s (**V**) |
| Index mtime rule | The index mtime must be newer than every working-tree file mtime. An index dated in the past forces a full re-hash (2.44 s). `cp` already gives a fresh mtime; `touch` is cheap insurance (**V**) |
| `git worktree add` on a non-empty path | `fatal: already exists`, also with `--force`. The order register-then-fill is fixed (**V**) |
| `git worktree remove` | Deletes 110k files inline in 4.28 s. klon must rename to `.trash`, drop the `.git` file, and run `git worktree prune` (**V**) |
| `mv` to `.trash` on the same filesystem, then `rm -rf` under `nice`/`ionice` | 0.00 s, then 3.4-4.0 s (**V**) |
| Plain `git worktree add` baseline | 6.0 s on this fixture, with no build state (**V**) |
| `git merge-tree --write-tree` | Absent on 2.34.1 (arrived in 2.38). The legacy form `git merge-tree <base> <a> <b>` finds a conflict (`changed in both`, 0.01 s) and a clean merge (**V**) |
| Blind spot | With `checkStat=minimal`, an edit that keeps the byte count and the mtime is invisible to `git status`. Build tools change the mtime, so the risk is small. Document it (**V**) |

---

## 12. Open questions

Resolved in the research pass (details in the research record §16): the APFS clone keeps attributes (**B** for times; a test stays in the suite); the Linux default is a snapshot when golden is a user-owned subvolume, else a 4-worker walk; nested repositories and submodules are excluded by default; `checkout` handles the two-tree update; `checkStat=minimal` plus a fresh index mtime is enough; `worktreeConfig` is not needed; the macOS QoS and jetsam APIs have no privilege check; systemd ≥ 252 delegates CPU; the jobserver blocks on an empty fifo by design; pasta is packaged on Ubuntu 24.04+; `lo0` aliases stay user-managed; the name is free; worktrunk is MIT OR Apache-2.0; `list` caches PR data with a TTL; the radar works on git 2.34.1 through the legacy `merge-tree` form (**V**, §11).

Still open. Each has a spike ticket or a decision point in a chunk.

| # | Question | Resolution path |
|---|---|---|
| Q1 | Does `mkfs.btrfs --rootdir` keep the owner of a seeded user directory, as `mkfs.ext4 -d` does? Does udisks accept `user_subvol_rm_allowed`? Can the user create and snapshot subvolumes below that directory? | **Resolved (V):** `--rootdir` keeps the owner. udisks refuses `user_subvol_rm_allowed`. The user creates and snapshots subvolumes, but cannot run `btrfs subvolume delete`, `list`, or `show`. klon deletes a klon with a background `rm -rf`. See `docs/spikes/2026-btrfs-loop-volume.md`. |
| Q2 | Bundle a static `mkfs.btrfs` (GPLv2, 1-2 MB) in the release asset, or print the install line? | **Resolved:** print the install line. Add an opt-in `--fetch-tools` that extracts the distribution package into `~/.local/share/klon/tools/` without root. See `docs/spikes/2026-btrfs-loop-volume.md` §12. |
| Q3 | Does a jetsam limit through `posix_spawnattr_setjetsam_ext` kill on a consumer Mac? | Spike S2: macOS spike. |
| Q4 | The spare plus a dirty golden: revert at spare creation or at claim? | Proposal: record the dirty paths at creation, revert at claim. Decide in the spare chunk. |
| Q5 | `add -- <cmd>` in tmux: exec only, or open a pane? | Proposal: exec. `--json` gives orchestrators what they need. |
| Q6 | `--only <dirs>` sparse-checkout cone for focused klons? | v2. |
| Q7 | `disk_budget` accounting: `btrfs fi du -s` where available, else count plus trash `du`? | Decide in the hibernate chunk. |
| Q8 | Rewrite symlink targets that point into golden? | Proposal: yes, in path fixup. Decide in the fixup chunk. |
| Q9 | Does Claude Code's `EnterWorktree` bypass `WorktreeCreate` (issue #36205)? | Spike S3: test with the plugin. |
| Q10 | Does make 4.3 ignore a `fifo:` jobserver with a warning or with an error? | **Resolved (V):** a fatal error. klon uses the pipe style (§5). |

---

## 13. Delivery

The specification `docs/klon-spec.md` gives the requirements, the chunks, and the acceptance criteria. Each chunk is one GitHub issue. Milestones:

| Milestone | Content | Result |
|---|---|---|
| v0 Local worktree replacement | `add`, `rm`, `list`, `prune`, `doctor`, the backends, the spare, `bench`, path fixup, the branch forms, `up`, `sync` | A usable, measured replacement, installed locally through `gh extension install .` |
| v0.1 Envelope | the env file, `run`, `shell`, `stop`, the jobserver, the fence, the scope, per-tree hooks, approvals, pasta | Guarantees instead of hope |
| v0.2 Integration certainty | the radar, `merge`, `check`, `claim`, the Claude Code plugin, hibernate | The developer sees conflicts before a merge |
| v0.3 Performance goals | `/goal` sessions against `bench`: M1, M4, M12; the index byte-splice if the numbers demand it | Targets met with raw samples |
| Release | precompiled assets, the CI matrix, docs | `gh extension install navaro1/gh-klon` |
| v0.4 macOS improvements | the `apfs-clone` backend, the Seatbelt fence, the QoS clamp and footprint poll, the jetsam spike, `lo0` | The full envelope on macOS. Last by request: these items need a Mac. |

---

## 14. References

- Research record, revision 2: `docs/klon-research-2026-09-03.md`. Its §20 has the full source list: competitors, filesystems, git, envelope, research papers, ecosystems. Its §19 has the harness path conventions.
- Evidence record: `docs/klon-evidence.md`. Pinned competitor commits, Apple and Linux clone facts, the research transfer table.
- Evidence-gated proposal: `docs/proposals/2026-09-03-evidence-gated-workspaces.md`. R1 to R21 and C0 to C12 of the receipt design, kept for v2.
- Claude Code `/goal`: https://code.claude.com/docs/en/goal
- gh precompiled extensions: https://github.com/cli/gh-extension-precompile

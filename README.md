# gh klon

`gh klon` is a `git worktree` replacement. It spawns a warm copy of a project for each coding agent.
A klon holds the tracked files of its branch plus the ignored files of golden, the main checkout.
The design is in `docs/klon-handoff.md`. The build order is in `docs/klon-spec.md`.

## Install

Install klon as a `gh` extension from the precompiled release:

```sh
gh extension install navaro1/gh-klon
```

`gh` downloads the release asset that matches its platform. Every release carries four assets:

| Asset suffix | Platform |
|---|---|
| `linux-amd64` | Linux x86-64, glibc |
| `linux-arm64` | Linux arm64, glibc |
| `darwin-amd64` | macOS 13 or newer, Intel |
| `darwin-arm64` | macOS 13 or newer, Apple silicon |

klon needs git 2.34.1 or newer. The Linux binaries build on Ubuntu 22.04, so they need a glibc
of that vintage; an older host builds klon from source (see below). Every host feature is
optional. `gh klon doctor` reports what this host has, and an absent feature never stops a command.

### Local development

Run this line from the repository root:

```sh
cargo build --release && ln -sf target/release/gh-klon gh-klon && gh extension install .
```

`gh` needs the executable in the repository root. `/gh-klon` is in `.gitignore`.
Check the install with `gh klon --version`.

## Quick start

```sh
gh klon add feature                   # a warm copy of golden at ../<repo>.wt/feature
gh klon list                          # every klon: branch, HEAD, dirty flag, radar columns
gh klon run feature -- cargo test     # a command inside the klon envelope
gh klon shell feature                 # an interactive shell inside the envelope
gh klon rm feature                    # rename to .trash, delete in the background
gh klon doctor                        # what this host has, and the open journal entries
```

## What `doctor` prints

`doctor` runs in any git repository. It reports the git version, the filesystem of golden, the
selected backend, and one row per host feature. This sample comes from klon v0.1.0 on Ubuntu
22.04 with ext4. The row list grows as chunks land.

```text
git                         2.34.1
filesystem                  ext4
backend                     copy: reflink unsupported
btrfs-progs                 absent: btrfs is not on PATH
cgroup.controllers          present: memory pids
inotify.max_user_instances  present: 128
inotify.max_user_watches    present: 65536
loopback                    present: 127.0.0.2 accepts a bind
make                        present: GNU Make 4.3
ninja                       absent: ninja is not on PATH
pasta                       absent: pasta is not on PATH
radar                       present: legacy merge-tree
reflink                     absent: reflink unsupported: EOPNOTSUPP
scope                       present: systemd 249 scope: MemoryHigh=63966M TasksMax=4096
slots                       present: no address in use
systemd-run                 present: systemd 249 (249.11-0ubuntu3.22)
journal: no open entry
```

On macOS, `backend` reports `copy` until the v0.4 milestone adds the `apfs-clone` backend.

## Commands

The table is the product shape from the handoff. The status column says what works today.

| Command | Meaning | Status |
|---|---|---|
| `gh klon add <branch>` | Same as `git worktree add`. O(1) when a spare or a snapshot exists. Warm. Branch resolution follows git DWIM. | done. The spare is not here yet; a btrfs snapshot gives the O(1) spawn. |
| `gh klon add origin/<branch>` | An explicit remote-tracking branch. | done |
| `gh klon add --pr <n>` | A pull request, forks included, from `refs/pull/<n>/head`. | done |
| `gh klon add --issue <n>` | A new branch named from the issue title. | done |
| `gh klon add <branch> -- <cmd...>` | Spawn, then run a command inside the envelope. | done |
| `gh klon list [--json]` | Path, branch, disk delta, RSS, live processes, PR number, checks, vs-base, vs-siblings, behind. | partial: path, branch, HEAD, dirty, locked, the loopback address, vs-base, vs-siblings, and behind print today; the disk, RSS, process, PR, and checks columns land with later chunks |
| `gh klon rm (<branch> \| --path <p>) [--merged] [--delete-branch] [--force]` | Same as `git worktree remove`. Deletes the branch only with `--merged` or `--delete-branch`. Async delete. Refuses a dirty tree or a tree with live processes without `--force`. | done |
| `gh klon prune` | Same as `git worktree prune`, plus journal cleanup. | done |
| `gh klon pr <branch>` | `gh pr create` from that tree. | done |
| `gh klon sync <branch> [--merge\|--onto <base>\|--fresh\|--all\|--check]` | Fetch, then fast-forward or rebase. `--check` is a dry run through `merge-tree`. | partial: only `sync <branch> --check` works; the other forms land with C14 |
| `gh klon merge <branch>` | Fetch, `pre_merge` hook, structured merge, fast-forward base, remove. Never pushes. | planned for v0.2 |
| `gh klon check <branch>` | v0.2. Run `[proof] steps` at a clean HEAD and record a receipt. | planned for v0.2 |
| `gh klon claim <branch> <paths...>` | v0.2. Record owned paths. `list` flags overlaps. | planned for v0.2 |
| `gh klon run <branch> -- <cmd...>` | Execute inside the envelope: fence, scope, env. | done on Linux; macOS gets its fence and scope in v0.4 |
| `gh klon shell <branch>` | An interactive shell inside the envelope. | done on Linux; macOS gets its fence and scope in v0.4 |
| `gh klon stop <branch>` | Kill the whole process tree of that klon. | done |
| `gh klon up` | In golden: fetch, `merge --ff-only`, approved `[warm] steps`, then a new spare. | partial: the fetch and the `[warm] steps` work; the spare lands with C9 |
| `gh klon hibernate <branch>` / `wake <branch>` | Stash the diff to `refs/klon/<name>` and delete the folder. `wake` is `add` plus apply. | planned |
| `gh klon init [--volume <size>] [--undo]` | One time. Make golden a btrfs subvolume, or create a sudo-free btrfs loop volume. Prints the plan and waits for `y`. `--undo` reverses it. | done |
| `gh klon doctor [--json] [--repair]` | Backend, git version, fence ABI, cgroup delegation, inotify limits, make and ninja versions, pasta, journal repair. | done. The row list grows as chunks land. |
| `gh klon bench [--json]` | Measure M1 to M12 on this repository against a manifest. | partial: the M1, M4, and M6 cells exist today |
| `gh klon lo0` | macOS. Print the `lo0` alias command and the LaunchDaemon one-liner. | planned for v0.4 |

## How `add` works

`add` does these steps:

1. Refuses a non-empty path. Refuses a path inside golden unless it is under `.claude/worktrees`, `.t3`, or a path that `.klonignore` excludes.
2. Registers the worktree with `git worktree add --no-checkout --detach --lock`.
3. Copies golden into the path. It skips `.git`, the destination, every registered worktree, and `.klonignore` matches.
4. Copies golden's index and sets `core.checkStat=minimal`, `core.untrackedCache=true`, and `index.version=4` in the shared config.
5. Runs `git checkout -q --force <branch>` and `git clean -fdq`, then unlocks the worktree.

The copy preserves read-only directories. If Git uses a split index, the copy includes its shared index file.
A failure after step 2 removes the registered worktree and prints the original error.
If cleanup fails, the command also prints the cleanup error.

Repository and destination paths must use UTF-8, a text encoding. They cannot contain newline characters.
Git 2.34 does not provide an unambiguous list format for paths with newline characters.
The destination cannot be inside the Git common directory, which holds shared repository data.

## btrfs: `gh klon init`

On btrfs, klon clones a klon with one `btrfs subvolume snapshot`. That takes
about 5 ms whatever the file count. It needs golden to be a btrfs subvolume that
you own. `gh klon init` converts a plain golden directory into one:

```sh
gh klon init                # prints the plan and asks; y converts golden
gh klon init --yes          # converts without a question
gh klon init --undo --yes   # converts golden back into a plain directory
```

A subvolume cannot be made from a directory in place, so `init` copies golden
into `<golden>.klon-sub` with `FICLONE` and swaps the two paths with two
renames. Golden keeps its path and every byte. The replaced copy shares its
blocks with the new golden, so a background process deletes it without freeing
user data.

`init` replaces golden and then deletes the original, so it checks the copy
three ways before the swap. It refuses a FIFO, a socket, or a device node,
which `FICLONE` cannot copy. It runs `git fsck --connectivity-only` on the
copy. It compares every ref, HEAD, and index file before and after the walk,
and refuses when a git command moved the repository under it. Golden stays as
it is whenever a check fires. Let every build and every git command in golden
and in every klon finish first.

The swap gives the path a new directory, so a shell that stands in golden still
holds the old one. `init` prints the `cd` line for that case. Run it, or open a
new shell.

`init` refuses a golden that is not on btrfs with `not btrfs`. On a golden that
already has the wanted shape it exits 0 and changes nothing.

Two host facts limit what klon does with subvolumes. `btrfs subvolume show`,
`list`, and `delete` need root, so klon detects a subvolume with `stat` and
deletes a klon with `btrfs subvolume delete` only where the filesystem carries
the `user_subvol_rm_allowed` mount option. Everywhere else `rm` falls back to
the background byte delete, which removes a subvolume too.

A snapshot does not copy a nested subvolume: it leaves an empty directory in
its place. `add` refuses rather than hand over a klon that lost those files.
Exclude the path in `.klonignore`, or pass `--backend reflink-walk`.

klon looks for the `btrfs` binary on `PATH`, and under `$KLON_BTRFS_TOOLS` when
that variable names a directory. A user can unpack `btrfs-progs` there without
root.

## Configuration

`add` reads `.klon.toml` from the golden root. All keys are optional.
The `path` key is a path template with two placeholders: `{repo}` is the golden
directory name and `{branch}` is the new branch. A relative template resolves
against the golden root.

```toml
path = "../{repo}.wt/{branch}"   # the default
```

Klon refuses a template that resolves to `/`, the home directory, or the
repository root. It refuses an unknown placeholder.

`up` runs the `[warm] steps` in golden with `sh -c`, in order, and stops at the
first failure. The first `up` asks before it runs any step from `.klon.toml`:

```
klon: .klon.toml asks to run:
  warm.steps: cargo build
Approve? [y/N]
```

The answer stores the SHA-256 of `.klon.toml` in `<config home>/klon/approvals.toml`.
`--yes` approves without a prompt. A run without a terminal and without `--yes`
refuses with `needs approval` and runs nothing. A one-byte change to `.klon.toml`
asks again. The config home is `KLON_CONFIG_HOME`, else `XDG_CONFIG_HOME`, else
`~/.config`. Unknown keys draw one warning and never fail the load.

## The journal and `doctor`

`add` and `rm` write one journal entry per klon to `<common>/klon/journal/<name>.json`
before each step. The entry holds the operation, the state, the path, the branch,
and the start time. It holds no repository content. A completed command deletes
its entry, so every entry that survives marks an interrupted command.

```sh
gh klon doctor              # the host report and the open entries
gh klon doctor --repair     # close every open entry, one printed line per action
```

`doctor` reports the git version, the filesystem of golden, the selected backend,
and one row per host feature. Each host feature reports `present`,
`absent`, or `broken` with a reason. A feature that this host does not have never
stops a command.

`--repair` moves each entry to the prior valid state:

| Operation and state | Action |
|---|---|
| `add` `planned` | Delete the entry. Unregister the worktree when git already registered one. |
| `add` `registered` or `cloned` | Unlock the worktree, remove it with force, delete the entry. |
| `add` `checked-out` | Unlock the worktree, delete the entry. The klon stays. |
| `add` `ready` | Delete the entry. The klon is complete. |
| `rm` `removing` | Delete the `.git` file in the trash copy, start the background delete, run `git worktree prune`, delete the entry. |
| `rm` in any earlier state | Delete the entry. The klon stays. |
| `init` `planned` or `copied` | Delete the staging copy. Golden never moved. |
| `init` `swapped`, golden missing | Rename `<golden>.klon-old` back to golden, then delete the staging copy. |
| `init` `swapped`, golden present | Delete the staging copy and the replaced copy. |
| `init` `ready` | Delete the replaced copy. Golden is complete. |

A repeated command repairs its own entry too. `add` closes the entry of its
destination before it validates the path, so a second `add` after an interrupted
one completes without a `doctor` run. It prints one `klon: recovery:` line per
action.

A repair that cannot finish keeps the entry. `doctor --repair` then prints the
report, names the reason, and exits non-zero, so the next run tries again.

An entry with an unknown `version` fails closed: `doctor` exits non-zero with
`unknown journal version` and changes nothing. Upgrade klon in that case.

## JSON output

`--json` makes `add`, `list`, `rm`, `doctor`, `init`, and `bench` print one JSON
document on stdout instead of the human report. Each document carries a `schema`
field: `klon.add/1`, `klon.list/1`, `klon.rm/1`, `klon.doctor/1`,
`klon.init/1`, and `klon.bench/1`. An
error still goes to stderr as text and keeps the same exit code.

```sh
gh klon add --json feature
{"schema":"klon.add/1","path":"/w/repo.wt/feature","branch":"feature","head":"1a2b...","backend":"copy","duration_ms":812}
```

A new field may appear in a later version. A removed or retyped field bumps the
version suffix. `tests/schema.rs` holds the documented field set of each schema.

`up` and `prune` print no document. They refuse `--json` instead of ignoring it.

## `bench`

`gh klon bench` measures klon against plain `git worktree add` on a fixture that
it generates itself. It never touches the repository it runs in, apart from the
result file.

```sh
gh klon bench                              # every cell this host may run
gh klon bench --cell m1-add-10k --json     # one cell, one JSON document
gh klon bench --release                    # 30 warm and 10 cold samples
gh klon bench --out /tmp/results           # another directory for the result
```

`bench/manifests/v1.toml` fixes the run before it starts: the fixture seed, the
two profile shapes, the cells, the run counts, the timer points, and the pass
rule. The binary embeds that file. A result names the manifest version and a
`fixture_hash` over the seed and the shapes, so two results compare only when
their hashes match.

| Cell | Metric | Measures | Budget |
|---|---|---|---|
| `m1-add-10k` | M1 | `add` on 10k tracked files and 250 MB of ignored state | p50 ≤ 1000 ms |
| `m1-add-100k` | M1 | `add` on 100k tracked files and 2 GB of ignored state | p50 ≤ 1000 ms |
| `m4-status-100k` | M4 | `git status` in a fresh tree: the first call and the later calls | p50 ≤ 500 ms and ≤ 150 ms |
| `m6-rm-100k` | M6 | `rm`: the time until the command returns | p50 ≤ 100 ms |

Each cell runs twice, once for the backend that klon probes and once for
`git worktree add`. The two records carry one cell name and differ in `backend`.
The samples of the two tools are interleaved in a random order, and the order is
recorded. Only the measured command is inside the timer.

Before the samples, klon builds one more tree and compares it with golden: the
ignored directory against golden's, plus a clean `git status`. A mismatch sets
`timing_valid: false` on every record of that cell. A fast wrong answer is not a
result.

The result lands in `bench/results/<date>-<host>.json` with the schema
`klon.bench/1`. It holds the raw samples, `p50_ms`, `p95_ms`, the run order, the
correctness verdict, and an environment record: the processor, the memory, the
operating system, the kernel, the filesystem and its mount options, the git
version, the klon commit, and the fixture hash.

| Variable | Effect |
|---|---|
| `KLON_BENCH_DIR` | Where the fixture is built. Default: `$HOME/.cache/klon/bench` |
| `KLON_FIXTURE` | `100k` lets the three 100k cells run. Without it they are skipped with a reason |
| `KLON_BENCH_DROP_CACHES` | A shell command that drops the page cache. Without one every cell is `warm-only` and klon runs no cold samples |
| `KLON_BENCH_RUNS` | Override the sample count of every record |
| `KLON_BENCH_SMOKE` | Replace every profile with a tiny shape. For a smoke test only |
| `KLON_BENCH_ORDER_SEED` | Repeat one random run order |
| `KLON_BENCH_INJECT_MISMATCH` | Damage one file before the correctness check, to prove the void path |

The default fixture directory is `$HOME/.cache/klon/bench`, not the system
temporary directory: `gh` from the snap store cannot read `/tmp`, so a bench
under snap could not reach its own fixture there.

`KLON_BENCH_RUNS` and `KLON_BENCH_SMOKE` change what a run means. Both appear in
the result, and a smoke run gives its own `fixture_hash`, so a shortened run can
never pass for a measurement.

## Known limitations

- The `core.checkStat=minimal` blind spot. Git compares only the size and the
  mtime of a file. An edit that keeps the byte count and the mtime is invisible
  to `git status`. Build tools change the mtime, so the risk is small.
- The git floor is 2.34.1. Features that a newer git brings degrade to the
  legacy form. The conflict radar on git 2.34 runs the legacy `git merge-tree`
  form; `doctor` reports it.
- macOS until v0.4. klon uses the `copy` backend and runs every command without
  a fence and without a scope. `doctor` reports each absent part. The v0.4
  milestone adds the `apfs-clone` backend, the Seatbelt fence, and the QoS clamp.
- The btrfs delete needs root or a mount option. `btrfs subvolume delete`,
  `list`, and `show` need root. Where the mount carries `user_subvol_rm_allowed`,
  klon deletes a subvolume without root. Everywhere else `rm` falls back to a
  background `rm -rf`.
- The `btrfs-volume` loop setup needs an active local session. `udisksctl`
  grants `loop-setup` and `mount` without a password only in the active session
  of a local user. A session over SSH asks for a password or fails, and klon
  prints the reason.
- The release binaries need a glibc of the Ubuntu 22.04 vintage on Linux. An
  older host builds klon from source.

## Tests

```sh
cargo test
```

The tests generate a 10k-file fixture in a temporary directory. They need `git` on `PATH`.

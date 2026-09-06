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
fence.residual              present: refs/heads/main stays writable under the fence: git needs <common>/refs; hooks and config stay read-only
inotify.max_user_instances  present: 128
inotify.max_user_watches    present: 65536
landlock                    present: ABI 3
loopback                    present: 127.0.0.2 accepts a bind
make                        present: GNU Make 4.3
ninja                       absent: ninja is not on PATH
pasta                       absent: pasta is not on PATH
radar                       present: legacy merge-tree
reflink                     absent: reflink unsupported: EOPNOTSUPP
scope                       present: systemd 249 scope: MemoryHigh=63966M TasksMax=4096
slots                       present: no address in use
systemd-run                 present: systemd 249 (249.11-0ubuntu3.22)
volume                      absent: no klon volume; gh klon init --volume <size> makes one
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
| `gh klon list [--json]` | Path, branch, disk delta, RSS, live processes, PR number, checks, receipt, claims, vs-base, vs-siblings, behind. | partial: path, branch, HEAD, dirty, locked, the loopback address, the receipt mark, the claim count, vs-base, vs-siblings, and behind print today; the disk, RSS, process, PR, and checks columns land with later chunks |
| `gh klon rm (<branch> \| --path <p>) [--merged] [--delete-branch] [--force]` | Same as `git worktree remove`. Deletes the branch only with `--merged` or `--delete-branch`. Async delete. Refuses a dirty tree or a tree with live processes without `--force`. | done |
| `gh klon prune` | Same as `git worktree prune`, plus journal cleanup and a sweep of the receipts older than 30 days. | done |
| `gh klon pr <branch>` | `gh pr create` from that tree. | done |
| `gh klon sync <branch> [--merge\|--onto <base>\|--fresh\|--all\|--check] [--force] [--json]` | Fetch, then fast-forward or rebase. `--check` is a dry run through `merge-tree`. | done. `--json` prints one document per klon, one per line. |
| `gh klon merge <branch> [--no-ff\|--ff-only] [--keep] [--no-check] [--json]` | Fetch, `pre_merge` hook, the `check` receipt gate, structured merge, advance base, remove. Never pushes. | done. Where `.klon.toml` names `[proof] steps`, the branch tip needs a passing receipt; `--no-check` skips that gate. |
| `gh klon check <branch> [--json]` | Run the approved `[proof] steps` at a clean HEAD and record a receipt under `<common>/klon/receipts/<commit>.json`. | done |
| `gh klon claim <branch> <paths...> [--release] [--json]` | Record the paths a klon owns. A second klon cannot take an overlapping path. `list` flags an overlap and `check` names every change outside the claims. | done |
| `gh klon run <branch> -- <cmd...>` | Execute inside the envelope: fence, scope, env. | done on Linux; macOS gets its fence and scope in v0.4 |
| `gh klon shell <branch>` | An interactive shell inside the envelope. | done on Linux; macOS gets its fence and scope in v0.4 |
| `gh klon stop <branch>` | Kill the whole process tree of that klon. | done |
| `gh klon up [--no-spare] [--json]` | In golden: fetch, `merge --ff-only`, approved `[warm] steps`, then a new spare. | done. It refuses a dirty golden, a golden off `base`, and a golden that diverged from the remote. |
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

## ext4: `gh klon init --volume`

ext4 has no snapshot, so `add` copies bytes there. `gh klon init --volume` gives
an ext4 laptop the same 5 ms clone without a partition, without a reformat, and
without a password:

```sh
gh klon init --volume 60G --yes   # builds the volume and moves golden onto it
gh klon init --volume --undo      # moves golden back and removes the volume
```

klon creates a sparse image under `~/.local/share/klon/`, formats it with
`mkfs.btrfs -L klon-<repo>`, and asks udisks to attach and mount it. The image
seeds one directory that you own, because the mount root itself belongs to
`root`. Golden moves into it as a subvolume, and a symlink takes golden's old
path, so every absolute path in git, in a build cache, and in your shell history
still resolves. `git worktree repair` writes every worktree path out again.

The image costs no disk space until klon fills it, so ask for more than you need.

`init --volume` refuses a host that would ask for a password. udisks grants the
attach and the mount to an **active local session** only, so an ssh session and
a headless runner are refused with one line. It refuses a dirty golden with
`dirty`, a repository that already has a volume, and a host without
`btrfs-progs`, where it prints the install line. Each refusal changes nothing.

After a reboot the volume is down and golden's symlink points at nothing. The
next `gh klon add` attaches and mounts the image again, which takes about a
second and no password. No shell can enter a dangling symlink, so run that
command from the directory above golden: klon finds the repository, brings the
volume up, and works in it.

`--undo` copies golden back to its old filesystem, empties the volume, and
detaches it. It refuses while klons live on the volume, because they go away
with it; `--force` removes them. klon releases the loop device and deletes the
image only when udisks reports this user as its owner. Otherwise the image stays
and klon prints the `rm -f` line for it, because a release of a foreign loop
device raises a password dialog.

klon never bundles `mkfs.btrfs`: `btrfs-progs` is GPLv2 and every distribution
packages it. Set `$KLON_BTRFS_TOOLS` to use an unpacked copy without root.

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

The `[merge]` table says how `merge` joins a branch to base. `no-ff` is the
default and writes a merge commit; `ff-only` refuses a branch that needs one.
The `--no-ff` and `--ff-only` flags win over the key.

```toml
[merge]
ff = "no-ff"                     # or "ff-only"
```

## `merge`

`gh klon merge <branch>` lands a klon's branch in base and removes the klon.
It never pushes. The steps run in this order, and each one refuses before the
next one changes anything:

1. Refuse a locked klon, a golden that holds a merge that stopped, a dirty
   golden, a dirty klon, and a golden that is not on `base`.
2. `git fetch origin`. A repository with no `origin` remote draws one line.
3. Run the merge gate. It has two halves, and both can apply. The executable
   `<klon>/.klon/hooks/pre_merge` runs inside the klon under the envelope, and
   its failure prints `pre_merge failed: <cmd>`. Then, where `.klon.toml` names
   `[proof] steps`, the branch tip that step 5 lands needs a passing `check`
   receipt; see `check` below. `merge` does not run the steps itself. The gate
   names that tip and never the klon's live HEAD, because a hook can detach the
   klon between the check and the merge. klon reads the tip before and after
   the gate and refuses a tip that moved, so the commit that lands is the
   commit the gate proved.
4. Configure the mergiraf merge driver when `mergiraf` is on PATH. klon writes
   `merge.mergiraf.driver` to the repository config and two generated lines to
   `<common>/info/attributes`. A host without mergiraf keeps git's line merge,
   and klon drops its own generated lines and keys again.
5. Merge the commit the gate proved. On a conflict klon prints the conflicting
   paths and aborts, so golden stays where it stood. A merge that a `commit-msg`
   hook refuses is aborted too: git leaves that one with no conflicting path.
6. Remove the klon. `--keep` skips the removal, and a klon with a live process
   stays with one line. The branch itself stays: `rm --merged` deletes that.
   From step 5 on, a failure costs the removal and never the command: the merge
   is in golden's history, and no report may call that a failure.

`merge` sets `merge.conflictStyle` and `rerere.enabled=true` in the repository
config. `zdiff3` needs git 2.35; an older git gets `diff3`, because it rejects
the value it does not know and every merge in the repository would then fail.

## `check` and the receipt

`gh klon check <branch>` runs the approved `[proof] steps` inside the klon and
records what happened. `merge` reads that record instead of running the steps
again, so a long test suite runs once, when the agent asks for it.

```toml
[proof]
steps = ["cargo fmt --check", "cargo clippy --all-targets -- -D warnings", "cargo test"]
```

```sh
gh klon check feature        # run the steps, write the receipt
gh klon check --json feature # the same, as one klon.check/1 document
gh klon merge feature        # reads the receipt; refuses without a passing one
gh klon merge --no-check feature   # land it without a receipt
```

`check` refuses a dirty klon and writes nothing: a receipt names a commit, and
work outside that commit would make the receipt a lie. It refuses a repository
with no `[proof] steps`. Each step runs as `sh -c` inside the klon under the
envelope, in file order, and the run stops at the first failure. The steps need
one approval per `.klon.toml` content hash, the same as the `[warm] steps`.

A test suite takes minutes, and the agent that owns the klon can commit inside
that window. `check` reads HEAD before the first step and again after the last
one, and it writes nothing when the two differ: the steps saw two trees and
prove neither commit.

The receipt lands at `<common>/klon/receipts/<commit>.json`:

```json
{
  "version": 1,
  "commit": "b1946ac92492d2347c6235b4d2611184",
  "tree": "4b825dc642cb6eb9a060e54bf8d69288",
  "branch": "feature",
  "steps_hash": "9f86d081884c7d659a2feaa0c55ad015",
  "results": [{ "cmd": "cargo test", "status": "pass", "duration_ms": 8421 }],
  "status": "pass",
  "duration_ms": 8433,
  "created": "2026-09-06T10:00:00Z",
  "claim_escape": []
}
```

**A receipt holds no environment values.** No variable, no working directory,
and no host name reaches the file. Only the step text from `.klon.toml` is
recorded, so the file is safe to read, to copy, and to show.

`merge` refuses with one of three lines:

| Line | What happened |
|---|---|
| `receipt missing` | Nothing has checked the branch. Run `gh klon check <branch>`. |
| `receipt stale` | The klon committed after the check, or the `[proof] steps` changed. Run the check again. |
| `receipt failed` | The steps ran and one of them failed. Fix the branch, then check again. |

## `claim` and owned paths

Two agents in two klons edit one repository. Nothing stops them from touching
one file, and the conflict then appears at the merge, hours later. A claim
moves that discovery to the front.

```sh
gh klon claim feature src/api docs/api.md   # take two paths
gh klon claim feature --release docs/api.md # give one back
gh klon claim feature --release             # give every path back
```

A claim names a path inside the klon, relative to the klon root. Two paths
conflict when they are equal, or when one is a prefix of the other at a
component boundary: `src/app` conflicts with `src/app/main.rs`, and `src/app`
does not conflict with `src/apple`. The check and the append run under one
exclusive `flock` on `<common>/klon/claims.lock`, so of two commands that want
one path exactly one succeeds:

```
klon: claim conflict: src/app/main.rs held by feature
```

`claim` refuses a path with a `..` component, an empty path, a path with a
symlinked ancestor, and an absolute path outside the klon. The table lives at
`<common>/klon/claims.json`:

```json
{
  "version": 1,
  "claims": [
    { "klon": "feature", "path": "src/api", "kind": "dir",
      "created": "2026-09-06T10:00:00Z" }
  ]
}
```

`list` adds a column with the number of owned paths, and a `!` when one of them
is also owned by another klon. Only a hand edit can reach that state, because
the append refuses the pair.

`check` names every path the klon changed against base that no claim of the
klon covers, and records the list in the receipt:

```
klon: claim escape: src/other/main.rs
```

A klon that claimed nothing owns nothing, so nothing it changed escapes. An
escape does not fail the check; it tells the agent that its work left the paths
it announced. `rm` and `merge` release the claims of the klon they remove. A
hibernated klon keeps its claims: its work comes back, and the paths it owns
must still be its own when it does.

`list` shows the verdict in a receipt column before the radar columns: `✓` for
a passing receipt of the klon's HEAD, `✗` for a failed one, `stale`, and `-`
where the repository names no `[proof] steps` or nothing has checked the
branch. `list --json` carries the same answer in the `receipt` field as
`"pass"`, `"failed"`, `"stale"`, or null.

Receipts are keyed by commit, so a repository collects one file per checked
commit. `gh klon prune` removes every receipt older than 30 days.

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
| `init --volume` `planned`, `attached`, or `copied` | Delete the staged copy on the volume. Golden never moved. The image stays, and the report names it. |
| `init --volume` `swapped`, golden missing | Rename `<golden>.klon-old` back to golden, then delete the staged copy. |
| `init --volume` `swapped` or `ready`, golden is a symlink | Finish: write the volume record, repair the worktrees, delete the replaced copy. |
| `init --volume --undo` `swapped` or `ready`, golden is a directory | Drop the volume record and name the image. Golden is off the volume. |

A repair never deletes a volume image. It can hold the only copy of a path in
the window where the repair cannot tell what landed, so the report prints its
path and a person decides.

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

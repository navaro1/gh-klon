# gh klon

`gh klon` is a `git worktree` replacement. It spawns a warm copy of a project for each coding agent.
A klon holds the tracked files of its branch plus the ignored files of golden, the main checkout.
The design is in `docs/klon-handoff.md`. The build order is in `docs/klon-spec.md`.

## Status

C0 is done: `gh klon add <branch> [--path <p>]` with the `copy` backend for an existing local branch.
C10 is done: the `.klon.toml` loader, the command approval gate, and `gh klon up [--yes]`.
C3 is done: `gh klon rm`, `gh klon prune`, and `gh klon list`.
C4 is done: the journal, `gh klon doctor [--json] [--repair]`, and `--json` on four commands.

## Install for local development

Run this line from the repository root:

```sh
cargo build --release && ln -sf target/release/gh-klon gh-klon && gh extension install .
```

`gh` needs the executable in the repository root. `/gh-klon` is in `.gitignore`.
Check the install with `gh klon --version`.

## Use

```sh
gh klon add feature                       # creates ../<repo>.wt/feature
gh klon add feature --path /some/empty/dir
gh klon up --yes                          # runs the approved [warm] steps in golden
```

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

`doctor` reports the git version, the filesystem of golden, `btrfs-progs`, the two
inotify limits, `make`, `ninja`, and `pasta`. Each host feature reports `present`,
`absent`, or `broken` with a reason. A feature that this host does not have never
stops a command.

`--repair` moves each entry to the prior valid state:

| Operation and state | Action |
|---|---|
| `add` `planned` | Delete the entry. Unregister the worktree when git already registered one. |
| `add` `registered` or `cloned` | Unlock the worktree, remove it with force, delete the entry. |
| `add` `checked-out` | Unlock the worktree, delete the entry. The klon stays. |
| `add` `ready` | Delete the entry. The klon is complete. |
| `rm` `removing` | Delete the `.git` file in the trash copy, run `git worktree prune`, delete the entry. |
| `rm` in any earlier state | Delete the entry. The klon stays. |

An entry with an unknown `version` fails closed: `doctor` exits non-zero with
`unknown journal version` and changes nothing. Upgrade klon in that case.

## JSON output

`--json` makes `add`, `list`, `rm`, and `doctor` print one JSON document on stdout
instead of the human report. Each document carries a `schema` field:
`klon.add/1`, `klon.list/1`, `klon.rm/1`, and `klon.doctor/1`. An error still goes
to stderr as text and keeps the same exit code.

```sh
gh klon add --json feature
{"schema":"klon.add/1","path":"/w/repo.wt/feature","branch":"feature","head":"1a2b...","backend":"copy","duration_ms":812}
```

A new field may appear in a later version. A removed or retyped field bumps the
version suffix. `tests/schema.rs` holds the documented field set of each schema.

## Known blind spot

With `core.checkStat=minimal`, git compares only the size and the mtime of a file.
An edit that keeps the byte count and the mtime is invisible to `git status`.
Build tools change the mtime, so the risk is small.

## Tests

```sh
cargo test
```

The tests generate a 10k-file fixture in a temporary directory. They need `git` on `PATH`.

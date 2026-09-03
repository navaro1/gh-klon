# Klon Evidence Record

Date: 2026-09-03
Purpose: This record supports the decisions in [klon-handoff.md](klon-handoff.md).

No result in this record measures Klon.

## Evidence Labels

- **Fact:** A primary source or inspected source tree directly supports the statement.
- **Published claim:** A project or paper reports the result, but this review did not reproduce it.
- **Inference:** The conclusion combines facts and needs a Klon experiment.

## Research Snapshot

| Project | Inspected state | License | Primary source |
|---|---|---|---|
| Git | Current online manual | GPL-2.0 | [Worktree manual](https://git-scm.com/docs/git-worktree) |
| Worktrunk | v0.76.0 tag `de0052d8db917e46c1663979d376eff3f90c8119`; main `ce57d2d9972fd7d31763ed168a34f17680f1dccb` | MIT or Apache-2.0 | [Release](https://github.com/max-sixty/worktrunk/releases/tag/v0.76.0), [inspected main](https://github.com/max-sixty/worktrunk/tree/ce57d2d9972fd7d31763ed168a34f17680f1dccb) |
| git-sprout | v0.1.0 tag `b82cb66c28eb4bd6ab07af82e8bd881bbfdd7298`; main `fc5c052457694a197f0b37f9d34457efd181a14d` | MIT | [Release](https://github.com/alltuner/git-sprout/releases/tag/git-sprout-v0.1.0), [inspected main](https://github.com/alltuner/git-sprout/tree/fc5c052457694a197f0b37f9d34457efd181a14d) |
| gh-wt | `8e8f7b128de967494fdbcc35456ec5401875dd12` | MIT | [Inspected commit](https://github.com/HikaruEgashira/gh-wt/tree/8e8f7b128de967494fdbcc35456ec5401875dd12) |
| Grove | `1b0daaadb3e36552dd0e797e2082e8675466cae0` | Apache-2.0 | [Inspected commit](https://github.com/chrisbanes/grove/tree/1b0daaadb3e36552dd0e797e2082e8675466cae0) |
| Cow | v0.1.10 at `97b3e03cc952f21979cb9d13002e2d1f41906030` | MIT | [Inspected commit](https://github.com/joeinnes/cow/tree/97b3e03cc952f21979cb9d13002e2d1f41906030) |
| git-lazy-mount | `ecb969a95fdfe714b69e28d3df07292f238a52fd` | MIT or Apache-2.0 | [Inspected commit](https://github.com/mohsen1/git-lazy-mount/tree/ecb969a95fdfe714b69e28d3df07292f238a52fd) |

The review used release metadata and source trees available on the record date.

No release asset checksum belongs to this research snapshot.

Future benchmark reports must pin one exact artifact and its checksum.

## Git Baseline

**Fact:** A linked worktree shares repository data but has its own `HEAD` and index.

**Fact:** `git worktree add --no-checkout --lock` reserves a locked linked worktree without checkout.

**Fact:** Git says that `--lock` avoids the race from a separate lock command.

**Fact:** Git provides stable machine output through `list --porcelain -z`.

**Fact:** Git refuses removal of an unclean worktree without force.

These facts support handoff requirement R2.

Source: [Git worktree manual](https://git-scm.com/docs/git-worktree).

## Competitive Analysis

### Worktrunk

**Fact:** Worktrunk creates and manages standard Git worktrees.

**Fact:** It supports hooks, pull-request state, process control, port selection, and background cleanup.

**Fact:** `wt step copy-ignored` copies ignored files between worktrees.

**Fact:** Its default policy copies all ignored files except built-in exclusions.

**Fact:** `.worktreeinclude` can narrow that set.

**Fact:** `--require-include` makes the include file mandatory.

**Fact:** Its cache copy uses guarded per-file reflink or regular copy.

**Fact:** Its directory walk uses four Rayon worker threads.

**Published claim:** A 14 GB cache copy takes 20 seconds instead of two minutes.

**Published claim:** The reused cache reduces a first Rust build from about 68 seconds to about three seconds.

The published example does not state the machine, file count, run count, or raw samples.

These facts support handoff requirement R4 and the Worktrunk measurement method.

Sources: [copy command and claims](https://github.com/max-sixty/worktrunk/blob/ce57d2d9972fd7d31763ed168a34f17680f1dccb/src/cli/step.rs), [copy implementation](https://github.com/max-sixty/worktrunk/blob/ce57d2d9972fd7d31763ed168a34f17680f1dccb/src/copy.rs).

### git-sprout

**Fact:** git-sprout creates a real linked worktree through `git worktree add --no-checkout`.

**Fact:** It plans a checkout, creates a target-stat index, and lets Git complete the worktree.

**Fact:** It then runs the standard post-checkout hook.

**Fact:** Its differential fixtures cover filters, Git LFS, submodules, and sparse checkout.

**Fact:** They also cover split indexes, SHA-256, case collisions, hooks, and signals.

**Published claim:** A Linux kernel fixture uses 36 MB instead of 1,816 MB for 95,299 files.

**Published claim:** A 250 MB fixture takes 0.21 seconds instead of 0.85 seconds.

**Fact:** The current committed result marks itself as provisional and untrustworthy.

**Fact:** That result also states `baseline_only: true` and `differential_verified: false`.

Both compared columns therefore used Git in the committed result.

The committed result cannot support a verified comparative claim.

**Inference:** An upstream API can cost less to maintain than a source port.

Sources: [architecture](https://github.com/alltuner/git-sprout/blob/fc5c052457694a197f0b37f9d34457efd181a14d/README.md), [result status](https://github.com/alltuner/git-sprout/blob/fc5c052457694a197f0b37f9d34457efd181a14d/bench/results.json), [benchmark contract](https://github.com/alltuner/git-sprout/blob/fc5c052457694a197f0b37f9d34457efd181a14d/bench/README.md).

### gh-wt

**Fact:** gh-wt clones an APFS reference tree with parallel `clonefile` calls.

**Fact:** Its benchmark uses an Apple M3, macOS 26.3, and 174,295 tracked files.

**Fact:** The fixture has a reported 2.42 GiB worktree size.

**Published claim:** Five fixed-order runs give a 15.52-second Git add mean.

**Published claim:** The same report gives 19.16 seconds warm and 58.93 seconds cold for gh-wt.

**Published claim:** Five same-tree workspaces allocate 0.24x the Git baseline space.

**Published claim:** The marginal allocation slope is about 28x smaller than Git.

**Fact:** The direct clone phase still costs about 13 seconds in that fixture.

**Inference:** This report gives the strongest inspected APFS measurement set.

Sources: [benchmark report](https://github.com/HikaruEgashira/gh-wt/blob/8e8f7b128de967494fdbcc35456ec5401875dd12/docs/benchmark.md), [APFS backend](https://github.com/HikaruEgashira/gh-wt/blob/8e8f7b128de967494fdbcc35456ec5401875dd12/lib/worktree.sh).

### Grove

**Fact:** Grove uses `cp -c -R` for its default APFS directory clone.

**Fact:** Its experimental mode uses a base sparse bundle and one shadow file per workspace.

**Published claim:** Grove says copy-on-write workspace creation takes less than one second.

The inspected repository does not publish a reproducible sample set for that claim.

**Fact:** Linux support remains outside its current base design.

Sources: [project description](https://github.com/chrisbanes/grove/blob/1b0daaadb3e36552dd0e797e2082e8675466cae0/README.md), [image backend](https://github.com/chrisbanes/grove/tree/1b0daaadb3e36552dd0e797e2082e8675466cae0/internal/image).

### Cow

**Fact:** Cow can clone a full APFS checkout through `clonefile`.

**Published claim:** Its documentation reports about 130 milliseconds for a 2 GB repository.

The inspected repository does not include a reproducible benchmark for that number.

**Fact:** Its linked-worktree mode delegates to normal Git worktree creation.

That mode does not keep the full-tree clone advantage.

**Fact:** The inspected continuous-integration run failed.

Sources: [project source](https://github.com/joeinnes/cow/tree/97b3e03cc952f21979cb9d13002e2d1f41906030), [inspected CI run](https://github.com/joeinnes/cow/actions/runs/23318762715).

### git-lazy-mount

**Fact:** git-lazy-mount uses FUSE and currently targets Linux.

**Published claim:** Its 20-repository setup uses 5.5x less disk and takes 3.0x less time than shallow clones.

**Fact:** Its top-level and detailed reports give different agent-task totals.

One report gives 19 of 20 wins and 2.57x.

The other gives 20 of 20 wins and 3.17x.

The conflicting totals cannot support a verified comparative claim.

Sources: [top-level claims](https://github.com/mohsen1/git-lazy-mount/blob/ecb969a95fdfe714b69e28d3df07292f238a52fd/README.md), [detailed benchmarks](https://github.com/mohsen1/git-lazy-mount/blob/ecb969a95fdfe714b69e28d3df07292f238a52fd/benchmarks/README.md).

### Competitive conclusion

**Fact:** Whole-directory clone behavior already exists in several tools.

**Fact:** Parallel per-file clone behavior also exists.

**Inference:** Neither behavior alone gives Klon a new product category.

**Inference:** Warm generations, path claims, and content receipts can change the developer outcome.

This review found no inspected tool that combines those parts with ordinary linked worktrees on macOS and Linux.

This statement is a research inference.

It is not a patent or novelty opinion.

## Operating-System Correctness

### macOS

**Fact:** APFS clone calls create private copy-on-write files.

**Fact:** Apple strongly discourages `clonefile` for a directory hierarchy.

**Fact:** Apple directs directory clone users to recursive `copyfile` behavior.

**Fact:** A recursive copy is not an atomic directory snapshot.

**Fact:** Apple defines the result as undefined when the source hierarchy changes during traversal.

These limits support the immutable generation source in the handoff.

Sources: [Apple APFS APIs](https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/APFS_Guide/ToolsandAPIs/ToolsandAPIs.html), [clonefile manual](https://keith.github.io/xcode-man-pages/clonefile.2.html), [copyfile manual](https://keith.github.io/xcode-man-pages/copyfile.3.html).

### Linux

**Fact:** `FICLONE` clones one complete regular file.

**Fact:** Source and destination must use the same file system.

**Fact:** Each file clone is atomic against concurrent file writes.

**Inference:** A directory walk of many atomic file clones is not one atomic tree snapshot.

These limits support handoff requirements R5 and R13.

Source: [Linux FICLONE manual](https://man7.org/linux/man-pages/man2/FICLONE.2const.html).

### Shared safeguards

**Fact:** A writable hardlink shares one inode.

An in-place write, truncation, or mode change can alter the source object.

These limits support handoff requirements R5, R13, and R14.

APFS reports use volume free-block deltas.

Linux reports use available allocated or exclusive-byte interfaces.

`du` alone cannot prove copy-on-write savings.

## Research Transfer

| Research | Reported evidence | Klon transfer | Limit |
|---|---|---|---|
| [STORM](https://arxiv.org/abs/2605.20563) | +18.7 on Commit0-Lite and +1.4 on PaperBench against a worktree baseline. | Run a Klon claim ablation. | This analogy does not validate Klon claims. |
| [AgentRoom](https://arxiv.org/abs/2608.23740) | The abstract assigns the main benefit to coordination. | Keep claims and explicit workspace state. | Klon does not adopt a shared conflict-free file system. |
| [StagedWorkspace](https://arxiv.org/abs/2608.18050) | Parsed views and review diffs bind to content hashes. | Bind proof receipts to exact Git trees. | The domain includes more than source repositories. |
| [Riker](https://www.usenix.org/conference/atc22/presentation/curtsinger) | Median initial overhead is 8.8 percent, with 94 percent of Make speedup retained. | Verify reuse without stale build results. | Riker is a build system, not a workspace tool. |
| [Bazel CI study](https://arxiv.org/abs/2405.00796) | Long builds report median gains of 4.22x and 4.71x from two cache types. | Optimize first proof, not only checkout. | The study data does not measure Klon. |
| [Nix store model](https://edolstra.github.io/pubs/nspfssd-lisa2004-final.pdf) | Immutable input-addressed objects support atomic generation changes. | Use one small immutable warm generation. | Klon does not adopt the full Nix model. |
| [Agent pull-request study](https://arxiv.org/abs/2607.04697) | Cross-agent pairs report 41.7 percent conflicts against 19.8 percent within one agent. | Measure claim escape and integration waste. | The paper does not prove path claims prevent semantic conflicts. |
| [CrashMonkey](https://www.usenix.org/conference/osdi18/presentation/mohan) | Fault exploration finds file-system crash errors through bounded workloads. | Stop each state change in fault tests. | Klon tests application transactions, not a file system. |

## First-Principles Conclusions

**Inference:** At high concurrency, repeated proof work and integration waste can exceed checkout cost.

These findings support the handoff metrics, warm generations, path claims, and content receipts.

## Deferred Experiments

These experiments require new evidence before product adoption:

- direct APFS directory clone
- APFS image shadows
- btrfs subvolume snapshots
- OverlayFS and composefs views
- git-sprout tracked-state integration
- FUSE lazy materialization
- host-wide build job control
- process and network isolation

Each implementation decision must satisfy the reimplementation rule in the handoff.

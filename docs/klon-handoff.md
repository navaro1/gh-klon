# Klon: Evidence-Gated Agent Workspaces

Date: 2026-09-03
Status: Ready
Scope: A portable local workspace system for independent software agents on macOS and Linux.

## 1. Objective & Non-Goals

### Objective

Klon will maximize accepted, verified changes per developer hour.

Klon will make each agent workspace warm, isolated, and proven.

- **Warm:** The first required proof reuses safe build state.
- **Isolated:** A Klon file write cannot change another worktree or generation file.
- **Proven:** A content-bound receipt records the exact state and successful proof set.

The priority order is correctness, developer time, portability, resource cost, and raw spawn speed.

### Reimplementation rule

Klon will use an existing executable, library, or upstream API first.

Klon can reimplement a part only when evidence proves one condition:

1. The new part improves a primary metric by at least 2x on two representative fixtures.
2. The existing part cannot meet a required correctness or portability property.

The decision record must include raw evidence, maintenance cost, and a removal plan.

### Non-goals

- Klon will not replace Git object storage, references, hooks, or branch rules.
- Klon will not merge source code through a shared conflict-free file system.
- Klon will not require a permanent manager process.
- Klon will not promise semantic conflict prevention.
- Klon will not require administrator access, FUSE, systemd, or a network namespace.
- Klon will not rewrite unknown cache files or database files.
- Klon will not optimize remote agent orchestration in the first release.

## 2. Context & Sources (grounding)

### Current evidence

The detailed source record is in [klon-evidence.md](klon-evidence.md).

The evidence gives these decisions:

- Git must remain the authority for linked worktree state.
- Safe warm state gives more value than another tracked-file copy method.
- A writable hardlink fallback is unsafe.
- macOS and Linux need different clone adapters behind one contract.
- A Linux file clone does not create a point-in-time directory snapshot.
- A macOS directory clone can race with source changes.
- A verified connector must check claims at integration because raw tools can bypass them.
- Receipts must bind proof results to content hashes.
- Published competitor timings need local reproduction before Klon uses them as facts.

### Product transition

Existing tools mostly optimize workspace creation or terminal workflow.

Klon will optimize the full path from workspace request to accepted proof.

The phase transition replaces a copied directory with a verified workspace transaction.

The transaction combines a Git worktree, warm state, path claims, and a proof receipt.

### Terms

- **Proof:** A required command that checks a change.
- **Proof set:** The ordered commands and relevant configuration for one receipt.
- **Claim:** An exact file path or directory prefix that one active workspace owns.
- **Generation:** An immutable and proven warm-state tree.
- **Receipt:** A record that binds successful proofs to exact workspace content.
- **Git oracle:** The result from the installed Git command for the same applicable operation.

### Assumptions

- The host has Git 2.34.1 or later.
- A repository can contain untrusted files and hooks.
- Agent tools can write outside the Klon command path.
- The first release uses Rust and ships as a GitHub CLI extension.

### Trust boundary

Klon protects its managed paths and validates content at each contract boundary.

Klon does not sandbox an unrestricted same-user process in the portable core.

Such a process can bypass a claim, a receipt check, or a raw Git policy.

The receipt guarantee applies only to Klon proof and verification commands.

Linked worktrees share Git objects, references, repository configuration, and hooks.

Klon provides working-file isolation, not full repository-state isolation.

### Support floor

The core supports macOS 13 or later on arm64 and x86-64.

The core supports Linux kernel 5.15 or later on arm64 and x86-64.

Linux builds support glibc 2.31 or later and musl 1.2 or later.

Git 2.34.1 is the minimum supported Git version.

The release suite tests each minimum and each current stable version.

## 3. Requirements & Acceptance Criteria

### Functional requirements

| ID | Requirement | Acceptance criterion |
|---|---|---|
| R1 | `gh klon add` creates an ordinary linked Git worktree. | Git commands recognize the result without a Klon process. |
| R2 | Installed Git owns administrative records, hooks, and branch rules. | Differential tests match the Git oracle for each supported case. |
| R3 | Core commands work on each supported macOS and Linux target without optional services or administrator access. | The same core suite passes at each stated support floor. |
| R4 | Warm cache copy uses only trusted allowed entries from an immutable generation. | Tests detect no nonignored file leak or unapproved policy. |
| R5 | Clone failure uses a regular copy or Git fallback. | No fallback creates a shared writable inode. |
| R6 | Each warm generation is immutable and remains pinned while a workspace uses it. | A failed publish or collection cannot damage an active generation. |
| R7 | Each command that changes state uses a durable transaction record and an idempotent repair path. | Each injected fault reaches the prior or completed state. Only verified external corruption permits diagnosis alone. |
| R8 | Path claims reject exact and directory-prefix overlap as one atomic operation. | Exactly one conflicting concurrent claim succeeds. |
| R9 | `gh klon ready` rejects changed paths outside active claims. | A claim escape prevents a receipt. |
| R10 | `ready` creates a content-bound receipt only after proofs pass on one committed snapshot. | A failed proof or snapshot difference creates no valid receipt. |
| R11 | A receipt becomes stale after a relevant state change. | A base, commit, generation, claim, proof, or execution-manifest change invalidates it. |
| R12 | Each shipped command has versioned JSON output. | Schema tests reject an undocumented incompatible change. |
| R13 | A target-path operation probe selects each clone capability. | The command reports the selected adapter and fallback reason. |
| R14 | Klon reports only reproducible performance results. | Each claim links to raw samples and full environment data. |
| R15 | Removal validates the managed path and refuses dirty work by default. | Tests prove that Klon cannot delete an unowned or dirty path silently. |
| R16 | Receipts contain no secret environment values or full command output. | Redaction tests detect no configured secret canary. |
| R17 | Core operation needs no permanent Klon manager. | A new process can inspect and repair all durable state. |
| R18 | Every durable Klon format has a version and a safe migration contract. | Unknown future versions fail closed, and interrupted upgrades restore valid state. |
| R19 | Each cache type has a provider contract for relocation and dependency validity. | Unknown cache formats and failed provider proofs cannot enter a generation. |
| R20 | Integration consumers use only a fresh verified receipt and its exact commit. | A stale receipt, changed claim, or different commit prevents verified consumption. |
| R21 | Each workspace has one trusted and immutable base commit. | `ready` and `verify` reject a caller-selected or changed base. |

### Primary metrics

Klon has four primary metrics.

They measure the local system and do not decide whether a reviewer accepts a change.

Product studies separately measure accepted merged changes per developer hour.

| Metric | Definition | Direction |
|---|---|---|
| `Correctness_errors` | Observable differences from the Git oracle in the applicable differential matrix. | Zero |
| `T_first_proof` | Time from the add request to the first successful required proof. | Lower |
| `Throughput_N` | Valid ready receipts per hour for a fixed trace at concurrency N. | Higher |
| `Disk_delta_N` | New physical allocation for N workspaces. | Lower |

`T_ready` starts at the add request and stops when Klon creates a valid receipt.

Diagnostic metrics include `T_ready`, first status time, CPU, RSS, I/O, and waste ratio.

Other diagnostic metrics include interactive p99 latency, write amplification, recovery p95, and developer steps.

The benchmark also records break-even N and claim escape count.

### Measurement method

- Fixtures contain 10,000, 100,000, and 500,000 tracked files.
- Cache fixtures contain 250 MB, 2 GB, and 14 GB of allowed ignored state.
- Each fixture manifest fixes file count, size distribution, depth, links, metadata, and content hashes.
- Branch changes contain 0, 10, 1,000, and 10,000 changed paths.
- Each target cell uses at least 100 independent warm runs and 100 independent cold runs.
- An exploratory cell can use 30 warm runs and 10 cold runs.
- An exploratory cell cannot pass a p95 target.
- The harness uses a random order and records that order.
- Reports show p50, p95, and a 95% bootstrap confidence interval.
- Interactive p99 uses at least 10,000 latency samples.
- The harness records hardware, operating system, file system, mount options, and Git version.
- The harness also records tool commits, fixture hashes, commands, and raw samples.
- A correctness mismatch invalidates all performance results for that cell.
- A warm unchanged proof must compile zero units and download zero bytes.
- A warm unchanged proof must also restore zero remote cache entries.
- The harness records generation cost and the break-even workspace count.
- Disk delta includes generations, reference trees, indexes, receipts, and pending collection.
- Deterministic trace replay is the primary workload.
- A fixed agent and model configuration is a secondary workload.

A versioned benchmark manifest fixes every fixture, trace, timer, baseline, and compatibility rule.

It also fixes the target statistic, confidence rule, and failure rule before a measured run.

The external trace runner gives each tool the same final tree, cache input, and proof commands.

It records command return, cache readiness, first proof, and full trace as separate times.

A lower-is-better target passes when its 95% ratio interval stays below the stated limit.

A higher-is-better target passes when its 95% ratio interval stays above the stated limit.

Latency targets use p95 unless the target states another statistic.

Throughput and disk targets use the median.

### Benchmark targets

Klon compares only compatible, reproduced configurations.

| Baseline | Target |
|---|---|
| Git | `T_first_proof` is at most 0.50x on two cache-heavy fixtures per operating system. |
| Git | `Throughput_4` is at least 2.00x on two fixed task suites. |
| Git | `Disk_delta_4` is at most 0.50x on clone-capable file systems. |
| Git | `Correctness_errors` is zero. |
| Worktrunk | `T_first_proof` is at most 0.50x on one ignored-metadata fixture. |
| Worktrunk | The `T_first_proof` compatible-fixture geometric mean is at most 1.00x. |
| git-sprout | Tracked-state differential parity is equal. |
| git-sprout | Tracked-only add time is at most 1.10x on eligible fixtures. |
| git-sprout | `T_first_proof` is at most 0.50x on two cache-heavy fixtures. |
| Fastest compatible APFS tool | `T_first_proof` is at most 0.50x before a spawn phase-change claim. |
| Best compatible competitor | `Throughput_4` is at least 1.50x on two task suites. |
| No-agent host | Interactive p99 latency is at most 1.25x. |

Worktrunk tests measure both pre-start and post-start workspace states.

Git tests include default and tuned parallel checkout settings.

Release reports pin competitor binaries and source commits.

The manifest defines compatibility as safe support for the exact platform and fixture.

The first manifest uses `rust-100k-14g-v1` and `node-100k-2g-v1` as cache-heavy fixtures.

It uses `edit-proof-small-v1` and `cross-file-proof-v1` as throughput task suites.

Klon can release without a phase-change claim.

Klon can make that claim only after all related gates pass.

## 4. Design (HOW)

### Components

Klon starts with four concrete components.

1. The Git wrapper delegates repository state changes to installed Git.
2. The cache engine builds generations and copies allowed warm state.
3. The state store records claims and receipts under the common Git directory.
4. The proof runner executes required commands and returns filtered results.

The cache engine has separate policy, copy backend, and generation store interfaces.

An operating-system copy adapter selects only the file data copy method.

The state store has separate claim ledger and receipt store interfaces.

Both state interfaces can use one SQLite database.

### Workspace transaction

`gh klon add` uses this transaction:

The trusted local policy names the integration target and its expected object identifier.

`add` records the branch merge base with that target as the immutable workspace base.

`ready` and `verify` do not accept a replacement base argument.

1. Klon validates the final target and writes a small durable transaction marker.
2. Git creates the final linked worktree with its normal checkout and `--lock`.
3. Each checkout hook observes the final path and the normal Git environment.
4. Klon pins and copies allowed ignored state when the request selects a generation.
5. Klon validates the Git state and cache boundaries.
6. Klon unlocks the worktree and removes the marker.

Unsupported option sets delegate directly to Git at the final path without cache acceleration.

Submodule and unsupported path cases also use that path.

Every direct Git fallback stays inside the same marker lifecycle.

The transaction marker contains no repository content.

A repeated command must reach the prior valid state or the completed state.

`doctor` can inspect and repair all known transaction states after it ships.

| Marker state | Durable fact | Recovery result |
|---|---|---|
| `planned` | The target passed validation. | Retry Git add or remove the marker. |
| `git-created` | Git lists a locked final-path worktree. | Validate it or remove it through Git. |
| `cache-restored` | A generation and copy manifest exist. | Verify the manifest or remove the worktree. |
| `validated` | Git and cache checks passed. | Unlock the worktree and remove the marker. |

An explanation is sufficient only when verified external corruption prevents safe repair.

Klon then leaves all uncertain content in place and returns a nonzero status.

Add and removal use marker journals.

Claims use one SQLite transaction.

Receipt publication uses a journal and a SQLite compare-and-set operation.

Generation publication uses a journal and an atomic pointer change.

Lease and collection operations use the repository generation lock.

### Cache policy

The default cache allowlist is empty.

A user-local allowlist is trusted.

A repository allowlist needs explicit consent for its exact content hash.

Klon stores that consent outside the repository.

The portable cache adapter uses regular byte copies from an immutable generation.

The macOS adapter uses recursive `copyfile` clone behavior after a target-path probe.

The Linux adapter uses per-file `FICLONE` after a target-path probe.

Each adapter matches the regular-copy metadata contract within user permissions.

The contract covers file type, content, symlink target, mode, modification time, extended attributes, and access control lists.

The copy walk never crosses a mount boundary.

Each destination regular path gets an inode that is private from the generation.

The contract does not preserve source hardlink groups.

Every backend must produce exact equality with the allowed generation manifest.

Any source or destination mismatch stops the transaction and quarantines the generation.

No adapter uses writable hardlinks.

The default policy excludes these entries:

- `.git` paths and nested worktrees
- Klon state
- sockets, FIFOs, and device files
- nonignored untracked files
- declared database paths without a stopped and consistent provider

The repository can declare cache include and exclude rules.

Klon never applies a generic path rewrite to an unknown artifact.

Each cache type uses a named provider contract.

The contract declares path rules, input dependencies, relocation rules, metadata, and a validation proof.

Klon rejects an unknown cache format or a failed provider proof.

### Immutable warm generation

`gh klon warm` builds a generation in a hidden directory.

The generation identifier binds the base tree, platform, warm command, policy, execution manifest, and actual file manifest.

Klon runs the declared proof before publication.

Klon then restores the candidate into a fresh locked worktree with regular copies.

The provider proof must pass again in that worktree before publication.

Tracked state must still equal the base commit.

No nonignored untracked path can remain.

Klon publishes the generation pointer as one atomic change.

Klon makes each published generation read-only and verifies its manifest before use.

A manifest change marks the generation as corrupt and prevents new use.

An active workspace pins its generation.

The generation store serializes lease acquisition and collection with one repository lock.

Klon acquires the durable lease before any generation access.

Collection removes only unreferenced Klon generations.

Klon never creates a generation from another generation.

### Claims and receipts

Claims and receipts use SQLite under `git rev-parse --git-common-dir`.

Klon introduces SQLite when path claims ship.

Before that release, Git locks and transaction marker files control add operations.

A claim names an exact file or a normalized directory prefix.

The database transaction checks all overlaps before it adds a claim.

Klon probes the target file-system name comparison behavior.

Each claim stores a collision key for case and Unicode-equivalent paths.

Klon rejects a symlink ancestor, an ambiguous alias, or a cross-claim hardlink.

Raw tools can bypass a claim during work.

`ready` therefore checks every changed path against active claims.

`ready` requires a clean worktree at one committed `HEAD`.

It rejects nonignored untracked files and dirty recursive submodule state.

The proof input manifest binds the commit, tree, recursive submodule commits, and selected generation manifest.

It records an explicit `none` value when no generation exists.

It also binds the proof configuration and the execution environment.

`ready` creates a private locked proof worktree at the exact commit.

The proof checkout disables unapproved Git hooks.

An approved setup hook runs as an explicit proof step.

Git materializes each recursive submodule at its exact commit without network access.

An unavailable submodule object prevents a receipt.

It restores the bound generation when present and verifies the complete destination manifest.

Klon hashes every proof-visible input byte and required metadata before execution.

This manifest includes filter output and approved setup output.

The proof policy declares all writable output roots.

The proof runner executes all commands only in that private proof worktree.

A post-proof check rejects a change outside the declared writable output roots.

A journaled compare-and-set publishes the receipt only when all bound inputs still match.

A required Git filter failure prevents a receipt.

Klon rejects an optional filter unless the policy explicitly accepts pass-through behavior.

A receipt contains only these fields:

- schema version and workspace identifier
- base, commit, and tree object identifiers
- recursive submodule state and generation manifest identifier
- claim-set hash and proof-set hash
- proof input manifest hash and execution manifest hash
- ordered proof results with command identifier, status, duration, and output digest
- creation time

Klon does not store full proof output or secret environment values in a receipt.

### Proof execution

The proof runner uses the private proof worktree as its explicit working directory.

It applies the declared environment policy before command start.

It starts each proof set in one cancellable process group.

It can stream output to the user.

It redacts declared secret patterns before it calculates the output digest.

It stores only the filtered result and that digest.

The execution manifest identifies Klon, Git, the platform, each proof tool, and each material runtime.

It records a content digest or an immutable verified package identity for each executable.

It records material dynamic-library identities when they can change proof behavior.

It hashes approved nonsecret environment values.

It uses a keyed digest for an approved secret input and never stores that value.

### Receipt consumption

`gh klon verify <receipt>` rechecks the receipt, claims, generation, and proof policy.

It returns the exact proven commit identifier through versioned JSON.

An integration connector must submit that exact identifier.

A raw Git push or external merge bypasses this contract.

Klon does not claim verified integration for a bypass operation.

### Command surface

- `gh klon add`
- `gh klon list`
- `gh klon warm`
- `gh klon claim`
- `gh klon ready`
- `gh klon verify`
- `gh klon doctor`
- `gh klon rm`

Each command adds a versioned JSON schema when it ships.

The Git wrapper owns the complete workspace removal sequence.

Each later feature adds its cleanup action before that feature ships.

Removal releases claims, removes pins, preserves receipt history, and records an audit event.

Every marker, manifest, pointer, pin, and database schema has a format version.

Klon refuses an unknown future version without a state change.

A database migration uses one transaction and a verified backup.

Fault tests cover interrupted upgrades.

Starting with release three, upgrade tests cover the two prior stable releases.

The first release uses an internal benchmark harness.

It does not expose a public benchmark command or competitor adapter.

### Portable behavior

| Concern | macOS | Linux |
|---|---|---|
| Tracked state | Installed Git | Installed Git |
| Safe cache fallback | Regular byte copy | Regular byte copy |
| Fast cache adapter | Recursive `copyfile` clone | Per-file `FICLONE` |
| Initial transaction state | Marker files | Marker files |
| Claims and receipts | SQLite | SQLite |
| Optional service | None | None |

Direct APFS clone, image shadows, and file-system snapshots remain future experiments.

OverlayFS, composefs, tracked sprout logic, and resource controls also remain future experiments.

Each experiment must pass the reimplementation rule in Section 1.

### Test tiers

Per-change tests run on native macOS and Linux with standard Git.

They cover regular copy, focused differential cases, and relevant fault points.

The per-change suite has a ten-minute p95 limit on the reference CI hosts.

Long compatibility and performance cells run in parallel outside that suite.

Nightly tests cover APFS, ext4, btrfs, and XFS.

They also cover Git variants, path metadata, and concurrency 1, 2, 4, and 8.

Release tests cover macOS and Linux on arm64 and x86-64.

They cover the oldest supported Git and the current Git release.

They also cover SHA-1, SHA-256, reftable, filters, submodules, and the full fault matrix.

The matrix uses pairwise coverage plus explicit high-risk cells.

It does not require every possible combination in each pull request.

## 5. Boundaries

### Ask first

- Ask before Klon changes repository policy or proof commands.
- Ask before Klon uses a database provider or stops a process.
- Ask before Klon removes an active or dirty workspace.

### Destructive safeguards

- Resolve each deletion target to an explicit managed path.
- Reject a repository root, home directory, or unresolved variable.
- Recheck ownership, active state, locks, and dirt immediately before removal.
- Prefer a recoverable move before permanent removal where practical.
- Report each removed object and its recovery status.

## 6. Open Questions

None.

## 7. Chunks and Acceptance Criteria

### Delivery policy

Klon ships an opt-in walking skeleton after C7.

Each later capability stays disabled by default until its own acceptance criteria pass.

Each capability keeps a tested rollback to the prior portable path.

### C0 — Portable Git add
**Status:** `[ ]` pending
**Build:** Create the Rust extension, `add --json`, one oracle fixture, a versioned marker, and final-path Git checkout.
**AC:**
- Git recognizes each result as a valid linked worktree.
- Supported branch rules, hooks, exit status, and diagnostics match the Git oracle.
- Checkout hooks observe the final working directory and normal Git environment values.
- Tests expose shared Git references, objects, configuration, and hooks.
- A Klon working-file write cannot alter another worktree file.
- Add records the trusted integration target and immutable merge-base commit.
- A repeated add reaches the prior valid state or the completed state after a forced stop.
- Each direct Git fallback uses the same marker lifecycle.
- The command ships a versioned JSON schema.
- Core operation needs no optional service or administrator access.
**Depends on:** — · **Traces to:** R1, R2, R3, R7, R12, R17, R18, R21

### C1 — Lifecycle safety
**Status:** `[ ]` pending
**Build:** Add `list`, `doctor`, and safe `rm`. Add each command JSON schema with that command.
**AC:**
- Removal refuses dirty work unless the user gives explicit approval.
- Path checks reject an unowned path and every broad deletion target.
- Repeated `doctor` operations give the same valid result.
- Each injected fault reaches the prior valid state or the completed state.
- Verified external corruption returns an exact diagnosis without a destructive change.
- A new process can inspect all durable state.
**Depends on:** C0 · **Traces to:** R7, R12, R15, R17, R18

### C2 — Internal evidence harness
**Status:** `[ ]` pending
**Build:** Extend the oracle harness. Add seeded fixtures, a versioned benchmark manifest, raw samples, statistics, and environment records.
**AC:**
- The same seed creates the same fixture and trace.
- A differential mismatch invalidates the related timing result.
- Reports contain required sample counts, percentiles, intervals, and environment data.
- A report with missing source data cannot support a performance claim.
- The harness reports all four primary metrics that apply to the current command set.
- The manifest fixes every decision input before a measured run.
- The same external runner controls every compared tool.
**Depends on:** C0 · **Traces to:** R2, R14

### C3 — Proof runner foundation
**Status:** `[ ]` pending
**Build:** Add cancellable proof execution, execution manifests, filtered result records, output digests, and secret-canary tests.
**AC:**
- Cancellation stops the complete proof process group.
- Each result contains a command identifier, status, duration, and output digest.
- Stored results contain no full output or secret environment value.
- The manifest binds content or package identities for Klon, Git, proof tools, runtimes, and material libraries.
- The manifest binds the platform, policy, and approved environment inputs.
- A secret input uses only a keyed digest in durable state.
- A forced stop leaves no successful partial result.
- Library tests run on each support-floor platform.
**Depends on:** C0 · **Traces to:** R3, R7, R10, R16, R17, R18

### C4 — Atomic path claims
**Status:** `[ ]` pending
**Build:** Add SQLite, a claim ledger, file-system collision keys, atomic overlap checks, migrations, and audit records.
**AC:**
- Exactly one of two overlapping concurrent claim requests succeeds.
- Prefix checks respect path component boundaries.
- Claim keys match the target file-system case and Unicode behavior.
- Normalization rejects parent traversal, symlink ancestors, ambiguous aliases, and cross-claim hardlinks.
- `claim --release` and workspace removal record audit events.
- Every claim binds the immutable workspace base.
- A forced stop cannot leave a partial claim.
- Unknown future schemas fail closed.
- Migration tests cover an interrupted migration and all available prior schemas.
- The command ships a versioned JSON schema.
**Depends on:** C1 · **Traces to:** R7, R8, R12, R15, R17, R18, R21

### C5 — Committed proof snapshot
**Status:** `[ ]` pending
**Build:** Add clean-commit validation, recursive submodule checks, claim validation, and a complete proof input manifest.
**AC:**
- Dirty tracked state or a nonignored untracked path prevents readiness.
- A dirty or unavailable recursive submodule prevents readiness.
- Every base-to-commit changed path belongs to an active claim.
- Ready reads the recorded base and accepts no replacement base.
- The manifest binds the commit, tree, recursive submodules, claims, proof policy, and execution environment.
- The first version binds `generation_id` to `none`.
- The operation does not change the developer worktree or index.
**Depends on:** C2, C4 · **Traces to:** R2, R9, R10, R11, R21

### C6 — Content-bound ready receipt
**Status:** `[ ]` pending
**Build:** Add a private proof worktree, `ready`, the receipt store, proof integration, and journaled publication.
**AC:**
- Git creates the locked proof worktree at the exact committed snapshot.
- Git materializes exact recursive submodules without network access.
- The checkout disables unapproved hooks and rejects unapproved optional filters.
- The input manifest hashes all proof-visible bytes and required metadata.
- Proofs run only in the private proof worktree.
- A required filter failure prevents a receipt.
- An unapproved optional filter prevents a receipt.
- A post-proof change outside declared writable roots prevents a receipt.
- Publication compares all bound inputs and creates a hidden receipt reference.
- Each receipt has one ordered result entry for every proof command.
- Removal preserves receipt history and records workspace removal.
- A forced stop leaves no valid partial receipt.
- A restart removes or resumes an incomplete locked proof worktree.
- The command ships a versioned JSON schema.
**Depends on:** C3, C5 · **Traces to:** R2, R7, R9, R10, R11, R12, R15, R16, R17, R18

### C7 — Receipt consumption contract
**Status:** `[ ]` pending
**Build:** Add `verify`. Recheck each receipt and return its exact proven commit for an integration connector.
**AC:**
- Verification fails after any bound state or policy change.
- Verification returns only the exact commit that the receipt proves.
- Verification reads the recorded base and accepts no replacement base.
- A connector contract requires submission of that exact commit.
- A changed claim, generation, receipt reference, or proof policy fails closed.
- A raw bypass has no verified-integration status.
- The command ships a versioned JSON schema.
**Depends on:** C6 · **Traces to:** R11, R12, R17, R20, R21

### C8 — Warm generation publication
**Status:** `[ ]` pending
**Build:** Add `warm`, cache provider contracts, one generation identifier, one manifest, hidden proof, and atomic publication.
**AC:**
- A failed build or publication leaves the prior generation valid.
- Git constructs the hidden source at the exact committed base.
- The default allowlist is empty.
- Repository policy needs consent for its exact content hash.
- Each cache type proves relocation and dependency validity.
- Unknown cache formats and surviving child processes prevent publication.
- Unsafe live database state prevents publication.
- A generation never uses another generation as its source.
- The proof runner must pass before publication.
- Tracked files remain equal to the base after each warm and proof command.
- No nonignored untracked path exists at publication.
- A fresh regular-copy restore passes the provider proof before publication.
- The manifest binds every proof-visible ignored file and its metadata.
- Direct mutation causes quarantine and blocks use.
- The command ships versioned JSON and a versioned generation format.
**Depends on:** C1, C2, C3 · **Traces to:** R3, R4, R6, R7, R10, R12, R14, R16, R18, R19

### C9 — Safe regular cache restore
**Status:** `[ ]` pending
**Build:** Add atomic generation leases, workspace pins, regular byte copy, and exact destination verification.
**AC:**
- Lease acquisition and collection cannot race.
- The destination manifest exactly equals the expected generation manifest.
- No destination regular path shares an inode with its generation source.
- Symlinks and mount points cannot escape a declared boundary.
- Tests reject special files, nested worktrees, and unsafe database paths.
- Source corruption aborts the copy and quarantines the generation.
- A copy failure leaves a valid worktree and a recoverable marker.
- A ready proof worktree restores and binds the same selected generation.
- A generation change makes the workspace receipt stale.
- Removal releases the pin only after Git removes the worktree.
- The harness measures first-proof time, cache bytes, and break-even workspace count.
**Depends on:** C2, C6, C8 · **Traces to:** R3, R4, R5, R6, R7, R10, R11, R14, R15, R19

### C10 — Generation collection
**Status:** `[ ]` pending
**Build:** Add generation inspection and safe collection through `warm --gc`.
**AC:**
- An active workspace or receipt lease prevents generation removal.
- Collection removes only a Klon generation under the managed store.
- Selection, lease, and collection interleaving tests pass.
- Each injected fault leaves every selected generation valid or restores the prior state.
- Repeated collection gives the same valid result.
- JSON output lists each retained and removed generation with its reason.
**Depends on:** C9 · **Traces to:** R6, R7, R12, R15, R17, R18

### C11 — macOS clone acceleration
**Status:** `[ ]` pending
**Build:** Add a recursive macOS `copyfile` adapter behind the copy backend interface.
**AC:**
- A target-path operation probe selects clone support.
- The destination manifest exactly equals the expected generation manifest.
- A destination write cannot change its generation source.
- The shared suite covers links, modes, times, extended attributes, access control lists, and mount boundaries.
- The adapter rebuilds hardlink groups as private regular files.
- Source corruption aborts the copy and quarantines the generation.
- An unsupported or failed clone selects regular copy and reports the reason.
- The adapter stays disabled until correctness passes and Section 1 permits its use.
**Depends on:** C2, C9 · **Traces to:** R3, R5, R12, R13, R14

### C12 — Linux clone acceleration
**Status:** `[ ]` pending
**Build:** Add a per-file Linux `FICLONE` adapter behind the copy backend interface.
**AC:**
- A target-path operation probe selects clone support.
- The destination manifest exactly equals the expected generation manifest.
- A destination write cannot change its generation source.
- The shared suite covers links, modes, times, extended attributes, access control lists, and mount boundaries.
- The adapter rebuilds hardlink groups as private regular files.
- Source corruption aborts the copy and quarantines the generation.
- A file error selects regular copy for that transaction and reports the reason.
- The adapter stays disabled until correctness passes and Section 1 permits its use.
**Depends on:** C2, C9 · **Traces to:** R3, R5, R12, R13, R14

### Release milestone tasks

- Build each release archive once and record its provenance, checksum, and signature.
- Install-test those exact archives on clean support-floor hosts.
- Promote the same bytes through beta and stable channels.
- Test rollback and recovery with the prior stable release.
- Package macOS arm64, macOS x86-64, Linux glibc, and Linux musl targets.
- Publish the supported Git and file-system compatibility matrix.
- Publish the reproduced Git and competitor comparison report.
- Publish public beta installation, recovery, and limitation documents.

## Definition of Done

- All R1 through R21 requirements pass their acceptance criteria.
- All C0 through C12 chunks are complete.
- Native macOS and Linux release tests pass.
- Differential tests report zero correctness errors.
- Fault tests cover each durable state change.
- Raw benchmark data and environment records support each public performance claim.
- The release report states every passed and failed benchmark target.
- No core command needs an optional service or administrator access.
- A clean host can install Klon, create a workspace, prove it, repair it, and remove it.

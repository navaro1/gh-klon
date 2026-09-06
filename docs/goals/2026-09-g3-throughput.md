# G3 — M12 build throughput at N=6

Date: 2026-09-06
Goal: spec §7 G3. Requirement: spec §3 R19. Metric: handoff §8 M12.
Issue: [#40](https://github.com/navaro1/gh-klon/issues/40).

Condition: `gh klon bench --cell m12-throughput-n6 --json` reports
`ratio >= 0.80` on the development laptop.

Host: Ubuntu 22.04.5, kernel 6.2.0-36, ext4 on NVMe, git 2.34.1,
12th Gen Intel Core i9-12900H, 20 CPUs (6 P-cores with two threads each and
8 E-cores), 62 GB RAM, systemd 249, Landlock ABI 3. The `copy` backend served
every measurement. No btrfs volume was in use.

## 1. Result

The condition holds on `origin/main` at commit `4d4686a`. klon needed no code
change, so the goal closes with a measurement and a report.

| Record | `t_solo_ms` | `t_wall6_ms` | `ratio` | `tokens` | `timing_valid` |
|---|---|---|---|---|---|
| klon, `copy` backend | 2481.6 | 14376.7 | **1.036** | 18 | true |
| baseline, `git worktree add` | 2317.5 | 9687.2 | 1.435 | none | true |

The klon record passes: `1.036 >= 0.80`, and the pass rule in the manifest is
`pass_ratio = 0.80`. The margin is 29 %.

The host runs other agents. Every number above comes from a run that started
with a one-minute load average below 6. Section 3 gives the same cell under
load for comparison.

## 2. What the cell measures

`ratio = (6 × T_solo) / T_wall6`.

- `T_solo` is the median of three builds, each alone in a fresh klon.
- `T_wall6` is the time from the first start to the last finish of six builds
  that run at once, each in its own klon with its own `target/`.
- The fixture is the M12 fixture of `bench/manifests/v1.toml`: a cargo
  workspace of 48 member crates, 150 generated functions per crate, seed
  20260905. The crates depend on nothing, so one build already fills the token
  pool. Golden is cold, so every builder compiles all 48 crates.
- Every build, solo and concurrent, runs under `gh klon run`. The jobserver,
  the resource scope, and the write fence are inside the timer. The baseline
  builds bare, which is what a `git worktree add` user gets.

## 3. Baseline before any change

No source file changed, so the numbers below are both the baseline and the
result. Two runs of the same cell, at two host loads:

| Run | one-minute load at the start | `t_solo_ms` | `t_wall6_ms` | `ratio` |
|---|---|---|---|---|
| under other agents | 39 | 4994.1 | 19808.5 | 1.513 |
| quiet host | 5.4 | 2481.6 | 14376.7 | 1.036 |

Host load raises the ratio. A busy machine slows the solo build more than it
slows the six-way run, because the six-way run already claims a larger share of
the CPUs. The quiet number is therefore the honest one, and it is the number
this report claims.

## 4. Why the ratio sits above one

Two effects raise it.

**A solo build cannot use the whole machine.** The store holds `nproc - 2` = 18
tokens (`src/envelope/jobserver.rs`, `target()`). A cargo process owns one
implicit token, so one build runs at most 19 compile processes on 20 CPUs. Six
builds share the same 18 tokens and own six implicit ones, so they reach at
most 24. The concurrent phase therefore reaches every CPU while the solo phase
leaves one idle.

**Six builds overlap their serial parts.** Each `cargo build` spends time that
no token covers: `gh klon run` startup, the workspace manifest parse, the
fingerprint scan of 48 crates, and the final metadata write. A solo build pays
that time alone. Six concurrent builds pay it at the same time.

The second effect grows with N, and the measurements show it. C31 measured
0.98 at N=2 with the same 18 tokens. This run measures 1.036 at N=6. A fixed
serial part per build produces exactly that shape.

## 5. Where the envelope still costs throughput

The two records disagree about absolute speed, and the report must say so.

| | klon | baseline | klon cost |
|---|---|---|---|
| one build alone | 2481.6 ms | 2317.5 ms | +7 % |
| six builds at once | 14376.7 ms | 9687.2 ms | +48 % |

klon is 7 % slower for one build and 48 % slower for six. The cost that grows
with N is the shared token cap. Six bare cargo processes run up to 20 compile
jobs each, 120 in total on 20 CPUs; six klon builds share 18 tokens and reach
24, which R19 requires. The extra concurrency of the bare run hides the dead
time between two short `rustc` calls. Each `rustc` in this fixture compiles 150
trivial functions and lives well under a second, so that dead time is a large
share of the work, and only the bare run can hide it.

That is the mechanism, not a measured attribution. The write fence, the systemd
scope, and `nice -n 10` also separate the two records, and the +7 % on the solo
build is the joint cost of all four at N=1. Section 7 (f) tried to separate the
token cap from the rest and failed: the host load moved between 9 and 30 while
the experiment ran, so its rows are not comparable. The share each part takes
of the 48 % is therefore unmeasured.

That gap is the price of R18 and R19: a bounded machine. The M12 ratio cannot
see it, because both of its terms carry the same envelope. Both numbers belong
in the record, and the gap is a known limitation, not an M12 failure.

The per-klon times of the klon record are uneven: one builder finished in
3.8 s and the rest between 11.3 s and 14.4 s. A fifo jobserver hands a token to
whichever client reads first, and cargo holds every token it takes until the
unit it started has finished, so an early builder keeps a large share. The
unevenness costs `t_wall6` little: the machine stays saturated until the last
builder is nearly done, and a single remaining builder may then hold all 18
tokens.

## 6. The permitted knobs, and why none of them moved

Spec §7 G3 permits four changes: the token count, the `MemoryHigh` formula,
the jobserver top-up timing, and `nice` values. The condition holds without any
of them, and each has its own reason to stay as it is.

**The token count.** Raising it is the knob most likely to shrink the gap in
section 5. klon keeps `nproc - 2`. Spec §7 C17 and handoff §5 both fix that
count, the two spare CPUs are what keeps the person's own session responsive
while agents build, and the goal condition holds without the change. Section 7
(f) tried to measure what a higher count would do and produced nothing a reader
should trust. A count change is a design decision for the spec, and it needs a
quiet host to justify; it is not a side effect of a goal session that already
passed.

**The `MemoryHigh` formula.** `total/(N+1)` gives 8.9 GB per klon at N=6 on
this 62 GB host, which section 7 (c) shows from `doctor` while the six klons
were alive. The fixture never approached it: 48 crates of 150 trivial functions
keep every `rustc` well under 200 MB. Memory is not what bounds this cell.

**The jobserver top-up timing.** The current rule is correct for this cell.
Only the first of the six `run` calls finds the store idle and fills it; the
other five see the shared hold and skip the fill, so no klon writes a token
that a live client owns. The fill happens once, before any build starts, and
costs one `flock` per `run`.

**`nice` values.** systemd 249 accepts `CPUWeight` and ignores it, so klon
falls back to `nice -n 10` (handoff §5, §11). All measured builds, the solo
ones included, carry the same niceness, so the value cancels inside the ratio.
Lowering it would only take CPU from the person's own session, which is what
the setting exists to protect.

## 7. Independent evidence

Every block below is verbatim shell output.

### (a) The bench cell, one JSON document

Run on a quiet host from `~/.cache/klon`, with `KLON_BENCH_DIR` pointing at a
scratch fixture directory:

```
$ KLON_BENCH_DIR=$HOME/.cache/klon/benchg3 gh-klon bench \
    --cell m12-throughput-n6 --json
```

```json
{"schema":"klon.bench/1","timestamp":"2026-09-06T12:20:08Z","release":false,"smoke":false,"manifest":{"version":1,"path":"bench/manifests/v1.toml","seed":20260905,"warm_runs":10,"cold_runs":5},"environment":{"hostname":"pl-workstation","cpu_model":"12th Gen Intel(R) Core(TM) i9-12900H","cpu_cores":20,"memory_total_kb":65501588,"os":"Ubuntu 22.04.5 LTS","kernel":"Linux 6.2.0-36-generic","arch":"x86_64","bench_dir":"/home/navaro/.cache/klon/benchg3","filesystem":"ext4","mount_options":"rw,relatime,errors=remount-ro","git_version":"2.34.1","klon_version":"0.1.0","klon_commit":"4d4686a","fixture_hash":"d8d1fb138740b469","order_seed":1784611322326685826,"drop_caches":"none"},"records":[{"cell":"m12-throughput-n6","metric":"M12","profile":"p10k","profile_shape":{"tracked_files":10000,"dirs":100,"ignored_files":2500,"ignored_file_bytes":100000,"changed_files":20,"added_files":2},"fixture":"rust","fixture_shape":{"crates":48,"functions":150},"backend":"copy","spare":false,"cold":false,"cache_drop":"warm-only","timer":"wall: from the first build start to the last build finish of the concurrent builders","runs":3,"order":[3,4,5],"samples_ms":[3791.534698,2481.630453,2430.76617],"p50_ms":2481.630453,"p95_ms":3791.534698,"first_p50_ms":null,"steady_p50_ms":null,"steady_samples_ms":[],"warm_reached":null,"units_compiled":null,"unique_bytes":null,"method":null,"ratio":1.0356909035181632,"t_solo_ms":2481.630453,"t_wall6_ms":14376.666501000002,"per_klon_ms":[13068.463822,3796.970047,11298.54895,14376.025330999999,11455.950321,14076.702167000001],"builders":6,"tokens":18,"correctness":{"matched":true,"ignored_manifest":"not-applicable: the path fixup rewrites golden's path inside the rust state","tracked":"on feature at 4d2aff6e69dac62097a38e2549d9f7cb1e152358","status":"clean","removal":"not-applicable: the cell removes no tree","build":"ok"},"timing_valid":true,"pass_p50_ms":300000,"pass_steady_p50_ms":null,"pass_units_compiled":null,"pass_ratio":0.8,"pass":true},{"cell":"m12-throughput-n6","metric":"M12","profile":"p10k","profile_shape":{"tracked_files":10000,"dirs":100,"ignored_files":2500,"ignored_file_bytes":100000,"changed_files":20,"added_files":2},"fixture":"rust","fixture_shape":{"crates":48,"functions":150},"backend":"git-worktree-add","spare":false,"cold":false,"cache_drop":"warm-only","timer":"wall: from the first build start to the last build finish of the concurrent builders","runs":3,"order":[0,1,2],"samples_ms":[2488.57199,2317.451549,2190.808614],"p50_ms":2317.451549,"p95_ms":2488.57199,"first_p50_ms":null,"steady_p50_ms":null,"steady_samples_ms":[],"warm_reached":null,"units_compiled":null,"unique_bytes":null,"method":null,"ratio":1.4353650278293353,"t_solo_ms":2317.451549,"t_wall6_ms":9687.228701,"per_klon_ms":[9629.173771,9571.65501,9472.797567000001,9518.868747999999,9684.325323,9402.265594999999],"builders":6,"tokens":null,"correctness":{"matched":true,"ignored_manifest":"not-applicable: the path fixup rewrites golden's path inside the rust state","tracked":"on feature at 4d2aff6e69dac62097a38e2549d9f7cb1e152358","status":"clean","removal":"not-applicable: the cell removes no tree","build":"ok"},"timing_valid":true,"pass_p50_ms":300000,"pass_steady_p50_ms":null,"pass_units_compiled":null,"pass_ratio":0.8,"pass":null}],"skipped":[]}
```

The klon record reads `"ratio":1.0356909035181632`, `"timing_valid":true`,
`"tokens":18`, `"pass":true`, and `"pass_ratio":0.8`.

### (b) Three solo builds, each alone in a fresh klon under `gh klon run`

The fixture is the same workspace: a script reproduces `generate_rust` from
`src/bench/fixture.rs` with the same seed, the same 48 crates, and the same 150
functions. The `sha256` of the concatenated crate bodies matches the one that a
standalone copy of `crate_body` prints, so the sources are byte-identical.

Each line is `/usr/bin/time -f %e gh-klon run --path <klon> -- cargo build
--offline`, run in a klon that `gh klon add` had just made:

```
### load before
5.91 14.07 16.78 9/3968 1732694

### (b) three solo builds, each alone in a fresh klon under gh klon run
solo1 2.62
solo2 7.18
solo3 3.57
```

The median is 3.57 s. `solo2` met a load spike from another agent.

### (c) Six klons build at once, each with its own `target/`

```
### (c) six klons build at once, each with its own target/
--- the caps that six live klons produce:
scope     systemd 249 scope: MemoryHigh=9138M TasksMax=4096
jobserver /run/user/1000/klon/jobserver: 18 of 18 tokens, a klon holds the store open
slots     6 addresses in use
before 1788698545.483712164
after  1788698563.475158817
n0 5.82
n1 17.95
n2 14.57
n3 17.97
n4 17.79
n5 17.73

### load after
21.62 17.11 17.70 10/4010 1738546
```

The reader recomputes:

- `T_wall6 = 1788698563.475158817 − 1788698545.483712164 = 17.9914 s`
- `T_solo = median(2.62, 7.18, 3.57) = 3.57 s`, the same rule the cell uses
- `ratio = 6 × 3.57 / 17.9914 = 1.191`

That is above the 0.75 the acceptance line asks for. The most severe reading,
which takes the fastest solo build instead of the median, gives
`6 × 2.62 / 17.9914 = 0.874`, still above 0.75.

The `MemoryHigh=9138M` line is `62 GB / 7`, so `total/(N+1)` behaved as
designed with six live klons, and `slots` confirms the six.

The load rose from 5.91 to 21.62 during this block, so these numbers are
noisier than the bench numbers in (a). They pass either way.

### (d) `git diff --stat origin/main -- src/bench/ bench/`

```
$ git diff --stat origin/main -- src/bench/ bench/
$
```

No change. Nothing under `src/bench/` or `bench/` differs from `origin/main`,
so no hunk needs a justification and the manifest is untouched. The only file
this branch adds is this report.

### (e) The last 20 lines of `cargo test`

The whole run went to a file, so the counts below come from the complete
output and not from a truncated tail.

```
$ cargo test > test-full.log 2>&1; echo "exit=$?"
exit=0
$ grep -c '^test result:' test-full.log
29
$ grep '^test result:' test-full.log | grep -vc '^test result: ok'
0
```

The run holds 29 `test result:` lines, one per integration test binary plus
the unit tests of the binary target, and none of them is anything but `ok`.
The last twenty lines of the same file:

```
$ tail -20 test-full.log
test add_after_init_volume_keeps_the_ignored_manifest ... ok
test a_kill_before_the_move_leaves_golden_alone ... ok
test a_klon_made_before_the_move_still_works ... ok
test add_of_the_100k_fixture_on_a_volume_is_fast ... ok
test add_reattaches_a_volume_that_went_down ... ok
test init_volume_moves_golden_and_doctor_reports_the_snapshot_backend ... ok
test init_volume_on_a_dirty_golden_refuses ... ok
test undo_restores_golden_on_the_old_filesystem ... ok
test init_volume_without_btrfs_progs_prints_the_install_line ... ok

test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s

     Running tests/zero_compile.rs (target/debug/deps/zero_compile-f0c0b1d200eb4820)

running 2 tests
test cargo_build_in_a_fresh_klon_compiles_nothing ... ok
test pnpm_install_in_a_fresh_klon_changes_nothing ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.25s

```

### (f) What the token count would do

The report tried to separate the token cap from the rest of the envelope. The
experiment ran one solo build and one six-way build per setting: the shipped 18
tokens, `KLON_NO_JOBSERVER=1` (the fence and the scope stay, only the build
slots go), and 30 tokens. The result is not usable, and the report states that
rather than hide it.

```
tokens=18  solo=11.75 wall6=14.951663196 ratio=4.715 per_klon= 14.66 14.80 14.73 14.48 14.94 14.30 load=9.61
tokens=off solo=2.19  wall6=11.196155787 ratio=1.173 per_klon= 11.06 11.18 11.14 11.13 11.15 11.15 load=30.42
tokens=30  solo=27.92 wall6=29.137625217 ratio=5.749 per_klon= 27.92 27.95 28.66 27.84 29.11 27.97 load=25.98
tokens=18  solo=17.29 wall6=23.406806393 ratio=4.432 per_klon= 22.76 23.19 22.98 22.89 21.72 23.39 load=20.26
```

The `load` column is the one-minute average at the end of each row. It moved
from 9.6 to 30.4 and back, and the solo builds swung from 2.19 s to 27.92 s on
the same fixture. No row is comparable with another, so the rows say nothing
about the token count. The rows do repeat one shape from section 5: with the
jobserver off, the six builders finish within 0.12 s of one another, while with
18 tokens they spread further apart.

A trustworthy token experiment needs a host that stays quiet for about ten
minutes. This one never did. G3 does not need the answer, because the condition
holds at the shipped count.

## 8. Acceptance

| AC line from spec §7 G3 | Verdict |
|---|---|
| The bench output shows `ratio >= 0.80`. | pass: 1.036, section 7 (a) |
| The `date` boundaries and the six `/usr/bin/time` lines give at least 0.75 when recomputed. | pass: 1.191, section 7 (c) |
| The last 20 lines of `cargo test` show `test result: ok`. | pass: section 7 (e) |

Constraints from the same section:

| Constraint | Verdict |
|---|---|
| `cargo test` stays green | pass, section 7 (e) |
| no manifest change | pass: `bench/manifests/v1.toml` is untouched, section 7 (d) |
| no change under `src/bench/` without a justification | pass: no change at all, section 7 (d) |
| turn cap 25 | pass: three measured attempts |

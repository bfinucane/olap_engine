# `olap_engine` test baseline — characterisation report

**Revision tested:** `dbc7da8215cb7de40679222e6e0dfe0fda8864da` (`dbc7da8`, "Add write-cost
benchmarks for Phase 1b baseline"), branch `main`, working tree clean, `main` ahead of
`origin/main` by 2 commits. This is exactly the frozen HEAD the handoff specified, so the
serialise-and-freeze plan was honoured.

**Toolchain:** `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `cargo 1.97.1 (c980f4866 2026-06-30)`.

**Scope:** run the suite, characterise what it covers, report. No engine code was changed and
no fixture was re-blessed. §§1–3 and §§5–6 describe the revision *as found*; **§4a records the
additive test work that followed, which added 19 tests (5 → 24) without touching `src/`
behaviour.**

**What this document is for.** It answers one question: *what does the existing suite actually
guarantee at this revision?* The answer is that the end-to-end guarantee is real and green,
while the engine internals — the part Phase 1b is about to rewrite — have no test-level
contract at all. §4 is therefore a menu for deciding what to harden *before* Phase 1b starts,
not a work order; nothing in it is urgent, and the only item with a wrong-answer failure mode
is #4 (`has_consolidated_coordinate`).

**Read this first if you read nothing else:** §1 (the baseline is genuine and reproducible),
§3 (0 unit tests in ~3,935 lines, as found), **§4a (the gap-closing pass: 19 tests added, and
the two real defects they found)**.

---

## 1. Commands and results

The repo lives outside the session workspace, so `cargo` cannot write `target/` under the
default sandbox. The first attempt reproduced the documented blocker exactly:

```
cargo test --offline      -> exit 101
error: failed to open: ...\olap_engine\target\debug\.cargo-build-lock
Caused by: Zugriff verweigert (os error 5)
```

With the sandbox widened for that command only:

| Command | Exit | Result |
|---|---|---|
| `cargo check --offline` | **0** | clean, 0 warnings, 0 errors |
| `cargo test --offline` | **0** | **5 passed / 0 failed** across 7 test targets |

Per-target breakdown of `cargo test`:

```
unittests src\lib.rs          running 0 tests   ok
unittests src\bin\bench_trie  running 0 tests   ok
unittests src\bin\bench_write running 0 tests   ok
unittests src\main.rs         running 0 tests   ok
tests\persistence_tests.rs    running 4 tests   ok. 4 passed; 0 failed
tests\script_tests.rs         running 1 test    ok. 1 passed; 0 failed
Doc-tests olap_engine         running 0 tests   ok
```

The baseline in the handoff is **reproduced exactly** and is stable across repeated runs.
`cargo test` is not flaky.

### Independent check of the golden files

The harness result was not taken on trust. Each of the 13 fixtures was run directly through
`target\debug\olap_engine.exe` and its output compared byte-for-byte (after CRLF
normalisation) with `tests/scripts/<name>.expected.txt`:

```
result: 13/13 goldens reproduce exactly; mismatched: none
```

So every golden is current. Six fixtures exit 0 and seven exit 1 **by design** — the seven are
precisely the ones whose golden transcript contains an intentional error.
(Raw evidence: `olap-test-evidence/golden-verification.txt` beside this file.)

---

## 2. What the suite actually covers

| Suite | Tests | What it genuinely covers |
|---|---|---|
| `tests/persistence_tests.rs` | 4 | `save`→`load` round trip preserving numerics, a string cell, a *second* hierarchy and its consolidation; garbage file and a file too short for the header both yield `NotACatalog`; a correct-magic/newer-version file yields `UnsupportedVersion`, not a misread; two consecutive saves leave no `.tmp` sibling debris. Runs against the library, no REPL. |
| `tests/script_tests.rs` | 1 | One `#[test]` that loops over **13 `.sql` fixtures** and diffs the built binary's batch output against a golden, then checks the exit code is consistent with the golden text. |
| everything else | 0 | `src/lib.rs`, `src/main.rs`, both bench binaries, and doc-tests contain no tests at all. |

**Breadth inside the golden suite** (counted from the golden transcripts, which echo every
command):

- 243 commands total: 52 `SELECT`, 47 `INSERT`, 21 `CREATE`, 51 `.rollup`, 18 `.import*`,
  9 `.order`, 6 `.tree`, 4 `.splash`, 3 `.create_hierarchy`, 1 each of `.dimensions`,
  `.delete_member`, `.detach`.
- 12 deliberate negative cases among the 24 `Error:` occurrences: 6 in `join` (wrong join key,
  unprojected key, unknown column, `ON` rejected, no conformed dimension, ambiguous conformed
  dimension), 2 in `hierarchies` (hierarchy name collision), 1 each in `create_duplicate`,
  `select_no_from`, `order` (cross-parent reorder) and `delete_member`.
- Genuinely valuable semantics **are** pinned: fractional *negative* consolidation weights
  (`Variant = Actuals - Plan` in `aggregation` and `splash`), ragged/blank-skipping
  `.import_lvl`, `.import_pc` rollups, measure pivoting (`SELECT Sales, Costs`), conformed
  dimensions shared between cubes, `.splash` even split / `ADD` mode / negative-weight
  allocation, `.detach`, `.delete_member` purging data from a second cube, `.order`
  hierarchy-scoping, and the multi-hierarchy read path resolved through the `Ops` hierarchy
  over shared leaves.
- **One intentional contract that no fixture pins:** re-`INSERT`ing an existing coordinate
  overwrites the stored cell (confirmed by the author as designed upsert behaviour). Defect B
  is exactly why this matters — if that behaviour regressed to "first write wins", no fixture
  would notice.

### Coverage limits the suite does **not** reach

Verified by running the binary on a throwaway 14-command script (removed afterwards):

| Behaviour | Actual result at HEAD | Covered by a fixture? |
|---|---|---|
| `DELETE FROM P WHERE ...` | `Error: Unsupported SQL command.` | no — **planned feature** (author-confirmed: not built yet) |
| `SELECT * FROM P` | `Error: Column '*' not found in primary or joined cube.` | no — **deliberate non-goal** (author-confirmed) |
| `GROUP BY` / aggregate functions | no support at all in `engine/sql.rs` | no — **planned feature** (author-confirmed) |
| `INSERT` over an existing cell | overwrites (100 → 999) — **intentional upsert semantics, confirmed by the author** (the REPL reports "Upserted 1 row(s)") | no |
| unqualified second-hierarchy axis read | works and follows creation order | `hierarchies.sql` only ever reads hierarchy-qualified |
| `.cubes`, `.cube`, `.save`, `.run`, `.help` | implemented in `main.rs` | no |
| any concurrent read/write | not exercised anywhere | no |
| process killed mid-save | not exercised anywhere | no |

`join.sql` is the only place `CREATE ATTRIBUTE TABLE` is exercised, and it only ever *reads*
attributes. The attribute-cube *write* path has no coverage at all — the five `examples/*.sql`
files (`createattribute.sql`, `showAttributes.sql`, …) exist for exactly that and are not run
by anything.

---

## 3. `src/` unit tests: **0** *(as found — since addressed; see §4a)*

At the revision under test, `grep` for `#[test]` / `#[cfg(test)]` / `mod tests` over `src/`
returned **no matches**. Every one of the following was untested at unit level:

| Module | Lines |
|---|---|
| `src/main.rs` (REPL, batch runner, all 16 `.meta` commands) | 730 |
| `src/cube/cube.rs` (`write`, `query_slice`, `query_consolidated`, cache) | 578 |
| `src/dimension/dimension.rs` | 573 |
| `src/engine/sql.rs` (parser, measure pivot, attribute join) | 554 |
| `src/catalog/catalog.rs` (persistence, dimension sharing) | 419 |
| `src/dimension/hierarchy.rs` | 389 |
| `src/bin/bench_write.rs` | 282 |
| `src/bin/bench_trie.rs` | 273 |
| `src/storage/sparse_store.rs` (Patricia trie) | 134 |
| `src/cube/node.rs`, `src/cube/query.rs`, `src/query.rs`, `src/dimension/dictionary.rs`, `mod.rs` files | 104 |
| **total** | **~3,935** |

There is no `[dev-dependencies]` section in `Cargo.toml`, so no property-testing or snapshot
crate is available to the suite. Both benches (`bench_trie`, `bench_write`) are compiled but
never executed by `cargo test`; the write-cost benchmark added in the very commit under test is
therefore not part of the baseline at all.

---

## 4a. Gap-closing pass (added after this report was first written)

Five tests landed in three files. Every one is additive: **no engine code was changed**, no
golden was re-blessed, nothing was refactored.

| File | Added | What it pins |
|---|---|---|
| `src/storage/sparse_store.rs` | 14 unit tests (new `#[cfg(test)] mod tests`) | Exact-coordinate round trip; overwrite-on-rewrite (the storage half of the intentional upsert contract); unwritten/partial/over-long coordinates are `None`, not zero or panic; empty coordinate addresses the root; interior and deeper cells coexist; string cells survive; pruning removes only the targeted coordinate and prunes dead branches; full prune leaves no debris; `scan_subcube` filters intersect across dimensions and can match nothing; a 512-deep path stores, reads and scans; a 256-deep path prunes; `print_tree` writes to the buffer, not stdout. |
| `tests/persistence_tests.rs` | 2 tests (4 → 6) | **Interrupted save**: with the target held open denying replace, the save is refused and the previous database is left byte-identical — never truncated to zero bytes. **Over-long coordinate**: `Cube::write` *panics* where `write_splashed` returns a typed error. |
| `tests/concurrency_tests.rs` | 3 tests (new file) | The guarantee that holds (many threads reading one `Arc<RwLock<Dimension>>` see consistent data); the auto-trait floor 1b builds on (`Catalog`, `Cube`, `CellValue` are `Send`, and a catalog moves across a thread boundary); and the *absence* of concurrent mutation, stated where a future change will trip over it. |

**Resulting baseline: `cargo test --offline` exits 0 with 24 passed / 0 failed** (was 5), and
`cargo check --offline` is still clean with 0 warnings.

```
unittests src\lib.rs          14 passed    <- was 0
tests\concurrency_tests.rs     3 passed    <- new file
tests\persistence_tests.rs     6 passed    <- was 4
tests\script_tests.rs          1 passed    (13 fixtures, unchanged)
benches / main.rs / doc-tests  0 tests
```

Raw log: `olap-test-evidence/cargo-test-with-gap-tests.log`.

### Two concrete defects the new tests found

1. **`Cube::write` panics on a coordinate/dimension mismatch** (`cube.rs:154`,
   `self.dimensions[i]`). A public library API that aborts the caller instead of returning a
   typed error is a defect, and the same input is rejected properly by `write_splashed`
   (`cube.rs:208`). `engine/sql.rs:551` calls `write` on the fall-through path after
   `has_consolidated_coordinate` has already swallowed the mismatch (that helper uses `.get(i)`
   and returns `false`, `cube.rs:172-179`), so an over-long coordinate list is classified as
   "no consolidation here" and then panics. The test uses `catch_unwind` to characterise the
   current behaviour; **it should be inverted to assert an error once `write` returns
   `Result`.**
2. **A failed save leaks its temp file.** Reproduced live: the refused save left
   `.olap_persist_<...>.bin.<pid>.<n>.tmp` in the target directory. The temp is fsynced and is
   itself a complete, loadable catalogue, so this is debris rather than corruption — but the
   caller cannot clean it up, because the name is an implementation detail of
   `temp_sibling_path`. The interrupted-save test reports it with `eprintln!` and cleans up
   after itself rather than failing on it.

The interrupted-save test also settled the open question from defect C below: on this machine
the primary `rename` **is** refused while the destination is held open, so the fallback branch
is genuinely exercised — and the old database survived intact.

---

## 4. Ranked gaps

Ranked by risk to engine-internal correctness (the integration path the Plateau thread
concluded matters more than SQL-surface polish), not by ease.

| # | Gap | Why it matters | Est. effort | Status |
|---|---|---|---|---|
| 1 | **The trie had only an end-to-end test path and no asserted invariants.** `sparse_store.rs` is the storage engine Phase 1b will rewrite into a version store. | 1b's copy-on-write design must preserve exactly these behaviours; without unit tests there is no baseline to refactor against. | 1–2 days | **CLOSED** — 14 unit tests (§4a) |
| 2 | **No crash-during-save test.** Phase 1a's definition of done is "killing the process mid-save leaves the old or the new complete file". | It is the one durability guarantee the roadmap claims as *done*, and it is cheap to test. | 0.5 day | **CLOSED** — interrupted-save test reproduces the refused rename (§4a) |
| 3 | **Reachable panic sites in the read path.** `engine/sql.rs` unwraps `.position(...)` for dimension/measure lookup (lines 239, 265, 346) and parses measure order with `.find(" IN ").unwrap()` (line 36). A panic aborts the batch run with exit 101 and no message. | A client-driven query spec (the confirmed Plateau direction) will hit these far more often than a hand-written script. | 1 day | open — now has a sibling: `Cube::write` panics on the same class of input (§4a defect 1) |
| 4 | **`has_consolidated_coordinate` silently returns `false` on an out-of-range dimension index** (`cube.rs:175–178`). | This was an *inference* from reading the code. The test written to pin it showed the real behaviour is worse and different: `write` **panics** rather than mis-storing. See §4a defect 1. | 0.5 day | open — corrected and pinned by a characterisation test |
| 5 | **No concurrency test at all**, and the design notes say there is no safe multi-threaded mutation today (`Cube` by value, `Dimension` behind `Arc<RwLock<_>>`, global `query_cache.clear()`). | Phase 1b's whole purpose is concurrency; a test that documents today's *absent* guarantee stops 1b from silently claiming it. | 1 day | **CLOSED** — `tests/concurrency_tests.rs` (§4a) |
| 6 | **Untested public surface:** all five `examples/*.sql`, attribute-cube *writes*, and the `.save`/`.run`/`.cubes`/`.cube` REPL commands. Intent is now recorded for `DELETE`/`GROUP BY` (planned) and `SELECT *` (non-goal). | Cheap breadth. | 0.5 day | open |
| 7 | **No fixture pins the intentional upsert contract** (re-`INSERT` overwrites). | It is a deliberate semantic that had no guard at all. | 0.5 day | **partly closed** — pinned at the storage level (`second_write_to_the_same_coordinate_overwrites`); still unpinned end-to-end through SQL |
| 8 | **The golden exit-code check is a tautology** (defect A below). Low severity — it adds no protection, but it also hides nothing. | Cleaning it up makes the failing/passing split reviewable in a diff. | 0.25 day | open |
| 9 | **No coverage tooling available:** the `llvm-tools-preview` rustup component is not installed, so `-Cinstrument-coverage` profiles cannot be turned into a line-coverage report. | Turns "which modules are risky" into a number for every future assessment. | 0.5 day (install + one script) | open |

The instrumentation experiment was reverted: the profile directory was deleted and the default
(non-instrumented) profile rebuilt with a clean `cargo test`. No `database.bin` was created at
the repo root, and no golden file was re-blessed.

---

## 5. Things that look like failures but are not

1. **`One or more commands failed.`** appears **7 times** in `cargo test` output. It never
   reaches a golden file — it is printed to **stderr** by `run_batch` (main.rs:202) only when
   `--out` is absent, and the harness always passes `--out`. It corresponds one-to-one with
   the 7 by-design failing fixtures. `cargo test` still exits 0.
2. **`test result: ok. 0 passed` four times.** `main.rs`, the two benches and doc-tests.
   Not an error and not a disguised skip — there is genuinely nothing in them. (The lib target
   used to be in this list; it now runs 14 tests, see §4a.)
3. **Fixture exit code 1 for seven scripts.** Intentional: `create_duplicate`,
   `select_no_from`, `order`, `hierarchies`, `delete_member`, `join` and `import_missing_file`
   each end on an error, and `run_batch` maps any failed command to `ExitCode::FAILURE`. The
   `1` is the harness *observing* expected behaviour.
4. **`cargo` progress lines arrive on stderr and colour the PowerShell output red.** Cosmetic.
5. **Piping is still a trap.** `cargo test --offline 2>&1 | Select-Object -Last n` reported
   exit 1 in an earlier probe while the same run redirected to a file reported 0. Every number
   in this report comes from a redirected run with `$LASTEXITCODE` read separately.

---

## 6. Defects found

### A. The golden harness infers the expected exit code *from the golden text it is checking*

`script_tests.rs:124–131`:

```rust
let expect_failure = expected.contains("Error:") || expected.contains("failed");
if expect_failure == exit_ok { /* failure */ }
```

The exit code is **derivable** from the golden transcript: `run_script` returns `all_ok`
(main.rs:747–786), which is false exactly when some command printed an error, and an error
line is what `expected.contains("Error:")` detects. So this assertion is not a real check —
it is a **tautology** that can only ever fail when the two agree despite the transcript, i.e.
on the wording of the error message itself. Two consequences worth knowing:

- **Wording is load-bearing.** `import_missing_file.sql` is the near-miss: its golden says
  `Could not open '...'.` and `Import failed for '...'.` — only the literal word "failed" keeps
  the check consistent. Re-word that message and this check breaks while the transcript still
  passes; the fixture has to be re-blessed.
- **Substring matching also hits the echoed command lines** (`> ...`), so a future fixture
  whose *commands* contain the word "failed" flips the expectation.

What the harness therefore does **not** have is any independent notion of which fixtures are
supposed to fail — the `7 failing / 6 passing` split is implicit in the golden files and is
only as trustworthy as the last `UPDATE_GOLDEN=1` run. Correctness rests entirely on the
transcript diff, which is strong; the exit-code check adds ~nothing.

**Fix (optional, low priority):** make the expected outcome explicit data — a
`const EXPECT_FAILURE: &[&str]` list in the test, or a `<name>.exit` sidecar per fixture — so
the split is reviewable in a diff instead of being an emergent property of blessed text.

### B. The harness cannot detect a *dropped* command

`collect_scripts` reads whatever `.sql` files exist; `run_script` runs and diffs. If a fixture
is edited to delete the one line that proves, say, negative-weight allocation, and the golden
is re-blessed, the suite stays green with the guarantee gone. Nothing asserts that a fixture
still contains the commands it is meant to exercise. The 243-command tally in §2 was produced
by hand for this report; the suite does not track it.

**Fix:** assert a per-fixture command count and golden byte length, or commit the expected
command list next to each fixture.

### C. `save_to_disk`'s Windows fallback deletes the old file before it is replaced — **reachability unverified**

`catalog.rs:362–374`: the primary `std::fs::rename` is attempted first; only if it fails does
the code `remove_file(target)` and then rename the temp file over the now-free name. On that
branch there is a window where the previous database exists nowhere, and if the second rename
fails (disk full, AV/backup scanner holding a handle, destination on another volume) the
function returns `Err` with no `database.bin` on disk at all. The temp file is already fsynced,
so the data is recoverable by hand — but the guarantee stated in the doc comment ("a reader or
a crash sees either the complete OLD file or the complete NEW file") does not hold while the
target has been removed.

**Honest status: this is a code-reading finding, not a reproduced defect.** I did not manage to
reach the fallback — on this machine the primary `rename` succeeds even when the destination
already exists, which is why `save_is_non_destructive_and_leaves_no_temp_debris` passes while
exercising only the safe path. Two open questions worth answering before treating it as urgent:
is the fallback reachable at all on a supported Windows host, and does it run against a
network/AV-scanned destination?

**Fix if it is reachable:** `ReplaceFileW` / `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`
instead of remove-then-rename, or copy the old target to `database.bin.bak` before removing it.
Either way, the test has to hold the destination open to reach the branch.

### D. Golden harness reports a panic as a generic "output mismatch"

If a fixture makes the engine panic, `run_script` captures empty output and the failure is
reported as an output diff with `status.success() == false`. Nothing distinguishes "wrong
number printed" from "engine panicked". Since three `.unwrap()`s in the SQL path are reachable
from ordinary queries (§4 row 3), and one earlier panic (`SELECT 1`) already needed its own
fixture, this is worth a one-line improvement.

**Fix:** in `run_script`, when the process failed and the captured output is empty, surface the
exit code and a `The binary panicked (exit 101) while running <fixture>` message.

---

## 7. Reproducing this report

```powershell
Set-Location C:\Users\Startklar\Documents\GitHub\olap_engine
git log -1 --format='%H %s'          # expect dbc7da8...
cargo check --offline *> "$env:TEMP\olap-check.txt"; Write-Host "exit: $LASTEXITCODE"
cargo test  --offline *> "$env:TEMP\olap-test.txt";  Write-Host "exit: $LASTEXITCODE"
Select-String -Path "$env:TEMP\olap-test.txt" -Pattern '^test result:'
```

Both runs require `target/` to be writable, i.e. a sandbox wider than `workspace-write` (or a
copy of the repo inside the workspace — in which case the results describe the copy).
Redirect to a file; do not pipe, and read `$LASTEXITCODE` on its own line.

Raw logs from this pass:

- `olap-test-evidence/cargo-test-baseline.log` — the 5-test baseline (§1)
- `olap-test-evidence/cargo-test-with-gap-tests.log` — the 24-test suite after §4a
- `olap-test-evidence/cargo-check.log` — clean `cargo check`
- `olap-test-evidence/cargo-test-after-instrumented-build.log` — the restore run after the
  coverage experiment was reverted
- `olap-test-evidence/golden-verification.txt` — the 13/13 independent golden check

# Roadmap

This document lays out the intended direction of the Patricia OLAP Engine. The
guiding principle is **inside-out**: we harden the core (architecture,
concurrency, persistence) *before* layering user-facing features on top. New
capabilities should sit on a foundation that is already safe to build on.

The ordering below is **dependency-driven**, not priority-by-appeal. Each phase
assumes the previous one has landed, because later work (security, calculations,
query languages) is far cheaper — and sometimes only possible — once the
earlier structural work exists.

---

## Guiding principles

1. **Inside-out.** Fix the core before adding features. A feature that lands on
   a shaky foundation has to be rewritten when the foundation changes.
2. **One source of truth.** The catalog is the unit of consistency. Any state
   that must stay coherent (dimensions + cubes + caches) is governed by a single
   concurrency and persistence model.
3. **Explicit failure.** No `expect()`/`unwrap()` on I/O or persistence paths in
   the steady state; errors surface as typed results, not panics.
4. **Smallest reversible slice.** Prefer a thin, working vertical slice over a
   speculative framework. Land it, verify it, then widen it.

---

## Where we are today (checked against the code, 2026-10-28 — includes exact COUNTD)

These are the concrete facts the plan is built around — not aspirational. This
section is a snapshot, not a changelog: `git log` is the truth and
`WIRE-CHANGES.md` is the client-facing record.

* **Single-process, in-memory, versioned.** `Catalog` lives in one process and
  owns the data. Storage is an immutable Patricia trie with copy-on-write
  writes: `Cube` holds a `VersionStore`, a write publishes a new version, and a
  reader's snapshot is an O(1) root pointer (Phase 1b M1–M3).
* **Durable, atomic persistence.** `save_to_disk` writes a self-identifying
  magic+version envelope to a unique sibling temp file, `fsync`s it, then
  renames over the target; failures are typed `PersistError`s, not panics
  (Phase 1a). Saving is explicit (`.save`/`.exit`, `POST /v1/save`).
* **Server + client split, including writeback.** `olap_engine serve` owns the
  catalogue and answers protocol v1 over HTTP: reads (`/v1/datasets`, query,
  sample, describe, SQL console) and writes (`POST /v1/{dataset}/write`,
  `POST /v1/save`). Reads take a snapshot and do not block a writer; the
  catalog-wide `Mutex` is gone (M3). Writeback is last-write-wins with
  per-batch validation before any cell is written (WRITE-API-DESIGN.md).
* **Users, roles and access limits exist.** Authentication is required by
  default (bearer tokens); table access (`#TableSecurity`), dimension/member
  access (`#DimensionSecurity`), write permission and join-side checks are
  enforced (SECURITY-DESIGN.md S1–S5). S6 (sessions pin a security snapshot)
  and a few recorded limits remain open.
* **Two query front doors.** SQL (`SELECT`/`WHERE`/`JOIN`/`LIMIT`/`OFFSET`/
  `ORDER BY`; grouping comes from the selected dimensions — a `GROUP BY` clause
  is not read) and the native plan/wire protocol (dimensions, measures, filters,
  `orderBy`, `limit`, attribute joins). Both share the aggregation set
  `SUM/AVG/COUNT/MIN/MAX` plus exact `COUNTD` (`countDistinctExact: true`, see
  below). Calculated fields are evaluated from the client's AST over aggregates
  `SUM/AVG/COUNT/MIN/MAX` plus arithmetic, comparisons, `IF`, `ZN`, `IFNULL`
  (CALCULATIONS-DESIGN.md); `COUNTD` is not available inside a formula yet. MDX
  and calculated dimension members do not exist.

---

## Phase 1 — Client/Server architecture + transaction safety

**Why first.** This is the biggest structural gap, and Phases 2–4 all assume a
session/concurrency model that does not exist yet. It is cheap now and expensive
to retrofit later.

Phase 1 is deliberately split into **1a (durability)** and **1b (the version /
MVCC store)**, because they are different in kind: 1a is a small, contained
change that makes today's single-writer world safe; 1b is a storage-engine
redesign that delivers concurrency. 1a is correct regardless of what 1b becomes,
so it lands first and is never wasted work.

### Phase 1a — Durable persistence (small, do first)

**Goal.** Make `.save`/`.exit` crash-safe and non-destructive, with typed errors
instead of panics. This is the *durability* snapshot: a consistent point-in-time
copy taken purely for the act of writing to disk.

**Scope.**

1. **Atomic write (temp file + rename).** Never mutate the live file in place
   (`File::create` truncates it to zero *before* writing — a crash mid-save
   destroys the database). Serialize to a uniquely-named sibling temp file,
   `flush` + `sync_all` (fsync) it, then `rename` it over the target. `rename`
   on the same filesystem is atomic: a reader/crasher sees the complete old file
   or the complete new file, never a partial one.
2. **Typed errors.** Introduce `PersistError` (`Io` / `Encode` / `Decode` /
   `UnsupportedVersion`) implementing `std::error::Error`. `save_to_disk` /
   `load_from_disk` return `Result`; callers (the REPL) surface the error
   instead of the process aborting.
3. **Versioned, self-identifying format.** Wrap the payload with a `magic`
   header + `FORMAT_VERSION`, because `bincode` is **not self-describing** (no
   field names/types on the wire). Without a header, adding a field in a later
   phase would silently misread old files. On load: reject a bad `magic`,
   refuse a newer `version` (`UnsupportedVersion`), and run migrations for older
   ones (a no-op until the first real schema change).
4. **Consistent snapshot.** Take the snapshot once, under a single documented
   boundary, so the file reflects one coherent point in time (today's
   per-dimension locking can interleave with a concurrent writer).
5. **Pinned codec config.** Use an explicitly configured `bincode` (fixed
   endianness / varint policy) so the on-disk bytes don't shift under a
   dependency upgrade.

**Definition of done.** Killing the process mid-save leaves `database.bin` as
either the old complete file or the new complete file (never truncated); loading
a garbage/older file yields a clean typed error and no panic; a `save` → `load`
round-trip is covered by a permanent test.

### Phase 1b — Versioned (MVCC) storage engine (the real work)

**Goal.** Adopt Infor-OLAP-style **multiversion concurrency control** so long
operations proceed on the version they began on, writes create new versions,
and obsolete versions/pages are reclaimed when no reader is pinned to them.

**Background (why this is a storage-engine change, not a persistence patch).**
The Infor model divides memory into **pages**, keeps a **list of versions**,
where each version is the *list of pages it is made of*. A writer produces a new
version using **copy-on-write** (only changed pages are new; the rest are shared
by pointer); a long-running reader keeps walking its captured version and never
observes the writer's changes. When the reader finishes, references drop, the
version retires, and pages reachable only from retired versions are freed.

This is exactly the **snapshot** abstraction — but implemented so readers and
writers run *concurrently*, which is the part Phase 1a does **not** provide
(1a's snapshot freezes everything for the duration of the write).

**Fitting it to this codebase.** The `SparseStore` is a Patricia trie, which is
already a *persistent data structure in spirit* — unchanged subtrees are shared
naturally. So the sharing unit here should likely be **trie nodes / subtrees**
rather than literal fixed-size pages: a "version" is a root pointer plus the set
of nodes created since. This mirrors the classic immutable-trie approach and
avoids importing a page allocator that the workload may not need.

**Caveat to weigh (the memory-churn cost).** Copy-on-write has real overhead.
Infor hit memory-churn problems hard enough to write their *own* `malloc`
replacement because the platform allocator was too slow for their page churn.
Node/subtree-level CoW can similarly *increase* churn and write amplification for
small writes. This must be **measured**, not assumed: the version model pays off
for large analytical scans over big cubes, and we should confirm the same is true
here before committing. A memory-churn/allocator strategy is an explicit part of
1b's scope, not an afterthought.

**Scope.**

1. Storage becomes version-addressed: a version = root pointer + created nodes.
2. Copy-on-write writes; readers pin a version and never block on writers.
3. Version retirement + node reclamation once no reader is pinned (GC).
4. A memory/allocator strategy for churn, informed by measurement.
5. `query_cache` becomes version-aware rather than globally cleared.

**Definition of done.** A long read continues on its version while a concurrent
write commits a new one; readers never see a torn state; retired versions are
reclaimed; churn is measured and bounded.

**Out of scope (both 1a and 1b).** Authentication, authorization, network
security, replication.

### Phase 1c — Protocol + server/client split

**Goal.** A server process that owns the catalog and answers requests from one
or more clients; the in-process/batch path keeps working for tests.

**Scope.**

1. **Protocol.** A serializable request/response enum (serde/bincode-friendly).
   The existing SQL/REPL command handling becomes the server's request handler —
   no query semantics change.
2. **Split.** `server` mode (owns the catalog, accepts connections) and `client`
   mode (REPL that forwards commands). The golden-test harness keeps running
   against an embedded catalog.
3. **Transaction boundary.** Define an explicit write transaction (commit all
   cube writes + cache invalidation + durable persistence, or roll back), on top
   of the 1b version store. **Landed narrowly with writeback:** a batch is
   fully validated before any cell is written, writes are last-write-wins, and
   durability is an explicit `POST /v1/save`. Cross-request transactions and
   optimistic locking (`ifVersion`/409) are deliberately absent — see
   `WRITE-API-DESIGN.md` §5/§10.

**Definition of done.** A server serves reads and writes from multiple clients
without data races or cache corruption; the golden-test harness still passes
against the embedded path.


---

## Phase 2 — Users, security, and data-access limits

**Why here.** Once there is a server and a session concept, identity has a
natural home. Access control is best expressed as a constraint applied to the
data the query engine already resolves.

**Goal.** Authenticated sessions, roles, and row/slice-level access limits —
enforced at the engine's existing chokepoints.

**Scope.**

1. **Identity & roles.**
   * Users, password hashing (a maintained crate — e.g. `argon2`), and roles.
   * Persist users/ACLs alongside the catalog (extend the versioned on-disk
     format from Phase 1).

2. **Authorization model.**
   * Coarse: which cubes and dimensions a role may reference at all.
   * Fine (slice-level): which **members** a role may read. `query_slice`
     already computes per-dimension leaf-id sets and weight maps — that is the
     correct place to **intersect with an "allowed members" set**, so access is
     enforced on resolved coordinates, not on SQL text.

3. **Isolation guarantees.**
   * Write isolation (who may INSERT/splash/rollup); a single write lock per
     transaction is the starting point.
   * Audit the "no silent data leak" property: a denied member must read as
     absent, not as an error that reveals its existence, where policy requires.

**Definition of done.** A session can be authenticated; a role's member-level
restrictions are provably enforced on read results; denied operations fail
cleanly without leaking structure.

---

## Phase 3 — Calculations

**Why here.** Calculated members are the highest-value "calculations" feature and
map cleanly onto the existing weighted-aggregation model. They should land
*after* security so they can themselves be permissioned.

**Goal.** Calculated measures and calculated members as first-class citizens.

**Scope.**

1. **Calculated measures** — derived members expressed over base measures
   (e.g. `Profit = Revenue - Costs`). The engine already multiplies per-leaf
   weights (`weight_maps`) and splashes values, so a calculated measure is a
   derived member whose value is an expression over base measures.
2. **Calculated dimension members** — members defined by an expression rather
   than a stored consolidation tree.
3. **Expression evaluation** — a small, sandboxed expression evaluator over
   resolved measures (arithmetic + a curated function set); no arbitrary code.
4. **Persistence & ordering** — store calculations with the cube; define their
   position in display order and how they interact with `.order`.

**Definition of done.** A calculated measure can be defined, queried like any
other measure, permissioned, and survives save/load.

**Design note.** Keep calculation semantics small and unambiguous now; a future
MDX layer (Phase 4) should be able to *reuse* this evaluator rather than
duplicate it.

---

## Phase 4 — Multidimensional query syntax (MDX)

**Why last.** MDX is a full query language (axes, tuples, sets) that *subsumes* a
large part of what calculations express. Building it before calculations and
security would force rewrites. Treat MDX as a north star and **arrive at it
incrementally**.

**Goal.** Multidimensional query expressions, approached via a pragmatic
intermediate step.

**Scope.**

1. **Intermediate: richer SQL first.** Extend the current SQL surface
   (calculated columns in `SELECT`, `WITH` clauses, members as axes) to learn the
   semantics we actually need before committing to an MDX grammar.
2. **MDX surface (later).** A parser and evaluator for a defined subset: axes,
   tuples, set expressions, and reuse of the Phase 3 evaluator.
3. **Unified semantic layer.** Ensure MDX and SQL resolve members, hierarchies,
   and calculations through the *same* engine entry points so behavior cannot
   diverge.

**Definition of done (intermediate).** The richer SQL surface is stable and
exercised by golden tests, and its semantics are documented as the contract MDX
must satisfy.

**Explicit non-goal for now.** A complete, spec-conformant MDX implementation.
This is a multi-phase project of its own, not "a next step."

---

## Where things stand

**Phase 1b — the versioned (MVCC) storage engine — is largely landed.**

* **M1** the trie is persistent (`Arc<Node>`, path-copy writes). `snapshot()` is O(1).
* **M2** `Cube` holds a `VersionStore`: a version that advances on write and a
  version-tagged cache. The four global `query_cache.clear()` calls are gone.
* **M3** the server's catalog-wide `Mutex` is gone. Reads take a snapshot and never
  block a writer; `Cube` is `Sync`.
* **M4** (structural dimension changes versioned) is **deferred — re-assess, not
  fixed.** The original reason ("no concurrent mutation surface") is now only
  partly true: the HTTP write endpoint exists, and `createMissing: true` creates
  members while readers are active (`cube.write` → `Dimension::add_leaf`). What
  is still CLI-only is hierarchy *structure* (`rollup`/`detach`/`order`), which
  is the race M4 was designed to close, so it is not urgent — but the premise
  must be re-checked before it is quoted again. The part of M4 with value today
  landed anyway: structural changes bump every cube's version, so a cached
  aggregate cannot survive a `.rollup`
  (`versioning_tests::a_structural_change_invalidates_cached_aggregations`).

The measure-before-committing gate was honoured: `bench_mvcc` measured the old store
first, the first gate FAILED, and the cost was re-derived and **accepted explicitly**
(writes 2.7 KB / 2.8 µs, scans +33%/+20%) on the property that matters — per-write cost
is bounded by fan-out, not by cube size. Details in `PHASE-1B-DESIGN.md`.

**Done: exact `COUNTD`.** It counts distinct BASE leaf values of the measure among
the group's non-empty cells: weights ignored (like AVG/MIN/MAX), empty group =
absent (`null`, never 0), numeric equality exact (`0.0 == -0.0`, all NaNs one
value, ±inf distinct), text exact and case-sensitive.
The per-group set is allocated only for COUNTD, so the other aggregations keep
their O(1) state. SQL accepts `COUNTD(x)` and `COUNT(DISTINCT x)` (they agree) and
refuses DISTINCT on other aggregates; COUNTD inside a calculated-field formula is
still refused — a documented limitation. Known pre-existing quirk, left unfixed:
`COUNT` over a text/attribute column returns the last text, not a cell count;
COUNTD is the supported text aggregation.

**Open work** (verified against the code, 2026-10-28; ranked by user-visible value
per unit of risk, because some items are blocked on a Plateau-facing protocol
decision rather than on engine code):

1. **MEDIAN / STDEV / VAR** — MISSING, M. The non-additive aggregations still open:
   STDEV/VAR fit a streaming (Welford) accumulator, MEDIAN needs the values per
   group. COUNTD's per-group plumbing is now in place to share.
2. **Temporal bucketing (`datePart` / DATE_TRUNC)** — MISSING, L. `groupBy[].datePart`
   is a protocol field and Plateau already sends it, but the engine refuses it
   ("no temporal bucketing yet"). Needs a date/truncation model, so it is
   Plateau-facing design work before it is code.
3. **Measure-scope filters / Top-N** — MISSING, M–L. Refused with a typed error
   today, and `capabilities.filterOperators` says so honestly; needs a
   post-aggregation pass that interacts with ordering and `limit`.
4. **Dense (period) average** — design only, L, **blocked on a protocol decision**:
   `AGGREGATIONS-DESIGN.md` §8–8b leaves the aggregation name vs per-dimension
   denominator shape undecided, and it needs a period axis (see item 2).
5. **LOD expressions** — MISSING by design, L, low value: `supportsLod` is
   `false` and Plateau already computes LOD client-side, so engine support is a
   convenience, not a gap users hit today.

Also verified, not ranked above: SQL `GROUP BY`/`HAVING` clauses are parsed but
not read by the SQL front end (grouping derives from the projection) — the same
silent-ignore class that `ORDER BY` was fixed for. Calculated fields implement the
v1 set only (`ZN`/`IFNULL`; aggregates `SUM/AVG/COUNT/MIN/MAX`) — `COUNTD` is a
query aggregation, not a formula function. String and date functions, the `ROUND`
family and `CASE` are not implemented.

Client-facing changes are tracked in `WIRE-CHANGES.md`, which is the file the Plateau
session reads first.

---

## Tracking

| Phase | Theme | Depends on | Status |
|------:|---------------------------------------|------------|-------------|
| 1a | Durable persistence (atomic save) | — | **Done** |
| 1b | Versioned/MVCC storage engine | 1a | **M1–M3 done**; M4 deferred — **re-assess**: the HTTP write endpoint can create members concurrently, so the "no mutation surface" premise is only partly true (hierarchy structure is still CLI-only) |
| 1c | Protocol + server/client split | 1a, 1b | **Done for reads and writes**: protocol v1, HTTP server and Plateau client, plus writeback (`POST /v1/{dataset}/write`, `POST /v1/save`, `supportsWrite: true`). Batch-validated, last-write-wins; no cross-request transactions or `ifVersion` (deliberate) |
| 2 | Users, security, access limits | 1c | **S1–S5 done** (users/roles/tokens, table, dimension, write and join access). Open: S6 (sessions pin a security snapshot) and the recorded S2/S3 limits (`describe` cardinality, no `clear_rule`) |
| 3 | Calculations | 1c, 2 | **In part**: SUM/AVG/COUNT/MIN/MAX plus exact COUNTD (`countDistinctExact: true`; distinct base leaf values), calculated fields over the client's AST (`ZN`/`IFNULL`). Open: MEDIAN/STDEV/VAR, dense average, LOD; SQL `ORDER BY` now sorts |
| 4 | Multidimensional syntax (MDX) | 1c, 2, 3 | Not started |

Statuses were checked against the code on 2026-10-28, including the exact `COUNTD`
landing; `WIRE-CHANGES.md` is the client-facing record, and `git log` is the truth.

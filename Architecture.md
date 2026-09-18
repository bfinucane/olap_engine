# Architecture Notes
* Flat vs. Pivot: Standard SQL returns flat tables. However, if a cube is defined with a MEASURE dimension, the SQL engine will perform a "Measure Pivot" to return standard relational 2D results.
* Persistence: The database automatically saves its state to database.bin upon typing .exit and reloads it on startup.

## Multi-Hierarchy Dimensions
A dimension owns one or more **hierarchies** over the *same* base (leaf) members.
Every dimension is born with a **default hierarchy** named after the dimension, so
single-hierarchy usage is unchanged. A **dimension name** is a dynamic alias for
that dimension's default hierarchy; a **hierarchy name** stands for itself.
Hierarchy names are unique across the database.

**Where data lives:** measures are stored **only at leaf coordinates**. A leaf
belongs to every hierarchy of its dimension, so a single stored value aggregates
differently under each hierarchy purely through that hierarchy's tree — no
per-hierarchy duplication. Consequently, writes (`INSERT`, CSV `.import`) resolve
a member against the dimension's **default hierarchy**; only the read path
(`SELECT` / `.splash` / `.tree`, optionally with a `Hierarchy:Member` qualifier)
addresses a specific hierarchy.

## Roadmap
See [ROADMAP.md](ROADMAP.md) for the intended direction of the project. The plan
is deliberately **inside-out**: harden the core (architecture, concurrency,
persistence) before layering user-facing features (security, calculations, MDX)
on top.

## Durability vs. Versioning (Phase 1a vs. 1b)

Two related but distinct ideas, both called "a snapshot":

* **Durability snapshot (Phase 1a).** A consistent point-in-time copy taken
  *for the act of writing to disk*. It freezes everything for the duration of
  the write. Cheap, contained; its only goal is that a crash cannot truncate the
  database.

* **MVCC version store (Phase 1b).** State held as a set of immutable pieces
  (Infor-OLAP uses *pages*; here the natural unit is **Patricia-trie
  nodes/subtrees**, which the trie already shares). A *version* is the set of
  pieces it is made of. Writers use **copy-on-write** (only changed pieces are
  new); long readers keep the version they began on and never observe a writer's
  changes. Versions retire and their unreferenced pieces are reclaimed once no
  reader is pinned to them. This is what delivers *concurrency* — 1a does not.

Copy-on-write has real **memory-churn** cost (Infor wrote their own `malloc`
replacement because the platform allocator was too slow for page churn). Phase
1b must **measure** churn before committing to the model.




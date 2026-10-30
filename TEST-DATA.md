# Test data and database version — the pinned definition

This file defines **what "the test database" means** and how to prove you are looking at it.
The machine-readable source is [`model-manifest.v1.json`](model-manifest.v1.json); this page
explains it. Nothing here is stored in the database file itself — the file is git-ignored, so the
**manifest + the seed + the content fingerprint** are the definition, and the frozen release is
the exact artefact.

## 1. Identity of a database deployment

A running engine is characterised by three independent version axes. Record all three; none of
them alone is "the version".

| Axis | What it is | Where it comes from |
| --- | --- | --- |
| **Engine** | package `0.1.0`, engine git commit, `protocolVersion 1`, storage `FORMAT_VERSION 1` | `Cargo.toml`, `src/engine/plan.rs`, `src/catalog/catalog.rs` |
| **Contract** | `contract/PROTOCOL.md` v1 (owner-controlled) | `contract/STATUS.md` |
| **Model** | `Plateau default (seeded superstore) v1` — the dataset/column/row manifest below + **file SHA-256** + **content fingerprint** | `model-manifest.v1.json` |

The current pinned identity (2026-10-30, engine `bbaecc1`):

```
engine    0.1.0 @ bbaecc1, protocol 1, storage format 1
contract  v1
model     v1  "Plateau default (seeded superstore)"
          file size           42,978 B
          file sha256         F389C30E97EFBAF6DA6A97673A75C0F51AAC7AB385A6B9EC7091E98DFD4104F0
          content fingerprint E53819FB218FA9DD801347686FBDD944B123ED87AAA8A068DE2E4066B753707E
```

## 2. Where the artefacts live

| Path | What | Rule |
| --- | --- | --- |
| `shared\database.bin` | the frozen model the shared 8080 server serves | replaced only by a promoted release (D14); hash recorded in `contract/SHARED-ENGINE.md` |
| `shared\serve.exe` | the frozen engine binary for that release | same |
| `Patricia\olap_engine\database.bin` | the seeded reference | source of truth for regeneration; never served directly, never REPL-ed |
| `dev-work\database.bin` | the dev instance's scratch copy | may change freely; not the agreed model |

The database file is **git-ignored**, so a change to it is not recoverable from git. That is why
the exact release file is frozen in `shared\` and identified by SHA-256, and why the content
fingerprint (regeneration-stable) is recorded too.

## 3. The model (v1)

Seven user datasets, plus eight derived `#` metadata/control cubes. `Sales` and `Pivot` carry
the facts; `Attributes` and `RegionAttr` are attribute tables. **Known-empty:** `CostCentre`,
`Geography` and `ProductClass` carry populated *dimensions* (8 / 53 / 20 members) but **zero
cells** in this model — clients testing measure cubes must not expect rows there.

| Dataset | Columns (kind:cardinality) | Rows |
| --- | --- | --- |
| `Sales` | Region:dimension:4 · Segment:3 · Category:3 · value:measure:36 | 36 |
| `Pivot` | Region:4 · Category:3 · Measure:3 · value:36 | 36 |
| `CostCentre` | Account:8 · Measure:3 (measure dimension) | 0 |
| `Geography` | Geo:53 · Measure:3 (measure dimension) | 0 |
| `ProductClass` | Product:20 · Measure:3 (measure dimension) | 0 |
| `Attributes` | Region:4 · Category:3 · value:attribute:4 | 6 |
| `RegionAttr` | Region:4 · Attribute:1 (attribute table) | 4 |

Behavioural checks (must match exactly to £0.005):

| Check | Value |
| --- | --- |
| `Sales` by Region — Central | 6,436,941.57 |
| — East | 6,319,711.15 |
| — South | 6,441,942.55 |
| — West | 7,564,832.16 |
| — **grand total** | **26,763,427.43** |
| leaf groups (Region × Segment × Category) | 36 |

## 4. Verifying

```powershell
# Exact frozen artefact (shared release): size + SHA-256 + content fingerprint
Patricia\olap_engine\scripts\verify-model.ps1 -DatabaseFile shared\database.bin

# A file whose provenance you want to prove: also regenerate from the seed
Patricia\olap_engine\scripts\verify-model.ps1 -DatabaseFile shared\database.bin -Regenerate

# A running engine (datasets, columns, cardinalities, row counts, Sales checks)
Patricia\olap_engine\scripts\verify-model.ps1 -BaseUrl http://127.0.0.1:8080/v1 -Token <bearer>
```

Exit `0` = it matches the pinned model; `1` = mismatch (every differing field printed);
`2` = setup failure (no manifest, no file, server unreachable).

## 5. Regenerating the model

```
cd Patricia\olap_engine
target\debug\olap_engine.exe --db <scratch>\new.bin examples\Plateau\seed.sql
```

The seed is self-contained: script mode starts from an empty catalogue and ends with `.save`.
**Byte-level reproducibility is impossible** — bincode serialises HashMap-backed catalogues in
iteration order, so two runs give the same model, the same size, different SHA-256. Therefore:

- a **regeneration** is proven by the **content fingerprint**, never by comparing bytes;
- a **release** is pinned by the **exact file SHA-256** of the frozen artefact, which the owner
  reproduces independently.

## 6. Changing the model (versioning policy)

Any change to datasets, columns, cardinalities, row counts or the behavioural checks is a **new
model version**:

1. bump `modelVersion` in the manifest (v2, …) and update every field;
2. publish the new file SHA-256 and content fingerprint (regenerate first; fingerprint must be
   stable across two runs);
3. the user promotes a new frozen pair (D14) and broadcasts the restart;
4. the contract owner records it in `contract/SHARED-ENGINE.md`; a model change is a separate
   release event from an engine-code change.

Clients may assert the model themselves with `GET /v1/datasets` + `GET /v1/{dataset}`
(column names/cardinality/rowCount) and the Sales checks above; they cannot see the file hash, so
the manifest is the authority for that.

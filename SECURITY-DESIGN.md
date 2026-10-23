# Security Ã¢â‚¬â€ design proposal

**Status:** proposal, for decisions. No code written.
**Model:** Infor-style, per the user's description (users, roles, three kinds of access,
default ACCEPT). TM1 is referenced only where its naming helps; Infor is authoritative.
**Prerequisite:** Ã‚Â§0 (system cubes), which must land before any of this.

---

## 0. Prerequisite: read-only, automatically maintained system cubes

Before security, the engine needs tables that **describe the database itself** and stay
current on their own. Without them, a front end cannot list what exists Ã¢â‚¬â€ which is the
"lists" problem the user hit in MiniSheet Ã¢â‚¬â€ and security has nothing to hang its
dictionaries on.

Proposed system cubes, named with a `#` prefix Ã¢â‚¬â€ the Infor convention, chosen by the user
over TM1's `}`:

| Cube | Gives |
|---|---|
| `#Cubes(Cube, Property)` | every cube, with its measure dimension and whether it is an attribute table |
| `#Dimensions(Dimension, Property)` | every dimension, with its hierarchy count |
| `#Hierarchies(Dimension, Hierarchy)` | every hierarchy, including the default |
| `#Members(Dimension, Hierarchy, Member)` | every member of every hierarchy, consolidated ones included |
| `#Edges(Dimension, Hierarchy, Member, Parent)` | parent/child pairs with the rollup weight: the structure a client folds into a tree |
| `#Columns(Cube, Column, Property)` | the schema of EVERY cube (kind, dataType, cardinality), system cubes included |

**The `#` has one consequence worth stating**, because it is invisible until it bites:
in a URL, `#` begins the fragment, so a client addressing `/v1/%23Cubes` must
percent-encode it. The engine's router currently skips percent-decoding, so it does not
yet accept the encoded form Ã¢â‚¬â€ it must decode the dataset segment before matching. In SQL
there is no such problem: `SELECT Cube FROM #Cubes` parses unquoted (the quoted form does
not, which is true of this dialect generally, not specific to `#`).

Properties as a `Property` dimension (rather than columns) keeps them extensible without a
schema change, and mirrors how TM1 models control cubes.

Behaviour:

* **Read-only.** A write to a `#` cube is refused with a clear error; nothing in the engine
  writes them by hand.
* **Automatically refreshed** when the catalogue changes: on load, after `CREATE TABLE`,
  after any structural edit (the same moments that bump versions today), and after a
  member is created by a write.
* **Queryable like any cube** Ã¢â‚¬â€ that is the point: a client lists the database by sending a
  normal query, so there is no second metadata API to keep in sync.
* **Flagged** in `describe` as `isSystem: true`, so a UI can hide them from the cube picker
  while still being able to query them.

The security dictionaries in Ã‚Â§2 reuse `#Members` directly: `Role`, `Dimension` and `Member`
are ordinary dimensions whose members are created as rules are written.

---

## 1. Model

**Users Ã¢â€ â€™ Roles.** Groups are deliberately postponed (user's decision): they are a role
alias, and adding them later does not change the permission algebra. A user may hold
several roles; the effective permission is the **union** across roles, then narrowed by the
dimension and table rules (Ã‚Â§3).

**Default: ACCEPT.** A cube, dimension or member with no rule is visible at full access.
This is Infor's behaviour, chosen by the user. Its cost is stated honestly: a *newly added*
cube or member is visible to everyone until someone restricts it. Deny-by-default fails the
other way Ã¢â‚¬â€ new members are invisible until granted. Neither is free; this one fails open.

## 2. Control cubes

| Cube | Keyed by | Value |
|---|---|---|
| `#Users` | `User` | (existence; credentials live in the catalogue, not in a cube Ã¢â‚¬â€ see Ã‚Â§5) |
| `#Roles` | `Role` | (existence) |
| `#UserRoles` | `User, Role` | 1 |
| `#TableSecurity` | `Role, Cube` | `READ` \| `WRITE` \| `NONE` |
| `#DimensionSecurity` | `Role, Dimension, Member` | `READ` \| `WRITE` \| `NONE` |

`Member` accepts the wildcard `#ALL` for "the whole dimension", so a role is not forced to
enumerate 5,000 members to restrict one. A specific member row overrides the wildcard.

**Cell/slice security is out of scope for v1** (user's decision: rarely used, and the
per-cell evaluation cost is real). The schema reserves the shape Ã¢â‚¬â€ one
`#CellSecurity_<Cube>` per secured cube, keyed by the cube's own dimensions plus `Role` Ã¢â‚¬â€
so adding it later is additive, not a redesign. The user's observation matters here:
in practice those cubes have far fewer dimensions than the cubes they control, which is
why deferring is cheap.

## 3. Resolution and enforcement

**Effective level for a cell = the minimum of:**

1. **Table** Ã¢â‚¬â€ the role's level for the cube (`#TableSecurity`, default ACCEPT).
2. **Dimension** Ã¢â‚¬â€ for each of the cube's dimensions, the role's level for that member
   (`#DimensionSecurity`, most specific row wins, default ACCEPT).

Levels order as `NONE < READ < WRITE`, and the minimum applies: a level can only narrow.

Enforcement lands at two chokepoints that already exist:

* **Table**: the cube lookup in the plan/wire layer. A cube at `NONE` is reported as
  `unknown_dataset` Ã¢â‚¬â€ indistinguishable from a cube that does not exist, so the error does
  not reveal structure.
* **Dimension**: `query_slice`'s `scanner_filters`, the per-dimension leaf-id sets. Leaves
  the role may not read are removed *before* aggregation, so a total **changes** rather
  than announcing that a member exists but is hidden. That is the "no silent leak" property
  the roadmap asks for.

**Joins must check both sides.** An attribute join reaches into a second cube and a second
dimension: the joined cube needs table access, and the key dimension needs dimension access.
A join that skips this is a permission bypass, so it gets a test on day one.

## 4. What a denial looks like

| Situation | Result |
|---|---|
| Forbidden cube on read | `unknown_dataset` (looks absent) |
| Forbidden member on read | folded out of the leaf set; totals change; no error |
| Forbidden write | **loud error** Ã¢â‚¬â€ silently dropping a write is data loss |
| Forbidden attribute in a join | the attribute is absent for those keys |

Reads fail quietly because an error would confirm existence; writes fail loudly because
there is no silence that is safe.

## 5. Identity and transport

* `POST /v1/login {user, password}` Ã¢â€ â€™ `{token, expiresAt}`; passwords hashed with `argon2`
  and stored **in the catalogue**, not in a cube (a cube is queryable; hashes must not be).
* Every request carries `Authorization: Bearer <token>`.
* Tokens are random, stored hashed in memory, and expire; a restart forces re-login.
* **SSO is deferred but designed for**: it is only a second way to *acquire* a session Ã¢â‚¬â€ a
  trusted `X-Remote-User` header accepted when the server was started with a trust flag.
  Enforcement is identical, so it can be added without touching Ã‚Â§3.

## 6. Open / required (decision pending)

**Recommendation: required by default**, with `--allow-anonymous` for development. The
alternative Ã¢â‚¬â€ open while no users exist Ã¢â‚¬â€ is friendlier but makes "forgot to configure it"
indistinguishable from "deliberately open", which is the wrong default for a security
feature.

## 7. Slices

| # | Slice | Proof |
|---|---|---|
| S0 | **DONE** - system cubes (section 0), read-only and auto-refreshed | #Cubes/#Dimensions/#Hierarchies/#Members describe the catalogue; a write to a # cube is refused; the lists update after CREATE TABLE and after a new member; describe flags them isSystem; covered by catalog::system_cubes unit tests and serve_http decoding tests |
| S1 | **DONE** — users, roles, `POST /v1/login`, bearer tokens | login returns a token; a request without one is `401` when auth is required; a bad password is `401` without saying which part failed |
| S2 | table access | a role at `NONE` on a cube sees `unknown_dataset`; another role sees it |
| S3 | dimension access with wildcard + overrides | a denied member is absent from rows *and* from totals; the wildcard covers the rest |
| S4 | write permissions | a `READ`-only role's write is refused; an `ACCEPT`-default write still works |
| S5 | joins respect both sides | a join to a forbidden cube is absent; a join on a forbidden dimension yields absent attributes, not an error |
| S6 | session pins a security snapshot | changing a rule invalidates sessions lazily, the way data versions do |

Each slice leaves the suite green. S0 is independent and useful on its own Ã¢â‚¬â€ it is also
what MiniSheet needs for lists, with or without security.

## 8. Decisions still open

1. **Auth mode**: required (recommended) or open until the first user exists?
2. **Bootstrap**: how does the first admin get created Ã¢â‚¬â€ a CLI flag (`--add-user admin
   --role admin`) or a generated password printed once at first start?
3. **Wildcard spelling** for "whole dimension": `#ALL` inside `Member`, or a separate
   `#DimensionSecurity(Role, Dimension)` row?
4. **System cube visibility**: hidden from `describe`/`datasets` for non-admins, or listed
   with `isSystem: true` and left to the client to filter?
5. **Read-only roles and the SQL console**: the console runs as whoever logs in; should a
   `READ`-only role even see it, given it only issues SELECTs?

---

## 9. Implementation notes and deviations (S1)

Three decisions had to be made while building S1, each against a constraint the design did
not know about. They are recorded rather than silently absorbed.

1. **PBKDF2-HMAC-SHA256 instead of argon2.** This workspace builds with `--offline` against
   a 17-crate cache and cannot reach crates.io, so no crypto crate can be added. Rather
   than ship no hashing, `security::hash` implements SHA-256, HMAC-SHA256 and PBKDF2
   against published RFC/FIPS vectors. A stored record keeps an ALGORITHM TAG and its
   iteration count, so moving to argon2 later is a per-user upgrade, not a format change.
   PBKDF2 is not memory-hard: it is adequate for a localhost tool and NOT a substitute for
   argon2 on a network-exposed deployment.

2. **Identity lives in `security.json`, not in the catalogue and not in a cube.** Adding
   `users` to `Catalog` would change the bincode format and make every existing
   `database.bin` fail to load (or, worse, misread). Credentials must also not live in a
   cube, because cubes are queryable. So users, roles and password verifiers sit beside the
   database in a small JSON file: readable and repairable by hand, absent file = no users.
   The `#` security CUBES will still hold permissions (S2), as designed.

3. **Authentication is required by default and the server fails CLOSED.** With no users
   defined it refuses to start and prints the exact `--add-user` command, rather than
   starting open and hoping someone notices. `--allow-anonymous` exists for development but
   must be asked for. The test suites pass it explicitly.

## 10. The writable/read-only collision (for S2)

S0 made every `#`-prefixed cube read-only, on the reasoning that the engine rebuilds them.
The S2 permission cubes (`#TableSecurity`, `#DimensionSecurity`) are the opposite: the whole
point is that an admin WRITES them. `is_system()` (name prefix) therefore cannot remain the
read-only test; the rule becomes:

* **derived** system cubes (`#Cubes`, `#Dimensions`, `#Hierarchies`, `#Members`, `#Edges`,
  `#Columns`) — rebuilt from the catalogue, read-only;
* **control** cubes (`#TableSecurity`, `#DimensionSecurity`, ...) — admin-writable, and
  write access to them is reserved for an admin session once enforcement lands.

Until then the HTTP server has no write endpoint at all, so control cubes are writable only
from the CLI that owns the catalogue.
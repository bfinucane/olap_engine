# Handoff to the Plateau session — the engine now requires authentication

**Engine side:** commit `4428f8d` plus the two follow-ups in this handoff
(`capabilities` made public, `Authorization` allowed through CORS).
**Your tree is untouched.** Everything here is a wire change; the engine session does not
edit `tableau-clone/`.

---

## 1. What changed

Every `/v1` route now requires a bearer token, **except two**:

| Public | Why |
| --- | --- |
| `POST /v1/login` | it issues the token |
| `GET /v1/capabilities` | it is how a client discovers that a token is needed |

Everything else (`/v1/datasets`, `/v1/{dataset}`, `/v1/{dataset}/sample`,
`/v1/{dataset}/query`, `POST /v1/sql`) returns **401** without a valid token.

**Until Plateau logs in, it will get 401 on every data call.** That is expected, not a bug.

## 2. Feature-detect, do not assume

`GET /v1/capabilities` is public and carries a new field:

```json
{ "protocolVersion": 1, "authRequired": true, ... }
```

- `authRequired: false` — the engine is running in development mode (`--allow-anonymous`).
  Behave as before, no login screen.
- `authRequired: true` — show a login gate BEFORE any other request. Do not fire a burst of
  data requests and let them 401.

Because the flag is public, the discovery sequence is:

```
GET /v1/capabilities            (no token)
  authRequired === false -> continue as today
  authRequired === true  -> POST /v1/login, then continue with the token
```

## 3. Login

```
POST /v1/login
Content-Type: application/json
{"user": "alice", "password": "..."}

200 {"token": "<64 hex chars>", "user": "alice", "roles": ["reader"], "expiresAt": 1792805349}
401 {"error": "Invalid user name or password.", "code": "unauthorized"}
400 {"error": "...", "code": "invalid_argument"}      // malformed body
405 {"error": "...", "code": "method_not_allowed"}    // not a POST
```

- `expiresAt` is a **Unix timestamp in seconds** (not milliseconds).
- A wrong password and an unknown user return the **same** 401 body. Do not try to tell the
  user which was wrong; the engine deliberately does not.
- `roles` is informational for now. Permissions attached to roles arrive in engine S2; do
  not build behaviour on role names yet beyond displaying them.

## 4. Sending the token

Every protected request carries:

```
Authorization: Bearer <token>
```

Implement it once in a fetch wrapper so no call site can forget it — including `sample`,
`describe`, `query`, and the SQL passthrough. Do not put the token in a query string.

## 5. Handling 401 during a session

Any `401` with `code: "unauthorized"` means the token is missing, unknown or expired
(sessions last 12 hours).

1. Drop the stored token.
2. Show the login screen.
3. Do **not** retry the request in a loop; a retry storm against a required-auth engine is
   how this goes wrong.

There is no refresh endpoint. Re-login is the refresh.

## 6. Token storage

It is a bearer credential:

- keep it in memory or `sessionStorage` — not `localStorage` for a shared machine;
- never log it, never put it in a URL, never show it in the UI;
- a page reload may require a re-login; that is acceptable for now.

## 7. Logout

There is no logout endpoint yet: dropping the token client-side is sufficient (the server
session simply expires). If you want a real "sign out everywhere", ask and the engine
session will add `POST /v1/logout` (revoke).

## 8. CORS: the engine must run with `-Cors` for the dev server

Plateau's dev server is a different origin, so the engine must be started with CORS:

```
start-engine.cmd -Cors
```

The engine now advertises:

```
Access-Control-Allow-Headers: Content-Type, Authorization
```

Both matter: a browser sends a **preflight** before a request that carries
`Authorization`, and the preflight only passes if `Authorization` is listed. That was a
real bug in the first version of the auth change and it is fixed and tested.

## 9. The engine's SQL console page (`GET /sql`)

The page loads (it is just HTML), but its queries go through `POST /v1/sql`, which now needs
a token, and that page has no token field yet. **Do not link to it from Plateau** until the
token input is added (engine-side, pending). The engine session can do it on request.

## 10. Working without auth (UI development)

If you need the old no-auth behaviour while building:

```
start-engine.cmd -Cors -Anonymous
```

(or start `serve.exe ... --allow-anonymous`). Then `capabilities.authRequired` is `false`
and every route behaves as before. This is development only; do not ship a build that
depends on it.

## 11. Running an engine with auth for testing

Creating users is a CLI action, not a wire one:

```
# first time (prompts for a password)
Patricia\olap_engine\target\debug\serve.exe --add-user alice --role reader

# inspect
Patricia\olap_engine\target\debug\serve.exe --list-users

# start it
start-engine.cmd -Cors
```

Users and password verifiers live in `security.json` **beside the database file** the
engine was started with (`database.dev.bin` -> `security.json` in the engine directory).
The engine refuses to start when auth is required and no users exist — that is deliberate,
and it prints the exact `--add-user` command.

## 12. Compatibility

| Engine | `authRequired` | Plateau behaviour |
| --- | --- | --- |
| old (no auth) | field absent | treat as `false`; no login |
| new, `--allow-anonymous` | `false` | no login |
| new, default | `true` | login required |

Treat a missing field as `false`, so a Plateau build that ships before the engine upgrade
keeps working.

## 13. Paste-ready ENGINE-PROTOCOL.md text (that file is yours to edit)

> ### Authentication (protocol v1, additive)
>
> Every route requires `Authorization: Bearer <token>` except:
> `POST /v1/login` and `GET /v1/capabilities`.
>
> `GET /v1/capabilities` gains `"authRequired": boolean`. When `true`, a client must log in
> before any other call. When the field is absent (older engine), treat it as `false`.
>
> **Login**
>
> ```
> POST /v1/login
> {"user": string, "password": string}
>
> 200 {"token": string, "user": string, "roles": string[], "expiresAt": integer}
> 401 {"error": "Invalid user name or password.", "code": "unauthorized"}
> ```
>
> `expiresAt` is a Unix timestamp in seconds. A wrong password and an unknown user are
> indistinguishable by design.
>
> **Unauthorised responses** (any protected route without a valid token):
> `401 {"error": "Authentication required. POST /v1/login to obtain a bearer token.",
> "code": "unauthorized"}`.
>
> Auth is checked before routing, so an unauthenticated request to an unknown path is also
> 401: it cannot be used to probe which datasets exist.
>
> CORS (`--cors`) allows `Access-Control-Allow-Headers: Content-Type, Authorization`.

## 14. What to ask the engine session for

- `POST /v1/logout` (token revoke) — small.
- A token field on the `GET /sql` console page — small.
- Anything missing from this document that you need; the engine's `WIRE-CHANGES.md` is the
  running client changelog and is updated with each wire change.
